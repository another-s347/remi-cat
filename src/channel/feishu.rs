use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use bot_core::im_tools::{encode_agent_file_key, SubSessionBindingUpsertRequest};
use bot_core::{
    CatEvent, Content, ContentPart, ContextMetrics, ImAttachment, ImDocument, StreamOptions,
    TokenUsage,
};
use futures::StreamExt;
use im_feishu::{FeishuEvent, FeishuGateway, FeishuMessage};
use remi_agentloop::types::SubSessionEventPayload;
use tracing::{info, warn};
use user_store::UserStore;

use crate::app::{
    parse_session_reasoning_effort, CLI_CHANNEL, FEISHU_CHANNEL, SESSION_AGENT_ID_METADATA_KEY,
    SESSION_DEBUG_METADATA_KEY, SESSION_MODEL_PROFILE_METADATA_KEY,
    SESSION_REASONING_EFFORT_METADATA_KEY,
};
use crate::channel::{Channel, ChannelKind};
use crate::command::{process_runtime_commands, RuntimeCommandPipelineResult};
use crate::config::FeishuTransport;
use crate::core::{
    append_direct_sub_session_turn, sub_session_input_target, Runtime, SubSessionInputTarget,
};

#[path = "feishu/actions.rs"]
mod actions;
#[path = "feishu/bridge.rs"]
mod bridge;
#[path = "feishu/format.rs"]
mod format;
#[path = "feishu/reply_stream.rs"]
mod reply_stream;
#[path = "feishu/routing.rs"]
mod routing;
#[path = "feishu/settings.rs"]
mod settings;
#[path = "feishu/sub_session.rs"]
mod sub_session;

use actions::process_feishu_card_action;
pub(crate) use bridge::LocalImFileBridge;
use format::{
    fenced_block, format_feishu_sub_session_line, format_supervisor_progress, supervisor_reply_kind,
};
pub(crate) use format::{format_context_compaction_line, format_feishu_tool_line};
pub(crate) use reply_stream::FeishuReplyKind;
use reply_stream::FeishuReplyStream;
pub(crate) use routing::{
    feishu_session_channel_id, feishu_topic_channel_id, should_ignore_unaddressed_topic_start,
};
pub(crate) use settings::im_mode_from_env;
use settings::{feishu_hook_config_from_env, feishu_transport_from_env};
use sub_session::record_sub_session_event;

pub(crate) struct FeishuChannel {
    gateway: FeishuGateway,
}

impl FeishuChannel {
    pub(crate) fn new(gateway: FeishuGateway) -> Self {
        Self { gateway }
    }
}

impl Channel for FeishuChannel {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Feishu
    }

    fn run<'a>(
        &'a self,
        runtime: Rc<Runtime>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
        let gateway = self.gateway.clone();
        Box::pin(async move { run_feishu(runtime, gateway).await })
    }
}

pub(crate) async fn run_feishu(runtime: Rc<Runtime>, gateway: FeishuGateway) -> anyhow::Result<()> {
    info!("remi-cat single runtime starting Feishu gateway");
    let mut rx = match feishu_transport_from_env() {
        FeishuTransport::WebSocket => gateway.start().await?,
        FeishuTransport::EventHook => {
            gateway
                .start_event_hook(feishu_hook_config_from_env()?)
                .await?
        }
    };
    while let Some(event) = rx.recv().await {
        match event {
            FeishuEvent::MessageReceived(msg) => {
                let runtime = Rc::clone(&runtime);
                let gateway = gateway.clone();
                tokio::task::spawn_local(async move {
                    if let Err(err) = process_feishu_message(runtime, gateway, msg).await {
                        warn!("failed to process Feishu message: {err:#}");
                    }
                });
            }
            FeishuEvent::ReactionReceived(reaction) => {
                let text = format!("[user reacted with {}]", reaction.emoji_type);
                let msg = FeishuMessage {
                    message_id: reaction.message_id,
                    sender_user_id: reaction.sender_user_id,
                    chat_id: reaction.chat_id,
                    chat_type: "group".to_string(),
                    text,
                    images: Vec::new(),
                    files: Vec::new(),
                    documents: Vec::new(),
                    parent_id: None,
                    thread_id: reaction.thread_id,
                    at_bot: true,
                    mentions: Vec::new(),
                };
                let runtime = Rc::clone(&runtime);
                let gateway = gateway.clone();
                tokio::task::spawn_local(async move {
                    if let Err(err) = process_feishu_message(runtime, gateway, msg).await {
                        warn!("failed to process Feishu reaction: {err:#}");
                    }
                });
            }
            FeishuEvent::Unknown { event_type, .. } => {
                info!("ignored event type: {event_type}");
            }
            FeishuEvent::CardAction {
                card_message_id,
                action_value,
                user_open_id,
            } => {
                let runtime = Rc::clone(&runtime);
                let gateway = gateway.clone();
                tokio::task::spawn_local(async move {
                    if let Err(err) = process_feishu_card_action(
                        runtime,
                        gateway,
                        card_message_id,
                        action_value,
                        user_open_id,
                    )
                    .await
                    {
                        warn!("failed to process Feishu card action: {err:#}");
                    }
                });
            }
        }
    }
    Ok(())
}

