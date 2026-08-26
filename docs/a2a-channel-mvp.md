# A2A channel MVP

## Scope

The A2A channel exposes remi-cat as an A2A v1 server and is also the only
transport used by `agent__*` delegate tools. It does not replace ACP.

Supported in the MVP:

- public Agent Card discovery;
- HTTP+JSON message send and SSE message streaming;
- task get, list, cancel, and live subscribe operations;
- persisted A2A task snapshots;
- text input and streamed text artifacts;
- optional bearer-token authentication;
- one remi-cat session per A2A context.
- target-agent routing, structured activity, steer, approval, and question
  control through negotiated Remi extensions;
- outbound delegate discovery and HTTP/SSE execution.

Deferred:

- push notifications;
- gRPC and JSON-RPC bindings;
- extended Agent Cards;
- file, URL, and structured-data parts;
- multi-tenant identity federation.

## Runtime boundary

The Axum server cannot own the local `Rc<Runtime>`. The A2A executor therefore
uses a thread-safe handle to enqueue runs into the existing local execution
dispatcher. A2A supplies an explicit `a2a` platform override, so its session
and tool context remain distinct from other channels.

An A2A `contextId` maps to a persisted remi-cat channel session. An A2A
`taskId` maps to a single run and is also the cancellation key. Task snapshots
are stored separately from remi-cat sessions because a context can contain
multiple tasks.

## Configuration

- `REMI_A2A_ENABLED`: enable the inbound server explicitly. Managed local
  delegates use the in-process A2A transport and do not start a listener.
- `REMI_A2A_HOST`: listen host; defaults to `127.0.0.1`.
- `REMI_A2A_PORT`: listen port; defaults to `8788`.
- `REMI_A2A_PUBLIC_URL`: externally visible base URL advertised by the Agent
  Card; defaults to the listen URL.
- `REMI_A2A_TOKEN`: optional bearer token. A non-loopback listener is rejected
  when this is empty.
- `REMI_A2A_DELEGATE_URL`: optional global outbound delegate endpoint. Setting
  it selects HTTP instead of the in-process transport.
- `REMI_A2A_DELEGATE_ENDPOINTS`: optional JSON map from delegate ID to a remote
  endpoint and token environment-variable reference.

TLS is expected to terminate at a trusted reverse proxy when the endpoint is
exposed outside the machine.

## Acceptance

The MVP is accepted when all of the following are demonstrated:

1. Agent Card discovery advertises A2A v1, HTTP+JSON, text modes, streaming,
   and no push-notification capability.
2. A text message creates a task, resolves an `a2a` session, produces ordered
   artifact updates, and reaches `TASK_STATE_COMPLETED`.
3. `GetTask` and `ListTasks` return persisted task state after a store reload.
4. `CancelTask` reaches the active remi-cat cancellation token and the task
   reaches `TASK_STATE_CANCELED` without later completion artifacts.
5. Duplicate/overlapping execution of the same active task is rejected by the
   protocol handler.
6. Protected routes reject a missing or incorrect bearer token when a token is
   configured; Agent Card discovery remains public.
7. Unit and integration tests, formatting, `cargo check`, and an external A2A
   client smoke test pass. Build success alone is not protocol acceptance.
