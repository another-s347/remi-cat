//! `deep-agent` — full-featured ratatui TUI for remi-deepagent.
//!
//! Layout:
//! ```
//! ┌ Deep Agent ──────────────────────────────────┬─ Todos ──────────┐
//! │  You: <message>                               │  ☐ 1. Step one   │
//! │  Agent: streaming response ...               │  ☑ 2. Step two   │
//! │  ▶ bash("ls /tmp")                           ├─ Skills ─────────┤
//! │  ◀ total 3                                    │  create-rust-hw  │
//! └──────────────────────────────────────────────┴──────────────────┤
//! │ 🔧  bash("ls") → total 3   fs_write("x.rs") → ok               │
//! ├──────────────────────────────────────────────────────────────────┤
//! │ ❯ _                                                              │
//! │ [Enter] send  [PgUp/Dn] scroll  [Ctrl+C] quit   t:2  tok:1234   │
//! └──────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Usage:
//! ```sh
//! OPENAI_API_KEY=sk-... REMI_MODEL=gpt-4o cargo run -p remi-deepagent --bin deep-agent --features tui
//! ```
//!
//! Env vars:
//!   OPENAI_API_KEY / REMI_API_KEY
//!   REMI_BASE_URL / OPENAI_BASE_URL
//!   REMI_MODEL  (default: gpt-4o)
//!   REMI_SYSTEM (optional system prompt override)

use std::io;
use std::time::Duration;

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{FutureExt, StreamExt};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use remi_agentloop_deepagent::skill::store::FileSkillStore;
use remi_agentloop_deepagent::{
    DeepAgentBuilder, DeepAgentConfig, DeepAgentEvent, SkillEvent, TodoEvent,
};
use remi_core::types::Message as AgentMessage;
use remi_model::OpenAIClient;
use remi_transport::ReqwestTransport;

type AppModel = OpenAIClient<ReqwestTransport>;
use tokio::sync::mpsc;

// ── Message types ─────────────────────────────────────────────────────────────

/// Events produced by the agent task and consumed by the UI loop.
#[derive(Debug)]
enum AgentMsg {
    TextDelta(String),
    ToolCallStart {
        name: String,
    },
    ToolCallArgsDelta(String),
    ToolResult {
        name: String,
        result: String,
    },
    TodoAdded {
        id: u64,
        content: String,
    },
    TodoCompleted {
        id: u64,
    },
    TodoUpdated {
        id: u64,
        content: String,
    },
    TodoRemoved {
        id: u64,
    },
    SkillSaved(String),
    SkillDeleted(String),
    TurnStart(usize),
    Usage {
        prompt: u32,
        completion: u32,
    },
    /// Full conversation history returned at end of each run.
    History(Vec<AgentMessage>),
    Done,
    Error(String),
}

/// Commands sent from UI to the agent task (reserved for future stop/interrupt support).
#[allow(dead_code)]
#[derive(Debug)]
enum UiCmd {
    SendMessage(String),
}

// ── App state ─────────────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum AppMode {
    Idle,
    Running,
    // Done / error — return to Idle after showing message
}

/// A single entry in the chat history area.
enum ChatLine {
    UserMsg(String),
    AgentText(String),          // completed chunk
    ToolStart(String),          // "▶ name("
    ToolArgs(String),           // inline args
    ToolEnd,                    // ")"
    ToolResult(String, String), // (name, result_preview)
    SystemMsg(String),          // turn headers, status msgs
}

struct TodoItem {
    id: u64,
    content: String,
    done: bool,
}

struct ToolLogEntry {
    name: String,
    result: Option<String>,
}

struct App {
    // Chat
    chat: Vec<ChatLine>,
    streaming_text: String,      // current agent stream buffer
    streaming_tool_name: String, // current tool call name
    streaming_tool_args: String, // accumulating args
    chat_scroll: u16,
    auto_scroll: bool,

    // Side panels
    todos: Vec<TodoItem>,
    skills: Vec<String>,

    // Tool log bar (recent tool calls, one-liner format)
    tool_log: Vec<ToolLogEntry>,
    current_tool: Option<usize>, // index in tool_log that's in progress

    // Input
    input: String,
    cursor: usize,

    // Input history (Up/Down to navigate)
    input_history: Vec<String>, // oldest … newest
    history_idx: Option<usize>, // None = not browsing; Some(i) = showing history[i]
    input_draft: String,        // saved current input while browsing history

    // Cancellation
    cancel_tx: Option<tokio::sync::oneshot::Sender<()>>,
    last_ctrl_c: Option<std::time::Instant>,

