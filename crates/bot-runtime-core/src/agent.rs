use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Instant;

use async_stream::stream;
use futures::{Stream, StreamExt};
use remi_agentloop::prelude::{
    Agent, AgentConfig, AgentError, AgentState, Content, LoopInput, Message, ParsedToolCall, Role,
    ToolCallOutcome, ToolContext, ToolDefinition, ToolDefinitionContext,
};
use remi_agentloop::tool::registry::{DefaultToolRegistry, ToolRegistry};
use remi_agentloop::types::AgentEvent;

use crate::events::CoreAgentEvent;
use crate::loop_driver::{CoreAgentLoop, CoreDriveConfig, CoreDriveEvent};
use crate::profile::AgentProfile;

#[derive(Debug, Clone, Default)]
pub struct CoreStreamOptions {
    pub cancel: Option<Arc<tokio::sync::Notify>>,
}

impl CoreStreamOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cancel(mut self, cancel: Arc<tokio::sync::Notify>) -> Self {
        self.cancel = Some(cancel);
        self
    }
}

#[derive(Debug, Clone)]
pub struct CoreAgentConfig {
    pub agent: AgentConfig,
    pub profile: AgentProfile,
    pub metadata: Option<serde_json::Value>,
    pub max_tool_rounds: usize,
}

impl CoreAgentConfig {
    pub fn new(agent: AgentConfig, profile: AgentProfile) -> Self {
        Self {
            agent,
            profile,
            metadata: None,
            max_tool_rounds: 16,
        }
    }

    pub fn from_markdown(agent: AgentConfig, markdown: &str) -> anyhow::Result<Self> {
        Ok(Self::new(agent, AgentProfile::from_markdown(markdown)?))
    }

    pub fn effective_agent_config(&self) -> AgentConfig {
        effective_agent_config(&self.agent, &self.profile)
    }
}

pub struct CoreAgent {
    config: CoreAgentConfig,
    tools: Arc<DefaultToolRegistry>,
}

impl CoreAgent {
    pub fn new(config: CoreAgentConfig, tools: DefaultToolRegistry) -> Self {
        Self {
            config,
            tools: Arc::new(tools),
        }
    }

    pub fn from_markdown(
        agent: AgentConfig,
        markdown: &str,
        tools: DefaultToolRegistry,
    ) -> anyhow::Result<Self> {
        Ok(Self::new(
            CoreAgentConfig::from_markdown(agent, markdown)?,
            tools,
        ))
    }

    pub fn config(&self) -> &CoreAgentConfig {
        &self.config
    }

    pub fn profile(&self) -> &AgentProfile {
        &self.config.profile
    }

    pub fn effective_agent_config(&self) -> AgentConfig {
        self.config.effective_agent_config()
    }

    pub fn stream_text<I>(
        &self,
        inner: I,
        input: impl Into<String>,
        history: Vec<Message>,
    ) -> impl Stream<Item = CoreAgentEvent> + '_
    where
        I: Agent<Request = LoopInput, Response = AgentEvent, Error = AgentError> + 'static,
    {
        self.stream_content(inner, Content::text(input.into()), history)
    }

    pub fn stream_text_with_options<I>(
        &self,
        inner: I,
        input: impl Into<String>,
        history: Vec<Message>,
        options: CoreStreamOptions,
    ) -> impl Stream<Item = CoreAgentEvent> + '_
    where
        I: Agent<Request = LoopInput, Response = AgentEvent, Error = AgentError> + 'static,
    {
        self.stream_content_with_options(inner, Content::text(input.into()), history, options)
    }

    pub fn stream_content<I>(
        &self,
        inner: I,
        content: Content,
        history: Vec<Message>,
    ) -> impl Stream<Item = CoreAgentEvent> + '_
    where
        I: Agent<Request = LoopInput, Response = AgentEvent, Error = AgentError> + 'static,
    {
        let mut input = LoopInput::start_content(content).history(history);
        if let Some(model) = self.config.profile.model.clone().or(self
            .config
            .profile
            .models
            .primary
            .clone())
        {
            input = input.model(model);
        }
        if let Some(metadata) = self.config.metadata.clone() {
            input = input.metadata(metadata);
        }
        self.stream_loop_input(inner, input)
    }