async fn process_feishu_message(
    runtime: Rc<Runtime>,
    gateway: FeishuGateway,
    msg: FeishuMessage,
) -> anyhow::Result<()> {
    let channel_id = feishu_session_channel_id(&msg);
    let session_exists = runtime
        .sessions
        .lock()
        .await
        .channel_session_id(FEISHU_CHANNEL, &channel_id)
        .is_some();
    if should_ignore_unaddressed_topic_start(&msg, session_exists) {
        info!(
            chat_id = %msg.chat_id,
            thread_id = msg.thread_id.as_deref().unwrap_or(""),
            message_id = %msg.message_id,
            "ignored topic message because the topic session has not been started by an @mention"
        );
        return Ok(());
    }

    let sender_uuid = runtime
        .user_store
        .resolve_or_create(FEISHU_CHANNEL, &msg.sender_user_id);
    let sender_username = ensure_im_username(
        &runtime.user_store,
        &gateway,
        &sender_uuid,
        &msg.sender_user_id,
    )
    .await;
    let reaction_id = gateway.add_reaction(&msg.message_id, "THINKING").await.ok();
    let mut replies = FeishuReplyStream::new(gateway.clone(), msg.message_id.clone());
    collect_bot_reply(
        runtime,
        FEISHU_CHANNEL,
        msg.clone(),
        sender_username,
        Some(&mut replies),
    )
    .await?;
    replies.finish().await;
    if let Some(reaction_id) = reaction_id {
        gateway
            .delete_reaction(&msg.message_id, &reaction_id)
            .await
            .ok();
    }
    Ok(())
}

pub(crate) async fn collect_cli_bot_reply(
    runtime: Rc<Runtime>,
    msg: FeishuMessage,
    sender_username: Option<String>,
) -> anyhow::Result<String> {
    collect_bot_reply(runtime, CLI_CHANNEL, msg, sender_username, None).await
}