    // Workspace path (for auto-saving memory)
    workspace_dir: std::path::PathBuf,

    // Conversation history carried between runs
    last_history: Vec<AgentMessage>,

    // Status
    turn: usize,
    model: String,
    prompt_tokens: u32,
    completion_tokens: u32,
    mode: AppMode,
    status_msg: Option<String>, // transient status
}

impl App {
    fn new(model: String, workspace_dir: std::path::PathBuf) -> Self {
        Self {
            chat: vec![ChatLine::SystemMsg(
                "Type a message and press Enter to start.  Ctrl+C to quit.".into(),
            )],
            streaming_text: String::new(),
            streaming_tool_name: String::new(),
            streaming_tool_args: String::new(),
            chat_scroll: 0,
            auto_scroll: true,

            todos: vec![],
            skills: vec![],

            tool_log: vec![],
            current_tool: None,

            input: String::new(),
            cursor: 0,

            input_history: vec![],
            history_idx: None,
            input_draft: String::new(),

            cancel_tx: None,
            last_ctrl_c: None,

            workspace_dir,

            last_history: vec![],

            turn: 0,
            model,
            prompt_tokens: 0,
            completion_tokens: 0,
            mode: AppMode::Idle,
            status_msg: None,
        }
    }

    // ── Input helpers ──────────────────────────────────────────────────────