    pub fn stream_content_with_options<I>(
        &self,
        inner: I,
        content: Content,
        history: Vec<Message>,
        options: CoreStreamOptions,
    ) -> impl Stream<Item = CoreAgentEvent> + '_
    where
        I: Agent<Request = LoopInput, Response = AgentEvent, Error = AgentError> + 'static,
    {
        let mut input = LoopInput::start_content(content).history(history);
        if let Some(model) = self.config.profile.model.clone().or(self
            .config
            .profile
            .models
            .primary
            .clone())
        {
            input = input.model(model);
        }
        if let Some(metadata) = self.config.metadata.clone() {
            input = input.metadata(metadata);
        }
        self.stream_loop_input_with_options(inner, input, options)
    }

    pub fn stream_loop_input<I>(
        &self,
        inner: I,
        input: LoopInput,
    ) -> impl Stream<Item = CoreAgentEvent> + '_
    where
        I: Agent<Request = LoopInput, Response = AgentEvent, Error = AgentError> + 'static,
    {
        self.stream_loop_input_with_options(inner, input, CoreStreamOptions::default())
    }

    pub fn stream_loop_input_with_options<I>(
        &self,
        inner: I,
        mut input: LoopInput,
        options: CoreStreamOptions,
    ) -> impl Stream<Item = CoreAgentEvent> + '_
    where
        I: Agent<Request = LoopInput, Response = AgentEvent, Error = AgentError> + 'static,
    {
        let config = self.config.clone();
        let tools = Arc::clone(&self.tools);
        stream! {
            let allowed_tools = config.profile.tools.clone();
            let effective_config = config.effective_agent_config();
            let mut agent_loop = CoreAgentLoop::new(CoreDriveConfig {
                cancel: options.cancel,
                max_tool_rounds: Some(config.max_tool_rounds),
            });
            loop {
                input = apply_profile_to_input(input, &config.profile, &effective_config);
                let tool_def_ctx = build_tool_definition_ctx(&input);
                let tool_definitions = filter_tool_definitions(
                    tools.definitions_with_context(&tool_def_ctx),
                    &allowed_tools,
                );
                let run_input = inject_extra_tools(input.clone(), tool_definitions);

                let events_result = inner.chat(run_input).await;

                let events = match events_result {
                    Ok(events) => events,
                    Err(error) => {
                        yield CoreAgentEvent::Error(error);
                        return;
                    }
                };

                let mut events = std::pin::pin!(agent_loop.drive(events));
                let mut dispatch = None;

                while let Some(event) = events.next().await {
                    match event {
                        CoreDriveEvent::Text(delta) => yield CoreAgentEvent::Text(delta),
                        CoreDriveEvent::Thinking(content) => yield CoreAgentEvent::Thinking(content),
                        CoreDriveEvent::ToolCallStart { id, name } => {
                            yield CoreAgentEvent::ToolCallStart { id, name };
                        }
                        CoreDriveEvent::ToolCallArgumentsDelta { id, delta } => {
                            yield CoreAgentEvent::ToolCallArgumentsDelta { id, delta };
                        }
                        CoreDriveEvent::ToolResult { id, name, result } => {
                            yield CoreAgentEvent::ToolCallResult {
                                id,
                                name,
                                args: serde_json::Value::Null,
                                result,
                                success: true,
                                elapsed_ms: 0,
                            };
                        }
                        CoreDriveEvent::SubSession(event) => yield CoreAgentEvent::SubSession(event),
                        CoreDriveEvent::Usage(_) => {}
                        CoreDriveEvent::Checkpoint { state } => {
                            yield CoreAgentEvent::Checkpoint(state.clone());
                            yield CoreAgentEvent::StateUpdate(state.user_state.clone());
                        }
                        CoreDriveEvent::ToolDispatch(next_dispatch) => {
                            dispatch = Some(next_dispatch);
                            break;
                        }
                        CoreDriveEvent::Done { state, stats, .. } => {
                            yield CoreAgentEvent::Stats {
                                prompt_tokens: stats.prompt_tokens,
                                completion_tokens: stats.completion_tokens,
                                elapsed_ms: stats.elapsed_ms,
                            };
                            yield CoreAgentEvent::Done { state };
                            return;
                        }
                        CoreDriveEvent::Cancelled { .. } => {
                            yield CoreAgentEvent::Cancelled;
                            return;
                        }
                        CoreDriveEvent::Error { error, .. } => {
                            yield CoreAgentEvent::Error(error);
                            return;
                        }
                        CoreDriveEvent::ToolDelta { id, name, delta } => {
                            yield CoreAgentEvent::Custom {
                                event_type: "tool_delta".to_string(),
                                extra: serde_json::json!({ "id": id, "name": name, "delta": delta }),
                            };
                        }
                        CoreDriveEvent::Ignored => {}
                    }
                }

                let Some(dispatch) = dispatch else {
                    yield CoreAgentEvent::Done { state: None };
                    return;
                };
                if dispatch.tool_calls.is_empty() {
                    input = resume_input(dispatch.state, dispatch.completed_results);
                    continue;
                }

                let outcomes = execute_tools(
                    tools.as_ref(),
                    &allowed_tools,
                    &dispatch.state,
                    dispatch.tool_calls,
                ).await;
                let mut all_results = dispatch.completed_results;
                all_results.extend(outcomes);
                input = resume_input(dispatch.state, all_results);
            }
        }
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

async fn execute_tools(
    registry: &DefaultToolRegistry,
    allowed_tools: &[String],
    state: &AgentState,
    tool_calls: Vec<ParsedToolCall>,
) -> Vec<ToolCallOutcome> {
    let resume_map = HashMap::new();
    let mut outcomes = Vec::new();
    let ctx = tool_ctx_from_state(state);
    for call in &tool_calls {
        let start = Instant::now();
        if !tool_allowed(allowed_tools, &call.name) {
            outcomes.push(ToolCallOutcome::Error {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                error: format!("tool is not allowed for current agent: {}", call.name),
            });
            continue;
        }
        let result = registry
            .execute_parallel(std::slice::from_ref(call), &resume_map, &ctx)
            .await
            .into_iter()
            .next()
            .map(|(_, result)| result);
        let outcome = match result {
            Some(Ok(mut output)) => ToolCallOutcome::Result {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                content: collect_tool_output(&mut output).await,
            },
            Some(Err(error)) => ToolCallOutcome::Error {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                error: format!("{error}"),
            },
            None => ToolCallOutcome::Error {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                error: "tool execution returned no result".to_string(),
            },
        };
        tracing::debug!(
            tool_call_id = %call.id,
            tool_name = %call.name,
            elapsed_ms = start.elapsed().as_millis(),
            "core agent executed tool"
        );
        outcomes.push(outcome);
    }
    outcomes
}

fn filter_tool_definitions(
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

fn tool_allowed(allowed_tools: &[String], name: &str) -> bool {
    allowed_tools.is_empty() || allowed_tools.iter().any(|tool| tool == name)
}

fn resume_input(state: AgentState, results: Vec<ToolCallOutcome>) -> LoopInput {
    LoopInput::resume(state, results)
}

async fn collect_tool_output(output: &mut remi_agentloop::tool::BoxedToolResult<'_>) -> Content {
    match output {
        remi_agentloop::tool::ToolResult::Output(stream) => {
            let mut final_content = Content::text(String::new());
            let mut delta_text = String::new();
            while let Some(item) = stream.next().await {
                match item {
                    remi_agentloop::tool::ToolOutput::Result(content) => {
                        final_content = content;
                    }
                    remi_agentloop::tool::ToolOutput::Delta(delta) => {
                        delta_text.push_str(&delta);
                    }
                    remi_agentloop::tool::ToolOutput::SubSession(_) => {}
                }
            }
            if matches!(&final_content, Content::Text(text) if text.is_empty())
                && !delta_text.is_empty()
            {
                Content::text(delta_text)
            } else {
                final_content
            }
        }
        remi_agentloop::tool::ToolResult::Interrupt(request) => Content::text(
            serde_json::to_string(&request.data).unwrap_or_else(|_| "tool interrupted".to_string()),
        ),
    }
}

pub fn inject_extra_tools(input: LoopInput, extra: Vec<ToolDefinition>) -> LoopInput {
    match input {
        LoopInput::Start {
            content,
            history,
            mut extra_tools,
            model,
            temperature,
            max_tokens,
            user_name,
            metadata,
            message_metadata,
            user_state,
        } => {
            extra_tools.extend(extra);
            LoopInput::Start {
                content,
                history,
                extra_tools,
                model,
                temperature,
                max_tokens,
                user_name,
                metadata,
                message_metadata,
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
            content,
            mut history,
            extra_tools,
            model,
            temperature,
            max_tokens,
            user_name,
            metadata,
            message_metadata,
            user_state,
        } => {
            if !profile.system_prompt.trim().is_empty()
                && !history_contains_system_prompt(&history, &profile.system_prompt)
            {
                history.insert(0, Message::system(profile.system_prompt.clone()));
            }
            let model = model.or_else(|| effective_config.model.clone());
            LoopInput::Start {
                content,
                history,
                extra_tools,
                model,
                temperature,
                max_tokens,
                user_name,
                metadata,
                message_metadata,
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

pub fn tool_ctx_from_state(state: &AgentState) -> ToolContext {
    ToolContext {
        config: AgentConfig::default(),
        thread_id: Some(state.thread_id.clone()),
        run_id: state.run_id.clone(),
        metadata: state.config.metadata.clone(),
        user_state: Arc::new(RwLock::new(state.user_state.clone())),
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
        LoopInput::Resume { state, .. } | LoopInput::Cancel { state } => ToolDefinitionContext {
            thread_id: Some(state.thread_id.clone()),
            run_id: Some(state.run_id.clone()),
            metadata: state.config.metadata.clone(),
            user_state: state.user_state.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dynamic_tool::register_dynamic_tool_definitions;
    use futures::stream;
    use remi_agentloop::prelude::{
        Checkpoint, CheckpointStatus, RunId, StepConfig, ThreadId, Tool, ToolOutput, ToolResult,
    };

    struct EchoTool;

    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "Echoes a fixed response."
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _resume: Option<remi_agentloop::types::ResumePayload>,
            _ctx: &ToolContext,
        ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
            Ok(ToolResult::Output(stream::iter(vec![ToolOutput::text(
                "tool-output",
            )])))
        }
    }

    struct MarkdownToolInnerAgent;

    impl Agent for MarkdownToolInnerAgent {
        type Request = LoopInput;
        type Response = AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            req: Self::Request,
        ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
            match req {
                LoopInput::Start {
                    history,
                    extra_tools,
                    model,
                    ..
                } => {
                    assert_eq!(model.as_deref(), Some("test-model"));
                    assert!(
                        history.iter().any(|message| {
                            message.role == Role::System
                                && matches!(&message.content, Content::Text(text) if text == "You are a markdown agent.")
                        }),
                        "markdown profile system prompt was not injected: {history:?}"
                    );
                    let tool_names = extra_tools
                        .iter()
                        .map(|definition| definition.function.name.as_str())
                        .collect::<Vec<_>>();
                    assert_eq!(tool_names, vec!["echo"]);
                    Ok(stream::iter(vec![AgentEvent::NeedToolExecution {
                        state: AgentState::new(StepConfig::new("test-model")),
                        tool_calls: vec![ParsedToolCall {
                            id: "call-echo".to_string(),
                            name: "echo".to_string(),
                            arguments: serde_json::json!({}),
                        }],
                        completed_results: Vec::new(),
                    }]))
                }
                LoopInput::Resume { results, .. } => {
                    let content = results
                        .into_iter()
                        .find_map(|result| match result {
                            ToolCallOutcome::Result { content, .. } => Some(content.text_content()),
                            ToolCallOutcome::Error { .. } => None,
                        })
                        .unwrap_or_default();
                    Ok(stream::iter(vec![
                        AgentEvent::TextDelta(content),
                        AgentEvent::Done,
                    ]))
                }
                LoopInput::Cancel { .. } => Ok(stream::iter(vec![AgentEvent::Cancelled])),
            }
        }
    }

    struct ExternalToolInnerAgent;

    impl Agent for ExternalToolInnerAgent {
        type Request = LoopInput;
        type Response = AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            req: Self::Request,
        ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
            match req {
                LoopInput::Start { extra_tools, .. } => {
                    let tool_names = extra_tools
                        .iter()
                        .map(|definition| definition.function.name.as_str())
                        .collect::<Vec<_>>();
                    assert_eq!(tool_names, vec!["external_echo"]);
                    Ok(stream::iter(vec![AgentEvent::NeedToolExecution {
                        state: AgentState::new(StepConfig::new("test-model")),
                        tool_calls: vec![ParsedToolCall {
                            id: "call-external".to_string(),
                            name: "external_echo".to_string(),
                            arguments: serde_json::json!({}),
                        }],
                        completed_results: Vec::new(),
                    }]))
                }
                LoopInput::Resume { results, .. } => {
                    let content = results
                        .into_iter()
                        .find_map(|result| match result {
                            ToolCallOutcome::Result { content, .. } => Some(content.text_content()),
                            ToolCallOutcome::Error { .. } => None,
                        })
                        .unwrap_or_default();
                    Ok(stream::iter(vec![
                        AgentEvent::TextDelta(content),
                        AgentEvent::Done,
                    ]))
                }
                LoopInput::Cancel { .. } => Ok(stream::iter(vec![AgentEvent::Cancelled])),
            }
        }
    }

    struct PendingInnerAgent;

    impl Agent for PendingInnerAgent {
        type Request = LoopInput;
        type Response = AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            _req: Self::Request,
        ) -> Result<impl Stream<Item = AgentEvent>, AgentError> {
            Ok(stream::pending())
        }
    }

    struct CheckpointThenDoneInnerAgent;

    impl Agent for CheckpointThenDoneInnerAgent {
        type Request = LoopInput;
        type Response = AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            _req: Self::Request,
        ) -> Result<impl Stream<Item = AgentEvent>, AgentError> {
            let mut state = AgentState::new(StepConfig::new("test-model"));
            state.user_state = serde_json::json!({"latest": true});
            Ok(stream::iter(vec![
                AgentEvent::Checkpoint(Checkpoint::new(
                    ThreadId("thread-1".to_string()),
                    RunId("run-1".to_string()),
                    state,
                    None,
                    1,
                    CheckpointStatus::RunDone,
                    1,
                )),
                AgentEvent::Done,
            ]))
        }
    }

    #[tokio::test]
    async fn markdown_agent_injects_prompt_and_registered_tool_runs() {
        let markdown = r#"---
id: markdown
name: Markdown Agent
model: test-model
tools:
  - echo
---
You are a markdown agent.
"#;
        let mut tools = DefaultToolRegistry::new();
        tools.register(EchoTool);
        let agent = CoreAgent::from_markdown(AgentConfig::default(), markdown, tools).unwrap();
        let events = agent
            .stream_text(MarkdownToolInnerAgent, "hello", Vec::new())
            .collect::<Vec<_>>()
            .await;

        assert!(events
            .iter()
            .any(|event| matches!(event, CoreAgentEvent::Text(text) if text == "tool-output")));
        assert!(events
            .iter()
            .any(|event| matches!(event, CoreAgentEvent::Done { .. })));
    }

    #[tokio::test]
    async fn core_agent_can_be_reused_for_multiple_runs() {
        let markdown = r#"---
id: markdown
name: Markdown Agent
model: test-model
tools:
  - echo
---
You are a markdown agent.
"#;
        let mut tools = DefaultToolRegistry::new();
        tools.register(EchoTool);
        let agent = CoreAgent::from_markdown(AgentConfig::default(), markdown, tools).unwrap();

        let first = agent
            .stream_text(MarkdownToolInnerAgent, "hello", Vec::new())
            .collect::<Vec<_>>()
            .await;
        let second = agent
            .stream_text(MarkdownToolInnerAgent, "again", Vec::new())
            .collect::<Vec<_>>()
            .await;

        assert!(first
            .iter()
            .any(|event| matches!(event, CoreAgentEvent::Text(text) if text == "tool-output")));
        assert!(second
            .iter()
            .any(|event| matches!(event, CoreAgentEvent::Text(text) if text == "tool-output")));
    }

    #[tokio::test]
    async fn cancel_signal_yields_cancelled_event() {
        let markdown = r#"---
id: markdown
name: Markdown Agent
---
You are a markdown agent.
"#;
        let tools = DefaultToolRegistry::new();
        let agent = CoreAgent::from_markdown(AgentConfig::default(), markdown, tools).unwrap();
        let cancel = Arc::new(tokio::sync::Notify::new());
        let mut events = Box::pin(agent.stream_text_with_options(
            PendingInnerAgent,
            "hello",
            Vec::new(),
            CoreStreamOptions::new().with_cancel(Arc::clone(&cancel)),
        ));

        cancel.notify_one();

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.next())
            .await
            .expect("cancel should complete promptly");
        assert!(matches!(event, Some(CoreAgentEvent::Cancelled)));
    }

    #[tokio::test]
    async fn done_event_carries_last_checkpoint_state() {
        let markdown = r#"---
id: markdown
name: Markdown Agent
---
You are a markdown agent.
"#;
        let tools = DefaultToolRegistry::new();
        let agent = CoreAgent::from_markdown(AgentConfig::default(), markdown, tools).unwrap();
        let events = agent
            .stream_text(CheckpointThenDoneInnerAgent, "hello", Vec::new())
            .collect::<Vec<_>>()
            .await;

        assert!(events.iter().any(|event| matches!(
            event,
            CoreAgentEvent::Checkpoint(state) if state.user_state["latest"] == true
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            CoreAgentEvent::Done { state: Some(state) } if state.user_state["latest"] == true
        )));
    }

    #[tokio::test]
    async fn dynamic_tool_helper_registers_external_definition() {
        let markdown = r#"---
id: markdown
name: Markdown Agent
model: test-model
tools:
  - external_echo
---
You are a markdown agent.
"#;
        let mut tools = DefaultToolRegistry::new();
        register_dynamic_tool_definitions(
            &mut tools,
            vec![ToolDefinition {
                tool_type: "function".to_string(),
                function: remi_agentloop::tool::FunctionDefinition {
                    name: "external_echo".to_string(),
                    description: "Echo through an external executor.".to_string(),
                    parameters: serde_json::json!({"type": "object", "properties": {}}),
                    extra_prompt: None,
                },
            }],
            |name, _args, _resume, _ctx| async move {
                Ok(ToolResult::Output(stream::iter(vec![ToolOutput::text(
                    format!("{name}:ok"),
                )])))
            },
        );
        let agent = CoreAgent::from_markdown(AgentConfig::default(), markdown, tools).unwrap();
        let events = agent
            .stream_text(ExternalToolInnerAgent, "hello", Vec::new())
            .collect::<Vec<_>>()
            .await;

        assert!(events.iter().any(
            |event| matches!(event, CoreAgentEvent::Text(text) if text == "external_echo:ok")
        ));
    }

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

        let config = CoreAgentConfig::new(AgentConfig::default(), profile.clone());
        let effective_config = config.effective_agent_config();
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
}
