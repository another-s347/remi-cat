use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::Notify;

use remi_agentloop::prelude::{
    AgentConfig, AgentState, CancellationToken, ChatCtx, ChatCtxState, Content, ContentPart,
    LoopInput, Message, Role, ToolDefinition, ToolDefinitionContext,
};

use crate::profile::AgentProfile;

#[derive(Debug, Clone, Default)]
pub struct CoreStreamOptions {
    pub cancel: Option<CancellationToken>,
    pub steer: Option<Arc<CoreSteerQueue>>,
    pub async_agent: bool,
}

impl CoreStreamOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    pub fn with_steer(mut self, steer: Arc<CoreSteerQueue>) -> Self {
        self.steer = Some(steer);
        self
    }

    pub fn with_async_agent(mut self, enabled: bool) -> Self {
        self.async_agent = enabled;
        self
    }
}

#[derive(Debug, Clone)]
pub enum CoreSteerSource {
    User,
    UserNextTurn,
    BackgroundToolCompletion,
}

#[derive(Debug, Clone)]
pub struct CoreSteerInput {
    pub id: String,
    pub content: Content,
    pub preview: String,
    pub message_metadata: Option<serde_json::Value>,
    pub user_name: Option<String>,
    pub source: CoreSteerSource,
}

#[derive(Debug, Clone)]
pub struct CoreSteerBatch {
    pub ids: Vec<String>,
    pub content: Content,
    pub preview: String,
    pub count: usize,
    pub message_metadata: Option<serde_json::Value>,
    pub user_name: Option<String>,
}

#[derive(Debug, Default)]
pub struct CoreSteerQueue {
    pending: Mutex<VecDeque<CoreSteerInput>>,
    notify: Notify,
}

impl CoreSteerQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, input: CoreSteerInput) {
        self.pending
            .lock()
            .expect("steer queue lock poisoned")
            .push_back(input);
        self.notify.notify_one();
    }

    pub async fn notified(&self) {
        self.notify.notified().await;
    }

    pub fn drain_batch(&self) -> Option<CoreSteerBatch> {
        let mut pending = self.pending.lock().expect("steer queue lock poisoned");
        if pending.is_empty() {
            return None;
        }
        let drained = pending.drain(..).collect::<Vec<_>>();
        drop(pending);
        Some(merge_steer_inputs(drained))
    }
}

fn merge_steer_inputs(inputs: Vec<CoreSteerInput>) -> CoreSteerBatch {
    let count = inputs.len();
    let ids = inputs
        .iter()
        .map(|input| input.id.clone())
        .collect::<Vec<_>>();
    let preview = inputs
        .iter()
        .map(|input| input.preview.trim())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" / ");
    let content = if inputs.len() == 1 {
        if matches!(inputs[0].source, CoreSteerSource::UserNextTurn) {
            inputs[0].content.clone()
        } else {
            prefixed_steer_content(steer_prefix(&inputs[0]), inputs[0].content.clone())
        }
    } else {
        let mut parts = vec![ContentPart::text(steer_batch_prefix(&inputs))];
        for (idx, input) in inputs.iter().enumerate() {
            parts.push(ContentPart::text(format!(
                "\n{}. {}",
                idx + 1,
                steer_item_label(input)
            )));
            parts.extend(content_into_parts(input.content.clone()));
        }
        Content::parts(parts)
    };
    let message_metadata = inputs
        .last()
        .and_then(|input| input.message_metadata.clone());
    let user_name = inputs.last().and_then(|input| input.user_name.clone());
    CoreSteerBatch {
        ids,
        content,
        preview,
        count,
        message_metadata,
        user_name,
    }
}

fn steer_prefix(input: &CoreSteerInput) -> &'static str {
    match input.source {
        CoreSteerSource::User => "[User steer received while this run was active]\n",
        CoreSteerSource::UserNextTurn => "",
        CoreSteerSource::BackgroundToolCompletion => {
            "[Background tool task completed while this run was active]\n"
        }
    }
}