    fn input_insert(&mut self, c: char) {
        let idx = self
            .input
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len());
        self.input.insert(idx, c);
        self.cursor += 1;
    }

    fn input_backspace(&mut self) {
        if self.cursor > 0 {
            let idx = self
                .input
                .char_indices()
                .nth(self.cursor - 1)
                .map(|(i, _)| i)
                .unwrap();
            self.input.remove(idx);
            self.cursor -= 1;
        }
    }

    fn input_take(&mut self) -> String {
        self.cursor = 0;
        self.history_idx = None;
        self.input_draft.clear();
        let msg = std::mem::take(&mut self.input);
        // Push to history (skip consecutive duplicates)
        if self.input_history.last().map(|s| s.as_str()) != Some(&msg) {
            self.input_history.push(msg.clone());
        }
        msg
    }

    fn history_up(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        match self.history_idx {
            None => {
                // Enter history mode: save draft, show last entry
                self.input_draft = self.input.clone();
                let idx = self.input_history.len() - 1;
                self.history_idx = Some(idx);
                self.input = self.input_history[idx].clone();
                self.cursor = self.input.chars().count();
            }
            Some(0) => { /* already at oldest, do nothing */ }
            Some(i) => {
                let idx = i - 1;
                self.history_idx = Some(idx);
                self.input = self.input_history[idx].clone();
                self.cursor = self.input.chars().count();
            }
        }
    }

    fn history_down(&mut self) {
        match self.history_idx {
            None => { /* not in history mode */ }
            Some(i) if i + 1 >= self.input_history.len() => {
                // Back to draft
                self.history_idx = None;
                self.input = self.input_draft.clone();
                self.cursor = self.input.chars().count();
            }
            Some(i) => {
                let idx = i + 1;
                self.history_idx = Some(idx);
                self.input = self.input_history[idx].clone();
                self.cursor = self.input.chars().count();
            }
        }
    }

    // ── Stream buffer helpers ──────────────────────────────────────────────

    fn flush_streaming_text(&mut self) {
        if !self.streaming_text.is_empty() {
            let t = std::mem::take(&mut self.streaming_text);
            self.chat.push(ChatLine::AgentText(t));
        }
    }

    fn flush_streaming_tool(&mut self) {
        if !self.streaming_tool_name.is_empty() {
            let name = std::mem::take(&mut self.streaming_tool_name);
            let args = std::mem::take(&mut self.streaming_tool_args);
            self.chat.push(ChatLine::ToolStart(name));
            if !args.is_empty() {
                self.chat.push(ChatLine::ToolArgs(args));
            }
            self.chat.push(ChatLine::ToolEnd);
        }
    }

    /// Auto-save the conversation to `<workspace>/memory.md`.
    /// Called after every agent run (Done or Error).
    fn auto_save_memory(&self) {
        let memory_path = self.workspace_dir.join("memory.md");
        let _ = std::fs::create_dir_all(&self.workspace_dir);

        // Build markdown from all chat lines
        let mut md = String::new();
        for cl in &self.chat {
            match cl {
                ChatLine::UserMsg(m) => {
                    md.push_str(&format!("\n## User\n{m}\n"));
                }
                ChatLine::AgentText(t) => {
                    md.push_str(&format!("{t}\n"));
                }
                ChatLine::ToolStart(name) => {
                    md.push_str(&format!("- **tool**: `{name}`("));
                }
                ChatLine::ToolArgs(args) => {
                    let preview = if args.chars().count() > 200 {
                        format!("{}…", args.chars().take(200).collect::<String>())
                    } else {
                        args.clone()
                    };
                    md.push_str(&preview);
                }
                ChatLine::ToolEnd => {
                    md.push_str(")\n");
                }
                ChatLine::ToolResult(name, preview) => {
                    md.push_str(&format!("- **result** `{name}`: {preview}\n"));
                }
                ChatLine::SystemMsg(s) => {
                    // Skip boilerplate system msgs
                    if s.starts_with("──") || s.starts_with("✓") || s.starts_with("✗") {
                        md.push_str(&format!("\n---\n*{s}*\n"));
                    }
                }
            }
        }

        // Truncate to keep the last ~32KB (avoid unbounded growth)
        if md.len() > 32768 {
            let start = md.len() - 32768;
            // Find a clean line break to start from
            if let Some(pos) = md[start..].find('\n') {
                md = format!("(earlier history trimmed)\n\n{}", &md[start + pos + 1..]);
            }
        }

        let _ = std::fs::write(&memory_path, md.as_bytes());
    }

    // ── Handle incoming agent events ───────────────────────────────────────

    fn apply(&mut self, msg: AgentMsg) {
        match msg {
            AgentMsg::TurnStart(n) => {
                self.flush_streaming_text();
                self.flush_streaming_tool();
                self.turn = n;
                self.chat
                    .push(ChatLine::SystemMsg(format!("── turn {n} ──")));
            }
            AgentMsg::TextDelta(t) => {
                self.streaming_text.push_str(&t);
            }
            AgentMsg::ToolCallStart { name } => {
                self.flush_streaming_text();
                self.flush_streaming_tool();
                self.streaming_tool_name = name.clone();
                self.tool_log.push(ToolLogEntry { name, result: None });
                self.current_tool = Some(self.tool_log.len() - 1);
            }
            AgentMsg::ToolCallArgsDelta(d) => {
                self.streaming_tool_args.push_str(&d);
            }
            AgentMsg::ToolResult { name, result } => {
                self.flush_streaming_tool();
                let preview = if result.len() > 80 {
                    let cut = result
                        .char_indices()
                        .map(|(i, _)| i)
                        .take_while(|&i| i <= 80)
                        .last()
                        .unwrap_or(0);
                    format!("{}…", &result[..cut])
                } else {
                    result.clone()
                };
                self.chat
                    .push(ChatLine::ToolResult(name.clone(), preview.clone()));
                if let Some(idx) = self.current_tool {
                    if let Some(e) = self.tool_log.get_mut(idx) {
                        e.result = Some(preview);
                    }
                }
                self.current_tool = None;
            }
            AgentMsg::TodoAdded { id, content } => {
                self.todos.push(TodoItem {
                    id,
                    content,
                    done: false,
                });
            }
            AgentMsg::TodoCompleted { id } => {
                if let Some(t) = self.todos.iter_mut().find(|t| t.id == id) {
                    t.done = true;
                }
            }
            AgentMsg::TodoUpdated { id, content } => {
                if let Some(t) = self.todos.iter_mut().find(|t| t.id == id) {
                    t.content = content;
                }
            }
            AgentMsg::TodoRemoved { id } => {
                self.todos.retain(|t| t.id != id);
            }
            AgentMsg::SkillSaved(name) => {
                if !self.skills.contains(&name) {
                    self.skills.push(name);
                }
            }
            AgentMsg::SkillDeleted(name) => {
                self.skills.retain(|s| *s != name);
            }
            AgentMsg::Usage { prompt, completion } => {
                self.prompt_tokens = prompt;
                self.completion_tokens = completion;
            }
            AgentMsg::History(msgs) => {
                self.last_history = msgs;
            }
            AgentMsg::Done => {
                self.flush_streaming_text();
                self.flush_streaming_tool();
                self.mode = AppMode::Idle;
                self.cancel_tx = None;
                self.auto_save_memory();
                self.chat.push(ChatLine::SystemMsg("✓ Done.".into()));
                self.auto_scroll = true;
            }
            AgentMsg::Error(e) => {
                self.flush_streaming_text();
                self.flush_streaming_tool();
                self.mode = AppMode::Idle;
                self.cancel_tx = None;
                self.auto_save_memory();
                self.chat.push(ChatLine::SystemMsg(format!("✗ Error: {e}")));
            }
        }
        if self.auto_scroll {
            self.chat_scroll = u16::MAX; // clamped during render
        }
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.size();

    // ── Outer vertical split: chat+side | tool_log | input | status ──────
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),    // chat + side panels
            Constraint::Length(3), // tool log
            Constraint::Length(3), // input
            Constraint::Length(1), // status bar
        ])
        .split(area);

    render_main_area(app, frame, outer[0]);
    render_tool_log(app, frame, outer[1]);
    render_input(app, frame, outer[2]);
    render_status_bar(app, frame, outer[3]);
}

