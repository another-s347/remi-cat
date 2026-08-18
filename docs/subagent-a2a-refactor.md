# A2A-only subagent replacement

## Decision

Keep the model-facing `agent__<delegate>` tool contract and the existing
`SubSessionEvent` UI contract, but replace the execution and control path
behind them with A2A. There is no local/in-process subagent backend, fallback,
or long-lived dual-stack mode after this change.

Every delegate is an A2A agent. A managed local delegate is addressed through
the current remi-cat A2A service with an explicit agent-profile extension; a
remote delegate is addressed through its configured Agent Card. Both paths use
the A2A wire protocol and identical task semantics. The parent runtime never
constructs a delegate `AgentBuilder` or drives a delegate loop directly.

The replacement lands atomically: the A2A implementation must pass the parity
suite before the old executor is deleted in the same change. A temporary test
switch may exist on the development branch to compare traces, but it is not a
shipping configuration or runtime fallback.

## Parity surface carried by A2A

The current subagent implementation owns more than model invocation. It also
owns:

- delegate allowlisting and `agent__<name>` tool registration;
- named, persistent child sessions;
- serialization of concurrent turns for the same named session;
- handoff/steer into an active child run;
- cancellation propagation;
- child memory, environment context, hooks, tool policy, and approvals;
- fine-grained output, thinking, tool, error, and completion events;
- projection of those events into Web, TUI, Feishu, ACP, and session stores.

The A2A server now adds target-agent routing, structured activity artifacts,
interactive control endpoints, and handoff on top of core text execution,
streaming, persistence, authentication, and cancellation. Those extensions are
required during Agent Card discovery, so Remi never silently downgrades a
delegate and loses UI behavior.

A2A v1 servers allocate new task IDs. The client uses its own message ID and a
deterministic named-session context ID, learns the server task ID from the
stream, and never uses a parent tool-call ID to create an A2A task.

## Target architecture

```text
parent model
  -> agent__explorer(task, named)
  -> DelegateTool
  -> A2aDelegateClient
       -> Agent Card discovery and validation
       -> SendStreamingMessage / follow-up message
       -> GetTask / CancelTask / SubscribeToTask
  -> DelegateEvent mapper
  -> existing SubSessionEvent projections
       -> parent tool result
       -> TUI / Web / Feishu / ACP
       -> parent-child session metadata
```

The client transport must not leak A2A types into UI code. A small internal
domain model is the compatibility boundary:

```rust
trait DelegateTaskClient {
    async fn invoke(&self, request: DelegateRequest)
        -> Result<DelegateRun, DelegateError>;
    async fn continue_task(&self, request: DelegateContinuation)
        -> Result<DelegateRun, DelegateError>;
    async fn cancel(&self, task: DelegateTaskRef) -> Result<bool, DelegateError>;
}

struct DelegateRequest {
    invocation_id: String,
    parent_context: ParentContext,
    delegate_id: String,
    named_session: String,
    content: Content,
    overlap_policy: OverlapPolicy,
}

enum DelegateEvent {
    Started,
    OutputDelta(Content),
    Activity(DelegateActivity),
    InputRequired(DelegateInputRequest),
    AuthRequired(DelegateAuthRequest),
    Completed(Content),
    Failed(String),
    Canceled,
    HandedOff,
}
```

`RemiSubAgentTool` becomes a thin adapter from tool arguments to this client and
from `DelegateEvent` back to the existing `SubSessionEvent` stream. That keeps
the current channel/UI behavior stable while the execution backend changes.

## Identity and persistence mapping

| remi concept | A2A concept | Rule |
| --- | --- | --- |
| delegate endpoint | Agent Card + supported interface | Resolve and validate before registering the tool. |
| `agent__explorer` | configured A2A agent | Tool name stays stable; endpoint details remain hidden from the model. |
| parent tool-call ID | local invocation ID / message metadata | Never use it as a new A2A task ID. |
| named session | client-side delegate binding | Persist `(parent scope, delegate, named, endpoint fingerprint) -> contextId`. |
| one delegate turn | A2A Task | The server allocates the task ID. |
| follow-up in the same named session | new task in the same `contextId` | Use the prior task ID only when continuing an interrupted task. |
| child final output | final Artifact | Convert to the parent tool result and child-session history. |
| parent cancellation | `CancelTask` | Do not report canceled until the remote task acknowledges a terminal canceled state. |

Store client-side bindings separately from server task snapshots, for example
`a2a/delegate-bindings.json` initially and SQLite when concurrent writers are
introduced. A binding contains the endpoint identity, remote context ID,
active task ID, last terminal task ID, and update version. Changing an endpoint
must not silently reuse an old context.

## Delegate configuration

Keep the existing agent-profile string syntax, but change its meaning to a
managed local A2A delegate. Remote endpoint selection is host configuration,
not model-visible agent-profile data:

```json
{
  "reviewer": {
    "endpoint": "https://reviewer.example.com/a2a",
    "token_env": "REMI_A2A_REVIEWER_TOKEN"
  }
}
```