async fn collect_bot_reply(
    runtime: Rc<Runtime>,
    platform: &str,
    mut msg: FeishuMessage,
    sender_username: Option<String>,
    mut replies: Option<&mut FeishuReplyStream>,
) -> anyhow::Result<String> {
    let channel_id = feishu_session_channel_id(&msg);
    let session_id = runtime.sessions.lock().await.resolve_channel(
        platform,
        &channel_id,
        &runtime.root_agent_id,
    )?;
    if platform == FEISHU_CHANNEL && is_fork_command(msg.text.trim()) {
        let reply = handle_feishu_fork_command(&runtime, &session_id, &msg).await?;
        append_reply_chunk(
            &mut String::new(),
            &mut replies,
            FeishuReplyKind::Text,
            &reply,
        )
        .await;
        return Ok(reply);
    }
    let direct_sub_session_target = sub_session_input_target(&runtime, &session_id).await;
    if let Some(SubSessionInputTarget::Acp { acp_session_id, .. }) = &direct_sub_session_target {
        let reply = runtime
            .bot
            .acp_bound_message(acp_session_id, msg.text.trim())
            .await
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        append_direct_sub_session_turn(&runtime, &session_id, msg.text.trim(), &reply).await;
        append_reply_chunk(
            &mut String::new(),
            &mut replies,
            FeishuReplyKind::Text,
            &reply,
        )
        .await;
        return Ok(reply);
    }
    let (command_prefix, skill_injections, stream_thread_id) =
        if let Some(SubSessionInputTarget::Agent { sub_thread_id, .. }) = direct_sub_session_target
        {
            (String::new(), Vec::new(), sub_thread_id)
        } else {
            match process_runtime_commands(&runtime, &session_id, msg.text.trim()).await? {
                RuntimeCommandPipelineResult::Reply(reply) => {
                    append_reply_chunk(
                        &mut String::new(),
                        &mut replies,
                        FeishuReplyKind::Text,
                        &reply,
                    )
                    .await;
                    return Ok(reply);
                }
                RuntimeCommandPipelineResult::Continue {
                    text,
                    prefix,
                    skill_injections: injections,
                } => {
                    msg.text = text;
                    (prefix, injections, session_id.clone())
                }
            }
        };

    let im_attachments = msg
        .files
        .iter()
        .map(|f| ImAttachment {
            key: encode_agent_file_key(&msg.message_id, &f.file_key),
            name: f.file_name.clone(),
            mime_type: f.mime_type.clone(),
            size_bytes: f.size_bytes,
            file_type: f.file_type.clone(),
        })
        .collect();
    let im_documents = msg
        .documents
        .iter()
        .map(|d| ImDocument {
            url: d.url.clone(),
            title: d.title.clone(),
            doc_type: d.doc_type.clone(),
            token: d.token.clone(),
        })
        .collect();
    let (model_profile_id, reasoning_effort, agent_id) = {
        let sessions = runtime.sessions.lock().await;
        (
            sessions.metadata_string(&session_id, SESSION_MODEL_PROFILE_METADATA_KEY),
            parse_session_reasoning_effort(
                sessions.metadata_string(&session_id, SESSION_REASONING_EFFORT_METADATA_KEY),
            ),
            sessions.metadata_string(&session_id, SESSION_AGENT_ID_METADATA_KEY),
        )
    };
    let opts = StreamOptions {
        model_profile_id,
        reasoning_effort,
        agent_id,
        skill_injections,
        sender_user_id: Some(msg.sender_user_id.clone()),
        sender_username,
        message_id: Some(msg.message_id.clone()),
        chat_type: Some(msg.chat_type.clone()),
        platform: Some(platform.to_string()),
        todo_create_via_sdk: true,
        trigger_tools_enabled: true,
        im_attachments,
        im_documents,
        ..StreamOptions::default()
    };
    let content = build_message_content(
        &msg.text,
        &[],
        !msg.images.is_empty(),
        msg.files.len(),
        msg.documents.len(),
    );
    let mut output = command_prefix;
    let mut streaming_tool_names = HashMap::<String, String>::new();
    let debug_enabled = runtime
        .sessions
        .lock()
        .await
        .metadata_bool(&session_id, SESSION_DEBUG_METADATA_KEY);
    if !output.is_empty() {
        if let Some(replies) = replies.as_deref_mut() {
            replies.push(FeishuReplyKind::Text, &output).await;
        }
    }
    let mut supervisor_execution_started = false;
    let mut stream =
        std::pin::pin!(runtime
            .bot
            .stream_with_options(&stream_thread_id, content, opts));
    let timeout = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            event = stream.next() => {
                let Some(event) = event else { break };
                match event {
                    CatEvent::Text(delta) => {
                        supervisor_execution_started = false;
                        append_reply_chunk(&mut output, &mut replies, FeishuReplyKind::Text, &delta).await;
                    }
                    CatEvent::Thinking(content) => {
                        let chunk = format!("\n\n**Thinking**\n{}\n", fenced_block("text", &content));
                        supervisor_execution_started = false;
                        append_reply_chunk(&mut output, &mut replies, FeishuReplyKind::Thinking, &chunk).await;
                    }
                    CatEvent::ToolCallStart { id, name } => {
                        streaming_tool_names.insert(id.clone(), name.clone());
                        let pretty = bot_core::PrettyToolCall::started(
                            &id,
                            &name,
                            &serde_json::Value::Object(serde_json::Map::new()),
                        );
                        let line = format_feishu_tool_line(&pretty);
                        supervisor_execution_started = false;
                        update_tool_reply(&mut output, &mut replies, &id, &line, false).await;
                    }
                    CatEvent::ToolCallArgumentsDelta { .. } => {}
                    CatEvent::ToolCall { id, name, args } => {
                        streaming_tool_names.remove(&id);
                        let pretty = bot_core::PrettyToolCall::started(&id, &name, &args);
                        let line = format_feishu_tool_line(&pretty);
                        supervisor_execution_started = false;
                        update_tool_reply(&mut output, &mut replies, &id, &line, false).await;
                    }
                    CatEvent::ToolCallResult {
                        id,
                        name,
                        args,
                        result,
                        success,
                        elapsed_ms,
                    } => {
                        streaming_tool_names.remove(&id);
                        let pretty = bot_core::PrettyToolCall::completed(
                            &id, &name, &args, &result, success, elapsed_ms,
                        );
                        let line = format_feishu_tool_line(&pretty);
                        supervisor_execution_started = false;
                        update_tool_reply(&mut output, &mut replies, &id, &line, true).await;
                    }
                    CatEvent::SubSession(event) => {
                        record_sub_session_event(&runtime, &session_id, platform, &msg, &event).await;
                        if let Some(line) = format_feishu_sub_session_line(&event) {
                            let done = matches!(
                                event.payload,
                                SubSessionEventPayload::Done { .. } | SubSessionEventPayload::Error { .. }
                            );
                            update_sub_session_reply(
                                &mut output,
                                &mut replies,
                                &event.sub_thread_id.0,
                                &line,
                                done,
                            )
                            .await;
                        }
                    }
                    CatEvent::SupervisorProgress(progress) => {
                        let reply_kind = supervisor_reply_kind(&progress);
                        let mut chunk = String::new();
                        if !supervisor_execution_started {
                            chunk.push_str("\n\n---\n\n**Supervisor execution**\n");
                            supervisor_execution_started = true;
                        }
                        chunk.push_str(&format_supervisor_progress(&progress));
                        append_reply_chunk(&mut output, &mut replies, reply_kind, &chunk).await;
                    }
                    CatEvent::Supervisor(report) => {
                        let context = runtime.bot.workflow_status(&session_id).await.map(|instance| instance.context).unwrap_or(serde_json::Value::Null);
                        let chunk = bot_core::supervisor_workflow::format_prefix(&report, &context);
                        append_reply_chunk(&mut output, &mut replies, FeishuReplyKind::Supervisor, &chunk).await;
                        supervisor_execution_started = false;
                    }
                    CatEvent::ContextCompaction(event) => {
                        let line = format_context_compaction_line(&event);
                        let done = !matches!(event.status, bot_core::ContextCompactionStatus::Started);
                        update_context_compaction_reply(&mut output, &mut replies, &event.id, &line, done).await;
                        supervisor_execution_started = false;
                    }
                    CatEvent::ToolApprovalRequested(request) => {
                        if let Some(replies) = replies.as_deref_mut() {
                            replies.approval_requested(&request).await;
                        }
                        supervisor_execution_started = false;
                    }
                    CatEvent::ToolApprovalUpdated(request) => {
                        if let Some(replies) = replies.as_deref_mut() {
                            replies.approval_updated(&request).await;
                        }
                        supervisor_execution_started = false;
                    }
                    CatEvent::ToolApprovalResolved { request, decision } => {
                        if let Some(replies) = replies.as_deref_mut() {
                            replies.approval_resolved(&request, decision).await;
                        }
                        supervisor_execution_started = false;
                    }
                    CatEvent::UserQuestionRequested(request) => {
                        if let Some(replies) = replies.as_deref_mut() {
                            replies.user_question_requested(&request).await;
                        }
                        supervisor_execution_started = false;
                    }
                    CatEvent::UserQuestionUpdated(request) => {
                        if let Some(replies) = replies.as_deref_mut() {
                            replies.user_question_updated(&request).await;
                        }
                        supervisor_execution_started = false;
                    }
                    CatEvent::UserQuestionResolved { request, response } => {
                        if let Some(replies) = replies.as_deref_mut() {
                            replies.user_question_resolved(&request, &response).await;
                        }
                        supervisor_execution_started = false;
                    }
                    CatEvent::Stats {
                        prompt_tokens,
                        completion_tokens,
                        max_prompt_tokens,
                        elapsed_ms,
                    } => {
                        if !debug_enabled {
                            continue;
                        }
                        let (model_profile_id, agent_id) = {
                            let sessions = runtime.sessions.lock().await;
                            (
                                sessions.metadata_string(
                                    &session_id,
                                    SESSION_MODEL_PROFILE_METADATA_KEY,
                                ),
                                sessions
                                    .metadata_string(&session_id, SESSION_AGENT_ID_METADATA_KEY),
                            )
                        };
                        let context_tokens = runtime
                            .bot
                            .model_context_tokens_for_agent(
                                model_profile_id.as_deref(),
                                agent_id.as_deref(),
                            );
                        let context = ContextMetrics::from_usage(
                            TokenUsage {
                                prompt_tokens,
                                completion_tokens,
                                max_prompt_tokens,
                            },
                            context_tokens,
                        );
                        let chunk = format!(
                            "\n\n---\n**调试信息**\n\n**Stats** `tokens: {prompt_tokens}->{completion_tokens}` `context: {max_prompt_tokens}/{context_tokens} ({:.1}%)` `elapsed: {elapsed_ms}ms`",
                            context.percent
                        );
                        append_reply_chunk(&mut output, &mut replies, FeishuReplyKind::Stats, &chunk).await;
                    }
                    CatEvent::Error(err) => {
                        let chunk = format!(
                            "\n\n---\n**调试信息**\n\n**Error**\n{}",
                            fenced_block("text", &err.to_string())
                        );
                        append_reply_chunk(&mut output, &mut replies, FeishuReplyKind::Error, &chunk).await;
                        break;
                    }
                    CatEvent::Done => break,
                    _ => {}
                }
            }
            _ = &mut timeout => {
                let chunk = "\n\n---\n**调试信息**\n\n**Timeout** reply timed out";
                append_reply_chunk(&mut output, &mut replies, FeishuReplyKind::Error, chunk).await;
                break;
            }
        }
    }
    if output.trim().is_empty() {
        append_reply_chunk(
            &mut output,
            &mut replies,
            FeishuReplyKind::Text,
            "（无响应）",
        )
        .await;
    }
    Ok(output)
}