fn render_main_area(app: &mut App, frame: &mut Frame, area: Rect) {
    // Horizontal split: chat | todos+skills
    let horiz = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(area);

    render_chat(app, frame, horiz[0]);
    render_side(app, frame, horiz[1]);
}

fn render_chat(app: &mut App, frame: &mut Frame, area: Rect) {
    let inner_w = area.width.saturating_sub(2) as usize;

    // Build lines
    let mut lines: Vec<Line> = vec![];
    for cl in &app.chat {
        match cl {
            ChatLine::SystemMsg(s) => {
                lines.push(Line::from(Span::styled(
                    s.clone(),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            ChatLine::UserMsg(m) => {
                lines.push(Line::from(vec![
                    Span::styled(
                        "You  ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(m.clone()),
                ]));
            }
            ChatLine::AgentText(t) => {
                // word-wrap manually so line count is correct
                for raw_line in t.lines() {
                    if raw_line.is_empty() {
                        lines.push(Line::from(""));
                    } else {
                        lines.push(Line::from(vec![
                            Span::styled("     ", Style::default()),
                            Span::raw(raw_line.to_string()),
                        ]));
                    }
                }
            }
            ChatLine::ToolStart(name) => {
                lines.push(Line::from(vec![
                    Span::styled("  ▶  ", Style::default().fg(Color::Yellow)),
                    Span::styled(
                        name.clone(),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("("),
                ]));
            }
            ChatLine::ToolArgs(args) => {
                // Truncate long args — use char-boundary indexing to avoid
                // panics on multi-byte (e.g. CJK) characters.
                let max_chars = inner_w.saturating_sub(8);
                let preview = {
                    let mut chars = args.chars();
                    let mut s: String = chars.by_ref().take(max_chars).collect();
                    if chars.next().is_some() {
                        s.push('…');
                    }
                    s
                };
                lines.push(Line::from(vec![
                    Span::raw("       "),
                    Span::styled(preview, Style::default().fg(Color::Gray)),
                ]));
            }
            ChatLine::ToolEnd => {
                lines.push(Line::from(vec![Span::raw("       )")]));
            }
            ChatLine::ToolResult(name, preview) => {
                lines.push(Line::from(vec![
                    Span::styled("  ◀  ", Style::default().fg(Color::Green)),
                    Span::styled(name.clone(), Style::default().fg(Color::Green)),
                    Span::raw(": "),
                    Span::styled(preview.clone(), Style::default().fg(Color::DarkGray)),
                ]));
            }
        }
    }

    // Add live streaming text
    if !app.streaming_text.is_empty() {
        for (i, raw_line) in app.streaming_text.lines().enumerate() {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled("     ", Style::default()),
                    Span::raw(raw_line.to_string()),
                ]));
            } else {
                lines.push(Line::from(raw_line.to_string()));
            }
        }
        // blinking cursor indicator
        lines.push(Line::from(Span::styled(
            "▍",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::SLOW_BLINK),
        )));
    } else if !app.streaming_tool_name.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  ▶  ", Style::default().fg(Color::Yellow)),
            Span::styled(
                app.streaming_tool_name.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("("),
            Span::styled(
                app.streaming_tool_args.clone(),
                Style::default().fg(Color::Gray),
            ),
            Span::styled(
                "▍",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::SLOW_BLINK),
            ),
        ]));
    }

    let total_lines = lines.len() as u16;
    let visible = area.height.saturating_sub(2); // border
    let max_scroll = total_lines.saturating_sub(visible);

    // Clamp scroll
    if app.chat_scroll == u16::MAX || app.auto_scroll {
        app.chat_scroll = max_scroll;
    } else {
        app.chat_scroll = app.chat_scroll.min(max_scroll);
    }

    let block = Block::default()
        .title(Span::styled(
            " 💬 Chat ",
            Style::default().add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue));

    let para = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.chat_scroll, 0));

    frame.render_widget(para, area);
}

fn render_side(app: &App, frame: &mut Frame, area: Rect) {
    // Split side into todos (top) + skills (bottom)
    let todo_h = ((app.todos.len() + 4) as u16).min(area.height / 2).max(5);
    let side = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(todo_h), Constraint::Min(4)])
        .split(area);

    render_todos(app, frame, side[0]);
    render_skills(app, frame, side[1]);
}

