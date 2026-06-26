use std::rc::Rc;
use std::sync::Arc;

use bot_core::{CatEvent, Content, ImAttachment, ImDocument, StreamOptions};
use futures::{Stream, StreamExt};

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
    LocalTrigger,
}

impl ChatChannel {
    fn as_platform(&self) -> Option<String> {
        match self {
            Self::Cli => None,
            Self::Tui => Some("tui".to_string()),
            Self::Feishu => Some("feishu".to_string()),
            Self::Web => Some("web".to_string()),
            Self::LocalTrigger => None,
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
    pub(crate) todo_create_via_sdk: bool,
    pub(crate) trigger_tools_enabled: bool,
    pub(crate) trigger_run: bool,
    pub(crate) im_attachments: Vec<ImAttachment>,
    pub(crate) im_documents: Vec<ImDocument>,
    pub(crate) cancel: Option<Arc<tokio::sync::Notify>>,
    pub(crate) command_preprocess: bool,
    pub(crate) sub_session_routing: bool,
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
            todo_create_via_sdk: false,
            trigger_tools_enabled: false,
            trigger_run: false,
            im_attachments: Vec::new(),
            im_documents: Vec::new(),
            cancel: None,
            command_preprocess: true,
            sub_session_routing: true,
        }
    }

    pub(crate) fn trigger_text(
        session_id: impl Into<String>,
        channel: ChatChannel,
        text: impl Into<String>,
    ) -> Self {
        let mut request = Self::text(session_id, channel, text);
        request.command_preprocess = false;
        request.sub_session_routing = false;
        request.trigger_run = true;
        request
    }

    pub(crate) fn with_content(mut self, content: Content) -> Self {
        self.content = content;
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

    pub(crate) fn with_cancel(mut self, cancel: Arc<tokio::sync::Notify>) -> Self {
        self.cancel = Some(cancel);
        self
    }

    pub(crate) fn enable_sdk_todo_and_triggers(mut self) -> Self {
        self.todo_create_via_sdk = true;
        self.trigger_tools_enabled = true;
        self
    }

    pub(crate) fn enable_sdk_todo(mut self) -> Self {
        self.todo_create_via_sdk = true;
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

#[derive(Debug, Clone)]
pub(crate) enum CoreChatEvent {
    Prefix(String),
    Reply(String),
    Bot(CatEvent),
    Done,
}

impl Runtime {
    pub(crate) fn chat(self: Rc<Self>, request: ChatRequest) -> impl Stream<Item = CoreChatEvent> {
        async_stream::stream! {
            let user_text = request.content.text_content();
            let mut session_id = request.session_id.clone();
            let mut content = request.content.clone();
            let mut skill_injections = Vec::new();
            let mut persist_agent_child_turn = false;

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
                                todo_create_via_sdk: request.todo_create_via_sdk,
                                trigger_tools_enabled: request.trigger_tools_enabled,
                                trigger_run: request.trigger_run,
                                im_attachments: request.im_attachments,
                                im_documents: request.im_documents,
                                cancel: request.cancel,
                                ..StreamOptions::default()
                            };
                            let mut stream = std::pin::pin!(
                                self.bot.stream_workflow_start_with_options(
                                    &request.session_id,
                                    workflow_id,
                                    context,
                                    max_rounds,
                                    opts,
                                )
                            );
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
                todo_create_via_sdk: request.todo_create_via_sdk,
                trigger_tools_enabled: request.trigger_tools_enabled,
                trigger_run: request.trigger_run,
                im_attachments: request.im_attachments,
                im_documents: request.im_documents,
                cancel: request.cancel,
                ..StreamOptions::default()
            };
            let user_text_for_history = content.text_content();
            let mut assistant_text_for_history = String::new();
            let mut stream = std::pin::pin!(self.bot.stream_with_options(&session_id, content, opts));
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

    #[test]
    fn text_request_enables_user_turn_preprocessing_and_sub_session_routing() {
        let request = ChatRequest::text("thread-1", ChatChannel::Tui, "hello");

        assert_eq!(request.session_id, "thread-1");
        assert_eq!(request.content.text_content(), "hello");
        assert!(request.command_preprocess);
        assert!(request.sub_session_routing);
        assert!(!request.trigger_run);
        assert_eq!(request.platform(), Some("tui".to_string()));
    }

    #[test]
    fn trigger_request_keeps_dispatch_on_target_thread() {
        let request = ChatRequest::trigger_text("thread-1", ChatChannel::LocalTrigger, "run");

        assert_eq!(request.session_id, "thread-1");
        assert_eq!(request.content.text_content(), "run");
        assert!(!request.command_preprocess);
        assert!(!request.sub_session_routing);
        assert!(request.trigger_run);
        assert_eq!(request.platform(), None);
    }

    #[test]
    fn explicit_platform_overrides_channel_default() {
        let mut request = ChatRequest::text("thread-1", ChatChannel::Web, "hello");
        request.platform = Some("custom".to_string());

        assert_eq!(request.platform(), Some("custom".to_string()));
    }

    #[test]
    fn request_builders_apply_common_channel_metadata() {
        let request = ChatRequest::text("thread-1", ChatChannel::Web, "hello")
            .with_sender("user-1", Some("User One".to_string()))
            .with_message("msg-1", "p2p")
            .with_platform(Some("web".to_string()))
            .enable_sdk_todo_and_triggers();

        assert_eq!(request.sender_user_id.as_deref(), Some("user-1"));
        assert_eq!(request.sender_username.as_deref(), Some("User One"));
        assert_eq!(request.message_id.as_deref(), Some("msg-1"));
        assert_eq!(request.chat_type.as_deref(), Some("p2p"));
        assert_eq!(request.platform.as_deref(), Some("web"));
        assert!(request.todo_create_via_sdk);
        assert!(request.trigger_tools_enabled);
    }
}
