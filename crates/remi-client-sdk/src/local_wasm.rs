use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use base64::Engine as _;
use futures::{Stream, StreamExt as FuturesStreamExt};
use prost_types::{Struct as ProstStruct, Value as ProstValue};
use remi_agentloop::agent::Agent;
use remi_agentloop::prelude::{ChatCtx, ChatCtxState, ToolDefinition};
use remi_agentloop::protocol::ProtocolEvent;
use remi_agentloop::state::AgentState;
use remi_agentloop::tracing::{
    ExternalToolResultTrace, InterruptTrace, LangSmithTracer, ModelEndTrace, ModelStartTrace,
    ResumeTrace, RunEndTrace, RunStartTrace, RunStatus, ToolCallTrace, ToolEndTrace,
    ToolExecutionHandoffTrace, ToolOutcomeTrace, ToolStartTrace, Tracer, TurnStartTrace,
};
use remi_agentloop::types::{
    Content, ContentPart, FunctionCall, ImageUrlDetail, LoopInput, Message, MessageId,
    ParsedToolCall, Role, RunId, SpanKind, SpanNode, ThreadId, ToolCallMessage, ToolCallOutcome,
};
use remi_agentloop_wasm::WasmAgentWithHttp;
use serde_json::{Value as JsonValue, json};

use crate::chat_client::proto as chat_proto;
use crate::chat_client::{ChatStreamEvent, chat_stream_event};
use crate::chat_types::{ChatLocalWasmSource, ChatRuntimeBackend, ChatRuntimeConfig};
use crate::remi_uri::{RemiUri, RemiUriLocation, mime_from_extension};

pub(crate) type ChatEventStream =
    Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, String>> + Send>>;

#[derive(Clone)]
struct PendingToolCallTrace {
    id: String,
    name: String,
    arguments_json: String,
    local_tool_started: bool,
    local_tool_started_at: Option<Instant>,
}

struct PendingModelTrace {
    turn: usize,
    call_index: usize,
    started_at: Instant,
    response_text: String,
    prompt_tokens: u32,
    completion_tokens: u32,
}

struct LocalWasmRunTracer {
    tracer: LangSmithTracer,
    model: String,
    input_messages: Vec<Message>,
    run_id: Option<RunId>,
    thread_id: Option<ThreadId>,
    run_span: Option<SpanNode>,
    run_started_at: Option<chrono::DateTime<chrono::Utc>>,
    metadata: Option<JsonValue>,
    assistant_output: String,
    prompt_tokens: u32,
    completion_tokens: u32,
    current_turn: usize,
    next_model_call_index: usize,
    current_tool_names: Vec<String>,
    pending_model: Option<PendingModelTrace>,
    pending_tool_calls: Vec<PendingToolCallTrace>,
    pending_resume_outcomes: Option<Vec<ToolOutcomeTrace>>,
    pending_resume_payload_count: usize,
    pending_start_custom_events: Vec<(String, JsonValue)>,
}

