use std::rc::Rc;

use bot_core::{
    CatEvent, Content, ImAttachment, ImDocument, SteerInput, SteerSubmitResult, StreamOptions,
};
use futures::{Stream, StreamExt};
use remi_agentloop::prelude::CancellationToken;

use super::{
    append_direct_sub_session_turn, sub_session_input_target, Runtime, SubSessionInputTarget,
};
use crate::app::{
    parse_session_reasoning_effort, SESSION_AGENT_ID_METADATA_KEY,
    SESSION_MODEL_PROFILE_METADATA_KEY, SESSION_REASONING_EFFORT_METADATA_KEY,
};
use crate::command::{process_runtime_commands, RuntimeCommandPipelineResult};

#[derive(Debug, Clone)]
pub(crate) enum ChatChannel {
    Cli,
    Tui,
    Feishu,
    Web,
    Acp,
}

impl ChatChannel {
    fn as_platform(&self) -> Option<String> {
        match self {
            Self::Cli => None,
            Self::Tui => Some("tui".to_string()),
            Self::Feishu => Some("feishu".to_string()),
            Self::Web => Some("web".to_string()),
            Self::Acp => Some("acp".to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ChatRequest {
    pub(crate) session_id: String,
    pub(crate) content: Content,
    pub(crate) channel: ChatChannel,
    pub(crate) sender_user_id: Option<String>,
    pub(crate) sender_username: Option<String>,
    pub(crate) message_id: Option<String>,
    pub(crate) chat_type: Option<String>,
    pub(crate) platform: Option<String>,
    pub(crate) im_attachments: Vec<ImAttachment>,
    pub(crate) im_documents: Vec<ImDocument>,
    pub(crate) cancel: Option<CancellationToken>,
    pub(crate) command_preprocess: bool,
    pub(crate) sub_session_routing: bool,
    pub(crate) async_agent: bool,
    pub(crate) model_profile_id: Option<String>,
    pub(crate) reasoning_effort: Option<bot_core::ReasoningEffort>,
    pub(crate) agent_id: Option<String>,
}

impl ChatRequest {
    pub(crate) fn text(
        session_id: impl Into<String>,
        channel: ChatChannel,
        text: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            content: Content::text(text.into()),
            channel,
            sender_user_id: None,
            sender_username: None,
            message_id: None,
            chat_type: None,
            platform: None,
            im_attachments: Vec::new(),
            im_documents: Vec::new(),
            cancel: None,
            command_preprocess: true,
            sub_session_routing: true,
            async_agent: false,
            model_profile_id: None,
            reasoning_effort: None,
            agent_id: None,
        }
    }

    pub(crate) fn with_content(mut self, content: Content) -> Self {
        self.content = content;
        self
    }

    pub(crate) fn with_runtime_overrides(
        mut self,
        model_profile_id: Option<String>,
        reasoning_effort: Option<bot_core::ReasoningEffort>,
        agent_id: Option<String>,
        async_agent: Option<bool>,
    ) -> Self {
        self.model_profile_id = model_profile_id;
        self.reasoning_effort = reasoning_effort;
        self.agent_id = agent_id;
        if let Some(async_agent) = async_agent {
            self.async_agent = async_agent;
        }
        self
    }

    pub(crate) fn with_sender(
        mut self,
        sender_user_id: impl Into<String>,
        sender_username: Option<String>,
    ) -> Self {
        self.sender_user_id = Some(sender_user_id.into());
        self.sender_username = sender_username;
        self
    }

    pub(crate) fn with_message(
        mut self,
        message_id: impl Into<String>,
        chat_type: impl Into<String>,
    ) -> Self {
        self.message_id = Some(message_id.into());
        self.chat_type = Some(chat_type.into());
        self
    }

    pub(crate) fn with_platform(mut self, platform: Option<String>) -> Self {
        self.platform = platform;
        self
    }

    pub(crate) fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    pub(crate) fn with_async_agent(mut self, enabled: bool) -> Self {
        self.async_agent = enabled;
        self
    }

    pub(crate) fn enable_sdk_todo(self) -> Self {
        self
    }

    pub(crate) fn with_im_context(
        mut self,
        im_attachments: Vec<ImAttachment>,
        im_documents: Vec<ImDocument>,
    ) -> Self {
        self.im_attachments = im_attachments;
        self.im_documents = im_documents;
        self
    }

    fn platform(&self) -> Option<String> {
        self.platform.clone().or_else(|| self.channel.as_platform())
    }
}

impl From<ChatRequest> for SteerInput {
    fn from(request: ChatRequest) -> Self {
        Self {
            session_id: request.session_id,
            content: request.content,
            sender_user_id: request.sender_user_id,
            sender_username: request.sender_username,
            message_id: request.message_id,
            chat_type: request.chat_type,
            platform: request.platform.or_else(|| request.channel.as_platform()),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum CoreChatEvent {
    Prefix(String),
    Reply(String),
    /// An active workflow is about to be evaluated by its supervisor.
    SupervisorStarted,
    Bot(CatEvent),
    Done,
}

impl Runtime {
    pub(crate) fn submit_steer(&self, request: ChatRequest) -> SteerSubmitResult {
        self.bot.submit_steer(request.into())
    }

    pub(crate) fn submit_next_turn(&self, request: ChatRequest) -> SteerSubmitResult {
        self.bot.submit_next_turn(request.into())
    }

    pub(crate) fn chat(self: Rc<Self>, request: ChatRequest) -> impl Stream<Item = CoreChatEvent> {
        async_stream::stream! {
            let user_text = request.content.text_content();
            if !user_text.trim_start().starts_with('/') {
                match self.submit_steer(request.clone()) {
                    SteerSubmitResult::Queued(event) => {
                        yield CoreChatEvent::Bot(CatEvent::SteerQueued(event.clone()));
                        yield CoreChatEvent::Reply(format!(
                            "Steer 已接收，将在下一次模型/工具空隙注入。id: {}",
                            event.steer_id
                        ));
                        yield CoreChatEvent::Done;
                        return;
                    }
                    SteerSubmitResult::NotRunning => {}
                }
            }
            let mut session_id = request.session_id.clone();
            let mut content = request.content.clone();
            let mut skill_injections = Vec::new();
            let mut persist_agent_child_turn = false;

            // Session-local runtime commands must be handled before a TUI
            // channel session is routed to its underlying sub-session. The
            // command layer resolves the channel id to the task thread id.
            if request.command_preprocess
                && (user_text.trim() == "/tasks" || user_text.trim().starts_with("/tasks "))
            {
                match process_runtime_commands(&self, &request.session_id, user_text.trim()).await {
                    Ok(RuntimeCommandPipelineResult::Reply(reply)) => {
                        yield CoreChatEvent::Reply(reply);
                    }
                    Ok(_) => {
                        yield CoreChatEvent::Bot(CatEvent::Error(
                            remi_agentloop::prelude::AgentError::other(
                                "unexpected non-reply result for /tasks",
                            ),
                        ));
                    }
                    Err(error) => {
                        yield CoreChatEvent::Bot(CatEvent::Error(
                            remi_agentloop::prelude::AgentError::other(error.to_string()),
                        ));
                    }
                }
                yield CoreChatEvent::Done;
                return;
            }

            let sub_session_target = if request.sub_session_routing {
                sub_session_input_target(&self, &session_id).await
            } else {
                None
            };

            match sub_session_target {
                Some(SubSessionInputTarget::Acp { acp_session_id, .. }) => {
                    match self.bot.acp_bound_message(&acp_session_id, user_text.trim()).await {
                        Ok(reply) => {
                            append_direct_sub_session_turn(&self, &session_id, user_text.trim(), &reply).await;
                            yield CoreChatEvent::Reply(reply);
                        }
                        Err(error) => {
                            yield CoreChatEvent::Bot(CatEvent::Error(remi_agentloop::prelude::AgentError::other(error.to_string())));
                        }
                    }
                    yield CoreChatEvent::Done;
                    return;
                }
                Some(SubSessionInputTarget::Agent { sub_thread_id, .. }) => {
                    session_id = sub_thread_id;
                    persist_agent_child_turn = true;
                }
                None if request.command_preprocess => {
                    match process_runtime_commands(&self, &request.session_id, user_text.trim()).await {
                        Ok(RuntimeCommandPipelineResult::Reply(reply)) => {
                            yield CoreChatEvent::Reply(reply);
                            yield CoreChatEvent::Done;
                            return;
                        }
                        Ok(RuntimeCommandPipelineResult::StartWorkflow { prefix, workflow_id, context, max_rounds }) => {
                            if !prefix.is_empty() {
                                yield CoreChatEvent::Prefix(prefix);
                            }
                            let platform = request.platform();
                            let (model_profile_id, reasoning_effort, agent_id) = {
                                let sessions = self.sessions.lock().await;
                                (
                                    sessions.metadata_string(&request.session_id, SESSION_MODEL_PROFILE_METADATA_KEY),
                                    parse_session_reasoning_effort(
                                        sessions.metadata_string(&request.session_id, SESSION_REASONING_EFFORT_METADATA_KEY),
                                    ),
                                    sessions.metadata_string(&request.session_id, SESSION_AGENT_ID_METADATA_KEY),
                                )
                            };
                            let opts = StreamOptions {
                                model_profile_id,
                                reasoning_effort,
                                agent_id,
                                sender_user_id: request.sender_user_id,
                                sender_username: request.sender_username,
                                message_id: request.message_id,
                                chat_type: request.chat_type,
                                platform,
                                im_attachments: request.im_attachments,
                                im_documents: request.im_documents,
                                cancel: request.cancel,
                                async_agent: request.async_agent,
                                ..StreamOptions::default()
                            };
                            yield CoreChatEvent::SupervisorStarted;
                            // Keep the workflow implementation behind a heap
                            // boundary. Its concrete async-stream type contains
                            // supervisor and agent branches that do not belong
                            // in this channel dispatcher's state machine.
                            let mut stream = Box::pin(self.bot.stream_workflow_start_with_options(
                                &request.session_id,
                                workflow_id,
                                context,
                                max_rounds,
                                opts,
                            ));
                            while let Some(event) = stream.next().await {
                                let done = matches!(event, CatEvent::Done);
                                yield CoreChatEvent::Bot(event);
                                if done {
                                    yield CoreChatEvent::Done;
                                    return;
                                }
                            }
                            yield CoreChatEvent::Done;
                            return;
                        }
                        Ok(RuntimeCommandPipelineResult::Continue { text, prefix, skill_injections: injections }) => {
                            if !prefix.is_empty() {
                                yield CoreChatEvent::Prefix(prefix);
                            }
                            content = Content::text(text);
                            skill_injections = injections;
                        }
                        Err(error) => {
                            yield CoreChatEvent::Bot(CatEvent::Error(remi_agentloop::prelude::AgentError::other(error.to_string())));
                            yield CoreChatEvent::Done;
                            return;
                        }
                    }
                }
                None => {}
            }

            let (stored_model_profile_id, stored_reasoning_effort, stored_agent_id) = {
                let sessions = self.sessions.lock().await;
                (
                    sessions.metadata_string(&request.session_id, SESSION_MODEL_PROFILE_METADATA_KEY),
                    parse_session_reasoning_effort(
                        sessions.metadata_string(&request.session_id, SESSION_REASONING_EFFORT_METADATA_KEY),
                    ),
                    sessions.metadata_string(&request.session_id, SESSION_AGENT_ID_METADATA_KEY),
                )
            };
            let model_profile_id = request.model_profile_id.clone().or(stored_model_profile_id);
            let reasoning_effort = request.reasoning_effort.or(stored_reasoning_effort);
            let agent_id = request.agent_id.clone().or(stored_agent_id);
            let platform = request.platform();
            let opts = StreamOptions {
                model_profile_id,
                reasoning_effort,
                agent_id,
                skill_injections,
                sender_user_id: request.sender_user_id,
                sender_username: request.sender_username,
                message_id: request.message_id,
                chat_type: request.chat_type,
                platform,
                im_attachments: request.im_attachments,
                im_documents: request.im_documents,
                cancel: request.cancel,
                async_agent: request.async_agent,
                ..StreamOptions::default()
            };
            let user_text_for_history = content.text_content();
            let mut assistant_text_for_history = String::new();
            if self
                .bot
                .workflow_status(&session_id)
                .await
                .is_some_and(|instance| {
                    matches!(instance.status, bot_core::WorkflowStatus::Active)
                })
            {
                yield CoreChatEvent::SupervisorStarted;
            }
            // The workflow dispatcher may contain a supervisor evaluation and
            // several continuation streams. Store it separately so this outer
            // chat stream remains small regardless of those branches.
            let mut stream = Box::pin(
                self.bot
                    .stream_active_workflow_with_options(&session_id, content, opts),
            );
            while let Some(event) = stream.next().await {
                if persist_agent_child_turn {
                    if let CatEvent::Text(delta) = &event {
                        assistant_text_for_history.push_str(delta);
                    }
                }
                let done = matches!(event, CatEvent::Done);
                yield CoreChatEvent::Bot(event);
                if done {
                    break;
                }
            }
            if persist_agent_child_turn {
                append_direct_sub_session_turn(
                    &self,
                    &request.session_id,
                    user_text_for_history.trim(),
                    &assistant_text_for_history,
                )
                .await;
            }
            yield CoreChatEvent::Done;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ChatChannel, ChatRequest};
    use bot_core::ReasoningEffort;

    #[test]
    fn text_request_enables_user_turn_preprocessing_and_sub_session_routing() {
        let request = ChatRequest::text("thread-1", ChatChannel::Tui, "hello");

        assert_eq!(request.session_id, "thread-1");
        assert_eq!(request.content.text_content(), "hello");
        assert!(request.command_preprocess);
        assert!(request.sub_session_routing);
        assert_eq!(request.platform(), Some("tui".to_string()));
    }

    #[test]
    fn runtime_overrides_are_carried_per_request() {
        let request = ChatRequest::text("thread", ChatChannel::Web, "hello")
            .with_runtime_overrides(
                Some("model-a".to_string()),
                Some(ReasoningEffort::High),
                Some("agent-a".to_string()),
                Some(true),
            );
        assert_eq!(request.model_profile_id.as_deref(), Some("model-a"));
        assert_eq!(request.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(request.agent_id.as_deref(), Some("agent-a"));
        assert!(request.async_agent);
    }

    #[test]
    fn explicit_platform_overrides_channel_default() {
        let mut request = ChatRequest::text("thread-1", ChatChannel::Web, "hello");
        request.platform = Some("custom".to_string());

        assert_eq!(request.platform(), Some("custom".to_string()));
    }

    #[test]
    fn acp_channel_sets_acp_platform() {
        let request = ChatRequest::text("thread-1", ChatChannel::Acp, "hello");

        assert_eq!(request.platform(), Some("acp".to_string()));
    }

    #[test]
    fn request_builders_apply_common_channel_metadata() {
        let request = ChatRequest::text("thread-1", ChatChannel::Web, "hello")
            .with_sender("user-1", Some("User One".to_string()))
            .with_message("msg-1", "p2p")
            .with_platform(Some("web".to_string()));

        assert_eq!(request.sender_user_id.as_deref(), Some("user-1"));
        assert_eq!(request.sender_username.as_deref(), Some("User One"));
        assert_eq!(request.message_id.as_deref(), Some("msg-1"));
        assert_eq!(request.chat_type.as_deref(), Some("p2p"));
        assert_eq!(request.platform.as_deref(), Some("web"));
    }
}
