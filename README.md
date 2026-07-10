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

## Runtime Profiles

Named profiles are independent runtime instances stored under
`.remi-cat/profiles/<name>`. Each profile has its own runtime config, agents,
models, sessions, memory, sandbox, and listening ports:

```bash
cargo run -- --profile dev setup
cargo run -- --profile prod setup

cargo run -- --profile dev
cargo run -- --profile prod
```

`REMI_PROFILE=dev` is equivalent to `--profile dev`. An explicitly exported
`REMI_DATA_DIR` takes precedence for compatibility with custom data paths.
The existing `.remi-cat/runtime.yaml` remains the un-named default profile for
normal CLI runs. `remi-cat acp agent` is meant to be launched by external ACP
clients, so without `REMI_DATA_DIR` it uses the HOME config root
(`$HOME/.remi_cat`, or `$HOME/.remi-cat` for compatibility).

Manage named profiles with:

```bash
cargo run -- profile list
cargo run -- profile create dev admin.port=8789 sandbox.kind=no_sandbox
cargo run -- profile show dev
cargo run -- profile start dev
cargo run -- profile status dev
cargo run -- profile restart dev
cargo run -- profile stop dev
cargo run -- profile delete dev --force
```

`remi_diagnostics` is a built-in self-diagnostic profile and agent. It uses the
current default model profile, starts with Feishu disabled, and runs with
`sandbox.kind=no_sandbox`, `shell.mode=local`, and `sandbox.host_dir=.` so it
can inspect Remi logs, session history, runtime config, profiles, agents, and
source files:

```bash
cargo run -- profile show remi_diagnostics
cargo run -- profile start remi_diagnostics
```

Configure a profile's agents and supervisor workflows from the CLI:

```bash
cargo run -- profile agent list dev
cargo run -- profile agent upsert dev ./agents/coder.md
cargo run -- profile agent set-default dev coder

cargo run -- profile workflow list dev
cargo run -- profile workflow upsert dev ./workflows/verify.json
cargo run -- profile workflow show dev verify
cargo run -- profile workflow delete dev verify
```

`agent upsert` accepts markdown agent profiles with YAML frontmatter.
`workflow upsert` accepts supervisor workflow JSON and validates it before
writing `<profile-data-dir>/workflows/<id>.json`.

Runtime config can also be updated non-interactively, which is useful for
scripts and local automation:

```bash
cargo run -- --profile dev setup --non-interactive admin.port=8789 sandbox.kind=no_sandbox
cargo run -- --profile dev config set admin.port=8790 feishu.transport=event_hook feishu.hook.port=8791
cargo run -- --profile dev sandbox set kind=docker host_dir=.remi-cat/profiles/dev container_name=remi-cat-sandbox-dev
```

These CLI updates write `runtime.yaml` directly and do not run the setup smoke
chat. Use interactive `setup` when you want credential prompting and model
verification.

During setup, an occupied Admin or Event Hook port is automatically advanced
to the next available port and the selected value is saved in `runtime.yaml`.
Normal startup never changes a saved port. Docker profiles also receive a
profile-specific container name, with a numeric suffix added on conflicts.

Background profile processes write metadata to
`<profile-data-dir>/run/remi-cat.pid.json` and append logs to
`<profile-data-dir>/logs/remi-cat.log`. `profile stop` sends SIGTERM and waits;
use `--force` to send SIGKILL when a process does not exit.

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
| `.remi-cat/workflows/*.json` | Supervisor workflow graph definitions. |
| `.remi-cat/runtime.yaml` | Primary runtime config written by `remi-cat setup`. |

The old daemon/agent split has been retired; `remi-cat` is the supported
runtime boundary. Shell execution is host-local only when explicitly enabled
with `REMI_SHELL_MODE=local` and the active agent profile allowlists `bash`.

## Web Chat

Normal `remi-cat` startup serves the embedded Web Chat UI at
`http://127.0.0.1:8787`. With Feishu credentials configured, Web Chat and
Feishu run together; without them Remi starts in Web-only mode. Override the
bind address with `REMI_ADMIN_HOST` and `REMI_ADMIN_PORT`.

Web sessions support creation, history restore, automatic titles, rename,
cancel, and deletion. The UI uses assistant-ui primitives through a Remi-owned
external-store adapter, while the versioned NDJSON API remains framework
independent.

The release binary embeds `web-ui/dist`. Rebuild it after frontend changes:

```bash
cd web-ui
npm ci
npm run build
```

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
  - codex
  - manage_yourself
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
Profiles can be created or replaced with `manage_yourself` using
`profile agent upsert`, or edited directly under `.remi-cat/agents`.
`manage_yourself` accepts only a top-level `command` string, for example:
`{"command":"profile list"}`.

## Supervisor Workflows

Place workflow definitions in `.remi-cat/workflows/<id>.json`, then start one
for the current IM session with:

```text
/workflow start <id> [--max-rounds N|unlimited] [--context {"key":"value"}]
/workflow status
/workflow stop
```