Legacy strings resolve to the current profile's A2A endpoint plus the delegate
agent ID; they never select an in-process executor. Tokens are secret
references, never serialized token values. The JSON is supplied through
`REMI_A2A_DELEGATE_ENDPOINTS`; the global `REMI_A2A_DELEGATE_URL` remains a
single-endpoint override for test and embedded deployments. Tool
registration fails closed when discovery, protocol version, authentication,
required extensions, or content modes are incompatible.

For remi-to-remi isolation, run each delegate as an application profile with
its own state, agent definition, tool policy, and A2A listener. Existing
profile process management can own the child process lifecycle. A remote A2A
agent is configured the same way except that remi does not own its process.

## Protocol event mapping

Core A2A provides the portable baseline:

| A2A event/state | internal delegate event |
| --- | --- |
| submitted/working status | `Started` / progress status |
| text Artifact update | `OutputDelta` |
| completed Task + Artifact | `Completed` |
| failed/rejected Task | `Failed` |
| canceled Task | `Canceled` |
| input-required Task | `InputRequired` |
| auth-required Task | `AuthRequired` |

Generic A2A agents may expose only this baseline. Remi-to-remi parity needs
negotiated extensions:

1. `urn:remi:a2a:activity-stream:v1`
   - structured tool start, argument delta, output, result, turn, and progress
     events;
   - no raw chain-of-thought requirement; reasoning summaries are optional and
     policy controlled;
   - every event carries a monotonic sequence and stable activity ID.
2. `urn:remi:a2a:interactive-input:v1`
   - schemas for tool approval and user questions while the core task is
     `INPUT_REQUIRED`;
   - a follow-up message on the same task carries the decision or answer.
3. `urn:remi:a2a:handoff:v1`
   - allows a new turn to steer an already-working named context;
   - required only for `overlap_policy: handoff`.
4. `urn:remi:a2a:invocation-context:v1`
   - parent context, parent task, tool-call, application, workspace, actor, and
     trace correlation without exposing them as model text.
5. `urn:remi:a2a:idempotency:v1`
   - deduplicates an initial send by invocation/message ID when the client loses
     the response before learning the server-issued task ID.

Extensions are declared in the Agent Card and activated explicitly by the
client. Missing required extensions are a registration or invocation error,
not a silent downgrade.

## Concurrency, continuation, and failure policy

The current named-session behavior must become explicit policy:

- `queue`: serialize turns for the same binding; portable to generic agents;
- `cancel_replace`: cancel the active task, then create a new task in the same
  context after terminal acknowledgment;
- `handoff`: steer an active task; allowed only when the handoff extension is
  negotiated.

Initial send is not blindly retried because a timeout may occur after the
remote agent created a task. Retry it only with negotiated idempotency. Reads,
subscriptions, and cancellation may use finite exponential backoff for
transient transport failures. Authentication, authorization, validation,
extension mismatch, and rejected tasks are not retried.

SSE reconnect uses `SubscribeToTask` plus the last observed event sequence.
Every projection is idempotent by `(remote task ID, event sequence)`. A
terminal state is monotonic: delayed working events can never regress a
completed, failed, rejected, or canceled child session.

## Security boundary

Moving a delegate out of process removes implicit sharing. The A2A caller must
not assume access to the parent's filesystem, secrets, memory, tools, or user
identity.

- Agent Card discovery is allowlisted by endpoint and expected identity.
- Non-loopback endpoints require TLS and configured authentication.
- Delegation metadata is minimized and schema validated.
- Remote artifacts are untrusted input and pass through existing size,
  content-type, and redaction boundaries.
- Approval remains an end-user decision; the parent agent cannot manufacture
  approval on the user's behalf.
- Trace propagation uses correlation IDs without credential propagation.

## Implemented atomic replacement

The cutover is implemented as one runtime mode:

- switch every `agent__<delegate>` tool to `A2aDelegateClient`;
- switch sub-session steer, approval/question response, and cancellation to
  A2A task control;
- delete local delegate `AgentBuilder` construction, local history ownership,
  and local steer/cancellation queues;
- keep only a small A2A task coordinator for named-session serialization and
  active remote task lookup;
- do not fall back locally on discovery, connection, execution, or stream
  failure; return a classified delegate error instead.

## Acceptance criteria

1. Existing agent profiles and `agent__<name>(task, named)` calls remain
   backward compatible.
2. A new named session creates a server-issued task and persists its remote
   context binding.
3. Reusing a name preserves context across parent turns and process restarts.
4. Named-session serialization and negotiated handoff behave deterministically
   under overlapping calls.
5. Parent cancellation reaches the remote task; no completion event appears
   after canceled is acknowledged.
6. Output and tool activities render consistently in TUI, Web, Feishu, and ACP
   without duplicate deltas or terminal regression.
7. Approval and user-question flows are proxied to the existing parent UI
   managers and resume the active remote task.
8. Initial sends are never blindly retried after an ambiguous network failure.
9. Authentication, extension, and endpoint-identity failures fail closed and
   do not leak secrets.
10. A2A client/server contract tests, cancellation, restart continuity, and a
    real-model parent-to-delegate canary pass before release.
