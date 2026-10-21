use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;

use base64::Engine as _;
use bot_core::im_tools::{encode_agent_file_key, ImUploadRequest, SubSessionBindingUpsertRequest};
use bot_core::{
    CatEvent, Content, ContentPart, ContextMetrics, ImAttachment, ImDocument, TokenUsage,
};
use futures::{FutureExt, StreamExt};
use im_feishu::{FeishuEvent, FeishuGateway, FeishuMessage};
use remi_agentloop::prelude::{CancellationToken, ProtocolEvent};
use tracing::{debug, info, warn};
use user_store::UserStore;

use crate::app::{
    CLI_CHANNEL, FEISHU_CHANNEL, SESSION_AGENT_ID_METADATA_KEY, SESSION_DEBUG_METADATA_KEY,
    SESSION_MODEL_PROFILE_METADATA_KEY,
};
use crate::channel::{Channel, ChannelKind};
use crate::config::FeishuTransport;
use crate::core::{ChatChannel, ChatRequest, CoreChatEvent, Runtime};
use crate::{
    parse_output, OutputCapabilities, OutputCapability, OutputEntity, OutputEntityKind,
    OutputNodeKind, OutputProtocolContext,
};

#[path = "feishu/actions.rs"]
mod actions;
#[path = "feishu/bridge.rs"]
mod bridge;
#[path = "feishu/format.rs"]
mod format;
#[path = "feishu/mentions.rs"]
mod mentions;
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
use format::{fenced_block, format_feishu_sub_session_line};
pub(crate) use format::{format_context_compaction_line, format_feishu_tool_line};
use reply_stream::{FeishuReplyKind, FeishuReplyStream};
pub(crate) use routing::{
    feishu_session_channel_id, feishu_topic_channel_id, should_ignore_unaddressed_topic_start,
};
use settings::feishu_hook_config;
use sub_session::record_sub_session_event;

const MAX_FEISHU_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_FEISHU_IMAGES: usize = 8;

type FeishuRunKey = (String, String);

#[derive(Clone, Default)]
struct FeishuActiveRuns(Rc<RefCell<HashMap<FeishuRunKey, HashMap<String, CancellationToken>>>>);

impl FeishuActiveRuns {
    fn register(&self, key: FeishuRunKey) -> FeishuRunRegistration {
        let cancel = CancellationToken::new();
        let run_id = uuid::Uuid::new_v4().to_string();
        self.0
            .borrow_mut()
            .entry(key.clone())
            .or_default()
            .insert(run_id.clone(), cancel.clone());
        FeishuRunRegistration {
            runs: self.clone(),
            key,
            run_id,
            cancel,
        }
    }

    fn stop(&self, key: &FeishuRunKey) -> usize {
        let runs = self.0.borrow();
        let Some(active) = runs.get(key) else {
            return 0;
        };
        for cancel in active.values() {
            cancel.cancel();
        }
        active.len()
    }
}

struct FeishuRunRegistration {
    runs: FeishuActiveRuns,
    key: FeishuRunKey,
    run_id: String,
    cancel: CancellationToken,
}

impl Drop for FeishuRunRegistration {
    fn drop(&mut self) {
        let mut runs = self.runs.0.borrow_mut();
        if let Some(active) = runs.get_mut(&self.key) {
            active.remove(&self.run_id);
            if active.is_empty() {
                runs.remove(&self.key);
            }
        }
    }
}

pub(crate) struct FeishuChannel {
    platform: String,
    gateway: FeishuGateway,
    transport: FeishuTransport,
    event_hook: crate::runtime_config::FeishuEventHookRuntimeConfig,
}

impl FeishuChannel {
    pub(crate) fn configured(
        platform: String,
        gateway: FeishuGateway,
        transport: FeishuTransport,
        event_hook: crate::runtime_config::FeishuEventHookRuntimeConfig,
    ) -> Self {
        Self {
            platform,
            gateway,
            transport,
            event_hook,
        }
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
        let platform = self.platform.clone();
        let transport = self.transport.clone();
        let event_hook = self.event_hook.clone();
        Box::pin(async move {
            run_feishu_configured(runtime, platform, gateway, transport, event_hook).await
        })
    }
}