impl LocalWasmRunTracer {
    fn from_loop_input(loop_input: &LoopInput, config: &ChatRuntimeConfig) -> Option<Self> {
        if !config.tracing.reporting_enabled {
            return None;
        }

        let api_key = config
            .tracing
            .langsmith_api_key
            .as_ref()
            .filter(|value| !value.trim().is_empty())?
            .clone();

        let mut tracer = LangSmithTracer::new(api_key);
        if let Some(project) = config
            .tracing
            .langsmith_project
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            tracer = tracer.with_project(project.clone());
        }
        if let Some(api_url) = config
            .tracing
            .langsmith_api_url
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            tracer = tracer.with_api_url(api_url.clone());
        }

        match loop_input {
            LoopInput::Start {
                message,
                history,
                model,
                metadata,
                extra_tools,
                ..
            } => {
                let mut input_messages = history.clone();
                input_messages.push(message.clone());

                let current_tool_names = extra_tools
                    .iter()
                    .map(|definition| definition.function.name.clone())
                    .collect::<Vec<_>>();

                Some(Self {
                    tracer,
                    model: model.clone().unwrap_or_else(|| "local-wasm".to_string()),
                    input_messages,
                    run_id: None,
                    thread_id: None,
                    run_span: None,
                    run_started_at: None,
                    metadata: metadata.clone(),
                    assistant_output: String::new(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    current_turn: 0,
                    next_model_call_index: 0,
                    current_tool_names,
                    pending_model: None,
                    pending_tool_calls: Vec::new(),
                    pending_resume_outcomes: None,
                    pending_resume_payload_count: 0,
                    pending_start_custom_events: Self::pending_start_custom_events(loop_input),
                })
            }
            LoopInput::Resume { state, results, .. } => Some(Self {
                tracer: tracer.attach_to_existing_run(),
                model: state.config.model.clone(),
                input_messages: state.messages.clone(),
                run_id: Some(state.run_id.clone()),
                thread_id: Some(state.thread_id.clone()),
                run_span: Some(Self::derive_run_span(&state.run_id)),
                run_started_at: None,
                metadata: state.config.metadata.clone(),
                assistant_output: String::new(),
                prompt_tokens: 0,
                completion_tokens: 0,
                current_turn: state.turn,
                next_model_call_index: state.model_call_seq,
                current_tool_names: state
                    .tool_definitions
                    .iter()
                    .map(|definition| definition.function.name.clone())
                    .collect(),
                pending_model: None,
                pending_tool_calls: Vec::new(),
                pending_resume_outcomes: Some(results.iter().map(Self::outcome_trace).collect()),
                pending_resume_payload_count: results.len(),
                pending_start_custom_events: Vec::new(),
            }),
        }
    }

    fn derive_run_span(run_id: &RunId) -> SpanNode {
        SpanNode::derived(SpanKind::Run, format!("run:{}", run_id.0), None)
    }

    fn derive_model_span(&self, turn: usize, call_index: usize) -> Option<SpanNode> {
        self.run_span.as_ref().map(|run_span| {
            run_span.derived_child(SpanKind::Model, format!("turn:{turn}:call:{call_index}"))
        })
    }

    fn derive_tool_span(&self, tool_call_id: &str, tool_name: &str) -> Option<SpanNode> {
        self.run_span.as_ref().map(|run_span| {
            run_span.derived_child(SpanKind::Tool, format!("{tool_name}:{tool_call_id}"))
        })
    }

    fn pending_start_custom_events(loop_input: &LoopInput) -> Vec<(String, JsonValue)> {
        match loop_input {
            LoopInput::Start { extra_tools, .. } if !extra_tools.is_empty() => {
                vec![(
                    "tool_definitions".to_string(),
                    serde_json::json!({
                        "tool_definition_count": extra_tools.len(),
                        "tool_definition_names": extra_tools.iter().map(|definition| definition.function.name.clone()).collect::<Vec<_>>(),
                        "tool_definitions": extra_tools,
                    }),
                )]
            }
            _ => Vec::new(),
        }
    }

    fn outcome_trace(outcome: &ToolCallOutcome) -> ToolOutcomeTrace {
        match outcome {
            ToolCallOutcome::Result {
                tool_call_id,
                tool_name,
                content,
            } => ToolOutcomeTrace {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                result: Some(content.text_content()),
                error: None,
            },
            ToolCallOutcome::Error {
                tool_call_id,
                tool_name,
                error,
            } => ToolOutcomeTrace {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                result: None,
                error: Some(error.clone()),
            },
        }
    }

    async fn emit_initial_resume_traces(&mut self) {
        let Some(run_id) = self.run_id.clone() else {
            return;
        };

        let outcomes = self.pending_resume_outcomes.take().unwrap_or_default();

        self.tracer
            .on_resume(&ResumeTrace {
                span: self
                    .run_span
                    .clone()
                    .unwrap_or_else(|| Self::derive_run_span(&run_id)),
                run_id: run_id.clone(),
                payloads_count: self.pending_resume_payload_count,
                outcomes: outcomes.clone(),
                timestamp: chrono::Utc::now(),
            })
            .await;

        for outcome in outcomes {
            self.tracer
                .on_external_tool_result(&ExternalToolResultTrace {
                    run_id: run_id.clone(),
                    tool_call_id: outcome.tool_call_id,
                    tool_name: outcome.tool_name,
                    result: outcome.result,
                    error: outcome.error,
                    timestamp: chrono::Utc::now(),
                })
                .await;
        }
    }

    async fn emit_pending_start_custom_events(&mut self) {
        let Some(run_id) = self.run_id.clone() else {
            return;
        };

        for (name, payload) in self.pending_start_custom_events.drain(..) {
            let data = match payload {
                JsonValue::Object(mut object) => {
                    object.insert("run_id".to_string(), JsonValue::String(run_id.0.clone()));
                    JsonValue::Object(object)
                }
                other => serde_json::json!({
                    "run_id": run_id.0,
                    "payload": other,
                }),
            };

            self.tracer.on_custom(&name, &data).await;
        }
    }

    fn upsert_pending_tool_call(&mut self, id: &str, name: &str) -> &mut PendingToolCallTrace {
        if let Some(index) = self
            .pending_tool_calls
            .iter()
            .position(|call| call.id == id)
        {
            let call = &mut self.pending_tool_calls[index];
            if call.name == "unknown" && !name.is_empty() {
                call.name = name.to_string();
            }
            return call;
        }

        self.pending_tool_calls.push(PendingToolCallTrace {
            id: id.to_string(),
            name: if name.is_empty() {
                "unknown".to_string()
            } else {
                name.to_string()
            },
            arguments_json: String::new(),
            local_tool_started: false,
            local_tool_started_at: None,
        });
        self.pending_tool_calls
            .last_mut()
            .expect("pending tool call should exist")
    }

    fn pending_tool_call_traces(&self) -> Vec<ToolCallTrace> {
        self.pending_tool_calls
            .iter()
            .map(|call| ToolCallTrace {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: parse_tool_arguments_json(&call.arguments_json),
                result: None,
                interrupted: false,
                duration: std::time::Duration::ZERO,
            })
            .collect()
    }

    async fn start_model_turn(&mut self, turn: usize) {
        let Some(run_id) = self.run_id.clone() else {
            return;
        };

        self.current_turn = turn;
        let call_index = self.next_model_call_index;
        self.next_model_call_index += 1;
        self.pending_model = Some(PendingModelTrace {
            turn,
            call_index,
            started_at: Instant::now(),
            response_text: String::new(),
            prompt_tokens: 0,
            completion_tokens: 0,
        });
        self.pending_tool_calls.clear();

        self.tracer
            .on_turn_start(&TurnStartTrace {
                span: self
                    .derive_model_span(turn, call_index)
                    .unwrap_or_else(|| Self::derive_run_span(&run_id)),
                run_id: run_id.clone(),
                turn,
                timestamp: chrono::Utc::now(),
            })
            .await;

        self.tracer
            .on_model_start(&ModelStartTrace {
                span: self
                    .derive_model_span(turn, call_index)
                    .unwrap_or_else(|| Self::derive_run_span(&run_id)),
                run_id,
                turn,
                call_index,
                model: self.model.clone(),
                messages: self.input_messages.clone(),
                tools: self.current_tool_names.clone(),
                timestamp: chrono::Utc::now(),
            })
            .await;
    }

    async fn finish_pending_model(&mut self) {
        let Some(run_id) = self.run_id.clone() else {
            self.pending_model = None;
            self.pending_tool_calls.clear();
            return;
        };

        let Some(model) = self.pending_model.take() else {
            return;
        };

        let tool_calls = self.pending_tool_call_traces();
        self.tracer
            .on_model_end(&ModelEndTrace {
                span: self
                    .derive_model_span(model.turn, model.call_index)
                    .unwrap_or_else(|| Self::derive_run_span(&run_id)),
                run_id,
                turn: model.turn,
                call_index: model.call_index,
                response_text: if model.response_text.is_empty() {
                    None
                } else {
                    Some(model.response_text)
                },
                tool_calls,
                prompt_tokens: model.prompt_tokens,
                completion_tokens: model.completion_tokens,
                duration: model.started_at.elapsed(),
                timestamp: chrono::Utc::now(),
            })
            .await;
    }

    async fn ensure_local_tool_started(&mut self, id: &str, name: &str) {
        let Some(run_id) = self.run_id.clone() else {
            return;
        };

        let turn = self.current_turn;
        let call = self.upsert_pending_tool_call(id, name);
        if call.local_tool_started {
            return;
        }

        call.local_tool_started = true;
        call.local_tool_started_at = Some(Instant::now());
        let tool_call_id = call.id.clone();
        let tool_name = call.name.clone();
        let arguments = parse_tool_arguments_json(&call.arguments_json);

        self.tracer
            .on_tool_start(&ToolStartTrace {
                span: self
                    .derive_tool_span(&tool_call_id, &tool_name)
                    .unwrap_or_else(|| Self::derive_run_span(&run_id)),
                run_id,
                turn,
                tool_call_id,
                tool_name,
                arguments,
                timestamp: chrono::Utc::now(),
            })
            .await;
    }

    async fn on_protocol_event(&mut self, event: &ProtocolEvent) {
        match event {
            ProtocolEvent::RunStart {
                thread_id,
                run_id,
                metadata,
            } => {
                let timestamp = chrono::Utc::now();
                let run_id = RunId(run_id.clone());
                let thread_id = ThreadId(thread_id.clone());
                self.run_id = Some(run_id.clone());
                self.thread_id = Some(thread_id.clone());
                let run_span = Self::derive_run_span(&run_id);
                self.run_span = Some(run_span.clone());
                self.run_started_at = Some(timestamp);
                if metadata.is_some() {
                    self.metadata = metadata.clone();
                }
                self.tracer
                    .on_run_start(&RunStartTrace {
                        span: run_span,
                        thread_id: Some(thread_id.clone()),
                        run_id: run_id.clone(),
                        model: self.model.clone(),
                        system_prompt: None,
                        input_messages: self.input_messages.clone(),
                        metadata: self.metadata.clone(),
                        timestamp,
                    })
                    .await;
                self.emit_pending_start_custom_events().await;
            }
            ProtocolEvent::TurnStart { turn } => {
                self.start_model_turn(*turn).await;
            }
            ProtocolEvent::Delta { content, .. } => {
                self.assistant_output.push_str(content);
                if let Some(model) = self.pending_model.as_mut() {
                    model.response_text.push_str(content);
                }
            }
            ProtocolEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                self.prompt_tokens = *prompt_tokens;
                self.completion_tokens = *completion_tokens;
                if let Some(model) = self.pending_model.as_mut() {
                    model.prompt_tokens = *prompt_tokens;
                    model.completion_tokens = *completion_tokens;
                }
            }
            ProtocolEvent::ToolCallStart { id, name } => {
                let _ = self.upsert_pending_tool_call(id, name);
            }
            ProtocolEvent::ToolCallDelta {
                id,
                arguments_delta,
            } => {
                let call = self.upsert_pending_tool_call(id, "unknown");
                call.arguments_json.push_str(arguments_delta);
            }
            ProtocolEvent::ToolDelta { id, name, .. } => {
                self.finish_pending_model().await;
                self.ensure_local_tool_started(id, name).await;
            }
            ProtocolEvent::ToolResult { id, name, result } => {
                self.finish_pending_model().await;
                self.ensure_local_tool_started(id, name).await;

                if let Some(run_id) = self.run_id.clone() {
                    if let Some(call) = self
                        .pending_tool_calls
                        .iter_mut()
                        .find(|call| call.id == *id)
                    {
                        let tool_call_id = call.id.clone();
                        let tool_name = call.name.clone();
                        let duration = call
                            .local_tool_started_at
                            .map(|started_at| started_at.elapsed())
                            .unwrap_or_default();
                        let span = self
                            .derive_tool_span(&tool_call_id, &tool_name)
                            .unwrap_or_else(|| Self::derive_run_span(&run_id));
                        self.tracer
                            .on_tool_end(&ToolEndTrace {
                                span,
                                run_id,
                                turn: self.current_turn,
                                tool_call_id,
                                tool_name,
                                result: Some(result.clone()),
                                interrupted: false,
                                error: None,
                                duration,
                                timestamp: chrono::Utc::now(),
                            })
                            .await;
                    }
                }
            }
            ProtocolEvent::NeedToolExecution {
                state,
                tool_calls,
                completed_results,
            } => {
                self.finish_pending_model().await;
                self.input_messages = state.messages.clone();
                self.current_tool_names = state
                    .tool_definitions
                    .iter()
                    .map(|definition| definition.function.name.clone())
                    .collect();
                if let Some(run_id) = self.run_id.clone() {
                    self.tracer
                        .on_tool_execution_handoff(&ToolExecutionHandoffTrace {
                            run_id,
                            turn: state.turn,
                            tool_calls: tool_calls
                                .iter()
                                .map(Self::parsed_tool_call_trace)
                                .collect(),
                            completed_results: completed_results
                                .iter()
                                .map(Self::outcome_trace)
                                .collect(),
                            timestamp: chrono::Utc::now(),
                        })
                        .await;
                }
                self.pending_tool_calls.clear();
            }
            ProtocolEvent::Interrupt { interrupts } => {
                self.finish_pending_model().await;
                if let Some(run_id) = &self.run_id {
                    self.tracer
                        .on_interrupt(&InterruptTrace {
                            span: self
                                .run_span
                                .clone()
                                .unwrap_or_else(|| Self::derive_run_span(run_id)),
                            run_id: run_id.clone(),
                            interrupts: interrupts.clone(),
                            timestamp: chrono::Utc::now(),
                        })
                        .await;
                }
                self.finish(RunStatus::Interrupted, None).await;
            }
            ProtocolEvent::Custom { event_type, extra } => {
                let payload = match extra.clone() {
                    JsonValue::Object(mut object) => {
                        if let Some(run_id) = &self.run_id {
                            object
                                .entry("run_id".to_string())
                                .or_insert_with(|| JsonValue::String(run_id.0.clone()));
                        }
                        JsonValue::Object(object)
                    }
                    other => serde_json::json!({
                        "run_id": self.run_id.as_ref().map(|run_id| run_id.0.clone()),
                        "payload": other,
                    }),
                };
                self.tracer.on_custom(event_type, &payload).await;
            }
            ProtocolEvent::Done => {
                self.finish_pending_model().await;
                self.finish(RunStatus::Completed, None).await;
            }
            ProtocolEvent::Cancelled => {
                self.finish_pending_model().await;
                self.finish(RunStatus::Interrupted, None).await;
            }
            ProtocolEvent::Error { message, .. } => {
                self.finish_pending_model().await;
                self.finish(RunStatus::Error, Some(message.clone())).await;
            }
            _ => {}
        }
    }

    async fn finish(&mut self, status: RunStatus, error: Option<String>) {
        let Some(run_id) = self.run_id.clone() else {
            return;
        };

        let output_messages = if self.assistant_output.trim().is_empty() {
            Vec::new()
        } else {
            vec![Message {
                id: MessageId::new(),
                role: Role::Assistant,
                content: Content::Text(self.assistant_output.clone()),
                tool_calls: Some(Vec::new()),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
                metadata: None,
            }]
        };

        let started_at = self.run_started_at.unwrap_or_else(chrono::Utc::now);
        let ended_at = chrono::Utc::now();

        self.tracer
            .on_run_end(&RunEndTrace {
                span: self
                    .run_span
                    .clone()
                    .unwrap_or_else(|| Self::derive_run_span(&run_id)),
                run_id,
                status,
                output_messages,
                total_turns: 1,
                total_prompt_tokens: self.prompt_tokens,
                total_completion_tokens: self.completion_tokens,
                duration: (ended_at - started_at).to_std().unwrap_or_default(),
                error,
                timestamp: ended_at,
            })
            .await;
        self.tracer.on_flush().await;
        self.run_id = None;
        self.run_span = None;
        self.pending_tool_calls.clear();
        self.pending_model = None;
    }

    fn parsed_tool_call_trace(tool_call: &ParsedToolCall) -> ToolCallTrace {
        ToolCallTrace {
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            arguments: tool_call.arguments.clone(),
            result: None,
            interrupted: false,
            duration: std::time::Duration::ZERO,
        }
    }
}