A definition is a versioned directed graph. Every non-terminal node and edge
has a prompt. After each main-agent round, a fresh supervisor session receives
the incoming edge prompt, current node prompt, optional previous-node message,
workflow context, allowed outgoing edges, and the complete main-agent history.
It selects an edge and may send instructions back to the main agent.

```json
{
  "version": 1,
  "id": "verify",
  "name": "Verification workflow",
  "description": "Implement, verify, then stop.",
  "prompt": "Require evidence before completion.",
  "start_prompt": "Review the requested work.",
  "initial_node": "review",
  "terminal_node": "stop",
  "nodes": [
    { "id": "review", "prompt": "Find remaining implementation work." },
    { "id": "verify", "prompt": "Check tests and evidence." },
    { "id": "stop", "prompt": "The workflow is complete." }
  ],
  "edges": [
    { "id": "implement", "from": "review", "to": "verify", "prompt": "Request the missing implementation." },
    { "id": "revise", "from": "verify", "to": "review", "prompt": "Return when verification finds a defect." },
    { "id": "complete", "from": "verify", "to": "stop", "prompt": "Choose only with sufficient evidence." }
  ]
}
```

`/goal set ...` remains available and uses the built-in two-node goal workflow.

When the main agent asks the user for help via `ask_user_question`, the active
workflow automatically moves through the built-in control node
`ask_user_for_help`. The workflow is paused while waiting for the answer, then
resumes at the node it came from. This node is managed by the runtime and does
not need to be declared in workflow JSON.

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
overflow_bytes: 24000
auto_compress: true
```

The built-in example profiles are embedded into the `remi-cat` binary and are
seeded into `REMI_DATA_DIR/models` on first start. Set
`REMI_MODEL_PROFILE=<id>` to override the runtime default without changing code.
`base_url`, request `model`, image capability, and memory compression budgets
all come from that file. If `supports_images` is `false`, image or document
inputs fail fast with a clear error instead of being silently dropped.

Additional OpenAI-compatible built-in profiles include:

| Profile | Provider | API key env |
| --- | --- | --- |
| `mimo-v2.5-pro`, `mimo-v2.5` | Xiaomi MiMo | `MIMO_API_KEY` |
| `kimi-k2.7-code`, `kimi-k2.6` | Moonshot Kimi | `MOONSHOT_API_KEY` or `KIMI_API_KEY` |
| `glm-5.2`, `glm-5v-turbo` | Z.ai GLM | `GLM_API_KEY`, `ZHIPU_API_KEY`, or `BIGMODEL_API_KEY` |

All provider-specific keys fall back to `OPENAI_API_KEY` and then
`REMI_API_KEY`, so existing OpenAI-compatible deployments continue to work.

`auto_compress` controls whether short-term memory is compacted automatically.
When it is `false`, turns are still saved, but only explicit compaction paths
will summarize older history.

## Hooks

Remi hooks are Remi-owned by default and use Codex-compatible hook definitions.
The runtime discovers hooks from `REMI_DATA_DIR/hooks.json`,
`REMI_DATA_DIR/hooks/config.toml`, `.remi-cat/hooks.json`, and
`.remi-cat/hooks.toml` under the workspace root. Codex hook files are not read
unless explicitly imported with `REMI_IMPORT_CODEX_HOOKS=1` or
`import_codex_hooks = true` in a Remi hooks TOML config.

Hook trust and disabled state live under `REMI_DATA_DIR/hooks/`; manage them
with `remi-cat hooks` or the in-session `/hooks` command.

## ACP And Sub-Sessions

Top-level IM sessions enter the active root agent. ACP sessions and sub-agent
work are both created only through tools and are surfaced as `SubSession`
events. The session store records them under the same parent model, using
`kind: Acp` for ACP turns and `kind: Agent` for delegated agent turns. On
Feishu, a `SubSession(Start)` event automatically creates a private group chat
for that sub-session and stores the returned Feishu `chat_id` as the
sub-session channel binding. Parent agent context receives the final tool
result; intermediate child output stays observable as sub-session progress.
Codex is supported through the bundled standard ACP stdio adapter. Run
`remi-cat codex setup` or `remi-cat acp setup --client codex`; setup writes a
local profile that starts `remi-cat acp-adapter codex` via `acp.local_bin` and
`acp.local_args`. Use `--bin /path/to/codex` to choose the Codex CLI binary and
repeat `--arg` for Codex global startup arguments; those values are stored as
adapter argv, and the backend still talks only ACP. The model sees the `codex`
tool when the configured adapter command is executable.

Remi can also run as a standard ACP stdio agent with `remi-cat acp agent`; by
default this reads the HOME config root, while `REMI_DATA_DIR` can still point it
at a custom runtime directory.
Configure another Remi profile to launch it as an external local ACP process
with `remi-cat acp setup --client remi --bin /path/to/remi-cat --tool-name
acp__remi`; when `--bin` is provided for `--client remi` and no `--arg` is
specified, setup stores `acp.local_args: ["acp", "agent"]` automatically. A
Remi ACP configuration without `acp.local_bin` continues to use the in-process
local runner; setting `acp.local_bin` switches it to external stdio.

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
