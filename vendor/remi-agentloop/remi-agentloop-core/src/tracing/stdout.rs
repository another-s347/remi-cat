use crate::tracing::*;

/// Simple stdout tracer — prints all events to stderr
pub struct StdoutTracer;

impl Tracer for StdoutTracer {
    fn on_run_start(&self, event: &RunStartTrace) -> impl std::future::Future<Output = ()> {
        eprintln!("[tracer] run_start  run_id={}", event.run_id);
        async {}
    }
    fn on_run_end(&self, event: &RunEndTrace) -> impl std::future::Future<Output = ()> {
        eprintln!(
            "[tracer] run_end    run_id={} status={:?}",
            event.run_id, event.status
        );
        async {}
    }
    fn on_model_start(&self, event: &ModelStartTrace) -> impl std::future::Future<Output = ()> {
        eprintln!(
            "[tracer] model_start run_id={} turn={}",
            event.run_id, event.turn
        );
        async {}
    }
    fn on_model_end(&self, event: &ModelEndTrace) -> impl std::future::Future<Output = ()> {
        eprintln!(
            "[tracer] model_end  run_id={} turn={} tokens={}+{}",
            event.run_id, event.turn, event.prompt_tokens, event.completion_tokens
        );
        async {}
    }
    fn on_tool_start(&self, event: &ToolStartTrace) -> impl std::future::Future<Output = ()> {
        eprintln!(
            "[tracer] tool_start run_id={} tool={}",
            event.run_id, event.tool_name
        );
        async {}
    }
    fn on_tool_end(&self, event: &ToolEndTrace) -> impl std::future::Future<Output = ()> {
        eprintln!(
            "[tracer] tool_end   run_id={} tool={} interrupted={}",
            event.run_id, event.tool_name, event.interrupted
        );
        async {}
    }
    fn on_tool_execution_handoff(
        &self,
        event: &ToolExecutionHandoffTrace,
    ) -> impl std::future::Future<Output = ()> {
        eprintln!(
            "[tracer] handoff    run_id={} turn={} external_calls={} completed_results={}",
            event.run_id,
            event.turn,
            event.tool_calls.len(),
            event.completed_results.len()
        );
        async {}
    }
    fn on_external_tool_result(
        &self,
        event: &ExternalToolResultTrace,
    ) -> impl std::future::Future<Output = ()> {
        eprintln!(
            "[tracer] ext_result run_id={} tool={} error={}",
            event.run_id,
            event.tool_name,
            event.error.is_some()
        );
        async {}
    }
    fn on_interrupt(&self, event: &InterruptTrace) -> impl std::future::Future<Output = ()> {
        eprintln!(
            "[tracer] interrupt  run_id={} count={}",
            event.run_id,
            event.interrupts.len()
        );
        async {}
    }
    fn on_resume(&self, event: &ResumeTrace) -> impl std::future::Future<Output = ()> {
        eprintln!(
            "[tracer] resume     run_id={} outcomes={}",
            event.run_id,
            event.outcomes.len()
        );
        async {}
    }
    fn on_turn_start(&self, event: &TurnStartTrace) -> impl std::future::Future<Output = ()> {
        eprintln!(
            "[tracer] turn_start run_id={} turn={}",
            event.run_id, event.turn
        );
        async {}
    }
}