fn parse_tool_arguments_json(arguments_json: &str) -> JsonValue {
    if arguments_json.trim().is_empty() {
        return json!({});
    }

    serde_json::from_str(arguments_json)
        .unwrap_or_else(|_| JsonValue::String(arguments_json.to_string()))
}

pub(crate) fn load_agent(source: &ChatLocalWasmSource) -> Result<Arc<WasmAgentWithHttp>, String> {
    let agent = match source {
        #[cfg(feature = "local-wasm-compiler")]
        ChatLocalWasmSource::File(path) => WasmAgentWithHttp::from_file(path),
        #[cfg(not(feature = "local-wasm-compiler"))]
        ChatLocalWasmSource::File(_) => {
            return Err(
                "File-based WASM loading requires the `local-wasm-compiler` feature".to_string(),
            );
        }
        // from_bytes auto-detects raw WASM vs precompiled .cwasm from magic bytes;
        // precompiled blobs work on all platforms (no compiler feature needed).
        ChatLocalWasmSource::Bytes(bytes) => WasmAgentWithHttp::from_bytes(bytes.as_slice()),
    }
    .map_err(|error| format!("Failed to load local WASM agent: {}", error.message))?;

    Ok(Arc::new(agent))
}

pub(crate) async fn start_stream(
    agent: Arc<WasmAgentWithHttp>,
    config: &ChatRuntimeConfig,
    session_id: &str,
    start_input: Option<chat_proto::ChatStartInput>,
    resume_input: Option<chat_proto::ChatResumeInput>,
) -> Result<ChatEventStream, String> {
    let (ctx, loop_input) = if let Some(start_input) = start_input {
        loop_input_from_start(start_input, config, session_id).await?
    } else if let Some(resume_input) = resume_input {
        loop_input_from_resume(resume_input)?
    } else {
        return Err("Missing chat start/resume input".to_string());
    };

    let mut run_tracer = LocalWasmRunTracer::from_loop_input(&loop_input, config);
    if let Some(tracer) = run_tracer.as_mut() {
        tracer.emit_initial_resume_traces().await;
    }

    Ok(Box::pin(async_stream::stream! {
        let event_stream = agent
            .chat(ctx, loop_input)
            .await
            .map_err(|error| error.message)?;
        futures::pin_mut!(event_stream);

        while let Some(event) = event_stream.next().await {
            if let Some(tracer) = run_tracer.as_mut() {
                tracer.on_protocol_event(&event).await;
            }
            tracing::debug!(
                event_type = protocol_event_type_name(&event),
                "[local_wasm] received guest protocol event"
            );
            yield map_protocol_event_to_chat_stream_event(event);
        }
    }))
}

