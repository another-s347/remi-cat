use std::pin::Pin;
use std::time::Instant;

use futures::{Stream, StreamExt};
use remi_agentloop::prelude::{
    AgentError, AgentState, CancellationToken, ParsedToolCall, ToolCallOutcome,
};
use remi_agentloop::types::{AgentEvent, SubSessionEvent};

#[derive(Debug, Clone, Default)]
pub struct CoreDriveConfig {
    pub cancel: Option<CancellationToken>,
    pub max_tool_rounds: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CoreUsageStats {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub max_prompt_tokens: u32,
    pub elapsed_ms: u64,
}

#[derive(Debug)]
struct CoreRunMetrics {
    started_at: Instant,
    prompt_tokens: u32,
    completion_tokens: u32,
    max_prompt_tokens: u32,
}

impl Default for CoreRunMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl CoreRunMetrics {
    fn new() -> Self {
        Self {
            started_at: Instant::now(),
            prompt_tokens: 0,
            completion_tokens: 0,
            max_prompt_tokens: 0,
        }
    }

    fn record_usage(&mut self, prompt_tokens: u32, completion_tokens: u32) -> CoreUsageStats {
        self.prompt_tokens = self.prompt_tokens.saturating_add(prompt_tokens);
        self.completion_tokens = self.completion_tokens.saturating_add(completion_tokens);
        self.max_prompt_tokens = self.max_prompt_tokens.max(prompt_tokens);
        self.snapshot()
    }