fn is_fork_command(command: &str) -> bool {
    command == "/fork" || command.starts_with("/fork ")
}

async fn handle_feishu_fork_command(
    runtime: &Runtime,
    source_session_id: &str,
    msg: &FeishuMessage,
) -> anyhow::Result<String> {
    if runtime.bot.is_thread_running(source_session_id).await {
        return Ok("当前 session 正在运行，结束或取消后再 fork。".to_string());
    }
    let title = msg
        .text
        .trim()
        .strip_prefix("/fork")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let temporary_channel_id = format!("fork:{}", uuid::Uuid::new_v4());
    let fork = runtime
        .sessions
        .lock()
        .await
        .fork_session(
            source_session_id,
            FEISHU_CHANNEL,
            &temporary_channel_id,
            title,
        )?
        .ok_or_else(|| anyhow::anyhow!("source session `{source_session_id}` not found"))?;
    if let Err(error) = runtime
        .bot
        .fork_thread_data(source_session_id, &fork.id, Some(&msg.sender_user_id))
        .await
    {
        let _ = runtime.sessions.lock().await.delete(&fork.id);
        return Err(anyhow::Error::from(error));
    }

    let binding = runtime
        .im_bridge
        .sub_session_binding_upsert(SubSessionBindingUpsertRequest {
            parent_session_id: source_session_id.to_string(),
            sub_session_id: fork.id.clone(),
            kind: "fork".to_string(),
            target: "session".to_string(),
            title: fork.title.clone(),
            platform: FEISHU_CHANNEL.to_string(),
            parent_channel_id: msg.chat_id.clone(),
            parent_thread_id: msg.thread_id.clone(),
            actor_user_id: Some(msg.sender_user_id.clone()),
        })
        .await;

    match binding {
        Ok(Some(binding)) => {
            runtime.sessions.lock().await.set_channel_binding(
                &fork.id,
                &binding.platform,
                &binding.channel_id,
            )?;
            Ok(format!(
                "已 fork 当前 session。\n\n新 session: `{}`\n标题: {}\n已创建新的飞书子会话入口。",
                fork.id,
                fork.title.as_deref().unwrap_or("新对话")
            ))
        }
        Ok(None) => Ok(format!(
            "已 fork 当前 session。\n\n新 session: `{}`\n标题: {}\n未创建飞书子会话入口，可通过 session id 访问。",
            fork.id,
            fork.title.as_deref().unwrap_or("新对话")
        )),
        Err(error) => Ok(format!(
            "已 fork 当前 session，但创建飞书子会话入口失败。\n\n新 session: `{}`\n标题: {}\n错误: {error:#}",
            fork.id,
            fork.title.as_deref().unwrap_or("新对话")
        )),
    }
}

