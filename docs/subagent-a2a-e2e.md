# Subagent A2A-only end-to-end acceptance

This matrix defines the release gate for the A2A-only subagent implementation.
“HTTP E2E” means a real loopback listener and serialized A2A HTTP/SSE payloads,
not a direct executor call. “Real model” uses the normal remi-cat binary and
configured model provider.

| Capability | Automated evidence | Real-process evidence |
| --- | --- | --- |
| Agent Card discovery and required extensions | `a2a_delegate::tests::invoke_discovers_agent_and_maps_real_sse_stream` | Parent-to-delegate canary completed through discovery and SSE. |
| Server-issued task and target-agent routing | `a2a_channel::tests::streaming_message_runs_through_dispatcher_and_persists_task` | Stored task metadata selected `explorer`. |
| Text, thinking, tool, and error activity | `a2a_channel::tests::activity_and_failure_events_cross_real_sse_boundary` | Real `now` call produced start, arguments, result, and thinking events. |
| Named persistent context | task-store and channel-session assertions | Two separate processes reused one context ID and recalled the prior result. |
| Nested delegates | sub-session event projection regression suite | `default -> explorer -> nested` returned `A2A_NESTED_OK`; both remote tasks completed. |
| Steer / handoff | `a2a_channel::tests::active_task_controls_cross_real_http_boundary`; `a2a_interactive_proxy_forwards_parent_decisions_over_http` | Control request crossed the live A2A task route. |
| Approval proxy | same HTTP control and parent-proxy tests | Request IDs are replaced locally; the decision is sent to the original remote task and ID. |
| User-question proxy | same HTTP control and parent-proxy tests | Answer is sent to the original remote task and question ID. |
| Cancellation | HTTP control test plus `json_task_store_terminal_state_is_monotonic` | CancelTask stopped a real model run; persisted state remained `CANCELED`. |
| SubagentStart / SubagentStop hooks | hook manager regression suite | Start context changed delegate output; Stop hook produced an external marker. |
| Per-agent endpoint and bearer authentication | public-card/auth protocol test | Authenticated per-agent endpoint succeeded; a wrong token failed with HTTP 401 and no local fallback. |
| Web, TUI, Feishu, ACP projections | existing channel-specific sub-session regression tests | Compatibility boundary remains `SubSessionEvent`. |
| Failure propagation and no fallback | failed SSE task test | Invalid bearer token surfaced as the delegate error. |

## Repeatable commands

```bash
cargo test -p remi-cat a2a_channel::tests::
SHELL=/bin/bash cargo test -p bot-core -- --test-threads=1
cargo test -p remi-cat --lib
cargo fmt --all -- --check
git diff --check
```

Real-provider tests intentionally use an isolated profile and state directory
under `/tmp`; credentials are read through the normal model-profile secret
resolution and are never written to test output or task snapshots.

## Application Profile A-to-B acceptance

The local Profile endpoint adds a second real-process boundary on top of the
HTTP/SSE subagent matrix. Its release gate is:

| Capability | Acceptance |
| --- | --- |
| Fresh profile initialization | `profile init --with-runtime` immediately passes `profile check --strict`, including every declared skills directory. |
| Registry discovery | A registry containing caller A and travel B returns only B for `--tag travel --intent plan-trip`. |
| Dynamic Agent Card | Starting B's endpoint returns B's manifest name and `urn:remi:a2a:binding:stdio-json:v1`. |
| Explicit A-to-B call | Running `--profile A profile ask @travel --named trip-001 ...` starts B on demand and returns `A_TO_B_OK`. |
| Cross-process named context | A second call with `trip-001`, after the first B process exits, recalls and returns `A_TO_B_OK`. |
| Model tool chain | A calls `external_agent_discover`, selects `@travel`, then calls `external_agent_ask`; the real B response is `DISCOVER_THEN_ASK_OK`. |
| Child cleanup | No `remi-cat` process remains after the call completes. |
| Direct remote-manifest reservation | Asking a direct `endpoint.type: remote` profile fails locally with `REMOTE_AGENT_NOT_IMPLEMENTED`; Profile Hub references use the Weaver-backed A2A proxy path. |

The dotenv or keyring source is process-level. A local child receives the
absolute `REMI_SECRET_DOTENV_PATH` or the shared keyring configuration rather
than looking for credentials under B's profile directory.
