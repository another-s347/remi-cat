# remi-cat

`remi-cat` is a single-process AI agent runtime for Feishu/Lark and local CLI
debugging. The runtime now ships as one executable: IM ingress, session routing,
agent execution, ACP support, and local tooling all run on the host.

## Quick Start

```bash
cargo run -- setup
```

After setup, for a one-shot local message:

```bash
cargo run -- --local --cli-im-once "Hello"
cargo run -- cli --channel support --user alice --name Alice "Hello from CLI"
```

The CLI entrypoint creates or resumes a top-level session keyed by
`platform=cli` and the selected channel id. Use different `--channel` values to
start separate conversations from the terminal.

To run only the management API:

```bash
cargo run -- admin
```

`remi-cat setup` writes `REMI_DATA_DIR/runtime.yaml`, seeds embedded model and
agent profiles, and verifies one real local chat round. For Feishu, run:

```bash
cargo run -- feishu init
```

To inspect Feishu CLI readiness later:

```bash
cargo run -- feishu doctor
```

After `feishu init` imports `FEISHU_*`, start the gateway with:

```bash
cargo run --release
```

By default Feishu inbound events use the Feishu WebSocket long connection. To
use Feishu Event Hook callbacks instead, run `cargo run -- setup` and choose
`event_hook` for `Feishu inbound transport`, then expose the configured local
URL, for example `http://127.0.0.1:8788/feishu/events`, through your deployment
or reverse proxy and enter that callback URL in the Feishu app event
subscription settings.

## Architecture

| Area | Description |
|---|---|
| `remi-cat` | The only delivered executable. Runs IM ingress and agent runtime in one host process. |
| `bot-core` | Agent loop integration, memory, tools, ACP, agent profiles, and sub-session event forwarding. |
| `.remi-cat/sessions.json` | Channel-to-session bindings and session metadata. |
| `.remi-cat/agents/*.md` | Markdown agent profiles with YAML frontmatter. |
| `.remi-cat/models/*.yaml` | File-backed model profiles for provider/model/capability settings. |
| `.remi-cat/runtime.yaml` | Primary runtime config written by `remi-cat setup`. |

The old daemon/agent split and Docker agent sandbox are no longer the default
runtime architecture. Shell execution is host-local only when explicitly enabled
with `REMI_SHELL_MODE=local` and the active agent profile allowlists `bash`.

## Agent Profiles

Each agent is configured by one Markdown file:

```markdown
---
id: default
name: Remi
description: General assistant
models:
  primary: default
  helper: deepseek-v4-flash
  vision: gpt-4o
tools:
  - skill__search
  - skill__get
  - skill__read_resource
  - todo__add
  - todo__list
  - memory__get_detail
  - acp__chat
  - agent__list
  - agent__upsert
delegates: []
max_turns: null
---
You are Remi, a helpful assistant.

When the user asks you to remember, store, keep, or use a long note,
transcript, document excerpt, or historical conversation as future context, use
`memory__upsert_named` to save a concise but complete named memory before
acknowledging it.

Use the unified `search` tool for discovery and recall. If the user asks about
their own prior facts, preferences, past actions, earlier conversations, saved
details, or anything phrased like "my ...", call `search` with `scope="memory"`
and distinctive keywords from the question before giving the final answer.
```

`tools` is an explicit allowlist. Tools not listed are not advertised and cannot
be executed by that agent. `models.primary` selects that agent's main model
profile, while `helper` and `vision` are reserved for future tool/model routing.
Profiles can be created or replaced with `agent__upsert`, or edited directly
under `.remi-cat/agents`.

## Model Profiles

Each model is configured by one YAML file under `.remi-cat/models`:

```yaml
id: deepseek-v4-flash
name: DeepSeek V4 Flash
provider: deepseek
model: deepseek-v4-flash
base_url: https://api.deepseek.com
max_output_tokens: 393216
context_tokens: 1000000
supports_images: false
short_term_tokens: 16000
overflow_bytes: 24000
auto_compress: true
```

The built-in example profiles are embedded into the `remi-cat` binary and are
seeded into `REMI_DATA_DIR/models` on first start. Set
`REMI_MODEL_PROFILE=<id>` to override the runtime default without changing code.
`base_url`, request `model`, image capability, and memory compression budgets
all come from that file. If `supports_images` is `false`, image or document
inputs fail fast with a clear error instead of being silently dropped.

`auto_compress` controls whether short-term memory is compacted automatically.
When it is `false`, turns are still saved, but only explicit compaction paths
will summarize older history.

## ACP And Sub-Sessions

Top-level IM sessions enter the active root agent. ACP sessions and sub-agent
work are both created only through tools and are surfaced as `SubSession`
events. The session store records them under the same parent model, using
`kind: Acp` for ACP turns and `kind: Agent` for delegated agent turns. On
Feishu, a `SubSession(Start)` event automatically creates a private group chat
for that sub-session and stores the returned Feishu `chat_id` as the
sub-session channel binding. Parent agent context receives the final tool
result; intermediate child output stays observable as sub-session progress.

## Admin API

The host runtime exposes a lightweight HTTP admin API by default on
`127.0.0.1:8787`:

| Endpoint | Description |
|---|---|
| `GET /api/agents` | List loaded Markdown agent profiles. |
| `GET /api/agents/{id}` | Inspect one agent profile. |
| `PUT /api/agents/{id}` | Create or replace an agent profile with `{ "markdown": "..." }`. |
| `DELETE /api/agents/{id}` | Delete an agent profile file. |
| `GET /api/sessions` | List top-level sessions and their sub-sessions. |
| `GET /api/sessions/{id}` | Inspect one session. |

## Building

```bash
cargo build --release
```

The release artifact is `target/release/remi-cat`.