async fn append_reply_chunk(
    output: &mut String,
    replies: &mut Option<&mut FeishuReplyStream>,
    kind: FeishuReplyKind,
    chunk: &str,
) {
    output.push_str(chunk);
    if let Some(replies) = replies.as_deref_mut() {
        replies.push(kind, chunk).await;
    }
}

async fn update_tool_reply(
    output: &mut String,
    replies: &mut Option<&mut FeishuReplyStream>,
    call_id: &str,
    line: &str,
    done: bool,
) {
    if let Some(replies) = replies.as_deref_mut() {
        replies.update_tool(call_id, line, done).await;
        if done {
            output.push_str(line);
            output.push('\n');
        }
    } else {
        output.push_str(line);
        output.push('\n');
    }
}

async fn update_context_compaction_reply(
    output: &mut String,
    replies: &mut Option<&mut FeishuReplyStream>,
    id: &str,
    line: &str,
    done: bool,
) {
    if let Some(replies) = replies.as_deref_mut() {
        replies.update_context_compaction(id, line, done).await;
        if done {
            output.push_str(line);
            output.push('\n');
        }
    } else {
        output.push_str(line);
        output.push('\n');
    }
}

async fn update_sub_session_reply(
    output: &mut String,
    replies: &mut Option<&mut FeishuReplyStream>,
    id: &str,
    line: &str,
    done: bool,
) {
    if let Some(replies) = replies.as_deref_mut() {
        replies.update_sub_session(id, line, done).await;
        if done {
            output.push_str(line);
            output.push('\n');
        }
    } else {
        output.push_str(line);
        output.push('\n');
    }
}