fn render_todos(app: &App, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = app
        .todos
        .iter()
        .map(|t| {
            let check = if t.done { "☑" } else { "☐" };
            let style = if t.done {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::CROSSED_OUT)
            } else {
                Style::default().fg(Color::White)
            };
            let max_w = area.width.saturating_sub(7) as usize;
            let content = if t.content.chars().count() > max_w {
                let truncated: String = t.content.chars().take(max_w).collect();
                format!("{}…", truncated)
            } else {
                t.content.clone()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{check} "), Style::default().fg(Color::Cyan)),
                Span::styled(content, style),
            ]))
        })
        .collect();

    let done = app.todos.iter().filter(|t| t.done).count();
    let total = app.todos.len();
    let title = if total > 0 {
        format!(" 📋 Todos ({done}/{total}) ")
    } else {
        " 📋 Todos ".to_string()
    };

    let list = List::new(items).block(
        Block::default()
            .title(Span::styled(
                title,
                Style::default().add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Magenta)),
    );
    frame.render_widget(list, area);
}

fn render_skills(app: &App, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = app
        .skills
        .iter()
        .map(|s| {
            let max_w = area.width.saturating_sub(5) as usize;
            let content = if s.chars().count() > max_w {
                let truncated: String = s.chars().take(max_w).collect();
                format!("{}…", truncated)
            } else {
                s.clone()
            };
            ListItem::new(Line::from(vec![
                Span::styled("◆ ", Style::default().fg(Color::Yellow)),
                Span::raw(content),
            ]))
        })
        .collect();

    let title = format!(" 💾 Skills ({}) ", app.skills.len());
    let list = List::new(items).block(
        Block::default()
            .title(Span::styled(
                title,
                Style::default().add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    frame.render_widget(list, area);
}

fn render_tool_log(app: &App, frame: &mut Frame, area: Rect) {
    // Show last N tool calls in a compact horizontal bar
    let inner_w = area.width.saturating_sub(2) as usize;

    let mut spans: Vec<Span> = vec![];
    let recent: Vec<_> = app.tool_log.iter().rev().take(6).collect();
    for (i, entry) in recent.into_iter().rev().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  │  ", Style::default().fg(Color::DarkGray)));
        }
        spans.push(Span::styled(
            entry.name.clone(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        match &entry.result {
            Some(r) => {
                spans.push(Span::raw("(…) → "));
                let preview = if r.chars().count() > 30 {
                    format!("{}…", r.chars().take(30).collect::<String>())
                } else {
                    r.clone()
                };
                spans.push(Span::styled(preview, Style::default().fg(Color::Green)));
            }
            None => {
                spans.push(Span::styled(
                    "(…) ⟳",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::SLOW_BLINK),
                ));
            }
        }
    }

    if spans.is_empty() {
        spans.push(Span::styled(
            "no tool calls yet",
            Style::default().fg(Color::DarkGray),
        ));
    }

    let _ = inner_w; // used for future truncation

    let para = Paragraph::new(Line::from(spans)).block(
        Block::default()
            .title(Span::styled(
                " 🔧 Tools ",
                Style::default().add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(para, area);
}

fn render_input(app: &App, frame: &mut Frame, area: Rect) {
    let inner_w = area.width.saturating_sub(4) as usize;

    // Build content with a cursor indicator
    let display = if app.input.len() > inner_w {
        // scroll right
        let start = app.input.len().saturating_sub(inner_w);
        app.input[start..].to_string()
    } else {
        app.input.clone()
    };

    let (before_cursor, after_cursor) = {
        let chars: Vec<char> = display.chars().collect();
        let cursor_in_display = app
            .cursor
            .saturating_sub(app.input.len().saturating_sub(display.len()));
        let before: String = chars[..cursor_in_display.min(chars.len())].iter().collect();
        let rest: String = chars[cursor_in_display.min(chars.len())..].iter().collect();
        (before, rest)
    };

    let cursor_char = if before_cursor.len() < display.len() || app.input.is_empty() {
        " "
    } else {
        " "
    };

    let running_indicator = if app.mode == AppMode::Running {
        Span::styled(
            " ⟳ running…",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::SLOW_BLINK),
        )
    } else {
        Span::styled(" ready", Style::default().fg(Color::DarkGray))
    };

    let line = Line::from(vec![
        Span::styled(
            "❯ ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(before_cursor),
        Span::styled(
            cursor_char,
            Style::default().bg(Color::White).fg(Color::Black),
        ),
        Span::raw(after_cursor),
        running_indicator,
    ]);

    let border_style = if app.mode == AppMode::Running {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Cyan)
    };

    let para = Paragraph::new(line).block(
        Block::default()
            .title(Span::styled(
                " Input ",
                Style::default().add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(border_style),
    );
    frame.render_widget(para, area);
}

fn render_status_bar(app: &App, frame: &mut Frame, area: Rect) {
    let model_short = if app.model.chars().count() > 20 {
        let truncated: String = app.model.chars().take(20).collect();
        format!("{}…", truncated)
    } else {
        app.model.clone()
    };

    let msg = if let Some(s) = &app.status_msg {
        s.clone()
    } else {
        format!(
            " model:{model_short}  turn:{}  tokens:{}/{}  [Enter] send  [↑↓] history  [PgUp/Dn] scroll  [Ctrl+C] cancel  [Ctrl+Q] quit",
            app.turn, app.prompt_tokens, app.completion_tokens
        )
    };

    let para = Paragraph::new(Span::styled(msg, Style::default().fg(Color::DarkGray)));
    frame.render_widget(para, area);
}

// ── Agent runner ──────────────────────────────────────────────────────────────

async fn run_agent_task(
    message: String,
    history: Vec<AgentMessage>,
    tx: mpsc::UnboundedSender<AgentMsg>,
    model: AppModel,
    cfg: DeepAgentConfig,
) {
    let agent = DeepAgentBuilder::new(model);
    let agent = cfg.apply_to_builder(agent).build();

    // Scope the stream so its borrow of `agent` is released before flush.
    {
        let stream = match agent.chat_with_history(&message, history).await {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.send(AgentMsg::Error(e.to_string()));
                return;
            }
        };
        let mut stream = std::pin::pin!(stream);

        while let Some(ev) = stream.next().await {
            let msg = match ev {
                DeepAgentEvent::Agent(ae) => match ae {
                    remi_core::types::AgentEvent::TurnStart { turn } => {
                        Some(AgentMsg::TurnStart(turn))
                    }
                    remi_core::types::AgentEvent::TextDelta(t) => Some(AgentMsg::TextDelta(t)),
                    remi_core::types::AgentEvent::ToolCallStart { name, .. } => {
                        Some(AgentMsg::ToolCallStart { name })
                    }
                    remi_core::types::AgentEvent::ToolCallArgumentsDelta { delta, .. } => {
                        Some(AgentMsg::ToolCallArgsDelta(delta))
                    }
                    remi_core::types::AgentEvent::ToolResult { name, result, .. } => {
                        Some(AgentMsg::ToolResult { name, result })
                    }
                    remi_core::types::AgentEvent::Usage {
                        prompt_tokens,
                        completion_tokens,
                    } => Some(AgentMsg::Usage {
                        prompt: prompt_tokens,
                        completion: completion_tokens,
                    }),
                    remi_core::types::AgentEvent::Done => Some(AgentMsg::Done),
                    remi_core::types::AgentEvent::Error(e) => Some(AgentMsg::Error(e.to_string())),
                    _ => None,
                },
                DeepAgentEvent::Todo(te) => match te {
                    TodoEvent::Added { id, content } => Some(AgentMsg::TodoAdded { id, content }),
                    TodoEvent::Completed { id } => Some(AgentMsg::TodoCompleted { id }),
                    TodoEvent::Updated { id, content } => {
                        Some(AgentMsg::TodoUpdated { id, content })
                    }
                    TodoEvent::Removed { id } => Some(AgentMsg::TodoRemoved { id }),
                },
                DeepAgentEvent::Skill(se) => match se {
                    SkillEvent::Saved { name, .. } => Some(AgentMsg::SkillSaved(name)),
                    SkillEvent::Deleted { name } => Some(AgentMsg::SkillDeleted(name)),
                },
                DeepAgentEvent::History(msgs) => Some(AgentMsg::History(msgs)),
            };
            if let Some(m) = msg {
                if tx.send(m).is_err() {
                    return; // UI closed — skip flush too
                }
            }
        }
    } // stream dropped here; borrow on `agent` ends

    // Flush pending tracing I/O (LangSmith HTTP PATCHes) before the
    // single-threaded runtime shuts down and kills background tasks.
    agent.flush_tracer().await;
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── Handle --init ─────────────────────────────────────────────────────
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args.iter().any(|a| a == "--init") {
        let force = raw_args.iter().any(|a| a == "--force");
        DeepAgentConfig::write_example(force)?;
        // Also write SOUL.md template into the workspace dir (uses default if
        // no config exists yet; skips without error if already present).
        let workspace_dir = DeepAgentConfig::load()
            .map(|cfg| cfg.agent.workspace_dir.clone())
            .unwrap_or_else(|_| std::path::PathBuf::from(".deepagent/workspace"));
        DeepAgentConfig::write_soul_template(&workspace_dir, force)?;
        println!("Edit deep-agent.toml then run without --init.");
        return Ok(());
    }

    // ── Load config ───────────────────────────────────────────────────────
    let cfg = DeepAgentConfig::load().map_err(|e| format!("Config error: {e}"))?;
    cfg.require_api_key()
        .map_err(|e| {
            eprintln!("{e}");
            std::process::exit(1);
        })
        .ok();

    // ── Model ─────────────────────────────────────────────────────────────
    let model_name = cfg.model.model.clone();
    let mut oai: AppModel = OpenAIClient::new(cfg.model.api_key.clone()).with_model(&model_name);
    if let Some(url) = &cfg.model.base_url {
        oai = oai.with_base_url(url.clone());
    }
    if let Some(policy) = &cfg.model.rate_limit_retry {
        oai = oai.with_rate_limit_retry(policy.clone());
    }

    // ── Terminal setup ─────────────────────────────────────────────────────
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // ── App state ──────────────────────────────────────────────────────────
    let mut app = App::new(model_name.clone(), cfg.agent.workspace_dir.clone());

    // Pre-populate skill list from the store so the panel shows existing
    // skills immediately on startup (not just after a save during this run).
    // Also run migration from the old flat-file path so skills are visible
    // on the very first launch after upgrading.
    {
        let skills_dir = cfg.agent.workspace_dir.join(".claude").join("skills");
        let _ = std::fs::create_dir_all(&skills_dir);
        // Migrate legacy flat skills (workspace/skills/<name>.md) if present
        let legacy_dir = cfg.agent.workspace_dir.join("skills");
        if legacy_dir.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&legacy_dir) {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("md") {
                        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                            let dest_dir = skills_dir.join(stem);
                            let dest = dest_dir.join("SKILL.md");
                            if !dest.exists() {
                                let _ = std::fs::create_dir_all(&dest_dir);
                                if let Ok(content) = std::fs::read_to_string(&p) {
                                    if std::fs::write(&dest, &content).is_ok() {
                                        let _ = std::fs::remove_file(&p);
                                    }
                                }
                            }
                        }
                    }
                }
                let _ = std::fs::remove_dir(&legacy_dir);
            }
        }
        let store = FileSkillStore::new(&skills_dir);
        app.skills = store
            .list_with_descriptions_sync()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
    }

    // ── Channels ───────────────────────────────────────────────────────────
    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<AgentMsg>();

    // ── Pre-fill from CLI args ─────────────────────────────────────────────
    // (Skip --config / --init flags already consumed above)
    let initial_task: String = raw_args
        .iter()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    if !initial_task.is_empty() {
        app.chat.push(ChatLine::UserMsg(initial_task.clone()));
        app.mode = AppMode::Running;
        let tx2 = agent_tx.clone();
        let oai2 = oai.clone();
        let cfg2 = cfg.clone();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        app.cancel_tx = Some(cancel_tx);
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let tx_cancel = tx2.clone();
                tokio::select! {
                    result = cancel_rx => {
                        // Only treat an explicit send(()) as a cancellation;
                        // a dropped sender (Err) means the task already
                        // finished and the UI cleaned up—don't send spurious
                        // "Cancelled." after every normal turn.
                        if result.is_ok() {
                            let _ = tx_cancel.send(AgentMsg::Error("Cancelled.".to_string()));
                        }
                    }
                    _ = run_agent_task(initial_task, vec![], tx2, oai2, cfg2) => {}
                }
            });
        });
    }

    // ── Event stream ───────────────────────────────────────────────────────
    let mut event_stream = EventStream::new();
    let tick = tokio::time::interval(Duration::from_millis(100));
    tokio::pin!(tick);

    // ── UI loop ────────────────────────────────────────────────────────────
    loop {
        terminal.draw(|frame| render(&mut app, frame))?;

        tokio::select! {
            // Crossterm keyboard/resize events
            maybe_event = event_stream.next().fuse() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        // Clear transient status on any key (re-set below if needed)
                        app.status_msg = None;
                        match (key.modifiers, key.code) {
                            // Ctrl+C: cancel running task; double-press to quit
                            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                                let now = std::time::Instant::now();
                                let double_press = app.last_ctrl_c
                                    .map(|t| now.duration_since(t).as_millis() < 500)
                                    .unwrap_or(false);
                                if double_press {
                                    break;
                                }
                                app.last_ctrl_c = Some(now);
                                if app.mode == AppMode::Running {
                                    if let Some(tx) = app.cancel_tx.take() {
                                        let _ = tx.send(());
                                    }
                                    // AgentMsg::Error("Cancelled.") will arrive and reset mode
                                    app.chat.push(ChatLine::SystemMsg(
                                        "⚠ Cancelling… (Ctrl+C again to force quit)".into()
                                    ));
                                } else {
                                    app.status_msg = Some(
                                        "Press Ctrl+C again to quit".to_string()
                                    );
                                }
                            }
                            // Ctrl+Q always quits immediately
                            (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
                                break;
                            }
                            // Send message on Enter (when Idle)
                            (KeyModifiers::NONE, KeyCode::Enter) => {
                                if app.mode == AppMode::Idle && !app.input.is_empty() {
                                    let msg = app.input_take();
                                    app.chat.push(ChatLine::UserMsg(msg.clone()));
                                    app.mode = AppMode::Running;
                                    app.auto_scroll = true;
                                    let tx2 = agent_tx.clone();
                                    let oai2 = oai.clone();
                                    let cfg2 = cfg.clone();
                                    let history2 = app.last_history.clone();
                                    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
                                    app.cancel_tx = Some(cancel_tx);
                                    std::thread::spawn(move || {
                                        let rt = tokio::runtime::Builder::new_current_thread()
                                            .enable_all()
                                            .build()
                                            .unwrap();
                                        rt.block_on(async {
                                            let tx_cancel = tx2.clone();
                                            tokio::select! {
                                                result = cancel_rx => {
                                                    if result.is_ok() {
                                                        let _ = tx_cancel.send(AgentMsg::Error("Cancelled.".to_string()));
                                                    }
                                                }
                                                _ = run_agent_task(msg, history2, tx2, oai2, cfg2) => {}
                                            }
                                        });
                                    });
                                }
                            }
                            // Backspace
                            (KeyModifiers::NONE, KeyCode::Backspace) => {
                                app.input_backspace();
                            }
                            // Scroll up in chat
                            (KeyModifiers::NONE, KeyCode::PageUp) => {
                                app.auto_scroll = false;
                                app.chat_scroll = app.chat_scroll.saturating_sub(5);
                            }
                            // Scroll down in chat
                            (KeyModifiers::NONE, KeyCode::PageDown) => {
                                app.chat_scroll = app.chat_scroll.saturating_add(5);
                                // auto_scroll re-enables when we hit bottom (clamped in render)
                            }
                            // Arrow keys: Left/Right move cursor; Up/Down browse history
                            (KeyModifiers::NONE, KeyCode::Left) => {
                                app.cursor = app.cursor.saturating_sub(1);
                            }
                            (KeyModifiers::NONE, KeyCode::Right) => {
                                let max = app.input.chars().count();
                                if app.cursor < max { app.cursor += 1; }
                            }
                            (KeyModifiers::NONE, KeyCode::Up) => {
                                app.history_up();
                            }
                            (KeyModifiers::NONE, KeyCode::Down) => {
                                app.history_down();
                            }
                            (KeyModifiers::NONE, KeyCode::Home) => {
                                app.cursor = 0;
                            }
                            (KeyModifiers::NONE, KeyCode::End) => {
                                app.cursor = app.input.chars().count();
                            }
                            // Delete forward
                            (KeyModifiers::NONE, KeyCode::Delete) => {
                                let max = app.input.chars().count();
                                if app.cursor < max {
                                    let idx = app.input.char_indices().nth(app.cursor).map(|(i,_)|i).unwrap();
                                    app.input.remove(idx);
                                }
                            }
                            // Clear input with Ctrl+U
                            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                                app.input.clear();
                                app.cursor = 0;
                            }
                            // Ctrl+End → scroll to bottom
                            (KeyModifiers::CONTROL, KeyCode::End) => {
                                app.auto_scroll = true;
                                app.chat_scroll = u16::MAX;
                            }
                            // Regular character input
                            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                                app.input_insert(c);
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => {
                        // terminal.autoresize() handles this; just redraw
                    }
                    Some(Err(e)) => {
                        app.status_msg = Some(format!("Terminal error: {e}"));
                    }
                    None => break,
                    _ => {}
                }
            }

            // Agent events
            maybe_msg = agent_rx.recv() => {
                if let Some(msg) = maybe_msg {
                    app.apply(msg);
                }
            }

            // Tick (drives blinking cursor / keep-alive redraw)
            _ = tick.tick() => {}
        }
    }

    // ── Cleanup ────────────────────────────────────────────────────────────
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