    fn snapshot(&self) -> CoreUsageStats {
        CoreUsageStats {
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            max_prompt_tokens: self.max_prompt_tokens,
            elapsed_ms: self.elapsed_ms(),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.started_at
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64
    }
}

#[derive(Debug, Clone)]
pub struct CoreToolDispatch {
    pub state: AgentState,
    pub tool_calls: Vec<ParsedToolCall>,
    pub completed_results: Vec<ToolCallOutcome>,
    pub tool_round: usize,
    pub stats: CoreUsageStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreCancelKind {
    Signal,
    Agent,
}

#[derive(Debug, Clone)]
pub enum CoreDriveEvent {
    Text(String),
    Thinking(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallArgumentsDelta {
        id: String,
        delta: String,
    },
    ToolResult {
        id: String,
        name: String,
        result: String,
    },
    SubSession(SubSessionEvent),
    Usage(CoreUsageStats),
    Checkpoint {
        state: AgentState,
    },
    ToolDispatch(CoreToolDispatch),
    Done {
        state: Option<AgentState>,
        stats: CoreUsageStats,
        tool_rounds: usize,
    },
    Cancelled {
        kind: CoreCancelKind,
        state: Option<AgentState>,
        stats: CoreUsageStats,
        tool_rounds: usize,
    },
    Error {
        error: AgentError,
        state: Option<AgentState>,
        stats: CoreUsageStats,
        tool_rounds: usize,
    },
    ToolDelta {
        id: String,
        name: String,
        delta: String,
    },
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CoreToolRoundLimit {
    Unlimited,
    Limited(usize),
}

impl Default for CoreToolRoundLimit {
    fn default() -> Self {
        Self::Unlimited
    }
}

struct CoreLoopDriver {
    cancel: Option<CancellationToken>,
    tool_round_limit: CoreToolRoundLimit,
    tool_rounds: usize,
    metrics: CoreRunMetrics,
    last_checkpoint_state: Option<AgentState>,
}

impl CoreLoopDriver {
    fn new(cancel: Option<CancellationToken>) -> Self {
        Self {
            cancel,
            tool_round_limit: CoreToolRoundLimit::Unlimited,
            tool_rounds: 0,
            metrics: CoreRunMetrics::new(),
            last_checkpoint_state: None,
        }
    }

    fn with_tool_round_limit(mut self, limit: CoreToolRoundLimit) -> Self {
        self.tool_round_limit = limit;
        self
    }

    async fn next<S>(&mut self, events: Pin<&mut S>) -> Option<CoreDriveEvent>
    where
        S: Stream<Item = AgentEvent>,
    {
        let event = match next_cancel_aware(events, self.cancel.as_ref()).await {
            CancelAwareNext::Item(event) => event,
            CancelAwareNext::Cancelled => {
                return Some(CoreDriveEvent::Cancelled {
                    kind: CoreCancelKind::Signal,
                    state: self.last_checkpoint_state.clone(),
                    stats: self.metrics.snapshot(),
                    tool_rounds: self.tool_rounds,
                });
            }
            CancelAwareNext::Ended => return None,
        };

        Some(self.drive_event(event))
    }

    fn drive_event(&mut self, event: AgentEvent) -> CoreDriveEvent {
        match event {
            AgentEvent::TextDelta(delta) => CoreDriveEvent::Text(delta),
            AgentEvent::ThinkingEnd { content } => CoreDriveEvent::Thinking(content),
            AgentEvent::ToolCallStart { id, name } => CoreDriveEvent::ToolCallStart { id, name },
            AgentEvent::ToolCallArgumentsDelta { id, delta } => {
                CoreDriveEvent::ToolCallArgumentsDelta { id, delta }
            }
            AgentEvent::ToolResult { id, name, result } => {
                CoreDriveEvent::ToolResult { id, name, result }
            }
            AgentEvent::SubSession(event) => CoreDriveEvent::SubSession(event),
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => CoreDriveEvent::Usage(self.metrics.record_usage(prompt_tokens, completion_tokens)),
            AgentEvent::Checkpoint(checkpoint) => {
                self.last_checkpoint_state = Some(checkpoint.state.clone());
                CoreDriveEvent::Checkpoint {
                    state: checkpoint.state,
                }
            }
            AgentEvent::NeedToolExecution {
                state,
                tool_calls,
                completed_results,
            } => {
                self.tool_rounds = self.tool_rounds.saturating_add(1);
                if let CoreToolRoundLimit::Limited(max_rounds) = self.tool_round_limit {
                    if self.tool_rounds > max_rounds {
                        return CoreDriveEvent::Error {
                            error: AgentError::tool(
                                "core_agent",
                                format!("exceeded the maximum of {max_rounds} tool rounds"),
                            ),
                            state: self.last_checkpoint_state.clone(),
                            stats: self.metrics.snapshot(),
                            tool_rounds: self.tool_rounds,
                        };
                    }
                }
                CoreDriveEvent::ToolDispatch(CoreToolDispatch {
                    state,
                    tool_calls,
                    completed_results,
                    tool_round: self.tool_rounds,
                    stats: self.metrics.snapshot(),
                })
            }
            AgentEvent::Done => CoreDriveEvent::Done {
                state: self.last_checkpoint_state.clone(),
                stats: self.metrics.snapshot(),
                tool_rounds: self.tool_rounds,
            },
            AgentEvent::Cancelled => CoreDriveEvent::Cancelled {
                kind: CoreCancelKind::Agent,
                state: self.last_checkpoint_state.clone(),
                stats: self.metrics.snapshot(),
                tool_rounds: self.tool_rounds,
            },
            AgentEvent::Error(error) => CoreDriveEvent::Error {
                error,
                state: self.last_checkpoint_state.clone(),
                stats: self.metrics.snapshot(),
                tool_rounds: self.tool_rounds,
            },
            AgentEvent::ToolDelta { id, name, delta } => {
                CoreDriveEvent::ToolDelta { id, name, delta }
            }
            AgentEvent::TurnStart { .. }
            | AgentEvent::RunStart { .. }
            | AgentEvent::ThinkingStart
            | AgentEvent::Custom { .. }
            | AgentEvent::Interrupt { .. } => CoreDriveEvent::Ignored,
        }
    }
}

pub struct CoreAgentLoop {
    driver: CoreLoopDriver,
}

impl CoreAgentLoop {
    pub fn new(config: CoreDriveConfig) -> Self {
        let limit = config
            .max_tool_rounds
            .map(CoreToolRoundLimit::Limited)
            .unwrap_or(CoreToolRoundLimit::Unlimited);
        Self {
            driver: CoreLoopDriver::new(config.cancel).with_tool_round_limit(limit),
        }
    }

    pub fn drive<'a, S>(&'a mut self, events: S) -> impl Stream<Item = CoreDriveEvent> + 'a
    where
        S: Stream<Item = AgentEvent> + 'a,
    {
        async_stream::stream! {
            let mut events = std::pin::pin!(events);
            while let Some(event) = self.driver.next(events.as_mut()).await {
                let should_stop = matches!(
                    event,
                    CoreDriveEvent::ToolDispatch(_)
                        | CoreDriveEvent::Done { .. }
                        | CoreDriveEvent::Cancelled { .. }
                        | CoreDriveEvent::Error { .. }
                );
                yield event;
                if should_stop {
                    break;
                }
            }
        }
    }
}

enum CancelAwareNext<T> {
    Item(T),
    Cancelled,
    Ended,
}

async fn next_cancel_aware<S, T>(
    mut events: Pin<&mut S>,
    cancel: Option<&CancellationToken>,
) -> CancelAwareNext<T>
where
    S: Stream<Item = T>,
{
    if let Some(cancel) = cancel {
        tokio::select! {
            _ = cancel.cancelled() => CancelAwareNext::Cancelled,
            event = events.next() => match event {
                Some(event) => CancelAwareNext::Item(event),
                None => CancelAwareNext::Ended,
            },
        }
    } else {
        match events.next().await {
            Some(event) => CancelAwareNext::Item(event),
            None => CancelAwareNext::Ended,
        }
    }
}
