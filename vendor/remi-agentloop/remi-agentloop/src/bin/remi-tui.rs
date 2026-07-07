/// Remi TUI — Claude Code-style streaming terminal interface
///
/// Usage:
///   OPENAI_API_KEY=sk-... remi-tui
///
/// Env vars:
///   OPENAI_API_KEY  or  REMI_API_KEY    — API key (required)
///   REMI_BASE_URL   or  OPENAI_BASE_URL — custom base URL
///   REMI_MODEL                          — model name (default: gpt-4o)
///   REMI_SYSTEM                         — system prompt
///   REMI_MAX_TURNS                      — max turns (default: 20)
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame, Terminal,
};
use remi_agentloop::prelude::*;

// ── Display types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum InputMode {
    Normal,
    Running,
    Interrupt,
}

#[derive(Debug, Clone)]
struct DisplayMessage {
    role: DisplayRole,
    content: String,
    /// Accumulated reasoning/thinking content (collapsed in display).
    thinking: String,
    tool_calls: Vec<ToolCallState>,
}

#[derive(Debug, Clone)]
enum DisplayRole {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
struct ToolCallState {
    id: String,
    name: String,
    args_buf: String,
    output: String,
    status: ToolStatus,
    started: Instant,
    elapsed: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
enum ToolStatus {
    Streaming,
    Running,
    Done,
    Interrupted,
    Error,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct PendingInterrupt {
    interrupts: Vec<InterruptInfo>,
    completed: Vec<ToolCallResult>,
}

// ── App state ─────────────────────────────────────────────────────────────────

struct AppState {
    messages: Vec<DisplayMessage>,
    streaming_content: String,
    /// Thinking content from the current turn (set at ThinkingEnd).
    streaming_thinking: String,
    /// True while a ThinkingStart has been received but ThinkingEnd has not yet arrived.
    is_thinking: bool,
    active_tools: Vec<ToolCallState>,
    pending_interrupt: Option<PendingInterrupt>,
    input: String,
    cursor: usize,
    mode: InputMode,
    scroll: u16,
    prompt_tokens: u32,
    completion_tokens: u32,
    model_name: String,
    should_quit: bool,
    status: String,
    command_history: Vec<String>,
    history_pos: Option<usize>,
    spinner_frame: usize,
    last_tick: Instant,
}

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl AppState {
    fn new(model_name: String) -> Self {
        Self {
            messages: Vec::new(),
            streaming_content: String::new(),
            streaming_thinking: String::new(),
            is_thinking: false,
            active_tools: Vec::new(),
            pending_interrupt: None,
            input: String::new(),
            cursor: 0,
            mode: InputMode::Normal,
            scroll: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            model_name,
            should_quit: false,
            status: String::new(),
            command_history: Vec::new(),
            history_pos: None,
            spinner_frame: 0,
            last_tick: Instant::now(),
        }
    }

    fn tick(&mut self) {
        if self.last_tick.elapsed() >= Duration::from_millis(100) {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER.len();
            self.last_tick = Instant::now();
        }
    }

    fn finalize_assistant_turn(&mut self) {
        if self.streaming_content.is_empty() && self.active_tools.is_empty() {
            self.streaming_thinking.clear();
            self.is_thinking = false;
            return;
        }
        self.is_thinking = false;
        self.messages.push(DisplayMessage {
            role: DisplayRole::Assistant,
            content: std::mem::take(&mut self.streaming_content),
            thinking: std::mem::take(&mut self.streaming_thinking),
            tool_calls: std::mem::take(&mut self.active_tools),
        });
    }

    fn push_user(&mut self, text: &str) {
        self.messages.push(DisplayMessage {
            role: DisplayRole::User,
            content: text.to_string(),
            thinking: String::new(),
            tool_calls: vec![],
        });
    }

    fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_add(3);
    }

    fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(3);
    }
}

// ── Agent event handler ───────────────────────────────────────────────────────

/// Returns `true` when the stream is complete (Done event).
fn handle_agent_event(app: &mut AppState, event: AgentEvent) -> bool {
    match event {
        AgentEvent::RunStart { .. } => {
            app.status = "running\u{2026}".to_string();
        }
        AgentEvent::TurnStart { turn } => {
            if turn > 1 {
                app.finalize_assistant_turn();
            }
        }
        AgentEvent::TextDelta(text) => {
            app.streaming_content.push_str(&text);
        }
        AgentEvent::ThinkingStart => {
            app.is_thinking = true;
            app.status = "thinking\u{2026}".to_string();
        }
        AgentEvent::ThinkingEnd { content } => {
            app.is_thinking = false;
            app.streaming_thinking = content;
        }
        AgentEvent::ToolCallStart { id, name } => {
            app.active_tools.push(ToolCallState {
                id,
                name,
                args_buf: String::new(),
                output: String::new(),
                status: ToolStatus::Streaming,
                started: Instant::now(),
                elapsed: None,
            });
        }
        AgentEvent::ToolCallArgumentsDelta { id, delta } => {
            if let Some(tc) = app.active_tools.iter_mut().find(|t| t.id == id) {
                tc.args_buf.push_str(&delta);
                tc.status = ToolStatus::Running;
            }
        }
        AgentEvent::ToolDelta { id, delta, .. } => {
            if let Some(tc) = app.active_tools.iter_mut().find(|t| t.id == id) {
                tc.output.push_str(&delta);
                tc.status = ToolStatus::Running;
            }
        }
        AgentEvent::ToolResult { id, result, .. } => {
            if let Some(tc) = app.active_tools.iter_mut().find(|t| t.id == id) {
                tc.output = result;
                tc.status = ToolStatus::Done;
                tc.elapsed = Some(tc.started.elapsed());
            }
        }
        AgentEvent::Interrupt { interrupts } => {
            let completed = app
                .active_tools
                .iter()
                .filter(|t| t.status == ToolStatus::Done)
                .map(|t| ToolCallResult {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    result: t.output.clone(),
                })
                .collect();

            for tc in app.active_tools.iter_mut() {
                if tc.status != ToolStatus::Done {
                    tc.status = ToolStatus::Interrupted;
                    tc.elapsed = Some(tc.started.elapsed());
                }
            }

            app.pending_interrupt = Some(PendingInterrupt {
                interrupts,
                completed,
            });
            app.mode = InputMode::Interrupt;
            app.status = "\u{26a0} interrupt \u{2014} [Y]es / [N]o".to_string();
        }
        AgentEvent::Usage {
            prompt_tokens,
            completion_tokens,
        } => {
            app.prompt_tokens += prompt_tokens;
            app.completion_tokens += completion_tokens;
        }
        AgentEvent::Error(e) => {
            app.status = format!("error: {e}");
            app.streaming_content
                .push_str(&format!("\n\n[ERROR] {e}\n"));
        }
        AgentEvent::Done => {
            app.finalize_assistant_turn();
            app.mode = InputMode::Normal;
            app.status.clear();
            return true;
        }
        AgentEvent::NewMessages(_) => {
            // Internal event — ignored in TUI mode
        }
        AgentEvent::NeedToolExecution { .. } => {
            // Not expected in TUI (all tools are local)
        }
    }
    false
}

// ── Key event handler ─────────────────────────────────────────────────────────

/// Returns `Some(input)` when user submits a message.
fn handle_key_normal(app: &mut AppState, event: crossterm::event::KeyEvent) -> Option<String> {
    use KeyCode::*;
    match event.code {
        Char('c') if event.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        Char('d') if event.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        Enter => {
            let input = app.input.trim().to_string();
            if input.is_empty() {
                return None;
            }
            if input.starts_with('/') {
                handle_command(app, &input);
                app.input.clear();
                app.cursor = 0;
                app.scroll = u16::MAX; // scroll to bottom to show response
                return None;
            }
            app.command_history.push(input.clone());
            app.history_pos = None;
            app.input.clear();
            app.cursor = 0;
            return Some(input);
        }
        Backspace => {
            if app.cursor > 0 {
                app.input.remove(app.cursor - 1);
                app.cursor -= 1;
            }
        }
        Delete => {
            if app.cursor < app.input.len() {
                app.input.remove(app.cursor);
            }
        }
        Left => {
            if app.cursor > 0 {
                app.cursor -= 1;
            }
        }
        Right => {
            if app.cursor < app.input.len() {
                app.cursor += 1;
            }
        }
        Home => app.cursor = 0,
        End => app.cursor = app.input.len(),
        Up => {
            let hist = &app.command_history;
            if hist.is_empty() {
                return None;
            }
            let pos = app
                .history_pos
                .map(|p| p.saturating_sub(1))
                .unwrap_or(hist.len() - 1);
            app.history_pos = Some(pos);
            app.input = hist[pos].clone();
            app.cursor = app.input.len();
        }
        Down => {
            if let Some(pos) = app.history_pos {
                let hist_len = app.command_history.len();
                if pos + 1 < hist_len {
                    let new_pos = pos + 1;
                    app.history_pos = Some(new_pos);
                    app.input = app.command_history[new_pos].clone();
                } else {
                    app.history_pos = None;
                    app.input.clear();
                }
                app.cursor = app.input.len();
            }
        }
        PageUp => app.scroll_up(),
        PageDown => app.scroll_down(),
        Char(c) => {
            app.input.insert(app.cursor, c);
            app.cursor += 1;
        }
        _ => {}
    }
    None
}

enum InterruptChoice {
    Approve,
    Reject,
    None,
}

fn handle_key_interrupt(app: &mut AppState, event: crossterm::event::KeyEvent) -> InterruptChoice {
    match event.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => InterruptChoice::Approve,
        KeyCode::Char('n') | KeyCode::Char('N') => InterruptChoice::Reject,
        KeyCode::Char('c') if event.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
            InterruptChoice::None
        }
        _ => InterruptChoice::None,
    }
}