async fn loop_input_from_start(
    input: chat_proto::ChatStartInput,
    config: &ChatRuntimeConfig,
    session_id: &str,
) -> Result<(ChatCtx, LoopInput), String> {
    let mut history = Vec::with_capacity(input.history.len());
    for message in input.history {
        history.push(history_message_from_proto(message).await?);
    }

    let current = input
        .current
        .ok_or_else(|| "ChatStartInput missing current message".to_string())?;

    let metadata = build_start_metadata(input.metadata.as_ref(), config, session_id)?;
    let model = match &config.backend {
        ChatRuntimeBackend::LocalWasm(local) => local.model.clone(),
        ChatRuntimeBackend::RemoteServer => None,
    };
    let mut extra_tools = Vec::with_capacity(input.extra_tools.len());
    for definition in input.extra_tools {
        let definition_json = prost_struct_to_json(&definition);
        let definition =
            serde_json::from_value::<ToolDefinition>(definition_json).map_err(|error| {
                format!("Invalid ChatStartInput extra_tools entry for local WASM chat: {error}")
            })?;
        extra_tools.push(definition);
    }

    let message = Message::user_content(content_from_input_message(current).await?);
    let user_state = input
        .user_state
        .as_ref()
        .map(prost_struct_to_json)
        .unwrap_or(JsonValue::Null);
    let ctx = ChatCtx::new(ChatCtxState::default().with_user_state(user_state));
    ctx.update(|state| state.metadata = metadata.clone());

    Ok((
        ctx,
        LoopInput::Start {
            message,
            history,
            extra_tools,
            model,
            temperature: None,
            max_tokens: None,
            metadata,
        },
    ))
}

fn loop_input_from_resume(
    input: chat_proto::ChatResumeInput,
) -> Result<(ChatCtx, LoopInput), String> {
    let state_json = input
        .state
        .as_ref()
        .map(prost_struct_to_json)
        .ok_or_else(|| "ChatResumeInput missing state".to_string())?;
    let state: AgentState = serde_json::from_value(state_json)
        .map_err(|error| format!("Invalid resume state for local WASM chat: {error}"))?;

    let mut results = Vec::with_capacity(input.results.len());
    for outcome in input.results {
        results.push(tool_outcome_from_proto(&outcome)?);
    }

    let ctx = ChatCtx::with_ids(
        state.thread_id.clone(),
        state.run_id.clone(),
        ChatCtxState {
            user_state: state.user_state.clone(),
            metadata: state.config.metadata.clone(),
            active_tool_chain: Vec::new(),
            span: None,
        },
    );

    Ok((
        ctx,
        LoopInput::Resume {
            state,
            pending_interrupts: Vec::new(),
            results,
        },
    ))
}

async fn history_message_from_proto(
    message: chat_proto::ChatHistoryMessage,
) -> Result<Message, String> {
    Ok(Message {
        id: if message.id.trim().is_empty() {
            MessageId::new()
        } else {
            MessageId(message.id)
        },
        role: role_from_proto(&message.role)?,
        content: content_from_proto(message.content, message.content_parts).await?,
        tool_calls: tool_calls_from_proto(&message.tool_calls)?,
        tool_call_id: trim_to_option(message.tool_call_id),
        name: None,
        reasoning_content: trim_to_option(message.reasoning_content),
        metadata: None,
    })
}

async fn content_from_input_message(
    input: chat_proto::ChatInputMessage,
) -> Result<Content, String> {
    content_from_proto(input.content, input.content_parts).await
}

async fn content_from_proto(
    content: String,
    content_parts: Vec<chat_proto::ChatContentPart>,
) -> Result<Content, String> {
    if content_parts.is_empty() {
        return Ok(Content::Text(content));
    }

    let has_text_part = content_parts.iter().any(|part| {
        matches!(
            part.value.as_ref(),
            Some(chat_proto::chat_content_part::Value::Text(_))
        )
    });

    let mut resolved_parts = Vec::new();
    if !has_text_part && !content.trim().is_empty() {
        resolved_parts.push(ContentPart::Text { text: content });
    }

    for part in content_parts {
        let value = match part.value {
            Some(chat_proto::chat_content_part::Value::Text(text)) => ContentPart::Text { text },
            Some(chat_proto::chat_content_part::Value::ImageUrl(image)) => ContentPart::ImageUrl {
                image_url: ImageUrlDetail {
                    url: image.url,
                    detail: trim_to_option(image.detail),
                },
            },
            Some(chat_proto::chat_content_part::Value::ResourceUri(resource)) => {
                resolve_resource_part(&resource.uri, &resource.resource_type).await?
            }
            None => return Err("Chat content part missing value".to_string()),
        };
        resolved_parts.push(value);
    }

    Ok(Content::Parts(resolved_parts))
}

async fn resolve_resource_part(uri: &str, resource_type: &str) -> Result<ContentPart, String> {
    match resource_type {
        "" | "image" => Ok(ContentPart::ImageUrl {
            image_url: ImageUrlDetail {
                url: resolve_image_resource_uri(uri).await?,
                detail: None,
            },
        }),
        "url" => Ok(ContentPart::Text {
            text: format!("[Link: {}]", uri),
        }),
        other => Err(format!(
            "Unsupported local WASM chat resource type '{}' for URI {}",
            other, uri
        )),
    }
}

async fn resolve_image_resource_uri(uri: &str) -> Result<String, String> {
    if uri.starts_with("https://") || uri.starts_with("http://") || uri.starts_with("data:") {
        return Ok(uri.to_string());
    }

    if uri.starts_with("remi://") {
        let parsed = RemiUri::parse(uri).map_err(|error| error.to_string())?;
        return match parsed.location {
            RemiUriLocation::Remote => Ok(parsed.path),
            RemiUriLocation::File => file_to_data_url(&parsed.path, &parsed.mime_type).await,
            RemiUriLocation::Local => Err(format!(
                "remi://local image URIs are not supported by the generic SDK local WASM backend: {}",
                uri
            )),
            RemiUriLocation::Inline => Ok(parsed.path),
        };
    }

    let mime = mime_for_path(uri);
    file_to_data_url(uri, &mime).await
}

async fn file_to_data_url(path: &str, mime: &str) -> Result<String, String> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| format!("Failed to read local chat image {}: {}", path, error))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:{};base64,{}", mime, encoded))
}

fn mime_for_path(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("jpg");
    mime_from_extension(ext).to_string()
}

fn role_from_proto(role: &str) -> Result<Role, String> {
    match role.trim().to_ascii_lowercase().as_str() {
        "" | "user" => Ok(Role::User),
        "system" => Ok(Role::System),
        "assistant" => Ok(Role::Assistant),
        "tool" => Ok(Role::Tool),
        other => Err(format!(
            "Unsupported chat role '{}' for local WASM backend",
            other
        )),
    }
}

fn tool_calls_from_proto(
    tool_calls: &[chat_proto::ChatToolCall],
) -> Result<Option<Vec<ToolCallMessage>>, String> {
    if tool_calls.is_empty() {
        return Ok(None);
    }

    let mut calls = Vec::with_capacity(tool_calls.len());
    for call in tool_calls {
        let arguments = call
            .arguments
            .as_ref()
            .map(prost_struct_to_json)
            .unwrap_or_else(|| json!({}));
        let arguments = serde_json::to_string(&arguments)
            .map_err(|error| format!("Failed to serialize tool call arguments: {error}"))?;

        calls.push(ToolCallMessage {
            id: call.id.clone(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: call.tool_name.clone(),
                arguments,
            },
        });
    }

    Ok(Some(calls))
}