fn steer_batch_prefix(inputs: &[CoreSteerInput]) -> String {
    let all_background = inputs
        .iter()
        .all(|input| matches!(input.source, CoreSteerSource::BackgroundToolCompletion));
    let all_next_turn = inputs
        .iter()
        .all(|input| matches!(input.source, CoreSteerSource::UserNextTurn));
    if all_background {
        "[Background tool tasks completed while this run was active]\nMultiple queued inputs arrived in order:\n".to_string()
    } else if all_next_turn {
        "[Queued user messages]\nMultiple user messages arrived in order:\n".to_string()
    } else {
        "[Queued inputs received while this run was active]\nMultiple queued inputs arrived in order:\n".to_string()
    }
}

fn steer_item_label(input: &CoreSteerInput) -> &'static str {
    match input.source {
        CoreSteerSource::User => "User steer: ",
        CoreSteerSource::UserNextTurn => "User message: ",
        CoreSteerSource::BackgroundToolCompletion => "Background tool task completion: ",
    }
}

fn prefixed_steer_content(prefix: &str, content: Content) -> Content {
    match content {
        Content::Text(text) => Content::text(format!("{prefix}{}", text.trim())),
        Content::Parts(parts) => {
            let mut prefixed = Vec::with_capacity(parts.len() + 1);
            prefixed.push(ContentPart::text(prefix.to_string()));
            prefixed.extend(parts);
            Content::parts(prefixed)
        }
    }
}

fn content_into_parts(content: Content) -> Vec<ContentPart> {
    match content {
        Content::Text(text) => vec![ContentPart::text(text.trim().to_string())],
        Content::Parts(parts) => parts,
    }
}

pub fn effective_agent_config(base: &AgentConfig, profile: &AgentProfile) -> AgentConfig {
    let mut config = base.clone();
    if config.model.is_none() {
        config.model = profile
            .model
            .clone()
            .or_else(|| profile.models.primary.clone());
    }
    if config.base_url.is_none() {
        config.base_url = profile.base_url.clone();
    }
    config
}

pub fn filter_tool_definitions(
    definitions: Vec<ToolDefinition>,
    allowed_tools: &[String],
) -> Vec<ToolDefinition> {
    if allowed_tools.is_empty() {
        return definitions;
    }
    definitions
        .into_iter()
        .filter(|definition| tool_allowed(allowed_tools, &definition.function.name))
        .collect()
}

pub fn tool_allowed(allowed_tools: &[String], name: &str) -> bool {
    allowed_tools.is_empty() || allowed_tools.iter().any(|tool| tool == name)
}

pub fn inject_extra_tools(input: LoopInput, extra: Vec<ToolDefinition>) -> LoopInput {
    match input {
        LoopInput::Start {
            message,
            history,
            mut extra_tools,
            model,
            temperature,
            max_tokens,
            metadata,
            user_state,
        } => {
            extra_tools.extend(extra);
            LoopInput::Start {
                message,
                history,
                extra_tools,
                model,
                temperature,
                max_tokens,
                metadata,
                user_state,
            }
        }
        other => other,
    }
}

pub fn apply_profile_to_input(
    input: LoopInput,
    profile: &AgentProfile,
    effective_config: &AgentConfig,
) -> LoopInput {
    match input {
        LoopInput::Start {
            message,
            mut history,
            extra_tools,
            model,
            temperature,
            max_tokens,
            metadata,
            user_state,
        } => {
            if !profile.system_prompt.trim().is_empty()
                && !history_contains_system_prompt(&history, &profile.system_prompt)
            {
                history.insert(0, Message::system(profile.system_prompt.clone()));
            }
            let model = model.or_else(|| effective_config.model.clone());
            LoopInput::Start {
                message,
                history,
                extra_tools,
                model,
                temperature,
                max_tokens,
                metadata,
                user_state,
            }
        }
        other => other,
    }
}

fn history_contains_system_prompt(history: &[Message], system_prompt: &str) -> bool {
    history.iter().any(|message| {
        message.role == Role::System
            && matches!(&message.content, Content::Text(text) if text == system_prompt)
    })
}

