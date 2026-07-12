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

## Run

Start the terminal UI (async background-tool handling is enabled by default):

```bash
cargo run -- tui
```

Use synchronous tool handling instead:

```bash
cargo run -- tui --sync
```

Start the default Web Chat and IM runtime:

```bash
cargo run --release
```

## License

MIT. See [LICENSE](LICENSE).