fn tool_outcome_from_proto(
    outcome: &chat_proto::ChatToolCallOutcome,
) -> Result<ToolCallOutcome, String> {
    let tool_call_id = outcome.tool_call_id.clone();
    let tool_name = outcome.tool_name.clone();
    let outcome = outcome
        .outcome
        .as_ref()
        .ok_or_else(|| "ChatToolCallOutcome missing result/error".to_string())?;

    Ok(match outcome {
        chat_proto::chat_tool_call_outcome::Outcome::Result(result) => ToolCallOutcome::Result {
            tool_call_id,
            tool_name,
            content: Content::Text(result.clone()),
        },
        chat_proto::chat_tool_call_outcome::Outcome::Error(error) => ToolCallOutcome::Error {
            tool_call_id,
            tool_name,
            error: error.clone(),
        },
        chat_proto::chat_tool_call_outcome::Outcome::Parts(parts_msg) => {
            // Encode image parts as data: URLs inside ImageUrl content parts.
            // The Kimi/OpenAI-compatible API only accepts "image_url" content type;
            // the custom "image_base64" type is rejected with HTTP 400.
            let content_parts = parts_msg
                .parts
                .iter()
                .filter_map(|part| match &part.value {
                    Some(chat_proto::chat_content_part::Value::ImageUrl(img))
                        if !img.data.is_empty() =>
                    {
                        Some(ContentPart::ImageUrl {
                            image_url: ImageUrlDetail {
                                url: format!(
                                    "data:{};base64,{}",
                                    img.media_type,
                                    base64::engine::general_purpose::STANDARD.encode(&img.data)
                                ),
                                detail: None,
                            },
                        })
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            ToolCallOutcome::Result {
                tool_call_id,
                tool_name,
                content: if content_parts.is_empty() {
                    Content::Text(String::new())
                } else {
                    Content::Parts(content_parts)
                },
            }
        }
    })
}

fn build_start_metadata(
    existing: Option<&ProstStruct>,
    config: &ChatRuntimeConfig,
    session_id: &str,
) -> Result<Option<JsonValue>, String> {
    let mut metadata = match existing {
        Some(existing) => prost_struct_to_json(existing),
        None => JsonValue::Object(serde_json::Map::new()),
    };

    let metadata_obj = metadata
        .as_object_mut()
        .ok_or_else(|| "Chat start metadata must be a JSON object".to_string())?;

    normalize_metadata_object_to_string_values(metadata_obj);

    metadata_obj
        .entry("session_id".to_string())
        .or_insert_with(|| JsonValue::String(session_id.to_string()));
    metadata_obj
        .entry("traffic_mark".to_string())
        .or_insert_with(|| JsonValue::String("local_wasm".to_string()));
    if !config.device_id.trim().is_empty() {
        metadata_obj
            .entry("device_id".to_string())
            .or_insert_with(|| JsonValue::String(config.device_id.clone()));
    }
    metadata_obj
        .entry("reporting_consent".to_string())
        .or_insert_with(|| JsonValue::String(config.tracing.reporting_enabled.to_string()));

    if let ChatRuntimeBackend::LocalWasm(local) = &config.backend {
        if !local.api_key.trim().is_empty() {
            metadata_obj
                .entry("api_key".to_string())
                .or_insert_with(|| JsonValue::String(local.api_key.clone()));
        }
        if let Some(base_url) = local
            .base_url
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            metadata_obj
                .entry("base_url".to_string())
                .or_insert_with(|| JsonValue::String(base_url.clone()));
        }
        if let Some(model) = local
            .model
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            metadata_obj
                .entry("model".to_string())
                .or_insert_with(|| JsonValue::String(model.clone()));
        }
    }

    if config.tracing.reporting_enabled {
        if let Some(api_key) = config
            .tracing
            .langsmith_api_key
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            metadata_obj
                .entry("langsmith_api_key".to_string())
                .or_insert_with(|| JsonValue::String(api_key.clone()));
        }
        if let Some(project) = config
            .tracing
            .langsmith_project
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            metadata_obj
                .entry("langsmith_project".to_string())
                .or_insert_with(|| JsonValue::String(project.clone()));
        }
        if let Some(api_url) = config
            .tracing
            .langsmith_api_url
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            metadata_obj
                .entry("langsmith_api_url".to_string())
                .or_insert_with(|| JsonValue::String(api_url.clone()));
        }
    }

    if metadata_obj.is_empty() {
        Ok(None)
    } else {
        Ok(Some(metadata))
    }
}