async fn ensure_im_username(
    user_store: &UserStore,
    gateway: &FeishuGateway,
    user_uuid: &str,
    channel_user_id: &str,
) -> Option<String> {
    if let Some(username) = user_store.username(user_uuid) {
        return Some(username);
    }
    match gateway.get_user_name(channel_user_id).await {
        Ok(Some(username)) if !username.trim().is_empty() => {
            let username = username.trim().to_string();
            let _ = user_store.set_username_if_missing(user_uuid, &username);
            Some(username)
        }
        _ => None,
    }
}

fn build_message_content(
    text: &str,
    image_urls: &[String],
    had_images: bool,
    attachment_count: usize,
    document_count: usize,
) -> Content {
    let trimmed = text.trim();
    let valid_images: Vec<String> = image_urls
        .iter()
        .map(|url| url.trim())
        .filter(|url| !url.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if !valid_images.is_empty() {
        let mut parts = Vec::new();
        if !trimmed.is_empty() {
            parts.push(ContentPart::text(trimmed.to_string()));
        }
        for data_url in valid_images {
            parts.push(ContentPart::image_url(data_url));
        }
        return Content::parts(parts);
    }
    if !trimmed.is_empty() {
        return Content::text(trimmed.to_string());
    }
    let fallback = match (had_images, attachment_count > 0, document_count > 0) {
        (true, _, _) => "[user sent image]",
        (false, true, true) => "[user sent attachment and document link]",
        (false, true, false) => "[user sent attachment]",
        (false, false, true) => "[user sent document link]",
        (false, false, false) => "[user sent an empty message]",
    };
    Content::text(fallback.to_string())
}