fn handle_command(app: &mut AppState, cmd: &str) {
    match cmd.trim() {
        "/help" | "/h" => {
            app.messages.push(DisplayMessage {
                role: DisplayRole::Assistant,
                content: concat!(
                    "Commands:\n",
                    "  /help      show this help\n",
                    "  /clear     clear conversation\n",
                    "  /quit      exit\n",
                    "\nKeyboard shortcuts:\n",
                    "  Enter      send message\n",
                    "  Ctrl+C     quit / abort\n",
                    "  PageUp/Down  scroll messages\n",
                    "  Up/Down    command history"
                )
                .to_string(),
                thinking: String::new(),
                tool_calls: vec![],
            });
            app.scroll = u16::MAX; // will be clamped to bottom in render
        }
        "/clear" => {
            app.messages.clear();
            app.streaming_content.clear();
            app.streaming_thinking.clear();
            app.is_thinking = false;
            app.active_tools.clear();
            app.pending_interrupt = None;
            app.prompt_tokens = 0;
            app.completion_tokens = 0;
            app.status = "conversation cleared".to_string();
        }
        "/quit" | "/exit" | "/q" => {
            app.should_quit = true;
        }
        other => {
            app.status = format!("unknown command: {other}  (try /help)");
        }
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn render(f: &mut Frame, app: &AppState) {
    let area = f.size();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(area);

    render_messages(f, chunks[0], app);
    render_status(f, chunks[1], app);
    render_input(f, chunks[2], app);
}

fn render_messages(f: &mut Frame, area: Rect, app: &AppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Remi ")
        .title_alignment(Alignment::Center)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = Vec::new();

    for msg in &app.messages {
        match msg.role {
            DisplayRole::User => {
                lines.push(Line::from(vec![
                    Span::styled(
                        "\u{276f} ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        msg.content.clone(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                lines.push(Line::from(""));
            }
            DisplayRole::Assistant => {
                // Show collapsed thinking summary if present
                if !msg.thinking.is_empty() {
                    let token_count = msg.thinking.split_whitespace().count();
                    lines.push(Line::from(Span::styled(
                        format!("\u{1f9e0} Thought for ~{} words", token_count),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )));
                }
                for content_line in msg.content.lines() {
                    lines.push(Line::from(Span::raw(content_line.to_string())));
                }
                for tc in &msg.tool_calls {
                    lines.extend(render_tool_lines(tc));
                }
                if !msg.content.is_empty() || !msg.tool_calls.is_empty() {
                    lines.push(Line::from(""));
                }
            }
        }
    }

    // Streaming content (in-progress assistant turn)
    if app.is_thinking {
        // Show a live thinking indicator while reasoning is still in progress
        lines.push(Line::from(Span::styled(
            "\u{1f9e0} Thinking\u{2026}",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
    }
    if !app.streaming_content.is_empty() {
        for content_line in app.streaming_content.lines() {
            lines.push(Line::from(Span::raw(content_line.to_string())));
        }
    }

    for tc in &app.active_tools {
        lines.extend(render_tool_lines(tc));
    }

    // Interrupt prompt
    if let Some(intr) = &app.pending_interrupt {
        lines.push(Line::from(""));
        for info in &intr.interrupts {
            lines.push(Line::from(Span::styled(
                format!(" \u{26a0}  INTERRUPT: {} ", info.kind),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(format!("   tool: {}", info.tool_name)));
            if let Ok(data_str) = serde_json::to_string_pretty(&info.data) {
                for data_line in data_str.lines().take(6) {
                    lines.push(Line::from(format!("   {}", data_line)));
                }
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "  [Y] Approve  ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "[N] Reject",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));
    }

    let total = lines.len() as u16;
    let visible = inner.height;
    let auto_scroll = total.saturating_sub(visible);

    let scroll_row = if app.mode == InputMode::Running {
        auto_scroll
    } else {
        app.scroll.min(auto_scroll)
    };

    f.render_widget(
        Paragraph::new(lines)
            .scroll((scroll_row, 0))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn render_tool_lines(tc: &ToolCallState) -> Vec<Line<'static>> {
    let (border_color, status_label) = match tc.status {
        ToolStatus::Streaming => (Color::DarkGray, "\u{2026} args".to_string()),
        ToolStatus::Running => (Color::Yellow, "\u{25d0} running".to_string()),
        ToolStatus::Done => {
            let elapsed = tc
                .elapsed
                .map(|d| format!("{:.1}s", d.as_secs_f64()))
                .unwrap_or_default();
            (Color::Green, format!("\u{2713}  {}", elapsed))
        }
        ToolStatus::Interrupted => (Color::Magenta, "\u{26a0} interrupted".to_string()),
        ToolStatus::Error => (Color::Red, "\u{2717} error".to_string()),
    };

    let header = format!(
        "\u{250c}\u{2500} {} \u{2500}\u{2500}\u{2500} {} \u{2500}\u{2510}",
        tc.name, status_label
    );
    let footer = "\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2518}".to_string();

    let mut result = vec![Line::from(Span::styled(
        header,
        Style::default().fg(border_color),
    ))];

    // Show output lines (up to 5)
    let show_text = if tc.output.is_empty() {
        tc.args_buf.chars().take(60).collect::<String>()
    } else {
        tc.output
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(5)
            .map(|l| l.chars().take(60).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    };

    for line in show_text.lines() {
        result.push(Line::from(Span::styled(
            format!("\u{2502} {}", line),
            Style::default().fg(Color::Reset),
        )));
    }

    result.push(Line::from(Span::styled(
        footer,
        Style::default().fg(border_color),
    )));
    result
}

fn render_status(f: &mut Frame, area: Rect, app: &AppState) {
    let spinner = if app.mode == InputMode::Running {
        format!("{} ", SPINNER[app.spinner_frame])
    } else {
        String::new()
    };

    let right = format!(
        " \u{2191}{} \u{2193}{}",
        app.prompt_tokens, app.completion_tokens
    );

    let left = if !app.status.is_empty() {
        format!("{}{}", spinner, app.status)
    } else {
        format!("{}{}", spinner, app.model_name)
    };

    let width = area.width as usize;
    let padding = width.saturating_sub(left.len() + right.len());
    let text = format!("{}{}{}", left, " ".repeat(padding), right);

    f.render_widget(
        Paragraph::new(text).style(Style::default().bg(Color::DarkGray).fg(Color::White)),
        area,
    );
}

fn render_input(f: &mut Frame, area: Rect, app: &AppState) {
    let (border_style, title) = match app.mode {
        InputMode::Normal => (
            Style::default().fg(Color::Cyan),
            " Input (Enter to send, /help) ",
        ),
        InputMode::Running => (
            Style::default().fg(Color::Yellow),
            " Running\u{2026} (Ctrl+C to abort) ",
        ),
        InputMode::Interrupt => (
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            " \u{26a0} Interrupt: [Y]es / [N]o ",
        ),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style);

    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.mode != InputMode::Running {
        let before = app.input[..app.cursor].to_string();
        let cursor_char = app.input.chars().nth(app.cursor).unwrap_or(' ').to_string();
        let after_start = app.cursor + cursor_char.len().min(1);
        let after = if after_start < app.input.len() {
            app.input[after_start..].to_string()
        } else {
            String::new()
        };

        let spans = vec![
            Span::raw(before),
            Span::styled(
                cursor_char,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(after),
        ];

        f.render_widget(Paragraph::new(Line::from(spans)), inner);
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> io::Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Tui) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("REMI_API_KEY"))
        .unwrap_or_default();

    let base_url = std::env::var("REMI_BASE_URL")
        .or_else(|_| std::env::var("OPENAI_BASE_URL"))
        .ok();

    let model_name = std::env::var("REMI_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());
    let system_prompt = std::env::var("REMI_SYSTEM").unwrap_or_default();
    let max_turns: usize = std::env::var("REMI_MAX_TURNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    if api_key.is_empty() {
        eprintln!("Error: OPENAI_API_KEY or REMI_API_KEY is required.");
        std::process::exit(1);
    }

    let mut oai = OpenAIClient::new(api_key).with_model(model_name.clone());
    if let Some(url) = base_url {
        oai = oai.with_base_url(url);
    }

    let mut builder = AgentBuilder::new().model(oai).max_turns(max_turns);
    if !system_prompt.is_empty() {
        builder = builder.system(system_prompt);
    }
    let agent = builder.build();

    let mut terminal = setup_terminal()?;
    let result = run_app(&mut terminal, agent, model_name).await;
    restore_terminal(&mut terminal)?;

    if let Err(e) = result {
        eprintln!("Error: {e}");
    }
    Ok(())
}

async fn run_app(
    terminal: &mut Tui,
    agent: BuiltAgent<OpenAIClient<ReqwestTransport>, NoStore>,
    model_name: String,
) -> io::Result<()> {
    let mut app = AppState::new(model_name);
    let mut term_events = EventStream::new();

    app.messages.push(DisplayMessage {
        role: DisplayRole::Assistant,
        content: format!(
            "Remi Agent ready \u{2014} model: {}  (type /help for commands)",
            app.model_name
        ),
        thinking: String::new(),
        tool_calls: vec![],
    });

    'main: loop {
        app.tick();
        terminal.draw(|f| render(f, &app))?;

        if app.should_quit {
            break 'main;
        }

        // ── Interrupt mode ────────────────────────────────────────────────
        if app.mode == InputMode::Interrupt {
            let Some(Ok(Event::Key(key))) = term_events.next().await else {
                break 'main;
            };

            match handle_key_interrupt(&mut app, key) {
                InterruptChoice::Approve => {
                    let Some(intr) = app.pending_interrupt.take() else {
                        continue;
                    };
                    // Build approval payloads
                    let payloads: Vec<ResumePayload> = intr
                        .interrupts
                        .iter()
                        .map(|i| ResumePayload {
                            interrupt_id: i.interrupt_id.clone(),
                            result: serde_json::Value::Bool(true),
                        })
                        .collect();

                    let resume_msg = payloads
                        .iter()
                        .zip(intr.interrupts.iter())
                        .map(|(p, i)| format!("[approved {} = {}]", i.tool_name, p.result))
                        .collect::<Vec<_>>()
                        .join("; ");

                    app.mode = InputMode::Running;
                    app.status = "resuming\u{2026}".to_string();

                    match agent.chat(resume_msg.into()).await {
                        Err(e) => {
                            app.status = format!("error: {e}");
                            app.mode = InputMode::Normal;
                        }
                        Ok(stream) => {
                            run_stream(terminal, &mut app, &mut term_events, stream).await?;
                        }
                    }
                }
                InterruptChoice::Reject => {
                    app.pending_interrupt = None;
                    app.mode = InputMode::Normal;
                    app.status = "interrupt rejected".to_string();
                    app.finalize_assistant_turn();
                }
                InterruptChoice::None => {}
            }
            continue;
        }

        // ── Normal input mode ─────────────────────────────────────────────
        let Some(Ok(event)) = term_events.next().await else {
            break 'main;
        };

        let Event::Key(key) = event else { continue };

        if let Some(input) = handle_key_normal(&mut app, key) {
            if app.should_quit {
                break 'main;
            }

            app.push_user(&input);
            app.mode = InputMode::Running;
            app.status = "thinking\u{2026}".to_string();
            terminal.draw(|f| render(f, &app))?;

            match agent.chat(input.into()).await {
                Err(e) => {
                    app.status = format!("error: {e}");
                    app.mode = InputMode::Normal;
                }
                Ok(stream) => {
                    run_stream(terminal, &mut app, &mut term_events, stream).await?;
                }
            }
        }

        if app.should_quit {
            break 'main;
        }
    }

    Ok(())
}

async fn run_stream(
    terminal: &mut Tui,
    app: &mut AppState,
    term_events: &mut EventStream,
    stream: impl futures::Stream<Item = AgentEvent>,
) -> io::Result<()> {
    let mut stream = std::pin::pin!(stream);

    loop {
        app.tick();
        terminal.draw(|f| render(f, app))?;

        tokio::select! {
            // Agent stream event
            agent_ev = stream.next() => {
                match agent_ev {
                    Some(event) => {
                        let done = handle_agent_event(app, event);
                        if done || app.mode == InputMode::Interrupt {
                            break;
                        }
                    }
                    None => {
                        app.finalize_assistant_turn();
                        app.mode = InputMode::Normal;
                        app.status.clear();
                        break;
                    }
                }
            }
            // Terminal events during streaming
            Some(Ok(Event::Key(key))) = term_events.next() => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    app.finalize_assistant_turn();
                    app.mode = InputMode::Normal;
                    app.status = "aborted".to_string();
                    break;
                }
                if key.code == KeyCode::PageUp { app.scroll_up(); }
                if key.code == KeyCode::PageDown { app.scroll_down(); }
            }
            // Re-render tick when no events
            _ = tokio::time::sleep(Duration::from_millis(33)) => {}
        }
    }

    Ok(())
}