fn map_protocol_event_to_chat_stream_event(
    event: ProtocolEvent,
) -> Result<ChatStreamEvent, String> {
    let mapped = match event {
        ProtocolEvent::RunStart {
            thread_id,
            run_id,
            metadata,
        } => ChatStreamEvent {
            event: Some(chat_stream_event::Event::RunStart(
                chat_proto::ChatRunStartEvent {
                    thread_id,
                    run_id,
                    metadata: metadata.as_ref().and_then(json_to_prost_struct),
                },
            )),
        },
        ProtocolEvent::Delta { content, role } => ChatStreamEvent {
            event: Some(chat_stream_event::Event::Delta(
                chat_proto::ChatDeltaEvent {
                    content,
                    role: role.unwrap_or_default(),
                },
            )),
        },
        ProtocolEvent::ThinkingStart => ChatStreamEvent {
            event: Some(chat_stream_event::Event::ThinkingStart(
                chat_proto::ChatThinkingStartEvent {},
            )),
        },
        ProtocolEvent::ThinkingEnd { content } => ChatStreamEvent {
            event: Some(chat_stream_event::Event::ThinkingEnd(
                chat_proto::ChatThinkingEndEvent { content },
            )),
        },
        ProtocolEvent::ToolCallStart { id, name } => ChatStreamEvent {
            event: Some(chat_stream_event::Event::ToolCallStart(
                chat_proto::ChatToolCallStartEvent {
                    id,
                    tool_name: name,
                },
            )),
        },
        ProtocolEvent::ToolCallDelta {
            id,
            arguments_delta,
        } => ChatStreamEvent {
            event: Some(chat_stream_event::Event::ToolCallDelta(
                chat_proto::ChatToolCallDeltaEvent {
                    id,
                    arguments_delta,
                },
            )),
        },
        ProtocolEvent::ToolDelta { id, name, delta } => ChatStreamEvent {
            event: Some(chat_stream_event::Event::ToolDelta(
                chat_proto::ChatToolDeltaEvent {
                    id,
                    tool_name: name,
                    delta,
                },
            )),
        },
        ProtocolEvent::ToolResult { id, name, result } => ChatStreamEvent {
            event: Some(chat_stream_event::Event::ToolResult(
                chat_proto::ChatToolResultEvent {
                    id,
                    tool_name: name,
                    result,
                },
            )),
        },
        ProtocolEvent::SubSession {
            parent_tool_call_id,
            sub_thread_id,
            sub_run_id,
            agent_name,
            title,
            depth,
            event,
        } => ChatStreamEvent {
            event: Some(chat_stream_event::Event::SubSession(
                chat_proto::ChatSubSessionEvent {
                    parent_tool_call_id,
                    sub_session_id: sub_thread_id,
                    sub_run_id,
                    agent_name,
                    title: title.unwrap_or_default(),
                    depth,
                    event: Some(match event {
                        remi_agentloop::prelude::SubSessionEventPayload::Start => {
                            chat_proto::chat_sub_session_event::Event::Start(
                                chat_proto::ChatSubSessionStartEvent {},
                            )
                        }
                        remi_agentloop::prelude::SubSessionEventPayload::Delta { content } => {
                            chat_proto::chat_sub_session_event::Event::Delta(
                                chat_proto::ChatSubSessionDeltaEvent { content },
                            )
                        }
                        remi_agentloop::prelude::SubSessionEventPayload::ThinkingStart => {
                            chat_proto::chat_sub_session_event::Event::ThinkingStart(
                                chat_proto::ChatSubSessionThinkingStartEvent {},
                            )
                        }
                        remi_agentloop::prelude::SubSessionEventPayload::ThinkingEnd { content } => {
                            chat_proto::chat_sub_session_event::Event::ThinkingEnd(
                                chat_proto::ChatSubSessionThinkingEndEvent { content },
                            )
                        }
                        remi_agentloop::prelude::SubSessionEventPayload::ToolCallStart { id, name } => {
                            chat_proto::chat_sub_session_event::Event::ToolCallStart(
                                chat_proto::ChatSubSessionToolCallStartEvent {
                                    id,
                                    tool_name: name,
                                },
                            )
                        }
                        remi_agentloop::prelude::SubSessionEventPayload::ToolCallArgumentsDelta { id, delta } => {
                            chat_proto::chat_sub_session_event::Event::ToolCallDelta(
                                chat_proto::ChatSubSessionToolCallDeltaEvent {
                                    id,
                                    arguments_delta: delta,
                                },
                            )
                        }
                        remi_agentloop::prelude::SubSessionEventPayload::ToolDelta { id, name, delta } => {
                            chat_proto::chat_sub_session_event::Event::ToolDelta(
                                chat_proto::ChatSubSessionToolDeltaEvent {
                                    id,
                                    tool_name: name,
                                    delta,
                                },
                            )
                        }
                        remi_agentloop::prelude::SubSessionEventPayload::ToolResult { id, name, result } => {
                            chat_proto::chat_sub_session_event::Event::ToolResult(
                                chat_proto::ChatSubSessionToolResultEvent {
                                    id,
                                    tool_name: name,
                                    result,
                                },
                            )
                        }
                        remi_agentloop::prelude::SubSessionEventPayload::TurnStart { turn } => {
                            chat_proto::chat_sub_session_event::Event::TurnStart(
                                chat_proto::ChatSubSessionTurnStartEvent { turn: turn as u32 },
                            )
                        }
                        remi_agentloop::prelude::SubSessionEventPayload::Done { final_output } => {
                            chat_proto::chat_sub_session_event::Event::Done(
                                chat_proto::ChatSubSessionDoneEvent {
                                    final_output: final_output.unwrap_or_default(),
                                },
                            )
                        }
                        remi_agentloop::prelude::SubSessionEventPayload::Error { message } => {
                            chat_proto::chat_sub_session_event::Event::Error(
                                chat_proto::ChatSubSessionErrorEvent { message },
                            )
                        }
                    }),
                },
            )),
        },
        ProtocolEvent::Interrupt { interrupts } => ChatStreamEvent {
            event: Some(chat_stream_event::Event::Interrupt(
                chat_proto::ChatInterruptStreamEvent {
                    interrupts: interrupts
                        .into_iter()
                        .map(|interrupt| chat_proto::ChatInterruptInfo {
                            interrupt_id: interrupt.interrupt_id.0,
                            tool_call_id: interrupt.tool_call_id,
                            tool_name: interrupt.tool_name,
                            kind: interrupt.kind,
                            data: json_to_prost_struct(&interrupt.data),
                        })
                        .collect(),
                },
            )),
        },
        ProtocolEvent::TurnStart { turn } => ChatStreamEvent {
            event: Some(chat_stream_event::Event::TurnStart(
                chat_proto::ChatTurnStartEvent { turn: turn as u32 },
            )),
        },
        ProtocolEvent::Usage {
            prompt_tokens,
            completion_tokens,
        } => ChatStreamEvent {
            event: Some(chat_stream_event::Event::Usage(
                chat_proto::ChatUsageEvent {
                    prompt_tokens,
                    completion_tokens,
                },
            )),
        },
        ProtocolEvent::Error { message, code } => ChatStreamEvent {
            event: Some(chat_stream_event::Event::Error(
                chat_proto::ChatErrorEvent {
                    message,
                    code: code.unwrap_or_default(),
                },
            )),
        },
        ProtocolEvent::Done => ChatStreamEvent {
            event: Some(chat_stream_event::Event::Done(chat_proto::ChatDoneEvent {
                state: None,
            })),
        },
        ProtocolEvent::Cancelled => ChatStreamEvent {
            event: Some(chat_stream_event::Event::Cancelled(
                chat_proto::ChatCancelledEvent {},
            )),
        },
        ProtocolEvent::NeedToolExecution {
            state,
            tool_calls,
            completed_results,
        } => ChatStreamEvent {
            event: Some(chat_stream_event::Event::NeedToolExecution(
                chat_proto::ChatNeedToolExecutionEvent {
                    state: serde_json::to_value(&state)
                        .ok()
                        .as_ref()
                        .and_then(json_to_prost_struct),
                    tool_calls: tool_calls
                        .iter()
                        .map(|call| build_chat_tool_call(&call.id, &call.name, &call.arguments))
                        .collect(),
                    completed_results: completed_results
                        .iter()
                        .map(tool_outcome_to_proto)
                        .collect(),
                },
            )),
        },
        ProtocolEvent::Custom { event_type, extra } => {
            return map_custom_chat_event(event_type, extra);
        }
    };

    Ok(mapped)
}

