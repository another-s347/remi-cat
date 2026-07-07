use crate::types::{InterruptInfo, Message, RunId, ThreadId};
use serde::Serialize;
use std::time::Duration;

#[cfg(feature = "tracing-langsmith")]
pub mod langsmith;
pub mod stdout;
#[cfg(feature = "tracing-langsmith")]
pub use langsmith::LangSmithTracer;

// ── Trace event structs ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct RunStartTrace {
    pub thread_id: Option<ThreadId>,
    pub run_id: RunId,
    pub model: String,
    pub system_prompt: Option<String>,
    pub input_messages: Vec<Message>,
    pub metadata: Option<serde_json::Value>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunEndTrace {
    pub run_id: RunId,
    pub status: RunStatus,
    pub output_messages: Vec<Message>,
    pub total_turns: usize,
    pub total_prompt_tokens: u32,
    pub total_completion_tokens: u32,
    pub duration: Duration,
    pub error: Option<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub enum RunStatus {
    Completed,
    Interrupted,
    Error,
    MaxTurnsExceeded,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelStartTrace {
    pub run_id: RunId,
    pub turn: usize,
    pub call_index: usize,
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelEndTrace {
    pub run_id: RunId,
    pub turn: usize,
    pub call_index: usize,
    pub response_text: Option<String>,
    pub tool_calls: Vec<ToolCallTrace>,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub duration: Duration,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallTrace {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub result: Option<String>,
    pub interrupted: bool,
    pub duration: Duration,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolStartTrace {
    pub run_id: RunId,
    pub turn: usize,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolEndTrace {
    pub run_id: RunId,
    pub turn: usize,
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: Option<String>,
    pub interrupted: bool,
    pub error: Option<String>,
    pub duration: Duration,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolOutcomeTrace {
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolExecutionHandoffTrace {
    pub run_id: RunId,
    pub turn: usize,
    pub tool_calls: Vec<ToolCallTrace>,
    pub completed_results: Vec<ToolOutcomeTrace>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InterruptTrace {
    pub run_id: RunId,
    pub interrupts: Vec<InterruptInfo>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResumeTrace {
    pub run_id: RunId,
    pub payloads_count: usize,
    pub outcomes: Vec<ToolOutcomeTrace>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExternalToolResultTrace {
    pub run_id: RunId,
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnStartTrace {
    pub run_id: RunId,
    pub turn: usize,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

// ── Tracer trait ──────────────────────────────────────────────────────────────

/// Pluggable tracing backend — all methods have default no-op implementations
#[allow(async_fn_in_trait)]
pub trait Tracer {
    async fn on_run_start(&self, _event: &RunStartTrace) {}
    async fn on_run_end(&self, _event: &RunEndTrace) {}
    async fn on_model_start(&self, _event: &ModelStartTrace) {}
    async fn on_model_end(&self, _event: &ModelEndTrace) {}
    async fn on_tool_start(&self, _event: &ToolStartTrace) {}
    async fn on_tool_end(&self, _event: &ToolEndTrace) {}
    async fn on_tool_execution_handoff(&self, _event: &ToolExecutionHandoffTrace) {}
    async fn on_external_tool_result(&self, _event: &ExternalToolResultTrace) {}
    async fn on_interrupt(&self, _event: &InterruptTrace) {}
    async fn on_resume(&self, _event: &ResumeTrace) {}
    async fn on_turn_start(&self, _event: &TurnStartTrace) {}
    async fn on_custom(&self, _name: &str, _data: &serde_json::Value) {}
    /// Flush any buffered/background I/O.  Called after the agent stream ends.
    async fn on_flush(&self) {}
}

// ── DynTracer — object-safe version ──────────────────────────────────────────

use std::future::Future as StdFuture;
use std::pin::Pin;
type BoxFuture<'a> = Pin<Box<dyn StdFuture<Output = ()> + 'a>>;

pub trait DynTracer: Send + Sync {
    fn on_run_start<'a>(&'a self, event: &'a RunStartTrace) -> BoxFuture<'a>;
    fn on_run_end<'a>(&'a self, event: &'a RunEndTrace) -> BoxFuture<'a>;
    fn on_model_start<'a>(&'a self, event: &'a ModelStartTrace) -> BoxFuture<'a>;
    fn on_model_end<'a>(&'a self, event: &'a ModelEndTrace) -> BoxFuture<'a>;
    fn on_tool_start<'a>(&'a self, event: &'a ToolStartTrace) -> BoxFuture<'a>;
    fn on_tool_end<'a>(&'a self, event: &'a ToolEndTrace) -> BoxFuture<'a>;
    fn on_tool_execution_handoff<'a>(
        &'a self,
        event: &'a ToolExecutionHandoffTrace,
    ) -> BoxFuture<'a>;
    fn on_external_tool_result<'a>(&'a self, event: &'a ExternalToolResultTrace) -> BoxFuture<'a>;
    fn on_interrupt<'a>(&'a self, event: &'a InterruptTrace) -> BoxFuture<'a>;
    fn on_resume<'a>(&'a self, event: &'a ResumeTrace) -> BoxFuture<'a>;
    fn on_turn_start<'a>(&'a self, event: &'a TurnStartTrace) -> BoxFuture<'a>;
    fn on_custom<'a>(&'a self, name: &'a str, data: &'a serde_json::Value) -> BoxFuture<'a>;
    fn on_flush<'a>(&'a self) -> BoxFuture<'a>;
}

impl<T: Tracer + Send + Sync> DynTracer for T {
    fn on_run_start<'a>(&'a self, event: &'a RunStartTrace) -> BoxFuture<'a> {
        Box::pin(Tracer::on_run_start(self, event))
    }
    fn on_run_end<'a>(&'a self, event: &'a RunEndTrace) -> BoxFuture<'a> {
        Box::pin(Tracer::on_run_end(self, event))
    }
    fn on_model_start<'a>(&'a self, event: &'a ModelStartTrace) -> BoxFuture<'a> {
        Box::pin(Tracer::on_model_start(self, event))
    }
    fn on_model_end<'a>(&'a self, event: &'a ModelEndTrace) -> BoxFuture<'a> {
        Box::pin(Tracer::on_model_end(self, event))
    }
    fn on_tool_start<'a>(&'a self, event: &'a ToolStartTrace) -> BoxFuture<'a> {
        Box::pin(Tracer::on_tool_start(self, event))
    }
    fn on_tool_end<'a>(&'a self, event: &'a ToolEndTrace) -> BoxFuture<'a> {
        Box::pin(Tracer::on_tool_end(self, event))
    }
    fn on_tool_execution_handoff<'a>(
        &'a self,
        event: &'a ToolExecutionHandoffTrace,
    ) -> BoxFuture<'a> {
        Box::pin(Tracer::on_tool_execution_handoff(self, event))
    }
    fn on_external_tool_result<'a>(&'a self, event: &'a ExternalToolResultTrace) -> BoxFuture<'a> {
        Box::pin(Tracer::on_external_tool_result(self, event))
    }
    fn on_interrupt<'a>(&'a self, event: &'a InterruptTrace) -> BoxFuture<'a> {
        Box::pin(Tracer::on_interrupt(self, event))
    }
    fn on_resume<'a>(&'a self, event: &'a ResumeTrace) -> BoxFuture<'a> {
        Box::pin(Tracer::on_resume(self, event))
    }
    fn on_turn_start<'a>(&'a self, event: &'a TurnStartTrace) -> BoxFuture<'a> {
        Box::pin(Tracer::on_turn_start(self, event))
    }
    fn on_custom<'a>(&'a self, name: &'a str, data: &'a serde_json::Value) -> BoxFuture<'a> {
        Box::pin(Tracer::on_custom(self, name, data))
    }
    fn on_flush<'a>(&'a self) -> BoxFuture<'a> {
        Box::pin(Tracer::on_flush(self))
    }
}

// ── CompositeTracer ───────────────────────────────────────────────────────────

pub struct CompositeTracer {
    tracers: Vec<Box<dyn DynTracer>>,
}

impl CompositeTracer {
    pub fn new() -> Self {
        Self {
            tracers: Vec::new(),
        }
    }
    pub fn add(mut self, tracer: impl Tracer + Send + Sync + 'static) -> Self {
        self.tracers.push(Box::new(tracer));
        self
    }
}

impl DynTracer for CompositeTracer {
    fn on_run_start<'a>(&'a self, event: &'a RunStartTrace) -> BoxFuture<'a> {
        Box::pin(async move {
            for t in &self.tracers {
                t.on_run_start(event).await;
            }
        })
    }
    fn on_run_end<'a>(&'a self, event: &'a RunEndTrace) -> BoxFuture<'a> {
        Box::pin(async move {
            for t in &self.tracers {
                t.on_run_end(event).await;
            }
        })
    }
    fn on_model_start<'a>(&'a self, event: &'a ModelStartTrace) -> BoxFuture<'a> {
        Box::pin(async move {
            for t in &self.tracers {
                t.on_model_start(event).await;
            }
        })
    }
    fn on_model_end<'a>(&'a self, event: &'a ModelEndTrace) -> BoxFuture<'a> {
        Box::pin(async move {
            for t in &self.tracers {
                t.on_model_end(event).await;
            }
        })
    }
    fn on_tool_start<'a>(&'a self, event: &'a ToolStartTrace) -> BoxFuture<'a> {
        Box::pin(async move {
            for t in &self.tracers {
                t.on_tool_start(event).await;
            }
        })
    }
    fn on_tool_end<'a>(&'a self, event: &'a ToolEndTrace) -> BoxFuture<'a> {
        Box::pin(async move {
            for t in &self.tracers {
                t.on_tool_end(event).await;
            }
        })
    }
    fn on_tool_execution_handoff<'a>(
        &'a self,
        event: &'a ToolExecutionHandoffTrace,
    ) -> BoxFuture<'a> {
        Box::pin(async move {
            for t in &self.tracers {
                t.on_tool_execution_handoff(event).await;
            }
        })
    }
    fn on_external_tool_result<'a>(&'a self, event: &'a ExternalToolResultTrace) -> BoxFuture<'a> {
        Box::pin(async move {
            for t in &self.tracers {
                t.on_external_tool_result(event).await;
            }
        })
    }
    fn on_interrupt<'a>(&'a self, event: &'a InterruptTrace) -> BoxFuture<'a> {
        Box::pin(async move {
            for t in &self.tracers {
                t.on_interrupt(event).await;
            }
        })
    }
    fn on_resume<'a>(&'a self, event: &'a ResumeTrace) -> BoxFuture<'a> {
        Box::pin(async move {
            for t in &self.tracers {
                t.on_resume(event).await;
            }
        })
    }
    fn on_turn_start<'a>(&'a self, event: &'a TurnStartTrace) -> BoxFuture<'a> {
        Box::pin(async move {
            for t in &self.tracers {
                t.on_turn_start(event).await;
            }
        })
    }
    fn on_custom<'a>(&'a self, name: &'a str, data: &'a serde_json::Value) -> BoxFuture<'a> {
        Box::pin(async move {
            for t in &self.tracers {
                t.on_custom(name, data).await;
            }
        })
    }
    fn on_flush<'a>(&'a self) -> BoxFuture<'a> {
        Box::pin(async move {
            for t in &self.tracers {
                t.on_flush().await;
            }
        })
    }
}
