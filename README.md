# remi-cat

remi-cat is a lightweight, pure-Rust agent runtime for running AI agents across terminal, web, IM, and ACP interfaces.

## Highlight Features

- **Asynchronous agent interaction:** Keep working while long-running tools and subagents run in the background, with live progress, steering, and cancellation.
- **Graph-based supervisor workflows:** Drive multi-step work through configurable workflow graphs in which a supervisor evaluates each round and directs the next agent action.

## Features

- Markdown-defined agents
- Skills
- Memory
- No MCP dependency
- Built-in SSH tool
- Subagents
- Terminal UI
- IM channels
- ACP
- Zellij and tmux split-pane support
- Lightweight, pure-Rust runtime

## Setup

Run the interactive setup wizard:

```bash
cargo run -- setup
```

To configure Feishu/Lark after setup:

```bash
cargo run -- feishu init
```

To connect the Codex tool through the official ACP adapter and Codex app-server:

```bash
npm install -g @agentclientprotocol/codex-acp@2.0.0
remi-cat codex setup
remi-cat codex doctor
```

`codex setup --bin` selects a different ACP executable, and repeated `--arg` values pass startup arguments to it. Existing profiles keep their configured `acp.local_bin` until setup is run again.

Automatic Feishu/Lark channel creation for ACP and agent sub-sessions
is disabled by default. To enable it, set this in the active profile's
`runtime.yaml` and restart the runtime:

```yaml
im:
  auto_open_sub_session_channel: true
```

When disabled (or omitted), sub-session history is still saved, and existing
channel bindings continue to work. `/fork` creates its new channel independently
of this setting.
Created sub-session channels use the main conversation's native Feishu COT
renderer for progress and tools, and send the final reply separately. Fork
history is replayed through that same renderer, one turn at a time. Panels replay persisted
sub-session events, including tool activity, without relying on a parent TUI.

You can also enable it with
`remi-cat config set im.auto_open_sub_session_channel=true`.

## Run

Start the terminal UI (async background-tool handling is enabled by default):

```bash
cargo run -- tui
```

Use synchronous tool handling instead:

```bash
cargo run -- tui --sync
```

Run the local Qwen service at `http://127.0.0.1:8080/v1`:

```bash
./scripts/local-qwen.sh
./scripts/local-qwen.sh prompt --channel local-qwen "你好"
```

This selects `.remi-cat/models/local-qwen.yaml` with a 65,536-token context
through `profiles/local-qwen/profile.yaml` and its runtime configuration,
and uses the unauthenticated service's `local` API key for this process. Build with
`cargo build` first, or set `REMI_LOCAL_BINARY` to an existing executable.
When running in WSL with a Windows-only service, the launcher starts a
loopback-only forwarding process using Windows `curl.exe`, preserves streaming,
and stops that process on exit. Loopback requests bypass environment HTTP proxies.
The launcher supplies `local` through a temporary credential file and removes it
on exit. Windows must keep the model service running.

Start the configured IM runtime:

```bash
cargo run --release
```

Send Markdown as an assistant message to an existing Feishu-bound Remi session
without starting a model turn:

```bash
remi-cat message send --session <session-id> --text '**Update:** done' --idempotency-key job-42 --json
printf '# Update\nDone.\n' | remi-cat message send --session <session-id> --stdin
remi-cat message status --session <session-id> --idempotency-key job-42 --json
```

Use the same idempotency key to explicitly retry an incomplete delivery. The
`--session` value is a Remi session ID, not a Feishu chat ID or CLI channel ID.
An uncertain send older than Feishu's one-hour UUID deduplication window must
be checked manually; it is not resent automatically.
Topic sessions need a reply anchor recorded from an incoming topic message;
older sessions without one fail safely until another topic message arrives.

## License

MIT. See [LICENSE](LICENSE).