fn map_custom_chat_event(event_type: String, extra: JsonValue) -> Result<ChatStreamEvent, String> {
    if event_type == "remi_agent" {
        match extra.get("type").and_then(|value| value.as_str()) {
            Some("need_tool_execution") => {
                let state = extra.get("state").cloned().unwrap_or(JsonValue::Null);
                let tool_calls = extra
                    .get("calls")
                    .and_then(|value| value.as_array())
                    .map(|calls| {
                        calls
                            .iter()
                            .filter_map(|call| {
                                let id = call.get("id").and_then(|value| value.as_str())?;
                                let typed_call = call.get("call")?;
                                let (tool_name, arguments) =
                                    normalize_external_tool_call(typed_call)?;
                                Some(build_chat_tool_call(id, &tool_name, &arguments))
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let completed_results = extra
                    .get("completed_results")
                    .and_then(|value| value.as_array())
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(json_to_chat_tool_outcome)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                return Ok(ChatStreamEvent {
                    event: Some(chat_stream_event::Event::NeedToolExecution(
                        chat_proto::ChatNeedToolExecutionEvent {
                            state: json_to_prost_struct(&state),
                            tool_calls,
                            completed_results,
                        },
                    )),
                });
            }
            Some("done") => {
                let state = extra.get("state").and_then(json_to_prost_struct);
                return Ok(ChatStreamEvent {
                    event: Some(chat_stream_event::Event::Done(chat_proto::ChatDoneEvent {
                        state,
                    })),
                });
            }
            _ => {}
        }
    }

    Ok(ChatStreamEvent {
        event: Some(chat_stream_event::Event::Custom(
            chat_proto::ChatCustomEvent {
                event_type,
                extra: json_to_prost_struct(&extra),
            },
        )),
    })
}

fn normalize_external_tool_call(call: &JsonValue) -> Option<(String, JsonValue)> {
    let obj = call.as_object()?;

    if let Some(tool_name) = obj.get("tool_name").and_then(|value| value.as_str()) {
        return Some((
            tool_name.to_string(),
            obj.get("arguments").cloned().unwrap_or_else(|| json!({})),
        ));
    }

    if obj.len() != 1 {
        return None;
    }

    let (variant, arguments) = obj.iter().next()?;
    let tool_name = match variant.as_str() {
        "Ls" => "ls_tool",
        "Cat" => "cat_tool",
        "Create" => "create_tool",
        "Tree" => "tree_tool",
        "ReadPath" => "cat_tool",
        "EditPath" => "edit_path_tool",
        "DeletePath" => "delete_path_tool",
        "MovePath" => "move_path_tool",
        "CreateTriggerSimple" | "CreateTimerTrigger" => "create_timer_trigger",
        "CreateTrigger" => "create_trigger",
        "DeleteTrigger" => "delete_trigger",
        "TestTrigger" => "test_trigger",
        "RetrieveEvents" => "retrieve_events",
        "AbstractEvents" => "abstract_events",
        "Fetch" | "ResolveUri" => "fetch",
        "HandoffToDeepAgent" => "handoff_to_deep_agent",
        _ => return None,
    };

    Some((tool_name.to_string(), arguments.clone()))
}

fn build_chat_tool_call(
    id: &str,
    tool_name: &str,
    arguments: &JsonValue,
) -> chat_proto::ChatToolCall {
    chat_proto::ChatToolCall {
        id: id.to_string(),
        tool_name: tool_name.to_string(),
        arguments: json_to_prost_struct(arguments),
    }
}

fn tool_outcome_to_proto(outcome: &ToolCallOutcome) -> chat_proto::ChatToolCallOutcome {
    match outcome {
        ToolCallOutcome::Result {
            tool_call_id,
            tool_name,
            content,
        } => chat_proto::ChatToolCallOutcome {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            outcome: Some(chat_proto::chat_tool_call_outcome::Outcome::Result(
                content.text_content(),
            )),
        },
        ToolCallOutcome::Error {
            tool_call_id,
            tool_name,
            error,
        } => chat_proto::ChatToolCallOutcome {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            outcome: Some(chat_proto::chat_tool_call_outcome::Outcome::Error(
                error.clone(),
            )),
        },
    }
}

fn json_to_chat_tool_outcome(value: &JsonValue) -> Option<chat_proto::ChatToolCallOutcome> {
    if let Some(result) = value.get("Result") {
        let result_value = result.get("content")?;
        let result_text = match result_value {
            JsonValue::String(text) => text.clone(),
            other => serde_json::to_string(other).ok()?,
        };
        return Some(chat_proto::ChatToolCallOutcome {
            tool_call_id: result.get("tool_call_id")?.as_str()?.to_string(),
            tool_name: result.get("tool_name")?.as_str()?.to_string(),
            outcome: Some(chat_proto::chat_tool_call_outcome::Outcome::Result(
                result_text,
            )),
        });
    }

    if let Some(error) = value.get("Error") {
        let error_value = error.get("error")?;
        let error_text = match error_value {
            JsonValue::String(text) => text.clone(),
            other => serde_json::to_string(other).ok()?,
        };
        return Some(chat_proto::ChatToolCallOutcome {
            tool_call_id: error.get("tool_call_id")?.as_str()?.to_string(),
            tool_name: error.get("tool_name")?.as_str()?.to_string(),
            outcome: Some(chat_proto::chat_tool_call_outcome::Outcome::Error(
                error_text,
            )),
        });
    }

    let tool_call_id = value.get("tool_call_id")?.as_str()?;
    let tool_name = value.get("tool_name")?.as_str()?;
    if let Some(result) = value.get("result") {
        let result_text = match result {
            JsonValue::String(text) => text.clone(),
            other => serde_json::to_string(other).ok()?,
        };
        return Some(chat_proto::ChatToolCallOutcome {
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            outcome: Some(chat_proto::chat_tool_call_outcome::Outcome::Result(
                result_text,
            )),
        });
    }
    if let Some(error) = value.get("error") {
        let error_text = match error {
            JsonValue::String(text) => text.clone(),
            other => serde_json::to_string(other).ok()?,
        };
        return Some(chat_proto::ChatToolCallOutcome {
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            outcome: Some(chat_proto::chat_tool_call_outcome::Outcome::Error(
                error_text,
            )),
        });
    }

    None
}

fn json_to_prost_struct(value: &JsonValue) -> Option<ProstStruct> {
    let JsonValue::Object(map) = value else {
        return None;
    };

    let fields = map
        .iter()
        .filter_map(|(key, value)| json_to_prost_value(value).map(|value| (key.clone(), value)))
        .collect();

    Some(ProstStruct { fields })
}

fn json_to_prost_value(value: &JsonValue) -> Option<ProstValue> {
    use prost_types::value::Kind;

    let kind = match value {
        JsonValue::Null => Kind::NullValue(0),
        JsonValue::Bool(value) => Kind::BoolValue(*value),
        JsonValue::Number(value) => Kind::NumberValue(value.as_f64()?),
        JsonValue::String(value) => Kind::StringValue(value.clone()),
        JsonValue::Array(values) => Kind::ListValue(prost_types::ListValue {
            values: values.iter().filter_map(json_to_prost_value).collect(),
        }),
        JsonValue::Object(_) => Kind::StructValue(json_to_prost_struct(value)?),
    };

    Some(ProstValue { kind: Some(kind) })
}

fn prost_struct_to_json(value: &ProstStruct) -> JsonValue {
    let mut map = serde_json::Map::new();
    for (key, value) in &value.fields {
        map.insert(key.clone(), prost_value_to_json(value));
    }
    JsonValue::Object(map)
}

fn normalize_metadata_object_to_string_values(
    metadata_obj: &mut serde_json::Map<String, JsonValue>,
) {
    metadata_obj.retain(|_, value| match value {
        JsonValue::Null => false,
        JsonValue::String(_) => true,
        JsonValue::Bool(inner) => {
            *value = JsonValue::String(inner.to_string());
            true
        }
        JsonValue::Number(inner) => {
            *value = JsonValue::String(inner.to_string());
            true
        }
        JsonValue::Array(_) | JsonValue::Object(_) => {
            *value = JsonValue::String(value.to_string());
            true
        }
    });
}

fn prost_value_to_json(value: &ProstValue) -> JsonValue {
    use prost_types::value::Kind;

    match value.kind.as_ref() {
        Some(Kind::NullValue(_)) | None => JsonValue::Null,
        Some(Kind::BoolValue(value)) => JsonValue::Bool(*value),
        Some(Kind::StringValue(value)) => JsonValue::String(value.clone()),
        Some(Kind::NumberValue(value)) => {
            prost_number_to_json_value(*value).unwrap_or(JsonValue::Null)
        }
        Some(Kind::StructValue(value)) => prost_struct_to_json(value),
        Some(Kind::ListValue(list)) => {
            JsonValue::Array(list.values.iter().map(prost_value_to_json).collect())
        }
    }
}

fn prost_number_to_json_value(number: f64) -> Option<JsonValue> {
    if !number.is_finite() {
        return None;
    }

    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    if number.fract() == 0.0 && number.abs() <= MAX_SAFE_INTEGER {
        if number >= 0.0 {
            return Some(JsonValue::Number(serde_json::Number::from(number as u64)));
        }
        return Some(JsonValue::Number(serde_json::Number::from(number as i64)));
    }

    serde_json::Number::from_f64(number).map(JsonValue::Number)
}

fn trim_to_option(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_types::{ChatLocalWasmConfig, ChatRuntimeBackend};

    #[test]
    fn start_metadata_injects_local_llm_fields() {
        let config = ChatRuntimeConfig {
            device_id: "device-1".to_string(),
            backend: ChatRuntimeBackend::LocalWasm(ChatLocalWasmConfig {
                source: ChatLocalWasmSource::Bytes(Arc::new(vec![])),
                api_key: "test-key".to_string(),
                base_url: Some("https://example.com/v1".to_string()),
                model: Some("gpt-test".to_string()),
            }),
            tracing: crate::chat_types::ChatTracingConfig {
                reporting_enabled: true,
                langsmith_api_key: Some("ls-test".to_string()),
                langsmith_project: Some("remi-local".to_string()),
                langsmith_api_url: Some("https://smith.example.com".to_string()),
            },
            ..Default::default()
        };

        let metadata = build_start_metadata(None, &config, "session-123")
            .expect("metadata")
            .expect("metadata object");

        assert_eq!(
            metadata.get("api_key").and_then(|value| value.as_str()),
            Some("test-key")
        );
        assert_eq!(
            metadata.get("base_url").and_then(|value| value.as_str()),
            Some("https://example.com/v1")
        );
        assert_eq!(
            metadata.get("model").and_then(|value| value.as_str()),
            Some("gpt-test")
        );
        assert_eq!(
            metadata.get("device_id").and_then(|value| value.as_str()),
            Some("device-1")
        );
        assert_eq!(
            metadata.get("session_id").and_then(|value| value.as_str()),
            Some("session-123")
        );
        assert_eq!(
            metadata
                .get("traffic_mark")
                .and_then(|value| value.as_str()),
            Some("local_wasm")
        );
        assert_eq!(
            metadata
                .get("reporting_consent")
                .and_then(|value| value.as_str()),
            Some("true")
        );
        assert_eq!(
            metadata
                .get("langsmith_api_key")
                .and_then(|value| value.as_str()),
            Some("ls-test")
        );
    }

    #[test]
    fn start_metadata_normalizes_existing_non_string_values() {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "share_entry".to_string(),
            ProstValue {
                kind: Some(prost_types::value::Kind::BoolValue(true)),
            },
        );
        fields.insert(
            "retry_count".to_string(),
            ProstValue {
                kind: Some(prost_types::value::Kind::NumberValue(2.0)),
            },
        );
        fields.insert(
            "nested".to_string(),
            ProstValue {
                kind: Some(prost_types::value::Kind::StructValue(ProstStruct {
                    fields: std::collections::BTreeMap::from([(
                        "flag".to_string(),
                        ProstValue {
                            kind: Some(prost_types::value::Kind::BoolValue(false)),
                        },
                    )]),
                })),
            },
        );

        let existing = ProstStruct { fields };
        let config = ChatRuntimeConfig {
            backend: ChatRuntimeBackend::LocalWasm(ChatLocalWasmConfig {
                source: ChatLocalWasmSource::Bytes(Arc::new(vec![])),
                api_key: String::new(),
                base_url: None,
                model: None,
            }),
            ..Default::default()
        };

        let metadata = build_start_metadata(Some(&existing), &config, "session-123")
            .expect("metadata")
            .expect("metadata object");

        assert_eq!(
            metadata.get("share_entry").and_then(|value| value.as_str()),
            Some("true")
        );
        assert_eq!(
            metadata.get("retry_count").and_then(|value| value.as_str()),
            Some("2")
        );
        assert_eq!(
            metadata.get("nested").and_then(|value| value.as_str()),
            Some("{\"flag\":false}")
        );
    }

    #[test]
    fn custom_need_tool_execution_maps_resolve_uri_to_fetch() {
        let event = ProtocolEvent::Custom {
            event_type: "remi_agent".to_string(),
            extra: json!({
                "type": "need_tool_execution",
                "state": { "phase": "AwaitingToolExecution" },
                "calls": [
                    {
                        "id": "tool-1",
                        "call": {
                            "ResolveUri": {
                                "uri": "https://example.com"
                            }
                        }
                    }
                ],
                "completed_results": []
            }),
        };

        let mapped = map_protocol_event_to_chat_stream_event(event).expect("mapped event");
        let Some(chat_stream_event::Event::NeedToolExecution(need_tool)) = mapped.event else {
            panic!("expected need_tool_execution event");
        };

        assert_eq!(need_tool.tool_calls.len(), 1);
        assert_eq!(need_tool.tool_calls[0].tool_name, "fetch");
    }

    #[test]
    fn custom_done_maps_state() {
        let event = ProtocolEvent::Custom {
            event_type: "remi_agent".to_string(),
            extra: json!({
                "type": "done",
                "state": {
                    "run_id": "run-1",
                    "thread_id": "thread-1",
                    "user_state": { "agent_mode": "deep" }
                }
            }),
        };

        let mapped = map_protocol_event_to_chat_stream_event(event).expect("mapped event");
        let Some(chat_stream_event::Event::Done(done)) = mapped.event else {
            panic!("expected done event");
        };

        let state = done
            .state
            .as_ref()
            .map(prost_struct_to_json)
            .expect("done state");
        assert_eq!(
            state.get("run_id").and_then(|value| value.as_str()),
            Some("run-1")
        );
        assert_eq!(
            state
                .get("user_state")
                .and_then(|value| value.get("agent_mode"))
                .and_then(|value| value.as_str()),
            Some("deep")
        );
    }

    #[test]
    fn sub_session_maps_to_chat_sub_session_event() {
        let event = ProtocolEvent::SubSession {
            parent_tool_call_id: "call-1".to_string(),
            sub_thread_id: "sub-thread-1".to_string(),
            sub_run_id: "sub-run-1".to_string(),
            agent_name: "calculator".to_string(),
            title: Some("4+2*2".to_string()),
            depth: 0,
            event: remi_agentloop::prelude::SubSessionEventPayload::ToolCallStart {
                id: "inner-call-1".to_string(),
                name: "multiply".to_string(),
            },
        };

        let mapped = map_protocol_event_to_chat_stream_event(event).expect("mapped event");
        let Some(chat_stream_event::Event::SubSession(sub_session)) = mapped.event else {
            panic!("expected sub_session event");
        };

        assert_eq!(sub_session.parent_tool_call_id, "call-1");
        assert_eq!(sub_session.sub_session_id, "sub-thread-1");
        assert_eq!(sub_session.sub_run_id, "sub-run-1");
        assert_eq!(sub_session.agent_name, "calculator");
        assert_eq!(sub_session.title, "4+2*2");

        let Some(chat_proto::chat_sub_session_event::Event::ToolCallStart(tool_call)) =
            sub_session.event
        else {
            panic!("expected nested tool_call_start event");
        };

        assert_eq!(tool_call.id, "inner-call-1");
        assert_eq!(tool_call.tool_name, "multiply");
    }

    #[tokio::test]
    async fn content_conversion_avoids_duplicate_text_parts() {
        let content = content_from_proto(
            "hello".to_string(),
            vec![
                chat_proto::ChatContentPart {
                    r#type: "text".to_string(),
                    value: Some(chat_proto::chat_content_part::Value::Text(
                        "hello".to_string(),
                    )),
                },
                chat_proto::ChatContentPart {
                    r#type: "image_url".to_string(),
                    value: Some(chat_proto::chat_content_part::Value::ImageUrl(
                        chat_proto::ChatImageContent {
                            url: "https://example.com/image.png".to_string(),
                            detail: "auto".to_string(),
                            data: Vec::new(),
                            media_type: String::new(),
                        },
                    )),
                },
            ],
        )
        .await
        .expect("content");

        let Content::Parts(parts) = content else {
            panic!("expected multipart content");
        };

        assert_eq!(parts.len(), 2);
    }
}

fn protocol_event_type_name(event: &ProtocolEvent) -> &'static str {
    match event {
        ProtocolEvent::RunStart { .. } => "run_start",
        ProtocolEvent::Delta { .. } => "delta",
        ProtocolEvent::ThinkingStart => "thinking_start",
        ProtocolEvent::ThinkingEnd { .. } => "thinking_end",
        ProtocolEvent::ToolCallStart { .. } => "tool_call_start",
        ProtocolEvent::ToolCallDelta { .. } => "tool_call_delta",
        ProtocolEvent::ToolDelta { .. } => "tool_delta",
        ProtocolEvent::ToolResult { .. } => "tool_result",
        ProtocolEvent::SubSession { .. } => "sub_session",
        ProtocolEvent::Interrupt { .. } => "interrupt",
        ProtocolEvent::TurnStart { .. } => "turn_start",
        ProtocolEvent::Usage { .. } => "usage",
        ProtocolEvent::Error { .. } => "error",
        ProtocolEvent::Done => "done",
        ProtocolEvent::Cancelled => "cancelled",
        ProtocolEvent::NeedToolExecution { .. } => "need_tool_execution",
        ProtocolEvent::Custom { .. } => "custom",
    }
}