pub fn tool_ctx_from_state(state: &AgentState) -> ChatCtx {
    tool_ctx_from_state_with_cancel(state, None)
}

pub fn tool_ctx_from_state_with_cancel(
    state: &AgentState,
    cancel: Option<CancellationToken>,
) -> ChatCtx {
    let ctx = ChatCtx::with_ids(
        state.thread_id.clone(),
        state.run_id.clone(),
        ChatCtxState {
            user_state: state.user_state.clone(),
            metadata: state.config.metadata.clone(),
            ..ChatCtxState::default()
        },
    );
    if let Some(cancel) = cancel {
        let token = ctx.runtime().cancellation();
        tokio::spawn(async move {
            cancel.cancelled().await;
            token.cancel();
        });
    }
    ctx
}

pub fn chat_ctx_from_input(input: &LoopInput, cancel: Option<CancellationToken>) -> ChatCtx {
    let ctx = match input {
        LoopInput::Start {
            metadata,
            user_state,
            ..
        } => ChatCtx::new(ChatCtxState {
            user_state: user_state.clone().unwrap_or(serde_json::Value::Null),
            metadata: metadata.clone(),
            ..ChatCtxState::default()
        }),
        LoopInput::Resume { state, .. } => ChatCtx::with_ids(
            state.thread_id.clone(),
            state.run_id.clone(),
            ChatCtxState {
                user_state: state.user_state.clone(),
                metadata: state.config.metadata.clone(),
                ..ChatCtxState::default()
            },
        ),
    };
    if let Some(cancel) = cancel {
        let token = ctx.runtime().cancellation();
        tokio::spawn(async move {
            cancel.cancelled().await;
            token.cancel();
        });
    }
    ctx
}

pub fn tool_definition_ctx_from_chat_ctx(ctx: &ChatCtx) -> ToolDefinitionContext {
    ToolDefinitionContext {
        thread_id: Some(ctx.thread_id()),
        run_id: Some(ctx.run_id()),
        metadata: ctx.metadata(),
        user_state: ctx.user_state(),
    }
}

pub fn tool_definition_ctx_from_state(state: &AgentState) -> ToolDefinitionContext {
    ToolDefinitionContext {
        thread_id: Some(state.thread_id.clone()),
        run_id: Some(state.run_id.clone()),
        metadata: state.config.metadata.clone(),
        user_state: state.user_state.clone(),
    }
}