async fn run_feishu_configured(
    runtime: Rc<Runtime>,
    platform: String,
    gateway: FeishuGateway,
    transport: FeishuTransport,
    event_hook: crate::runtime_config::FeishuEventHookRuntimeConfig,
) -> anyhow::Result<()> {
    info!(connector = %platform, "remi-cat runtime: initializing Feishu gateway connection");
    let mut rx = match transport {
        FeishuTransport::WebSocket => gateway.start().await?,
        FeishuTransport::EventHook => {
            gateway
                .start_event_hook(feishu_hook_config(&event_hook)?)
                .await?
        }
    };
    let active_runs = FeishuActiveRuns::default();
    while let Some(event) = rx.recv().await {
        match event {
            FeishuEvent::MessageReceived(msg) => {
                let runtime = Rc::clone(&runtime);
                let gateway = gateway.clone();
                let platform = platform.clone();
                let active_runs = active_runs.clone();
                tokio::task::spawn_local(async move {
                    if let Err(err) =
                        process_feishu_message(runtime, gateway, platform, msg, active_runs).await
                    {
                        warn!("failed to process Feishu message: {err:#}");
                    }
                });
            }
            FeishuEvent::ReactionReceived(reaction) => {
                debug!(message_id = %reaction.message_id, "ignored Feishu reaction");
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
    platform: String,
    msg: FeishuMessage,
    active_runs: FeishuActiveRuns,
) -> anyhow::Result<()> {
    let channel_id = feishu_session_channel_id(&msg);
    let run_key = (platform.clone(), channel_id.clone());
    let is_stop = msg.text.trim() == "/stop";
    let stopped = if is_stop {
        active_runs.stop(&run_key)
    } else {
        0
    };
    let session_exists = runtime
        .sessions
        .lock()
        .await
        .channel_session_id(&platform, &channel_id)
        .is_some();
    if should_ignore_unaddressed_topic_start(&msg, session_exists) && stopped == 0 {
        info!(
            chat_id = %msg.chat_id,
            thread_id = msg.thread_id.as_deref().unwrap_or(""),
            message_id = %msg.message_id,
            "ignored topic message because the topic session has not been started by an @mention"
        );
        return Ok(());
    }

    if is_stop {
        let reply = if stopped == 0 {
            "当前会话没有正在运行的回复。"
        } else {
            "已请求停止当前会话的回复。"
        };
        gateway.reply_text(&msg.message_id, reply).await?;
        return Ok(());
    }
    let run = active_runs.register(run_key);

    let sender_uuid = runtime
        .user_store
        .resolve_or_create(&platform, &msg.sender_user_id);
    let sender_username = ensure_im_username(
        &runtime.user_store,
        &gateway,
        &sender_uuid,
        &msg.sender_user_id,
    )
    .await;
    let reaction_id = gateway.add_reaction(&msg.message_id, "THINKING").await.ok();
    info!(
        chat_id = %msg.chat_id,
        message_id = %msg.message_id,
        chat_type = %msg.chat_type,
        thread_id = msg.thread_id.as_deref().unwrap_or(""),
        "created Feishu COT turn"
    );
    let mut replies =
        FeishuReplyStream::new(gateway.clone(), msg.chat_id.clone(), msg.message_id.clone());
    replies.start().await;
    let result = collect_bot_reply(
        runtime,
        &platform,
        msg.clone(),
        sender_username,
        Some(run.cancel.clone()),
        Some(&gateway),
        Some(&mut replies),
    )
    .await;
    drop(run);
    replies.finish().await;
    if let Some(reaction_id) = reaction_id {
        gateway
            .delete_reaction(&msg.message_id, &reaction_id)
            .await
            .ok();
    }
    result.map(|_| ())
}

pub(crate) async fn collect_cli_bot_reply(
    runtime: Rc<Runtime>,
    msg: FeishuMessage,
    sender_username: Option<String>,
) -> anyhow::Result<String> {
    collect_bot_reply(runtime, CLI_CHANNEL, msg, sender_username, None, None, None).await
}

fn async_agent_enabled() -> bool {
    std::env::var("REMI_ASYNC_AGENT")
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

struct PreparedFeishuInput {
    text: String,
    image_urls: Vec<String>,
    had_images: bool,
    attachments: Vec<ImAttachment>,
}

async fn prepare_feishu_input(
    gateway: Option<&FeishuGateway>,
    msg: &FeishuMessage,
) -> PreparedFeishuInput {
    let mut text = mentions::incoming_mention_text(&msg.text, &msg.mentions);
    let mut image_refs = msg
        .images
        .iter()
        .map(|key| (msg.message_id.clone(), key.clone()))
        .collect::<Vec<_>>();
    let mut attachments = msg
        .files
        .iter()
        .map(|file| im_attachment(&msg.message_id, file))
        .collect::<Vec<_>>();

    if let (Some(gateway), Some(parent_id)) = (gateway, msg.parent_id.as_deref()) {
        match gateway.fetch_parent_content(parent_id).await {
            Ok(Some(parent)) => {
                if let Some(quoted_text) = parent.text.as_deref() {
                    text = quoted_reply_text(quoted_text, &text);
                }
                image_refs.splice(
                    0..0,
                    parent
                        .images
                        .into_iter()
                        .map(|key| (parent_id.to_string(), key)),
                );
                attachments.extend(
                    parent
                        .files
                        .iter()
                        .map(|file| im_attachment(parent_id, file)),
                );
            }
            Ok(None) => {}
            Err(err) => warn!(
                parent_message_id = parent_id,
                "failed to load quoted Feishu message: {err:#}"
            ),
        }
    }

    let had_images = !image_refs.is_empty();
    let image_urls = if let Some(gateway) = gateway {
        download_feishu_images(gateway, image_refs).await
    } else {
        Vec::new()
    };
    PreparedFeishuInput {
        text,
        image_urls,
        had_images,
        attachments,
    }
}

fn im_attachment(owner_message_id: &str, file: &im_feishu::FeishuFile) -> ImAttachment {
    ImAttachment {
        key: encode_agent_file_key(owner_message_id, &file.file_key),
        name: file.file_name.clone(),
        mime_type: file.mime_type.clone(),
        size_bytes: file.size_bytes,
        file_type: file.file_type.clone(),
    }
}

fn quoted_reply_text(quoted: &str, reply: &str) -> String {
    let quoted = quoted.trim();
    let reply = reply.trim();
    match (quoted.is_empty(), reply.is_empty()) {
        (true, _) => reply.to_string(),
        (false, true) => format!("[Quoted message]\n{quoted}"),
        (false, false) => format!("[Quoted message]\n{quoted}\n\n[Reply]\n{reply}"),
    }
}

async fn download_feishu_images(
    gateway: &FeishuGateway,
    image_refs: Vec<(String, String)>,
) -> Vec<String> {
    let mut urls = Vec::new();
    let mut total_bytes = 0usize;
    for (message_id, image_key) in image_refs.into_iter().take(MAX_FEISHU_IMAGES) {
        match gateway.download_image(&message_id, &image_key).await {
            Ok((media_type, bytes)) => {
                if !media_type.starts_with("image/") {
                    warn!(%message_id, %image_key, %media_type, "ignored non-image Feishu resource");
                    continue;
                }
                if bytes.len() > MAX_FEISHU_IMAGE_BYTES
                    || total_bytes.saturating_add(bytes.len()) > MAX_FEISHU_IMAGE_BYTES
                {
                    warn!(
                        %message_id,
                        %image_key,
                        bytes = bytes.len(),
                        "ignored oversized Feishu image input"
                    );
                    continue;
                }
                total_bytes += bytes.len();
                let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                urls.push(format!("data:{media_type};base64,{encoded}"));
            }
            Err(err) => warn!(
                %message_id,
                %image_key,
                "failed to download Feishu image: {err:#}"
            ),
        }
    }
    urls
}

async fn collect_bot_reply(
    runtime: Rc<Runtime>,
    platform: &str,
    msg: FeishuMessage,
    sender_username: Option<String>,
    cancel: Option<CancellationToken>,
    gateway: Option<&FeishuGateway>,
    mut replies: Option<&mut FeishuReplyStream>,
) -> anyhow::Result<String> {
    let channel_id = feishu_session_channel_id(&msg);
    let session_id = runtime.sessions.lock().await.resolve_channel(
        platform,
        &channel_id,
        &runtime.root_agent_id,
    )?;
    if is_feishu_platform(platform) {
        if let Some(thread_id) = msg.thread_id.as_deref() {
            if msg.message_id.starts_with("om_") {
                let mut sessions = runtime.sessions.lock().await;
                sessions.set_metadata_string(
                    &session_id,
                    "feishu_reply_anchor",
                    &msg.message_id,
                )?;
                sessions.set_metadata_string(
                    &session_id,
                    "feishu_reply_anchor_thread_id",
                    thread_id,
                )?;
            }
        }
    }
    if is_feishu_platform(platform) && is_fork_command(msg.text.trim()) {
        let reply =
            handle_feishu_fork_command(&runtime, platform, &session_id, &msg, gateway).await?;
        append_reply_chunk(
            &mut String::new(),
            &mut replies,
            FeishuReplyKind::Text,
            &reply,
        )
        .await;
        if let Some(replies) = replies.as_deref_mut() {
            replies.commit_final_output().await?;
        }
        return Ok(reply);
    }
    let prepared = prepare_feishu_input(gateway, &msg).await;
    let im_attachments = prepared.attachments;
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
    let content = build_message_content(
        &prepared.text,
        &prepared.image_urls,
        prepared.had_images,
        im_attachments.len(),
        msg.documents.len(),
    );
    let channel = if is_feishu_platform(platform) {
        ChatChannel::Feishu
    } else {
        ChatChannel::Cli
    };
    let output_protocol = build_feishu_output_protocol(
        &runtime,
        platform,
        &session_id,
        &msg,
        sender_username.as_deref(),
        gateway,
    )
    .await;
    let mut system_prompt = output_protocol.context.prompt();
    if is_feishu_platform(platform) {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(&feishu_channel_context_prompt(platform, &session_id, &msg));
    }
    let mut request = ChatRequest::text(session_id.clone(), channel, prepared.text)
        .with_content(content)
        .with_sender(msg.sender_user_id.clone(), sender_username)
        .with_message(msg.message_id.clone(), msg.chat_type.clone())
        .with_platform(Some(platform.to_string()))
        .with_async_agent(async_agent_enabled())
        .with_output_protocol_prompt(Some(system_prompt))
        .enable_sdk_todo()
        .with_im_context(im_attachments, im_documents);
    if let Some(cancel) = cancel {
        request = request.with_cancel(cancel);
    }
    let debug_enabled = runtime
        .sessions
        .lock()
        .await
        .metadata_bool(&session_id, SESSION_DEBUG_METADATA_KEY);
    let mut stream = std::pin::pin!(Rc::clone(&runtime).chat(request));
    let mut forwarder = FeishuEventForwarder {
        runtime: &runtime,
        platform,
        msg: &msg,
        session_id: &session_id,
        debug_enabled,
        gateway,
        replies: &mut replies,
        state: FeishuForwarderState::new(output_protocol, msg.message_id.clone()),
    };
    'output: while let Some(event) = stream.next().await {
        let mut batch = vec![event];
        // Drain already-ready micro-chunks before persistence and COT updates.
        // Tool, turn and terminal boundaries remain in their original order.
        for _ in 0..127 {
            let Some(Some(event)) = stream.next().now_or_never() else {
                break;
            };
            if !merge_output_chunk(batch.last_mut().unwrap(), &event) {
                batch.push(event);
            }
        }
        for event in batch {
            if forwarder.forward_core_event(event).await {
                break 'output;
            }
        }
    }
    if should_emit_empty_fallback(&forwarder.state.output, forwarder.state.had_visible_event) {
        forwarder.append(FeishuReplyKind::Text, "（无响应）").await;
    }
    for child in forwarder.state.child_outputs.values_mut() {
        child.replies.finish().await;
        child.replies.delivery_result()?;
    }
    if let Some(replies) = forwarder.replies.as_deref() {
        replies.delivery_result()?;
    }
    Ok(forwarder.state.output)
}

fn merge_output_chunk(previous: &mut CoreChatEvent, next: &CoreChatEvent) -> bool {
    match (previous, next) {
        (CoreChatEvent::Bot(CatEvent::Text(left)), CoreChatEvent::Bot(CatEvent::Text(right)))
        | (
            CoreChatEvent::Bot(CatEvent::Thinking(left)),
            CoreChatEvent::Bot(CatEvent::Thinking(right)),
        ) => {
            left.push_str(right);
            true
        }
        (
            CoreChatEvent::Bot(CatEvent::SubSession(left)),
            CoreChatEvent::Bot(CatEvent::SubSession(right)),
        ) if left.sub_thread_id == right.sub_thread_id && left.sub_run_id == right.sub_run_id => {
            match (left.event.as_mut(), right.event.as_ref()) {
                (
                    ProtocolEvent::Delta {
                        content: left,
                        role: left_role,
                    },
                    ProtocolEvent::Delta {
                        content: right,
                        role: right_role,
                    },
                ) if left_role == right_role => {
                    left.push_str(right);
                    true
                }
                (
                    ProtocolEvent::ThinkingEnd { content: left },
                    ProtocolEvent::ThinkingEnd { content: right },
                ) => {
                    left.push_str(right);
                    true
                }
                _ => false,
            }
        }
        _ => false,
    }
}

async fn replay_feishu_history(
    runtime: &Runtime,
    platform: &str,
    channel_id: &str,
    session_id: &str,
    source: &FeishuMessage,
    gateway: &FeishuGateway,
    history: Vec<bot_core::ThreadHistoryMessage>,
) -> anyhow::Result<()> {
    let turns = feishu_history_turns(history);
    for (input, events) in turns {
        replay_feishu_history_turn(
            runtime, platform, channel_id, session_id, source, gateway, input, events,
        )
        .await?;
    }
    Ok(())
}

fn feishu_history_turns(
    history: Vec<bot_core::ThreadHistoryMessage>,
) -> Vec<(String, Vec<CoreChatEvent>)> {
    let mut turns: Vec<(String, Vec<CoreChatEvent>)> = Vec::new();
    for message in history {
        if message.role == "user" {
            turns.push((message.text, Vec::new()));
            continue;
        }
        if turns.is_empty() {
            turns.push(("已复制历史对话。".into(), Vec::new()));
        }
        let events = &mut turns.last_mut().unwrap().1;
        if message.role == "assistant" {
            if !message.text.is_empty() {
                events.push(CoreChatEvent::Bot(CatEvent::Text(message.text)));
            }
            if let Some(calls) = message
                .tool_calls
                .and_then(|value| value.as_array().cloned())
            {
                for call in calls {
                    let id = call["id"].as_str().unwrap_or_default().to_string();
                    let name = call["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let args = call["function"]["arguments"]
                        .as_str()
                        .and_then(|text| serde_json::from_str(text).ok())
                        .unwrap_or_default();
                    events.push(CoreChatEvent::Bot(CatEvent::ToolCall { id, name, args }));
                }
            }
        } else if message.role == "tool" {
            let (id, name, args, elapsed_ms) = if let Some(pretty) = message.pretty {
                (
                    pretty.id,
                    pretty.tool_name,
                    pretty.request,
                    pretty.elapsed_ms.unwrap_or(0),
                )
            } else {
                let id = message.tool_call_id.unwrap_or(message.id);
                let name = events
                    .iter()
                    .rev()
                    .find_map(|event| match event {
                        CoreChatEvent::Bot(CatEvent::ToolCall {
                            id: call_id, name, ..
                        }) if call_id == &id => Some(name.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "tool".into());
                (id, name, serde_json::Value::Null, 0)
            };
            events.push(CoreChatEvent::Bot(CatEvent::ToolCallResult {
                id,
                name,
                args,
                result: message.text.clone(),
                success: bot_core::tool_success(&message.text),
                elapsed_ms,
            }));
        }
    }
    for (_, events) in &mut turns {
        let mut final_text = String::new();
        for event in events.iter() {
            match event {
                CoreChatEvent::Bot(CatEvent::Text(text)) => final_text.push_str(text),
                _ => final_text.clear(),
            }
        }
        if !final_text.is_empty() {
            events.push(CoreChatEvent::ResponseCompleted {
                message_id: uuid::Uuid::new_v4().to_string(),
                text: final_text,
            });
        }
    }
    turns
}

async fn replay_feishu_history_turn(
    runtime: &Runtime,
    platform: &str,
    channel_id: &str,
    session_id: &str,
    source: &FeishuMessage,
    gateway: &FeishuGateway,
    input: String,
    events: Vec<CoreChatEvent>,
) -> anyhow::Result<()> {
    let chat_id = channel_id.split(":thread:").next().unwrap_or(channel_id);
    let anchor = if channel_id.contains(":thread:") {
        let anchor = runtime
            .im_bridge
            .sub_session_reply_anchor(platform, channel_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("fork topic has no reply anchor"))?;
        gateway.reply_text(&anchor, &input).await?
    } else {
        gateway.send_text(chat_id, &input).await?
    };
    let mut message = source.clone();
    message.chat_id = chat_id.to_string();
    message.message_id = anchor.clone();
    message.text = input;
    message.mentions.clear();
    message.images.clear();
    message.files.clear();
    message.documents.clear();
    let protocol =
        build_feishu_output_protocol(runtime, platform, session_id, &message, None, Some(gateway))
            .await;
    let mut replies = FeishuReplyStream::new(gateway.clone(), chat_id.to_string(), anchor.clone());
    replies.start().await;
    let mut reply_option = Some(&mut replies);
    let mut forwarder = FeishuEventForwarder {
        runtime,
        platform,
        msg: &message,
        session_id,
        debug_enabled: false,
        gateway: Some(gateway),
        replies: &mut reply_option,
        state: FeishuForwarderState::new(protocol, anchor),
    };
    for event in events {
        forwarder.forward_core_event(event).await;
    }
    forwarder.commit_final_output().await?;
    Ok(())
}

fn is_feishu_platform(platform: &str) -> bool {
    platform == FEISHU_CHANNEL || platform.starts_with("feishu:")
}

fn feishu_channel_context_prompt(platform: &str, session_id: &str, msg: &FeishuMessage) -> String {
    let context = serde_json::json!({
        "platform": platform,
        "chat_type": msg.chat_type,
        "chat_id": msg.chat_id,
        "thread_id": msg.thread_id,
        "message_id": msg.message_id,
        "parent_message_id": msg.parent_id,
        "sender_user_id": msg.sender_user_id,
        "remi_session_id": session_id,
    });
    format!(
        "## Feishu Channel Context\n\nCurrent incoming message (Feishu IDs, except remi_session_id): {context}\n\
         chat_id identifies the Feishu chat. thread_id is the Feishu topic/thread ID when present; null means this message is not in a topic. remi_session_id is internal and must not be used as a Feishu chat or thread ID.\n\
         If more chat context is needed, you may use lark-cli with these Feishu IDs when it is installed, authenticated, and authorized. Do not assume lark-cli is available or that chat history has already been read."
    )
}

fn is_fork_command(command: &str) -> bool {
    command == "/fork" || command.starts_with("/fork ")
}

async fn handle_feishu_fork_command(
    runtime: &Runtime,
    platform: &str,
    source_session_id: &str,
    msg: &FeishuMessage,
    gateway: Option<&FeishuGateway>,
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
        .fork_session(source_session_id, platform, &temporary_channel_id, title)?
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
            platform: platform.to_string(),
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
            if let Some(gateway) = gateway {
                let history = runtime.bot.thread_history(&fork.id).await;
                if let Err(error) = replay_feishu_history(runtime, &binding.platform, &binding.channel_id, &fork.id, msg, gateway, history).await {
                    return Ok(format!("已 fork 当前 session 并创建新入口，但同步对话失败。\n\n新 session: `{}`\n错误: {error:#}", fork.id));
                }
            }
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

#[derive(Debug, Clone)]
struct FeishuOutputProtocol {
    context: OutputProtocolContext,
    platform_user_ids: HashMap<String, String>,
    workspace: PathBuf,
}

const FEISHU_OUTPUT_PARTICIPANTS_METADATA_KEY: &str = "feishu_output_participants_v1";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FeishuOutputParticipant {
    reference: String,
    user_id: String,
    label: String,
}

impl FeishuOutputProtocol {
    fn rejects_mentions(&self, text: &str) -> bool {
        mentions::has_plain_mentions(text)
            || parse_output(text, &self.context)
                .diagnostics
                .iter()
                .any(|diagnostic| {
                    matches!(
                        diagnostic.code,
                        "invalid_mention_syntax"
                            | "unknown_entity"
                            | "mention_not_available"
                            | "broadcast_not_available"
                    )
                })
    }

    fn render(&self, text: &str) -> String {
        if self.rejects_mentions(text) {
            return "回复包含不允许的人员提及，已拦截。请使用有效的结构化提及后重试。".into();
        }
        parse_output(text, &self.context).render_mentions(
            |reference, kind, label| match kind {
                OutputEntityKind::User => self
                    .platform_user_ids
                    .get(reference)
                    .map(|user_id| format!("<at id={}></at>", escape_feishu_attribute(user_id))),
                OutputEntityKind::Agent => Some(format!("@{label}")),
            },
            |_| Some("<at id=all></at>".to_string()),
        )
    }

    async fn render_final(
        &self,
        runtime: &Runtime,
        msg: &FeishuMessage,
        text: &str,
        gateway: Option<&FeishuGateway>,
    ) -> String {
        // Reject invalid mentions before uploading resources or delivering attachments.
        if self.rejects_mentions(text) {
            return self.render(text);
        }
        let document = parse_feishu_image_output(text, &self.context);
        let mut rendered = text.to_string();
        for node in document.nodes.iter().rev() {
            let OutputNodeKind::Resource { path, label, image } = &node.kind else {
                continue;
            };
            if *image && path.starts_with("img_") {
                continue;
            }
            let kind = if *image { "图片" } else { "附件" };
            let replacement = match load_feishu_output_resource(path, *image, &self.workspace).await
            {
                Ok((file_name, mime_type, content)) => {
                    if *image {
                        // Upload only: the final card owns the image and its position.
                        let result = match gateway {
                            Some(gateway) => {
                                gateway
                                    .upload_file(&file_name, &mime_type, &content, "image")
                                    .await
                            }
                            None => Err(anyhow::anyhow!(
                                "Feishu image upload gateway is unavailable"
                            )),
                        };
                        match result {
                            Ok(key) => format!(
                                "![{}]({key})",
                                compact_resource_label(label).replace('\\', "\\\\")
                            ),
                            Err(error) => {
                                warn!("upload Feishu inline image failed: {error:#}");
                                format!("**[图片发送失败：{}]**", compact_resource_label(label))
                            }
                        }
                    } else {
                        match runtime
                            .im_bridge
                            .upload(ImUploadRequest {
                                platform: self.context.surface.clone(),
                                message_id: msg.message_id.clone(),
                                chat_id: msg.chat_id.clone(),
                                file_name,
                                mime_type,
                                content,
                                file_type: "stream".into(),
                            })
                            .await
                        {
                            Ok(_) => {
                                format!("**[{kind}已发送：{}]**", compact_resource_label(label))
                            }
                            Err(error) => {
                                warn!("upload Feishu output resource failed: {error:#}");
                                format!("**[{kind}发送失败：{}]**", compact_resource_label(label))
                            }
                        }
                    }
                }
                Err(error) => {
                    warn!("load Feishu output resource failed: {error:#}");
                    format!("**[{kind}不可用：{}]**", compact_resource_label(label))
                }
            };
            rendered.replace_range(node.start..node.end, &replacement);
        }
        self.render(&rendered)
    }
}

// Ordinary Markdown images need the same native upload as remi-resource images.
// Parse Markdown rather than matching text so code examples remain untouched.
fn parse_feishu_image_output(text: &str, context: &OutputProtocolContext) -> crate::OutputDocument {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let mut document = parse_output(text, context);
    let mut image = None::<(usize, String, String)>;
    for (event, range) in Parser::new_ext(text, Options::all()).into_offset_iter() {
        match event {
            Event::Start(Tag::Image { dest_url, .. }) => {
                image = Some((range.start, dest_url.into_string(), String::new()));
            }
            Event::Text(value) | Event::Code(value) => {
                if let Some((_, _, label)) = image.as_mut() {
                    label.push_str(&value);
                }
            }
            Event::End(TagEnd::Image) => {
                if let Some((start, path, label)) = image.take() {
                    if !path.starts_with("remi-resource:")
                        && !document
                            .nodes
                            .iter()
                            .any(|node| node.start <= start && node.end >= range.end)
                    {
                        document.nodes.push(crate::OutputNode {
                            start,
                            end: range.end,
                            kind: OutputNodeKind::Resource {
                                path,
                                label,
                                image: true,
                            },
                        });
                    }
                }
            }
            _ => {}
        }
    }
    document.nodes.sort_by_key(|node| node.start);
    document
}

async fn load_feishu_output_resource(
    path: &str,
    image: bool,
    workspace: &Path,
) -> anyhow::Result<(String, String, Vec<u8>)> {
    const MAX_OUTPUT_IMAGE_BYTES: usize = 10 * 1024 * 1024;
    if image && (path.starts_with("https://") || path.starts_with("http://")) {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?;
        let mut response = client.get(path).send().await?.error_for_status()?;
        if response
            .content_length()
            .is_some_and(|size| size > MAX_OUTPUT_IMAGE_BYTES as u64)
        {
            anyhow::bail!("output image exceeds 10 MiB");
        }
        let name = response
            .url()
            .path_segments()
            .and_then(|mut parts| parts.next_back())
            .filter(|name| !name.is_empty())
            .unwrap_or("image.png")
            .to_string();
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).trim().to_string())
            .unwrap_or_else(|| resource_mime_type(Path::new(&name)).to_string());
        if !mime.starts_with("image/") {
            anyhow::bail!("remote resource is not an image");
        }
        let mut content = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if content.len().saturating_add(chunk.len()) > MAX_OUTPUT_IMAGE_BYTES {
                anyhow::bail!("output image exceeds 10 MiB");
            }
            content.extend_from_slice(&chunk);
        }
        return Ok((name, mime, content));
    }
    let resolved = crate::resolve_resource_path(path, workspace, &[])?;
    if image && tokio::fs::metadata(&resolved).await?.len() > MAX_OUTPUT_IMAGE_BYTES as u64 {
        anyhow::bail!("output image exceeds 10 MiB");
    }
    let name = resolved
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("resource")
        .to_string();
    let mime = resource_mime_type(&resolved).to_string();
    Ok((name, mime, tokio::fs::read(resolved).await?))
}

fn compact_resource_label(label: &str) -> String {
    label
        .chars()
        .filter(|character| !matches!(character, '\n' | '\r' | '[' | ']'))
        .take(80)
        .collect::<String>()
}

fn resource_mime_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("ico") => "image/x-icon",
        Some("tif" | "tiff") => "image/tiff",
        Some("pdf") => "application/pdf",
        Some("txt" | "md") => "text/plain",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

fn escape_feishu_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

async fn build_feishu_output_protocol(
    runtime: &Runtime,
    platform: &str,
    session_id: &str,
    msg: &FeishuMessage,
    sender_username: Option<&str>,
    gateway: Option<&FeishuGateway>,
) -> FeishuOutputProtocol {
    let mut context = OutputProtocolContext::new(platform, session_id, &msg.chat_type, "a0");
    context.capabilities = OutputCapabilities {
        user_mentions: OutputCapability::Native,
        agent_mentions: OutputCapability::Disabled,
        broadcast_all: if msg.chat_type == "group" {
            OutputCapability::Native
        } else {
            OutputCapability::Disabled
        },
        agent_handoff: false,
        images: OutputCapability::Native,
        files: OutputCapability::Native,
        math: OutputCapability::Disabled,
        html: OutputCapability::Disabled,
        tables: OutputCapability::Native,
        charts: OutputCapability::Native,
    };
    context.entities.push(OutputEntity::new(
        "a0",
        OutputEntityKind::Agent,
        runtime.root_agent_id.clone(),
    ));

    let mut participants = runtime
        .sessions
        .lock()
        .await
        .metadata_value(session_id, FEISHU_OUTPUT_PARTICIPANTS_METADATA_KEY)
        .and_then(|value| serde_json::from_value::<Vec<FeishuOutputParticipant>>(value).ok())
        .unwrap_or_default();
    let sender_label = sender_username
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("当前用户");
    upsert_feishu_participant(&mut participants, &msg.sender_user_id, sender_label);

    for mention in &msg.mentions {
        let Some(user_id) = mention
            .open_id
            .as_deref()
            .or(mention.user_id.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        if participants
            .iter()
            .any(|participant| participant.user_id == user_id)
        {
            if let Some(name) = mention
                .name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                upsert_feishu_participant(&mut participants, user_id, name);
            }
            continue;
        }
        let label = if let Some(name) = mention
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            Some(name.to_string())
        } else {
            match gateway {
                Some(gateway) => gateway
                    .get_user_name(user_id)
                    .await
                    .ok()
                    .flatten()
                    .filter(|value| !value.trim().is_empty()),
                None => None,
            }
        }
        .unwrap_or_else(|| format!("会话用户{}", participants.len()));
        upsert_feishu_participant(&mut participants, user_id, &label);
    }

    let _ = runtime.sessions.lock().await.set_metadata_value(
        session_id,
        FEISHU_OUTPUT_PARTICIPANTS_METADATA_KEY,
        serde_json::to_value(&participants).unwrap_or_default(),
    );
    let mut platform_user_ids = HashMap::new();
    for participant in participants {
        context.entities.push(OutputEntity::new(
            &participant.reference,
            OutputEntityKind::User,
            &participant.label,
        ));
        platform_user_ids.insert(participant.reference, participant.user_id);
    }

    FeishuOutputProtocol {
        context,
        platform_user_ids,
        workspace: std::env::var_os("REMI_SANDBOX_HOST_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default(),
    }
}

fn upsert_feishu_participant(
    participants: &mut Vec<FeishuOutputParticipant>,
    user_id: &str,
    label: &str,
) {
    if let Some(participant) = participants
        .iter_mut()
        .find(|participant| participant.user_id == user_id)
    {
        if !label.trim().is_empty() {
            participant.label = label.trim().to_string();
        }
        return;
    }
    participants.push(FeishuOutputParticipant {
        reference: format!("u{}", participants.len()),
        user_id: user_id.to_string(),
        label: label.trim().to_string(),
    });
}

struct ChildFeishuOutput {
    session_id: String,
    message: FeishuMessage,
    replies: FeishuReplyStream,
    state: FeishuForwarderState,
}

struct FeishuForwarderState {
    output: String,
    active_reply_message_id: String,
    streaming_tool_names: HashMap<String, String>,
    streaming_tool_args: HashMap<String, String>,
    streaming_tool_output: HashMap<String, String>,
    narrative_kind: Option<FeishuReplyKind>,
    background_task_count: usize,
    had_visible_event: bool,
    supervisor_execution_started: bool,
    output_protocol: FeishuOutputProtocol,
    child_outputs: HashMap<String, ChildFeishuOutput>,
}

impl FeishuForwarderState {
    fn new(output_protocol: FeishuOutputProtocol, anchor: String) -> Self {
        Self {
            output: String::new(),
            active_reply_message_id: anchor,
            streaming_tool_names: HashMap::new(),
            streaming_tool_args: HashMap::new(),
            streaming_tool_output: HashMap::new(),
            narrative_kind: None,
            background_task_count: 0,
            had_visible_event: false,
            supervisor_execution_started: false,
            output_protocol,
            child_outputs: HashMap::new(),
        }
    }
}

struct FeishuEventForwarder<'a, 'b> {
    runtime: &'a Runtime,
    platform: &'a str,
    msg: &'a FeishuMessage,
    session_id: &'a str,
    debug_enabled: bool,
    gateway: Option<&'a FeishuGateway>,
    replies: &'b mut Option<&'b mut FeishuReplyStream>,
    state: FeishuForwarderState,
}

impl FeishuEventForwarder<'_, '_> {
    async fn forward_sub_session_output(
        &mut self,
        event: &remi_agentloop::types::SubSessionEvent,
    ) -> anyhow::Result<()> {
        let Some(gateway) = self.gateway else {
            return Ok(());
        };
        let binding = self
            .runtime
            .sessions
            .lock()
            .await
            .sub_session_channel_binding(self.session_id, &event.sub_thread_id.0);
        let Some(binding) = binding else {
            return Ok(());
        };
        let key = format!("{}:{}", event.sub_thread_id.0, event.sub_run_id.0);
        let child = if let Some(child) = self.state.child_outputs.remove(&key) {
            child
        } else {
            let session_id = self
                .runtime
                .sessions
                .lock()
                .await
                .channel_session_id(&binding.platform, &binding.channel_id)
                .ok_or_else(|| anyhow::anyhow!("bound child channel has no session"))?;
            let input = crate::sub_session::history_messages(event)
                .into_iter()
                .find(|message| matches!(message.role, remi_agentloop::prelude::Role::User))
                .map(|message| message.content.text_content())
                .or_else(|| event.title.clone())
                .unwrap_or_else(|| format!("{} 子会话", event.agent_name));
            let chat_id = binding
                .channel_id
                .split(":thread:")
                .next()
                .unwrap_or(&binding.channel_id)
                .to_string();
            let anchor = if binding.channel_id.contains(":thread:") {
                let source = self
                    .runtime
                    .im_bridge
                    .sub_session_reply_anchor(&binding.platform, &binding.channel_id)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("child topic has no reply anchor"))?;
                gateway.reply_text(&source, &input).await?
            } else {
                gateway.send_text(&chat_id, &input).await?
            };
            let mut message = self.msg.clone();
            message.message_id = anchor.clone();
            message.chat_id = chat_id.clone();
            message.text = input;
            message.images.clear();
            message.files.clear();
            message.documents.clear();
            message.mentions.clear();
            message.parent_id = None;
            message.thread_id = binding
                .channel_id
                .split_once(":thread:")
                .map(|(_, thread)| thread.to_string());
            let output_protocol = build_feishu_output_protocol(
                self.runtime,
                &binding.platform,
                &session_id,
                &message,
                None,
                Some(gateway),
            )
            .await;
            ChildFeishuOutput {
                session_id,
                message,
                replies: FeishuReplyStream::new(gateway.clone(), chat_id, anchor.clone()),
                state: FeishuForwarderState::new(output_protocol, anchor),
            }
        };
        let ChildFeishuOutput {
            session_id,
            message,
            mut replies,
            state,
        } = child;
        replies.start().await;
        let events = crate::sub_session::output_events(event, &state.streaming_tool_args);
        let state = {
            let mut reply_option = Some(&mut replies);
            let mut forwarder = FeishuEventForwarder {
                runtime: self.runtime,
                platform: &binding.platform,
                msg: &message,
                session_id: &session_id,
                debug_enabled: self.debug_enabled,
                gateway: Some(gateway),
                replies: &mut reply_option,
                state,
            };
            for event in events {
                let output = match event {
                    crate::ApplicationEvent::Cat(event) => CoreChatEvent::Bot(event),
                    crate::ApplicationEvent::ResponseCompleted { message_id, text } => {
                        CoreChatEvent::ResponseCompleted { message_id, text }
                    }
                    crate::ApplicationEvent::Done => CoreChatEvent::Done,
                    crate::ApplicationEvent::Reply(text) => CoreChatEvent::Reply(text),
                    crate::ApplicationEvent::Prefix(text) => CoreChatEvent::Prefix(text),
                    crate::ApplicationEvent::SupervisorStarted => CoreChatEvent::SupervisorStarted,
                };
                Box::pin(forwarder.forward_core_event(output)).await;
            }
            forwarder.state
        };
        self.state.child_outputs.insert(
            key,
            ChildFeishuOutput {
                session_id,
                message,
                replies,
                state,
            },
        );
        Ok(())
    }

    async fn forward_core_event(&mut self, event: CoreChatEvent) -> bool {
        match event {
            CoreChatEvent::SupervisorStarted => false,
            CoreChatEvent::Prefix(prefix) | CoreChatEvent::Reply(prefix) => {
                self.start_new_cell().await;
                self.append_narrative(FeishuReplyKind::Text, &prefix).await;
                false
            }
            CoreChatEvent::Done => {
                if let Err(error) = self.commit_final_output().await {
                    warn!("failed to deliver Feishu final output: {error:#}");
                }
                true
            }
            CoreChatEvent::ResponseCompleted { message_id, text } => {
                if self
                    .replies
                    .as_deref()
                    .is_some_and(|replies| !replies.accepts_response(&message_id))
                {
                    return false;
                }
                let mut reply_msg = self.msg.clone();
                reply_msg.message_id = self.state.active_reply_message_id.clone();
                let rendered = self
                    .state
                    .output_protocol
                    .render_final(self.runtime, &reply_msg, &text, self.gateway)
                    .await;
                if let Some(replies) = self.replies.as_deref_mut() {
                    if replies.set_pending_response(message_id, rendered) {
                        if let Err(error) = replies.commit_response_output().await {
                            warn!("failed to deliver Feishu response: {error:#}");
                            return true;
                        }
                    }
                }
                false
            }
            CoreChatEvent::Bot(event) => self.forward_cat_event(event).await,
        }
    }

    async fn forward_cat_event(&mut self, event: CatEvent) -> bool {
        match event {
            CatEvent::ToolDelta { id, name, delta } => {
                let output = self
                    .state
                    .streaming_tool_output
                    .entry(id.clone())
                    .or_default();
                output.push_str(&delta);
                let args = self
                    .state
                    .streaming_tool_args
                    .get(&id)
                    .and_then(|args| serde_json::from_str(args).ok())
                    .unwrap_or(serde_json::Value::Null);
                let pretty = bot_core::PrettyToolCall::started(&id, &name, &args);
                let line = format!(
                    "{}\n\n{}",
                    format_feishu_tool_line(&pretty),
                    fenced_block("text", output)
                );
                self.update_tool(&id, &line, false).await;
            }
            CatEvent::Text(delta) => {
                self.state.supervisor_execution_started = false;
                self.append_narrative(FeishuReplyKind::Text, &delta).await;
            }
            CatEvent::Thinking(content) => {
                self.state.supervisor_execution_started = false;
                self.append_narrative(FeishuReplyKind::Thinking, &content)
                    .await;
            }
            CatEvent::ToolCallStart { id, name } => {
                self.state
                    .streaming_tool_names
                    .insert(id.clone(), name.clone());
                self.state
                    .streaming_tool_args
                    .entry(id.clone())
                    .or_default();
                let pretty = bot_core::PrettyToolCall::started(
                    &id,
                    &name,
                    &serde_json::Value::Object(serde_json::Map::new()),
                );
                let line = format_feishu_tool_line(&pretty);
                self.state.supervisor_execution_started = false;
                self.update_tool(&id, &line, false).await;
            }
            CatEvent::ToolCallArgumentsDelta { id, delta } => {
                let args = self
                    .state
                    .streaming_tool_args
                    .entry(id.clone())
                    .or_default();
                args.push_str(&delta);
                if let (Some(name), Ok(value)) = (
                    self.state.streaming_tool_names.get(&id),
                    serde_json::from_str::<serde_json::Value>(args),
                ) {
                    let pretty = bot_core::PrettyToolCall::started(&id, name, &value);
                    let line = format_feishu_tool_line(&pretty);
                    self.update_tool(&id, &line, false).await;
                }
            }
            CatEvent::ToolCall { id, name, args } => {
                self.state
                    .streaming_tool_names
                    .insert(id.clone(), name.clone());
                self.state
                    .streaming_tool_args
                    .insert(id.clone(), args.to_string());
                let pretty = bot_core::PrettyToolCall::started(&id, &name, &args);
                let line = format_feishu_tool_line(&pretty);
                self.state.supervisor_execution_started = false;
                self.update_tool(&id, &line, false).await;
            }
            CatEvent::ToolCallResult {
                id,
                name,
                args,
                result,
                success,
                elapsed_ms,
            } => {
                self.state.streaming_tool_output.remove(&id);
                self.state.streaming_tool_names.remove(&id);
                self.state.streaming_tool_args.remove(&id);
                let pretty = bot_core::PrettyToolCall::completed(
                    &id, &name, &args, &result, success, elapsed_ms,
                );
                let line = format_feishu_tool_line(&pretty);
                self.state.supervisor_execution_started = false;
                self.update_tool(&id, &line, true).await;
            }
            CatEvent::SubSession(event) => {
                record_sub_session_event(
                    self.runtime,
                    self.session_id,
                    self.platform,
                    self.msg,
                    &event,
                )
                .await;
                if let Err(error) = self.forward_sub_session_output(&event).await {
                    warn!("failed to forward child through shared Feishu output: {error:#}");
                }
                if let Some(line) = format_feishu_sub_session_line(&event) {
                    let done = matches!(
                        event.event.as_ref(),
                        ProtocolEvent::Done
                            | ProtocolEvent::Error { .. }
                            | ProtocolEvent::Cancelled
                    ) || matches!(
                        event.event.as_ref(),
                        ProtocolEvent::Custom { event_type, .. } if event_type == "sub_session_done"
                    );
                    self.update_sub_session(&event.sub_thread_id.0, &line, done)
                        .await;
                }
            }
            CatEvent::BackgroundTasksWaiting { count } => {
                self.state.background_task_count = count;
                self.update_background_status().await;
            }
            CatEvent::SupervisorProgress(progress) => {
                self.forward_supervisor_progress(progress).await;
            }
            CatEvent::Supervisor(report) => {
                let context = self
                    .runtime
                    .bot
                    .workflow_status(self.session_id)
                    .await
                    .map(|instance| instance.context)
                    .unwrap_or(serde_json::Value::Null);
                let chunk = bot_core::supervisor_workflow::format_prefix(&report, &context);
                if self.finish_status("supervisor-decision", &chunk).await {
                    self.state.output.push_str(&chunk);
                } else {
                    self.start_new_cell().await;
                    self.append_narrative(FeishuReplyKind::Supervisor, &chunk)
                        .await;
                }
                self.state.supervisor_execution_started = false;
            }
            CatEvent::ContextCompaction(event) => {
                let line = format_context_compaction_line(&event);
                let done = !matches!(event.status, bot_core::ContextCompactionStatus::Started);
                self.update_context_compaction(&event.id, &line, done).await;
                self.state.supervisor_execution_started = false;
            }
            CatEvent::ToolApprovalRequested(request) => {
                if let Some(replies) = self.replies.as_deref_mut() {
                    self.state.had_visible_event = true;
                    replies.approval_requested(&request).await;
                }
                self.break_narrative();
                self.state.supervisor_execution_started = false;
            }
            CatEvent::ToolApprovalUpdated(request) => {
                if let Some(replies) = self.replies.as_deref_mut() {
                    replies.approval_updated(&request).await;
                }
                self.state.supervisor_execution_started = false;
            }
            CatEvent::ToolApprovalResolved { request, decision } => {
                if let Some(replies) = self.replies.as_deref_mut() {
                    replies.approval_resolved(&request, decision).await;
                }
                self.state.supervisor_execution_started = false;
            }
            CatEvent::UserQuestionRequested(request) => {
                if let Some(replies) = self.replies.as_deref_mut() {
                    self.state.had_visible_event = true;
                    replies.user_question_requested(&request).await;
                }
                self.break_narrative();
                self.state.supervisor_execution_started = false;
            }
            CatEvent::UserQuestionUpdated(request) => {
                if let Some(replies) = self.replies.as_deref_mut() {
                    replies.user_question_updated(&request).await;
                }
                self.state.supervisor_execution_started = false;
            }
            CatEvent::UserQuestionResolved { request, response } => {
                if let Some(replies) = self.replies.as_deref_mut() {
                    replies.user_question_resolved(&request, &response).await;
                }
                self.state.supervisor_execution_started = false;
            }
            CatEvent::SteerQueued(event) => {
                // The queued message gets a short ordinary reply. Its COT is
                // created only when the original run actually injects it.
                tracing::debug!(steer_id = %event.steer_id, "Feishu steer queued");
            }
            CatEvent::SteerInjected(event) => {
                if let Some(message_id) = event
                    .message_id
                    .as_deref()
                    .filter(|id| id.starts_with("om_"))
                {
                    if let Some(replies) = self.replies.as_deref_mut() {
                        replies.switch_to_steer(message_id).await;
                    }
                    self.state.active_reply_message_id = message_id.to_string();
                }
                let preview = compact_preview(&event.preview, 120);
                let line = if preview.is_empty() {
                    format!("✅ 已将 {} 条新消息注入当前运行", event.count)
                } else {
                    format!("✅ 已注入当前运行：{preview}")
                };
                self.update_status("steer", &line).await;
            }
            CatEvent::Stats {
                prompt_tokens,
                completion_tokens,
                max_prompt_tokens,
                elapsed_ms,
            } => {
                if self.debug_enabled {
                    self.append_stats(
                        prompt_tokens,
                        completion_tokens,
                        max_prompt_tokens,
                        elapsed_ms,
                    )
                    .await;
                }
            }
            CatEvent::ToolTaskCompleted(task) => {
                let result = task
                    .result_preview
                    .as_deref()
                    .or_else(|| task.recent_output.last().map(String::as_str))
                    .or(task.message.as_deref())
                    .unwrap_or_default();
                let success = task.success.unwrap_or(task.status == "completed");
                let pretty = bot_core::PrettyToolCall::completed(
                    &task.tool_call_id,
                    &task.tool_name,
                    &task.args,
                    result,
                    success,
                    task.elapsed_ms.unwrap_or(0),
                );
                let line = format_feishu_tool_line(&pretty);
                self.state.streaming_tool_names.remove(&task.tool_call_id);
                self.state.streaming_tool_args.remove(&task.tool_call_id);
                self.update_tool(&task.tool_call_id, &line, true).await;
                if self.state.background_task_count > 0 {
                    self.state.background_task_count -= 1;
                    self.update_background_status().await;
                }
            }
            CatEvent::Cancelled => {
                self.finish_streaming_tools("cancelled").await;
                self.append_narrative(FeishuReplyKind::Text, "\n\n_已取消。_")
                    .await;
                self.interrupt_run("cancelled").await;
                return true;
            }
            CatEvent::UserInterrupted { reason } => {
                self.finish_streaming_tools("interrupted").await;
                let chunk = if reason.trim().is_empty() {
                    "\n\n_已中断。_".to_string()
                } else {
                    format!("\n\n_已中断：{}_", reason.trim())
                };
                self.append_narrative(FeishuReplyKind::Text, &chunk).await;
                self.interrupt_run(reason.trim()).await;
                return true;
            }
            CatEvent::Error(err) => {
                self.finish_streaming_tools(&err.to_string()).await;
                crate::telemetry::capture_agent_error(&err, "feishu.chat");
                let chunk = format!(
                    "\n\n---\n**调试信息**\n\n**Error**\n{}",
                    fenced_block("text", &err.to_string())
                );
                self.append(FeishuReplyKind::Error, &chunk).await;
                return true;
            }
            CatEvent::StateUpdate(user_state) => {
                if let Some(markdown) = format_feishu_todo_state(&user_state) {
                    self.update_status("todo", &markdown).await;
                } else {
                    self.finish_status("todo", "✅ Todo 已全部完成").await;
                }
            }
            CatEvent::Done => {
                if self.state.background_task_count > 0 {
                    self.state.background_task_count = 0;
                    self.update_background_status().await;
                }
            }
            _ => {}
        }
        false
    }

    async fn forward_supervisor_progress(&mut self, progress: bot_core::SupervisorTraceEvent) {
        use bot_core::SupervisorTraceEvent;

        if !self.state.supervisor_execution_started {
            self.state.supervisor_execution_started = true;
        }
        match progress {
            SupervisorTraceEvent::Thinking { content } => {
                self.append_narrative(FeishuReplyKind::SupervisorThinking, &content)
                    .await;
            }
            SupervisorTraceEvent::ToolCallStart { id, name } => {
                self.state
                    .streaming_tool_names
                    .insert(id.clone(), name.clone());
                self.state
                    .streaming_tool_args
                    .entry(id.clone())
                    .or_default();
                let pretty = bot_core::PrettyToolCall::started(
                    &id,
                    &name,
                    &serde_json::Value::Object(serde_json::Map::new()),
                );
                self.update_tool(&id, &format_feishu_tool_line(&pretty), false)
                    .await;
            }
            SupervisorTraceEvent::ToolCallArgumentsDelta { id, delta } => {
                let args = self
                    .state
                    .streaming_tool_args
                    .entry(id.clone())
                    .or_default();
                args.push_str(&delta);
                if let (Some(name), Ok(value)) = (
                    self.state.streaming_tool_names.get(&id),
                    serde_json::from_str::<serde_json::Value>(args),
                ) {
                    let pretty = bot_core::PrettyToolCall::started(&id, name, &value);
                    self.update_tool(&id, &format_feishu_tool_line(&pretty), false)
                        .await;
                }
            }
            SupervisorTraceEvent::ToolCall { id, name, args } => {
                self.state
                    .streaming_tool_names
                    .insert(id.clone(), name.clone());
                self.state
                    .streaming_tool_args
                    .insert(id.clone(), args.to_string());
                let pretty = bot_core::PrettyToolCall::started(&id, &name, &args);
                self.update_tool(&id, &format_feishu_tool_line(&pretty), false)
                    .await;
            }
            SupervisorTraceEvent::ToolResult {
                id,
                name,
                args,
                result,
            } => {
                self.state.streaming_tool_names.remove(&id);
                self.state.streaming_tool_args.remove(&id);
                let success = bot_core::tool_success(&result);
                let pretty =
                    bot_core::PrettyToolCall::completed(&id, &name, &args, &result, success, 0);
                self.update_tool(&id, &format_feishu_tool_line(&pretty), true)
                    .await;
            }
            // Match the TUI: reserve a decision cell here, then resolve that
            // same card when the final workflow report arrives.
            SupervisorTraceEvent::OutputDelta { .. } | SupervisorTraceEvent::Output { .. } => {
                self.update_status("supervisor-decision", "⏳ **Supervisor** · making decision")
                    .await;
            }
            SupervisorTraceEvent::AgentMessage { content } => {
                self.replace_narrative(FeishuReplyKind::SupervisorMessage, &content)
                    .await;
            }
        }
    }

    async fn append_stats(
        &mut self,
        prompt_tokens: u32,
        completion_tokens: u32,
        max_prompt_tokens: u32,
        elapsed_ms: u64,
    ) {
        let (model_profile_id, agent_id) = {
            let sessions = self.runtime.sessions.lock().await;
            (
                sessions.metadata_string(self.session_id, SESSION_MODEL_PROFILE_METADATA_KEY),
                sessions.metadata_string(self.session_id, SESSION_AGENT_ID_METADATA_KEY),
            )
        };
        let context_tokens = self
            .runtime
            .bot
            .model_context_tokens_for_agent(model_profile_id.as_deref(), agent_id.as_deref());
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
        self.state.output.push_str(&chunk);
        if let Some(replies) = self.replies.as_deref_mut() {
            self.state.had_visible_event = true;
            replies.push_auxiliary(FeishuReplyKind::Stats, &chunk).await;
        }
    }

    async fn finish_streaming_tools(&mut self, reason: &str) {
        let tools = std::mem::take(&mut self.state.streaming_tool_names);
        let mut args_by_id = std::mem::take(&mut self.state.streaming_tool_args);
        for (id, name) in tools {
            let args = args_by_id
                .remove(&id)
                .and_then(|value| serde_json::from_str(&value).ok())
                .unwrap_or(serde_json::Value::Null);
            let pretty = bot_core::PrettyToolCall::completed(&id, &name, &args, reason, false, 0);
            self.update_tool(&id, &format_feishu_tool_line(&pretty), true)
                .await;
        }
    }

    async fn update_background_status(&mut self) {
        if self.state.background_task_count == 0 {
            self.finish_status("background", "✅ 后台任务已完成").await;
        } else {
            let line = format!(
                "⏳ 正在等待 {} 个后台任务完成",
                self.state.background_task_count
            );
            self.update_status("background", &line).await;
        }
    }

    async fn append(&mut self, kind: FeishuReplyKind, chunk: &str) {
        if !chunk.is_empty() && self.replies.is_some() {
            self.state.had_visible_event = true;
        }
        append_reply_chunk(&mut self.state.output, self.replies, kind, chunk).await;
    }

    async fn append_narrative(&mut self, kind: FeishuReplyKind, delta: &str) {
        let Some(chunk) = format_narrative_delta(self.state.narrative_kind, kind, delta) else {
            return;
        };
        self.state.narrative_kind = Some(kind);
        self.append(kind, &chunk).await;
    }

    async fn replace_narrative(&mut self, kind: FeishuReplyKind, content: &str) {
        if content.is_empty() {
            return;
        }
        self.state.narrative_kind = Some(kind);
        self.state.output.push_str(content);
        if let Some(replies) = self.replies.as_deref_mut() {
            self.state.had_visible_event = true;
            replies.replace(kind, content).await;
        }
    }

    fn break_narrative(&mut self) {
        self.state.narrative_kind = None;
    }

    async fn start_new_cell(&mut self) {
        self.break_narrative();
        if let Some(replies) = self.replies.as_deref_mut() {
            replies.start_new_cell().await;
        }
    }

    async fn commit_final_output(&mut self) -> anyhow::Result<()> {
        if let Some(replies) = self.replies.as_deref_mut() {
            replies.commit_final_output().await?;
        }
        Ok(())
    }

    async fn interrupt_run(&mut self, reason: &str) {
        if let Some(replies) = self.replies.as_deref_mut() {
            replies.interrupt_run(reason).await;
        }
    }

    async fn update_tool(&mut self, call_id: &str, line: &str, done: bool) {
        if self.replies.is_some() {
            self.state.had_visible_event = true;
        }
        if update_tool_reply(&mut self.state.output, self.replies, call_id, line, done).await {
            self.break_narrative();
        }
    }

    async fn update_context_compaction(&mut self, id: &str, line: &str, done: bool) {
        if self.replies.is_some() {
            self.state.had_visible_event = true;
        }
        if update_context_compaction_reply(&mut self.state.output, self.replies, id, line, done)
            .await
        {
            self.break_narrative();
        }
    }

    async fn update_sub_session(&mut self, id: &str, line: &str, done: bool) {
        if self.replies.is_some() {
            self.state.had_visible_event = true;
        }
        if update_sub_session_reply(&mut self.state.output, self.replies, id, line, done).await {
            self.break_narrative();
        }
    }

    async fn update_status(&mut self, id: &str, line: &str) {
        if let Some(replies) = self.replies.as_deref_mut() {
            self.state.had_visible_event = true;
            if replies.update_status(id, line).await {
                self.break_narrative();
            }
        }
    }

    async fn finish_status(&mut self, id: &str, line: &str) -> bool {
        if let Some(replies) = self.replies.as_deref_mut() {
            let finished = replies.finish_status(id, line).await;
            self.state.had_visible_event |= finished;
            finished
        } else {
            false
        }
    }
}

fn compact_preview(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    let mut preview = normalized.chars().take(max_chars).collect::<String>();
    preview.push('…');
    preview
}

fn should_emit_empty_fallback(output: &str, had_visible_event: bool) -> bool {
    output.trim().is_empty() && !had_visible_event
}

fn format_feishu_todo_state(user_state: &serde_json::Value) -> Option<String> {
    let todos = bot_core::todo::todos_from_user_state(user_state);
    if todos.is_empty() || todos.iter().all(|todo| todo.done) {
        return None;
    }
    let completed = todos.iter().filter(|todo| todo.done).count();
    let mut lines = vec![format!(
        "📝 **当前 Todo** · {completed}/{} 已完成",
        todos.len()
    )];
    let mut last_batch = None::<&str>;
    for todo in todos.iter().take(24) {
        let batch = todo.batch_title.as_deref().unwrap_or("未分组");
        if last_batch != Some(batch) {
            lines.push(format!("\n**{}**", compact_preview(batch, 80)));
            last_batch = Some(batch);
        }
        let marker = if todo.done { "✅" } else { "⬜" };
        lines.push(format!(
            "- {marker} #{} {}",
            todo.id,
            compact_preview(&todo.content, 120)
        ));
        if let Some(description) = todo.description.as_deref() {
            let description = compact_preview(description, 120);
            if !description.is_empty() {
                lines.push(format!("  - {description}"));
            }
        }
    }
    if todos.len() > 24 {
        lines.push(format!("\n_另有 {} 项未显示_", todos.len() - 24));
    }
    Some(lines.join("\n"))
}

fn format_narrative_delta(
    previous: Option<FeishuReplyKind>,
    kind: FeishuReplyKind,
    delta: &str,
) -> Option<String> {
    if delta.is_empty() {
        return None;
    }
    let _ = (previous, kind);
    Some(delta.to_string())
}

#[cfg(test)]
mod tests {
    use bot_core::{Content, ContentPart};
    use im_feishu::FeishuMessage;

    use super::{
        build_message_content, compact_preview, feishu_channel_context_prompt,
        format_feishu_todo_state, format_narrative_delta, quoted_reply_text,
        should_emit_empty_fallback, FeishuActiveRuns, FeishuOutputProtocol, FeishuReplyKind,
    };
    use crate::{
        OutputCapabilities, OutputCapability, OutputEntity, OutputEntityKind, OutputProtocolContext,
    };
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn fork_history_runs_final_resources_through_response_completion_per_turn() {
        let message = |role: &str, text: &str| bot_core::ThreadHistoryMessage {
            id: "test".into(),
            role: role.into(),
            text: text.into(),
            timestamp: None,
            tool_call_id: None,
            tool_calls: None,
            pretty: None,
        };
        let mut tool_call = message("assistant", "process narrative");
        tool_call.tool_calls =
            Some(serde_json::json!([{"id":"call","function":{"name":"read","arguments":"{}"}}]));
        let final_text = "![local](/workspace/a.png)\n![remote](https://example.com/b.png)\n[report](remi-resource:report.pdf)";
        let turns = super::feishu_history_turns(vec![
            message("user", "first request"),
            tool_call,
            message("assistant", "![probe](remi-resource:probe.png)"),
            message("user", "second request"),
            message("assistant", final_text),
        ]);
        assert_eq!(turns.len(), 2);
        for (index, expected) in ["![probe](remi-resource:probe.png)", final_text]
            .iter()
            .enumerate()
        {
            let Some(crate::CoreChatEvent::ResponseCompleted { text, .. }) = turns[index].1.last()
            else {
                panic!("resource rendering completion missing")
            };
            assert_eq!(text, expected);
            assert!(!text.contains("process narrative"));
        }
    }

    #[test]
    fn ready_child_chunks_merge_without_crossing_tool_or_run_boundaries() {
        use crate::CoreChatEvent;
        use bot_core::CatEvent;
        use remi_agentloop::prelude::{ProtocolEvent, RunId, SubSessionEvent, ThreadId};
        let event = |run: &str, protocol| {
            CoreChatEvent::Bot(CatEvent::SubSession(SubSessionEvent::new(
                "call",
                ThreadId("child".into()),
                RunId(run.into()),
                "acp",
                None,
                1,
                protocol,
            )))
        };
        let delta = |text: &str| ProtocolEvent::Delta {
            content: text.into(),
            role: None,
        };
        let mut left = event("run", delta("hello"));
        assert!(super::merge_output_chunk(
            &mut left,
            &event("run", delta(" world"))
        ));
        assert!(!super::merge_output_chunk(
            &mut left,
            &event("next-run", delta("new"))
        ));
        assert!(!super::merge_output_chunk(
            &mut left,
            &event(
                "run",
                ProtocolEvent::ToolCallStart {
                    id: "tool".into(),
                    name: "read".into()
                }
            )
        ));
        let CoreChatEvent::Bot(CatEvent::SubSession(left)) = left else {
            panic!("child event lost");
        };
        assert!(
            matches!(left.event.as_ref(), ProtocolEvent::Delta { content, .. } if content == "hello world")
        );
    }

    fn feishu_context_test_message(thread_id: Option<&str>) -> FeishuMessage {
        FeishuMessage {
            message_id: "om_message".into(),
            sender_user_id: "ou_sender".into(),
            chat_id: "oc_chat".into(),
            chat_type: "group".into(),
            text: "hello".into(),
            images: Vec::new(),
            files: Vec::new(),
            documents: Vec::new(),
            parent_id: None,
            thread_id: thread_id.map(str::to_string),
            at_bot: true,
            mentions: Vec::new(),
        }
    }

    #[test]
    fn feishu_channel_prompt_distinguishes_chat_topic_and_internal_session() {
        let prompt = feishu_channel_context_prompt(
            "feishu:work",
            "internal-session",
            &feishu_context_test_message(Some("omt_topic")),
        );
        assert!(prompt.contains("\"chat_id\":\"oc_chat\""));
        assert!(prompt.contains("\"thread_id\":\"omt_topic\""));
        assert!(prompt.contains("\"message_id\":\"om_message\""));
        assert!(prompt.contains("\"sender_user_id\":\"ou_sender\""));
        assert!(prompt.contains("\"remi_session_id\":\"internal-session\""));
        assert!(prompt.contains("lark-cli"));

        let no_topic = feishu_channel_context_prompt(
            "feishu",
            "internal-session",
            &feishu_context_test_message(None),
        );
        assert!(no_topic.contains("\"thread_id\":null"));
    }

    #[test]
    fn stop_cancels_only_active_runs_in_the_same_feishu_session() {
        let runs = FeishuActiveRuns::default();
        let key = ("feishu:one".to_string(), "chat:thread:topic".to_string());
        let other_key = ("feishu:one".to_string(), "chat:thread:other".to_string());
        let other_platform = ("feishu:two".to_string(), key.1.clone());
        let first = runs.register(key.clone());
        let second = runs.register(key.clone());
        let other = runs.register(other_key.clone());
        let another_platform = runs.register(other_platform.clone());

        assert_eq!(runs.stop(&key), 2);
        assert!(first.cancel.is_cancelled());
        assert!(second.cancel.is_cancelled());
        assert!(!other.cancel.is_cancelled());
        assert!(!another_platform.cancel.is_cancelled());

        drop(first);
        assert_eq!(runs.stop(&key), 1);
        drop(second);
        assert_eq!(runs.stop(&key), 0);
        assert_eq!(runs.stop(&other_key), 1);
    }

    #[test]
    fn feishu_protocol_renders_native_user_and_all_mentions() {
        let mut context = OutputProtocolContext::new("feishu", "c1", "group", "a0");
        context.capabilities = OutputCapabilities {
            user_mentions: OutputCapability::Native,
            broadcast_all: OutputCapability::Native,
            ..OutputCapabilities::default()
        };
        context
            .entities
            .push(OutputEntity::new("u0", OutputEntityKind::User, "Alice"));
        let protocol = FeishuOutputProtocol {
            context,
            platform_user_ids: HashMap::from([("u0".into(), "ou_alice".into())]),
            workspace: PathBuf::from("/workspace"),
        };
        assert_eq!(
            protocol.render("@[Alice](remi-mention:u0) @[所有人](remi-mention:all)"),
            "<at id=ou_alice></at> <at id=all></at>"
        );
        for invalid in [
            "@Alice",
            "@Unknown",
            "@[Unknown](remi-mention:ou_unknown)",
            "@[Alice](remi-mention:u0) plain @Alice",
        ] {
            let blocked = protocol.render(invalid);
            assert!(blocked.contains("已拦截"));
            assert!(!blocked.contains("<at"));
            assert!(!blocked.contains('@'));
        }
    }

    #[test]
    fn feishu_protocol_does_not_fake_all_in_p2p() {
        let context = OutputProtocolContext::new("feishu", "c1", "p2p", "a0");
        let protocol = FeishuOutputProtocol {
            context,
            platform_user_ids: HashMap::new(),
            workspace: PathBuf::from("/workspace"),
        };
        let source = "@[所有人](remi-mention:all)";
        assert!(protocol.render(source).contains("已拦截"));
    }

    #[test]
    fn thinking_deltas_share_one_section() {
        let first = format_narrative_delta(None, FeishuReplyKind::Thinking, "inspect")
            .expect("first thinking delta");
        let second = format_narrative_delta(
            Some(FeishuReplyKind::Thinking),
            FeishuReplyKind::Thinking,
            " more",
        )
        .expect("second thinking delta");

        assert_eq!(first, "inspect");
        assert_eq!(second, " more");
    }

    #[test]
    fn empty_thinking_end_does_not_create_content() {
        assert_eq!(
            format_narrative_delta(
                Some(FeishuReplyKind::Thinking),
                FeishuReplyKind::Thinking,
                "",
            ),
            None
        );
    }

    #[test]
    fn thinking_after_an_intervening_activity_starts_a_new_section() {
        let chunk = format_narrative_delta(None, FeishuReplyKind::Thinking, "resume")
            .expect("thinking after activity");
        assert_eq!(chunk, "resume");
    }

    #[test]
    fn quoted_message_is_added_to_the_model_input() {
        assert_eq!(
            quoted_reply_text("original", "answer"),
            "[Quoted message]\noriginal\n\n[Reply]\nanswer"
        );
        assert_eq!(
            quoted_reply_text("image caption", ""),
            "[Quoted message]\nimage caption"
        );
    }

    #[test]
    fn image_data_urls_are_preserved_as_multimodal_parts() {
        let content = build_message_content(
            "describe this",
            &["data:image/png;base64,YWJj".to_string()],
            true,
            0,
            0,
        );
        let Content::Parts(parts) = content else {
            panic!("image input should use multipart content");
        };
        assert!(matches!(
            parts.as_slice(),
            [ContentPart::Text { text }, ContentPart::ImageUrl { image_url }]
                if text == "describe this" && image_url.url == "data:image/png;base64,YWJj"
        ));
    }

    #[test]
    fn failed_image_download_remains_visible_to_the_model() {
        let content = build_message_content("inspect it", &[], true, 0, 0);
        assert!(matches!(
            content,
            Content::Text(text)
                if text.contains("inspect it") && text.contains("image content was unavailable")
        ));
    }

    #[test]
    fn todo_state_is_compact_and_hides_empty_or_completed_lists() {
        let active = serde_json::json!({
            "__todos": [{
                "id": 1,
                "content": "Ship Feishu channel",
                "description": "Run the live smoke test",
                "done": false,
                "batch_id": 1,
                "batch_title": "Release",
                "batch_index": 0
            }]
        });
        let card = format_feishu_todo_state(&active).expect("active todo card");
        assert!(card.contains("0/1 已完成"));
        assert!(card.contains("Ship Feishu channel"));
        assert!(format_feishu_todo_state(&serde_json::json!({})).is_none());

        let completed = serde_json::json!({
            "__todos": [{"id": 1, "content": "done", "done": true}]
        });
        assert!(format_feishu_todo_state(&completed).is_none());
    }

    #[test]
    fn status_preview_normalizes_and_truncates_unicode() {
        assert_eq!(compact_preview("one\n two", 20), "one two");
        assert_eq!(compact_preview("你好世界", 3), "你好世…");
    }

    #[test]
    fn visible_status_cards_suppress_the_empty_reply_fallback() {
        assert!(should_emit_empty_fallback("", false));
        assert!(!should_emit_empty_fallback("", true));
        assert!(!should_emit_empty_fallback("answer", false));
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
) -> bool {
    if let Some(replies) = replies.as_deref_mut() {
        let created = replies.update_tool(call_id, line, done).await;
        if done {
            output.push_str(line);
            output.push('\n');
        }
        created
    } else {
        output.push_str(line);
        output.push('\n');
        false
    }
}

async fn update_context_compaction_reply(
    output: &mut String,
    replies: &mut Option<&mut FeishuReplyStream>,
    id: &str,
    line: &str,
    done: bool,
) -> bool {
    if let Some(replies) = replies.as_deref_mut() {
        let created = replies.update_context_compaction(id, line, done).await;
        if done {
            output.push_str(line);
            output.push('\n');
        }
        created
    } else {
        output.push_str(line);
        output.push('\n');
        false
    }
}

async fn update_sub_session_reply(
    output: &mut String,
    replies: &mut Option<&mut FeishuReplyStream>,
    id: &str,
    line: &str,
    done: bool,
) -> bool {
    if let Some(replies) = replies.as_deref_mut() {
        let created = replies.update_sub_session(id, line, done).await;
        if done {
            output.push_str(line);
            output.push('\n');
        }
        created
    } else {
        output.push_str(line);
        output.push('\n');
        false
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
    if had_images && !trimmed.is_empty() {
        return Content::text(format!(
            "{trimmed}\n\n[user sent image, but image content was unavailable]"
        ));
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

#[cfg(test)]
#[path = "feishu/image_live_test.rs"]
mod image_live_test;

#[cfg(test)]
#[path = "feishu/review_live_test.rs"]
mod review_live_test;

#[cfg(test)]
#[path = "feishu/async_live_test.rs"]
mod async_live_test;

#[cfg(test)]
#[path = "feishu/mention_live_test.rs"]
mod mention_live_test;