pub fn build_tool_definition_ctx(input: &LoopInput) -> ToolDefinitionContext {
    match input {
        LoopInput::Start {
            metadata,
            user_state,
            ..
        } => ToolDefinitionContext {
            metadata: metadata.clone(),
            user_state: user_state.clone().unwrap_or(serde_json::Value::Null),
            ..ToolDefinitionContext::default()
        },
        LoopInput::Resume { state, .. } => tool_definition_ctx_from_state(state),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_prompt_is_not_duplicated() {
        let profile = AgentProfile::from_markdown(
            r#"---
id: markdown
name: Markdown Agent
---
You are a markdown agent.
"#,
        )
        .unwrap();
        let input =
            LoopInput::start("hello").history(vec![Message::system("You are a markdown agent.")]);

        let effective_config = effective_agent_config(&AgentConfig::default(), &profile);
        let LoopInput::Start { history, .. } =
            apply_profile_to_input(input, &profile, &effective_config)
        else {
            panic!("expected start input");
        };

        let count = history
            .iter()
            .filter(|message| {
                message.role == Role::System
                    && matches!(&message.content, Content::Text(text) if text == "You are a markdown agent.")
            })
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn effective_agent_config_uses_profile_defaults_without_overriding_explicit_config() {
        let profile = AgentProfile::from_markdown(
            r#"---
id: markdown
name: Markdown Agent
model: profile-model
base_url: https://profile.example/v1
---
You are a markdown agent.
"#,
        )
        .unwrap();

        let defaulted = effective_agent_config(&AgentConfig::default(), &profile);
        assert_eq!(defaulted.model.as_deref(), Some("profile-model"));
        assert_eq!(
            defaulted.base_url.as_deref(),
            Some("https://profile.example/v1")
        );

        let explicit = effective_agent_config(
            &AgentConfig::default()
                .with_model("explicit-model")
                .with_base_url("https://explicit.example/v1"),
            &profile,
        );
        assert_eq!(explicit.model.as_deref(), Some("explicit-model"));
        assert_eq!(
            explicit.base_url.as_deref(),
            Some("https://explicit.example/v1")
        );
    }

    #[test]
    fn steer_queue_drains_inputs_in_submission_order_as_one_batch() {
        let queue = CoreSteerQueue::new();
        queue.push(CoreSteerInput {
            id: "one".to_string(),
            content: Content::text("first"),
            preview: "first".to_string(),
            message_metadata: None,
            user_name: None,
            source: CoreSteerSource::User,
        });
        queue.push(CoreSteerInput {
            id: "two".to_string(),
            content: Content::text("second"),
            preview: "second".to_string(),
            message_metadata: None,
            user_name: Some("alice".to_string()),
            source: CoreSteerSource::User,
        });

        let batch = queue.drain_batch().expect("batch should be present");

        assert_eq!(batch.ids, vec!["one", "two"]);
        assert_eq!(batch.count, 2);
        assert_eq!(batch.user_name.as_deref(), Some("alice"));
        let text = batch.content.text_content();
        assert!(text.contains("1. User steer: first"));
        assert!(text.contains("2. User steer: second"));
        assert!(queue.drain_batch().is_none());
    }

    #[tokio::test]
    async fn steer_queue_notifies_background_waiters() {
        let queue = Arc::new(CoreSteerQueue::new());
        let waiter = {
            let queue = Arc::clone(&queue);
            tokio::spawn(async move { queue.notified().await })
        };
        queue.push(CoreSteerInput {
            id: "next".to_string(),
            content: Content::text("continue now"),
            preview: "continue now".to_string(),
            message_metadata: None,
            user_name: None,
            source: CoreSteerSource::User,
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("waiter should wake")
            .expect("waiter task should succeed");
    }

    #[test]
    fn next_turn_input_is_not_prefixed_as_a_steer() {
        let queue = CoreSteerQueue::new();
        queue.push(CoreSteerInput {
            id: "next".to_string(),
            content: Content::text("normal user message"),
            preview: "normal user message".to_string(),
            message_metadata: None,
            user_name: None,
            source: CoreSteerSource::UserNextTurn,
        });

        let batch = queue.drain_batch().expect("batch should be present");
        assert_eq!(batch.content.text_content(), "normal user message");
    }

    #[test]
    fn steer_queue_preserves_multimodal_content_parts() {
        let queue = CoreSteerQueue::new();
        queue.push(CoreSteerInput {
            id: "image".to_string(),
            content: Content::parts(vec![
                ContentPart::text("describe "),
                ContentPart::image_base64("image/png", "YWJj"),
                ContentPart::text(" please"),
            ]),
            preview: "describe image".to_string(),
            message_metadata: None,
            user_name: None,
            source: CoreSteerSource::User,
        });

        let batch = queue.drain_batch().expect("batch should be present");

        match batch.content {
            Content::Parts(parts) => {
                assert!(matches!(
                    parts.first(),
                    Some(ContentPart::Text { text })
                        if text == "[User steer received while this run was active]\n"
                ));
                assert!(matches!(
                    parts.get(1),
                    Some(ContentPart::Text { text }) if text == "describe "
                ));
                assert!(matches!(
                    parts.get(2),
                    Some(ContentPart::ImageBase64 { media_type, data })
                        if media_type == "image/png" && data == "YWJj"
                ));
                assert!(matches!(
                    parts.get(3),
                    Some(ContentPart::Text { text }) if text == " please"
                ));
            }
            other => panic!("expected parts content, got {other:?}"),
        }
    }
}
