use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use bot_core::{CatEvent, Content, StreamOptions};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Color, Line, Modifier, Span, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::{mpsc, Notify};
use tokio_stream::wrappers::UnboundedReceiverStream;
use unicode_width::UnicodeWidthStr;

use crate::{
    process_runtime_commands, CliConfig, Runtime, RuntimeCommandPipelineResult,
    SESSION_MODEL_PROFILE_METADATA_KEY,
};

const TUI_CHANNEL: &str = "tui";
const DEFAULT_TUI_SESSION: &str = "local-tui";
const MAX_HISTORY_CELLS: usize = 400;
const CODEX_CYAN: Color = Color::Rgb(109, 209, 255);
const CODEX_DIM: Color = Color::Rgb(118, 124, 134);
const CODEX_PANEL: Color = Color::Rgb(12, 14, 18);
const CODEX_BORDER: Color = Color::Rgb(63, 68, 78);
const CODEX_GREEN: Color = Color::Rgb(122, 222, 151);
const FOOTER_INDENT: &str = "  ";
const HISTORY_GUTTER_WIDTH: u16 = 9;
const QUIT_HINT_TIMEOUT: Duration = Duration::from_secs(1);

type CrosstermTerminal = Terminal<CrosstermBackend<Stdout>>;

pub(crate) async fn run_tui(runtime: Rc<Runtime>, cli: CliConfig) -> anyhow::Result<()> {
    let session_channel = if cli.channel_id == crate::CLI_CHAT_ID {
        DEFAULT_TUI_SESSION
    } else {
        &cli.channel_id
    };
    let session_id = runtime.sessions.lock().await.resolve_channel(
        TUI_CHANNEL,
        session_channel,
        &runtime.root_agent_id,
    )?;

    let mut terminal = TerminalGuard::enter()?;
    let mut app = TuiApp::new(runtime, cli, session_id).await;
    let result = app.run(&mut terminal.terminal).await;
    terminal.restore()?;
    result
}

struct TerminalGuard {
    terminal: CrosstermTerminal,
    restored: bool,
}

impl TerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        enable_raw_mode().context("enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)
            .context("enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("create terminal")?;
        Ok(Self {
            terminal,
            restored: false,
        })
    }

    fn restore(&mut self) -> anyhow::Result<()> {
        if self.restored {
            return Ok(());
        }
        disable_raw_mode().context("disable raw mode")?;
        execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            LeaveAlternateScreen
        )
        .context("leave alternate screen")?;
        self.terminal.show_cursor().context("show cursor")?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

struct TuiApp {
    runtime: Rc<Runtime>,
    cli: CliConfig,
    session_id: String,
    cells: Vec<HistoryCell>,
    input: String,
    cursor: usize,
    scroll: u16,
    command_catalog: Vec<CommandEntry>,
    popup_selected: usize,
    show_shortcuts: bool,
    quit_hint_until: Option<Instant>,
    running: bool,
    run_started_at: Option<Instant>,
    cancel: Option<Arc<Notify>>,
    queued_inputs: VecDeque<String>,
    input_history: Vec<String>,
    history_index: Option<usize>,
    status: StatusLine,
    active_tool_args: std::collections::HashMap<String, String>,
    bot_tx: mpsc::UnboundedSender<BotEvent>,
    bot_rx: UnboundedReceiverStream<BotEvent>,
}

impl TuiApp {
    async fn new(runtime: Rc<Runtime>, cli: CliConfig, session_id: String) -> Self {
        let (bot_tx, bot_rx) = mpsc::unbounded_channel();
        let mut app = Self {
            runtime,
            cli,
            session_id,
            cells: Vec::new(),
            input: String::new(),
            cursor: 0,
            scroll: 0,
            command_catalog: Vec::new(),
            popup_selected: 0,
            show_shortcuts: false,
            quit_hint_until: None,
            running: false,
            run_started_at: None,
            cancel: None,
            queued_inputs: VecDeque::new(),
            input_history: Vec::new(),
            history_index: None,
            status: StatusLine::default(),
            active_tool_args: std::collections::HashMap::new(),
            bot_tx,
            bot_rx: UnboundedReceiverStream::new(bot_rx),
        };
        app.refresh_command_catalog();
        app.cells.push(HistoryCell::system(
            "Remi Cat TUI ready. Type / for commands. Ctrl+C cancels a run or exits when idle.",
        ));
        app
    }

    async fn run(&mut self, terminal: &mut CrosstermTerminal) -> anyhow::Result<()> {
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(120));

        loop {
            terminal
                .draw(|frame| self.render(frame))
                .context("draw TUI frame")?;

            tokio::select! {
                maybe_event = events.next() => {
                    match maybe_event {
                        Some(Ok(event)) => {
                            if self.handle_terminal_event(event).await? {
                                break;
                            }
                        }
                        Some(Err(err)) => return Err(anyhow::Error::from(err)).context("read terminal event"),
                        None => break,
                    }
                }
                Some(event) = self.bot_rx.next() => {
                    self.handle_bot_event(event);
                }
                _ = tick.tick() => {
                    self.flush_status_elapsed();
                }
            }
        }
        Ok(())
    }

    async fn handle_terminal_event(&mut self, event: Event) -> anyhow::Result<bool> {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key).await,
            Event::Paste(text) => {
                if self.input.is_empty() && text == "?" {
                    self.show_shortcuts = !self.show_shortcuts;
                    return Ok(false);
                }
                self.insert_text(&text);
                Ok(false)
            }
            Event::Resize(_, _) => Ok(false),
            _ => Ok(false),
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('d') => return Ok(self.handle_cancel_or_quit()),
                KeyCode::Char('l') => {
                    self.scroll = 0;
                    return Ok(false);
                }
                KeyCode::Char('u') => {
                    self.scroll = self.scroll.saturating_add(8);
                    return Ok(false);
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => {
                if self.popup_visible() {
                    self.input.clear();
                    self.cursor = 0;
                }
                self.show_shortcuts = false;
                self.quit_hint_until = None;
                self.popup_selected = 0;
                Ok(false)
            }
            KeyCode::Char('?') if self.input.is_empty() => {
                self.show_shortcuts = !self.show_shortcuts;
                Ok(false)
            }
            KeyCode::Char('/')
                if self.input.is_empty() && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.show_shortcuts = !self.show_shortcuts;
                Ok(false)
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(12);
                Ok(false)
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(12);
                Ok(false)
            }
            KeyCode::Up if self.popup_visible() => {
                let len = self.filtered_commands().len();
                if len > 0 {
                    self.popup_selected = (self.popup_selected + len - 1) % len;
                }
                Ok(false)
            }
            KeyCode::Up if self.input.is_empty() => {
                self.recall_history(-1);
                Ok(false)
            }
            KeyCode::Up => {
                self.scroll = self.scroll.saturating_add(3);
                Ok(false)
            }
            KeyCode::Down if self.popup_visible() => {
                let len = self.filtered_commands().len();
                if len > 0 {
                    self.popup_selected = (self.popup_selected + 1) % len;
                }
                Ok(false)
            }
            KeyCode::Down if self.history_index.is_some() => {
                self.recall_history(1);
                Ok(false)
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_sub(3);
                Ok(false)
            }
            KeyCode::Tab if self.popup_visible() => {
                self.complete_selected_command();
                Ok(false)
            }
            KeyCode::Enter if self.popup_visible() && self.selected_command_accepts_arguments() => {
                self.complete_selected_command();
                Ok(false)
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_text("\n");
                Ok(false)
            }
            KeyCode::Enter => {
                self.submit().await?;
                Ok(false)
            }
            KeyCode::Backspace => {
                self.backspace();
                Ok(false)
            }
            KeyCode::Delete => {
                self.delete();
                Ok(false)
            }
            KeyCode::Left => {
                self.cursor = previous_char_boundary(&self.input, self.cursor);
                Ok(false)
            }
            KeyCode::Right => {
                self.cursor = next_char_boundary(&self.input, self.cursor);
                Ok(false)
            }
            KeyCode::Home => {
                self.cursor = 0;
                Ok(false)
            }
            KeyCode::End => {
                self.cursor = self.input.len();
                Ok(false)
            }
            KeyCode::Char(ch) => {
                self.insert_char(ch);
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    async fn submit(&mut self) -> anyhow::Result<()> {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return Ok(());
        }
        self.input.clear();
        self.cursor = 0;
        self.history_index = None;
        self.scroll = 0;
        self.popup_selected = 0;
        self.show_shortcuts = false;
        self.quit_hint_until = None;
        if self.running {
            self.queued_inputs.push_back(text);
            return Ok(());
        }
        self.start_turn(text);
        Ok(())
    }

    fn start_turn(&mut self, text: String) {
        self.input_history.push(text.clone());
        if self.input_history.len() > 100 {
            self.input_history.remove(0);
        }
        self.cells.push(HistoryCell::user(text.clone()));
        while self.cells.len() > MAX_HISTORY_CELLS {
            self.cells.remove(0);
        }

        let cancel = Arc::new(Notify::new());
        self.cancel = Some(Arc::clone(&cancel));
        self.running = true;
        self.run_started_at = Some(Instant::now());
        self.status.state = "running".to_string();
        self.status.last_error = None;

        let runtime = Rc::clone(&self.runtime);
        let session_id = self.session_id.clone();
        let sender_user_id = self.cli.user_id.clone();
        let sender_username = self.cli.username.clone();
        let tx = self.bot_tx.clone();
        tokio::task::spawn_local(async move {
            run_bot_turn(
                runtime,
                session_id,
                text,
                sender_user_id,
                sender_username,
                cancel,
                tx,
            )
            .await;
        });
    }

    fn handle_bot_event(&mut self, event: BotEvent) {
        match event {
            BotEvent::Prefix(text) => self.cells.push(HistoryCell::assistant(text)),
            BotEvent::Text(delta) => self.push_assistant_delta(&delta),
            BotEvent::Thinking(delta) => self.push_thinking_delta(&delta),
            BotEvent::ToolStart { id, name } => {
                self.active_tool_args.insert(id.clone(), String::new());
                self.cells.push(HistoryCell::tool(
                    id,
                    name,
                    String::new(),
                    "running".to_string(),
                    false,
                ));
            }
            BotEvent::ToolArgs { id, delta } => {
                let args = self.active_tool_args.entry(id.clone()).or_default();
                args.push_str(&delta);
                if let Some(cell) = self
                    .cells
                    .iter_mut()
                    .rev()
                    .find(|cell| matches!(&cell.kind, CellKind::Tool { id: tool_id, .. } if tool_id == &id))
                {
                    cell.append(&delta);
                }
            }
            BotEvent::ToolDone {
                id,
                name,
                result,
                success,
                elapsed_ms,
            } => {
                self.active_tool_args.remove(&id);
                self.cells.push(HistoryCell::tool(
                    id,
                    name,
                    result,
                    format!("{elapsed_ms}ms"),
                    success,
                ));
            }
            BotEvent::Supervisor(text) => self.cells.push(HistoryCell::system(text)),
            BotEvent::Stats {
                prompt_tokens,
                completion_tokens,
                max_prompt_tokens,
                elapsed_ms,
            } => {
                self.status.prompt_tokens = prompt_tokens;
                self.status.completion_tokens = completion_tokens;
                self.status.max_prompt_tokens = max_prompt_tokens;
                self.status.model_elapsed_ms = elapsed_ms;
            }
            BotEvent::Error(message) => {
                self.status.last_error = Some(message.clone());
                self.cells.push(HistoryCell::error(message));
                self.running = false;
                self.cancel = None;
                self.status.state = "error".to_string();
            }
            BotEvent::Done => {
                self.running = false;
                self.cancel = None;
                self.status.state = "idle".to_string();
                self.refresh_command_catalog();
                if let Some(next) = self.queued_inputs.pop_front() {
                    self.start_turn(next);
                }
            }
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        let root = frame.area();
        let activity_height = self.activity_height();
        let footer_height = self.footer_height();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(4),
                Constraint::Length(activity_height),
                Constraint::Length(footer_height),
                Constraint::Length(self.composer_height(root.width)),
            ])
            .split(root);

        self.render_status(frame, chunks[0]);
        self.render_history(frame, chunks[1]);
        self.render_activity(frame, chunks[2]);
        self.render_footer(frame, chunks[3]);
        self.render_composer(frame, chunks[4]);
        if self.popup_visible() {
            self.render_command_popup(frame, chunks[4]);
        }
    }

    fn render_status(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.flush_status_elapsed();
        let error = self
            .status
            .last_error
            .as_deref()
            .map(|value| format!("  {value}"))
            .unwrap_or_default();
        let line = Line::from(vec![
            Span::styled(
                " Remi Cat",
                Style::default().fg(CODEX_CYAN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", Style::default().fg(CODEX_DIM)),
            Span::styled(
                self.status.state.clone(),
                Style::default().fg(if self.running {
                    Color::Yellow
                } else {
                    CODEX_GREEN
                }),
            ),
            Span::styled(error, Style::default().fg(Color::Red)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn footer_context_line(&self) -> Line<'static> {
        let model_profile_id = self.runtime.sessions.try_lock().ok().and_then(|sessions| {
            sessions.metadata_string(&self.session_id, SESSION_MODEL_PROFILE_METADATA_KEY)
        });
        let model = self
            .runtime
            .bot
            .effective_model_profile(model_profile_id.as_deref())
            .profile
            .id
            .clone();
        let session = short_session_label(&self.session_id);
        Line::from(vec![
            Span::styled(format!("{model} "), Style::default().fg(CODEX_DIM)),
            Span::styled("· ", Style::default().fg(CODEX_DIM)),
            Span::styled(
                format!(
                    "{}+{} tokens ",
                    self.status.prompt_tokens, self.status.completion_tokens
                ),
                Style::default().fg(CODEX_DIM),
            ),
            Span::styled("· ", Style::default().fg(CODEX_DIM)),
            Span::styled(session, Style::default().fg(CODEX_DIM)),
        ])
    }

    fn render_activity(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let mut lines = Vec::new();
        if self.running {
            lines.push(Line::from(vec![
                Span::styled("  working", Style::default().fg(Color::Yellow)),
                Span::styled(" · ", Style::default().fg(CODEX_DIM)),
                Span::styled("Ctrl+C to interrupt", Style::default().fg(CODEX_DIM)),
            ]));
        }
        if let Some(next) = self.queued_inputs.front() {
            lines.push(Line::from(vec![
                Span::styled("  queued", Style::default().fg(CODEX_CYAN)),
                Span::styled(" · ", Style::default().fg(CODEX_DIM)),
                Span::styled(
                    truncate_for_width(next, area.width.saturating_sub(14)),
                    Style::default().fg(CODEX_DIM),
                ),
            ]));
        }
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Color::Black)),
            area,
        );
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        if self.show_shortcuts {
            let lines = vec![
                Line::from(Span::styled(
                    format!("{FOOTER_INDENT}shortcuts"),
                    Style::default()
                        .fg(CODEX_CYAN)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!(
                        "{FOOTER_INDENT}Enter send   Shift+Enter newline   Tab complete/queue   ? hide shortcuts"
                    ),
                    Style::default().fg(CODEX_DIM),
                )),
                Line::from(Span::styled(
                    format!(
                        "{FOOTER_INDENT}/ commands   Up/Down history or popup   PgUp/PgDn scroll   Ctrl+C cancel/exit"
                    ),
                    Style::default().fg(CODEX_DIM),
                )),
            ];
            frame.render_widget(
                Paragraph::new(lines).style(Style::default().bg(Color::Black)),
                area,
            );
            return;
        }
        let hints = if self.quit_hint_active() {
            Line::from(vec![
                Span::styled(
                    format!("{FOOTER_INDENT}Press Ctrl+C again to exit"),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled("  ·  Esc cancels", Style::default().fg(CODEX_DIM)),
            ])
        } else {
            self.base_footer_line()
        };
        let hint_width = hints.width() as u16;
        frame.render_widget(
            Paragraph::new(hints)
                .alignment(Alignment::Left)
                .style(Style::default().bg(Color::Black)),
            area,
        );
        if !self.show_shortcuts && area.width > 42 {
            let context = self.footer_context_line();
            let width = context.width() as u16;
            if hint_width.saturating_add(width).saturating_add(6) < area.width {
                let x = area.x + area.width.saturating_sub(width + 2);
                let context_area = Rect::new(x, area.y, width, 1);
                frame.render_widget(Paragraph::new(context), context_area);
            }
        }
    }

    fn base_footer_line(&self) -> Line<'static> {
        let running_hint = if self.running {
            "Ctrl+C cancel"
        } else {
            "Ctrl+C exit"
        };
        let hints = Line::from(vec![
            Span::styled(FOOTER_INDENT, Style::default().fg(CODEX_DIM)),
            Span::styled("?", Style::default().fg(Color::White)),
            Span::styled(" for shortcuts", Style::default().fg(CODEX_DIM)),
            Span::styled(" · ", Style::default().fg(CODEX_DIM)),
            Span::styled("Enter", Style::default().fg(Color::White)),
            Span::styled(" send", Style::default().fg(CODEX_DIM)),
            Span::styled(" · ", Style::default().fg(CODEX_DIM)),
            Span::styled("/", Style::default().fg(Color::White)),
            Span::styled(" commands", Style::default().fg(CODEX_DIM)),
            Span::styled(
                format!(" · {running_hint}"),
                Style::default().fg(Color::White),
            ),
        ]);
        hints
    }

    fn render_history(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut lines = Vec::new();
        for cell in &self.cells {
            lines.extend(cell.lines(area.width));
        }
        let scroll = history_scroll_offset(lines.len() as u16, area.height, self.scroll);
        let paragraph = Paragraph::new(lines)
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Gray).bg(Color::Black));
        frame.render_widget(Clear, area);
        frame.render_widget(paragraph, area);
    }

    fn render_composer(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(
            Block::default().style(Style::default().bg(CODEX_PANEL)),
            area,
        );
        let lines = if self.input.is_empty() {
            vec![Line::from(vec![
                Span::styled(
                    "›",
                    Style::default().fg(CODEX_CYAN).add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ", Style::default().fg(CODEX_DIM)),
                Span::styled(
                    "Message Remi or type / for commands",
                    Style::default().fg(CODEX_DIM),
                ),
            ])]
        } else {
            self.input
                .split('\n')
                .enumerate()
                .map(|(index, line)| {
                    let prefix = if index == 0 { "›  " } else { "   " };
                    Line::from(vec![
                        Span::styled(
                            prefix,
                            Style::default()
                                .fg(if index == 0 { CODEX_CYAN } else { CODEX_DIM })
                                .add_modifier(if index == 0 {
                                    Modifier::BOLD
                                } else {
                                    Modifier::empty()
                                }),
                        ),
                        Span::styled(line.to_string(), Style::default().fg(Color::White)),
                    ])
                })
                .collect()
        };
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
        if !self.running {
            let (x, y) = self.cursor_position(area);
            frame.set_cursor_position((x, y));
        }
    }

    fn render_command_popup(&self, frame: &mut Frame<'_>, composer_area: Rect) {
        let commands = self.filtered_commands();
        if commands.is_empty() {
            return;
        }
        let width = composer_area.width.min(78).max(40);
        let height = (commands.len() as u16 + 2).min(10);
        let x = composer_area.x + composer_area.width.saturating_sub(width);
        let y = composer_area.y.saturating_sub(height);
        let area = Rect::new(x, y, width, height);
        frame.render_widget(Clear, area);
        let rows = commands
            .iter()
            .take(height.saturating_sub(2) as usize)
            .enumerate()
            .map(|(index, command)| {
                let selected = index == self.popup_selected;
                let command_style = if selected {
                    Style::default().fg(Color::Black).bg(CODEX_CYAN)
                } else {
                    Style::default().fg(CODEX_CYAN)
                };
                let description_style = if selected {
                    Style::default().fg(Color::Black).bg(CODEX_CYAN)
                } else {
                    Style::default().fg(CODEX_DIM)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {:<22}", command.value.trim_end()), command_style),
                    Span::styled(command.description.clone(), description_style),
                ]))
            })
            .collect::<Vec<_>>();
        let list = List::new(rows).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" slash commands ")
                .border_style(Style::default().fg(CODEX_BORDER))
                .style(Style::default().bg(CODEX_PANEL)),
        );
        frame.render_widget(list, area);
    }

    fn insert_char(&mut self, ch: char) {
        self.input.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.popup_selected = 0;
        self.history_index = None;
        self.quit_hint_until = None;
    }

    fn insert_text(&mut self, text: &str) {
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.popup_selected = 0;
        self.history_index = None;
        self.quit_hint_until = None;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = previous_char_boundary(&self.input, self.cursor);
        self.input.replace_range(previous..self.cursor, "");
        self.cursor = previous;
        self.popup_selected = 0;
        self.history_index = None;
    }

    fn delete(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        let next = next_char_boundary(&self.input, self.cursor);
        self.input.replace_range(self.cursor..next, "");
        self.popup_selected = 0;
        self.history_index = None;
    }

    fn popup_visible(&self) -> bool {
        let first_line = self.input.lines().next().unwrap_or("");
        first_line.trim_start().starts_with('/') && !first_line.contains('\n')
    }

    fn filtered_commands(&self) -> Vec<&CommandEntry> {
        let normalized = self
            .input
            .trim_start()
            .trim_start_matches('/')
            .to_lowercase();
        let terms = normalized.split_whitespace().collect::<Vec<_>>();
        self.command_catalog
            .iter()
            .filter(|command| {
                let searchable = command.searchable.to_lowercase();
                terms.iter().all(|term| searchable.contains(term))
            })
            .collect()
    }

    fn complete_selected_command(&mut self) {
        let commands = self.filtered_commands();
        let Some(command) = commands.get(self.popup_selected).copied() else {
            return;
        };
        self.input = command.value.clone();
        self.cursor = self.input.len();
    }

    fn selected_command_accepts_arguments(&self) -> bool {
        let commands = self.filtered_commands();
        commands
            .get(self.popup_selected)
            .is_some_and(|command| command.accepts_arguments)
    }

    fn handle_cancel_or_quit(&mut self) -> bool {
        if self.popup_visible() {
            self.input.clear();
            self.cursor = 0;
            self.popup_selected = 0;
            return false;
        }
        if self.show_shortcuts {
            self.show_shortcuts = false;
            return false;
        }
        if self.running {
            if let Some(cancel) = &self.cancel {
                cancel.notify_waiters();
            }
            self.cells.push(HistoryCell::system("Interrupt requested."));
            return false;
        }
        if self.quit_hint_active() {
            return true;
        }
        self.quit_hint_until = Some(Instant::now() + QUIT_HINT_TIMEOUT);
        false
    }

    fn quit_hint_active(&self) -> bool {
        self.quit_hint_until
            .is_some_and(|deadline| Instant::now() <= deadline)
    }

    fn recall_history(&mut self, direction: isize) {
        if self.input_history.is_empty() {
            return;
        }
        let current = self.history_index.unwrap_or(self.input_history.len());
        let next = if direction < 0 {
            current.saturating_sub(1)
        } else {
            (current + 1).min(self.input_history.len())
        };
        self.history_index = (next < self.input_history.len()).then_some(next);
        self.input = self
            .history_index
            .and_then(|index| self.input_history.get(index).cloned())
            .unwrap_or_default();
        self.cursor = self.input.len();
    }

    fn refresh_command_catalog(&mut self) {
        let mut commands = static_commands();
        let mut skills = self.runtime.bot.skill_summaries();
        skills.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.source.cmp(&b.source)));
        commands.extend(skills.into_iter().map(|skill| CommandEntry {
            value: format!("/skill:{} ", skill.name),
            description: skill.description,
            accepts_arguments: true,
            searchable: format!("skill {} 技能 {}", skill.name, skill.source),
        }));
        self.command_catalog = commands;
    }

    fn push_assistant_delta(&mut self, delta: &str) {
        if let Some(cell) = self
            .cells
            .last_mut()
            .filter(|cell| matches!(cell.kind, CellKind::Assistant))
        {
            cell.append(delta);
        } else {
            self.cells.push(HistoryCell::assistant(delta.to_string()));
        }
    }

    fn push_thinking_delta(&mut self, delta: &str) {
        if let Some(cell) = self
            .cells
            .last_mut()
            .filter(|cell| matches!(cell.kind, CellKind::Thinking))
        {
            cell.append(delta);
        } else {
            self.cells.push(HistoryCell::thinking(delta.to_string()));
        }
    }

    fn flush_status_elapsed(&mut self) {
        if let Some(started) = self.run_started_at {
            self.status.elapsed_ms = started.elapsed().as_millis() as u64;
        }
    }

    fn composer_height(&self, width: u16) -> u16 {
        let rows = wrap_line_count(&self.input, width.saturating_sub(3)).max(1);
        rows.clamp(1, 6)
    }

    fn activity_height(&self) -> u16 {
        u16::from(self.running) + u16::from(!self.queued_inputs.is_empty())
    }

    fn footer_height(&self) -> u16 {
        if self.show_shortcuts {
            3
        } else {
            1
        }
    }

    fn cursor_position(&self, area: Rect) -> (u16, u16) {
        let before = &self.input[..self.cursor.min(self.input.len())];
        let inner_width = area.width.saturating_sub(3).max(1);
        let mut row = 0_u16;
        let mut col = 0_u16;
        for line in before.split('\n') {
            let width = UnicodeWidthStr::width(line) as u16;
            row = row.saturating_add(width / inner_width);
            col = width % inner_width;
        }
        if before.ends_with('\n') {
            row = row.saturating_add(1);
            col = 0;
        }
        (
            area.x + 3 + col.min(inner_width.saturating_sub(1)),
            area.y + row.min(area.height.saturating_sub(1)),
        )
    }
}

async fn run_bot_turn(
    runtime: Rc<Runtime>,
    session_id: String,
    text: String,
    sender_user_id: String,
    sender_username: String,
    cancel: Arc<Notify>,
    tx: mpsc::UnboundedSender<BotEvent>,
) {
    match process_runtime_commands(&runtime, &session_id, text.trim()).await {
        Ok(RuntimeCommandPipelineResult::Reply(reply)) => {
            let _ = tx.send(BotEvent::Prefix(reply));
            let _ = tx.send(BotEvent::Done);
            return;
        }
        Ok(RuntimeCommandPipelineResult::Continue {
            text,
            prefix,
            skill_injections,
        }) => {
            if !prefix.is_empty() {
                let _ = tx.send(BotEvent::Prefix(prefix));
            }
            let model_profile_id = runtime
                .sessions
                .lock()
                .await
                .metadata_string(&session_id, SESSION_MODEL_PROFILE_METADATA_KEY);
            let opts = StreamOptions {
                model_profile_id,
                skill_injections,
                sender_user_id: Some(sender_user_id),
                sender_username: Some(sender_username),
                message_id: Some(format!("tui-msg-{}", uuid::Uuid::new_v4())),
                chat_type: Some("p2p".to_string()),
                platform: Some(TUI_CHANNEL.to_string()),
                todo_create_via_sdk: true,
                trigger_tools_enabled: true,
                cancel: Some(cancel),
                ..StreamOptions::default()
            };
            let mut stream = std::pin::pin!(runtime.bot.stream_with_options(
                &session_id,
                Content::text(text),
                opts
            ));
            while let Some(event) = stream.next().await {
                match event {
                    CatEvent::Text(delta) => {
                        let _ = tx.send(BotEvent::Text(delta));
                    }
                    CatEvent::Thinking(delta) => {
                        let _ = tx.send(BotEvent::Thinking(delta));
                    }
                    CatEvent::ToolCallStart { id, name } => {
                        let _ = tx.send(BotEvent::ToolStart { id, name });
                    }
                    CatEvent::ToolCallArgumentsDelta { id, delta } => {
                        let _ = tx.send(BotEvent::ToolArgs { id, delta });
                    }
                    CatEvent::ToolCallResult {
                        id,
                        name,
                        result,
                        success,
                        elapsed_ms,
                        ..
                    } => {
                        let _ = tx.send(BotEvent::ToolDone {
                            id,
                            name,
                            result,
                            success,
                            elapsed_ms,
                        });
                    }
                    CatEvent::Supervisor(report) => {
                        let _ = tx.send(BotEvent::Supervisor(format!("{report:#?}")));
                    }
                    CatEvent::SupervisorProgress(progress) => {
                        let _ = tx.send(BotEvent::Supervisor(format!("{progress:#?}")));
                    }
                    CatEvent::Stats {
                        prompt_tokens,
                        completion_tokens,
                        max_prompt_tokens,
                        elapsed_ms,
                    } => {
                        let _ = tx.send(BotEvent::Stats {
                            prompt_tokens,
                            completion_tokens,
                            max_prompt_tokens,
                            elapsed_ms,
                        });
                    }
                    CatEvent::Error(error) => {
                        let _ = tx.send(BotEvent::Error(error.to_string()));
                    }
                    CatEvent::Done => break,
                    _ => {}
                }
            }
            let _ = tx.send(BotEvent::Done);
        }
        Err(error) => {
            let _ = tx.send(BotEvent::Error(error.to_string()));
            let _ = tx.send(BotEvent::Done);
        }
    }
}

#[derive(Debug)]
enum BotEvent {
    Prefix(String),
    Text(String),
    Thinking(String),
    ToolStart {
        id: String,
        name: String,
    },
    ToolArgs {
        id: String,
        delta: String,
    },
    ToolDone {
        id: String,
        name: String,
        result: String,
        success: bool,
        elapsed_ms: u64,
    },
    Supervisor(String),
    Stats {
        prompt_tokens: u32,
        completion_tokens: u32,
        max_prompt_tokens: u32,
        elapsed_ms: u64,
    },
    Error(String),
    Done,
}

struct StatusLine {
    state: String,
    prompt_tokens: u32,
    completion_tokens: u32,
    max_prompt_tokens: u32,
    model_elapsed_ms: u64,
    elapsed_ms: u64,
    last_error: Option<String>,
}

impl Default for StatusLine {
    fn default() -> Self {
        Self {
            state: "idle".to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            max_prompt_tokens: 0,
            model_elapsed_ms: 0,
            elapsed_ms: 0,
            last_error: None,
        }
    }
}

#[derive(Clone)]
struct CommandEntry {
    value: String,
    description: String,
    accepts_arguments: bool,
    searchable: String,
}

fn static_commands() -> Vec<CommandEntry> {
    [
        ("/tools", "显示当前 Agent 可用的工具", false),
        ("/goal status", "查看当前会话目标", false),
        ("/goal set ", "设置目标；可嵌套 /skill:<name>", true),
        ("/goal clear", "清除目标", false),
        ("/workflow status", "查看 workflow 状态", false),
        ("/workflow start ", "启动 workflow", true),
        ("/workflow stop", "停止 workflow", false),
        ("/compact", "压缩记忆", false),
        ("/clear", "清空当前 session 历史", false),
        ("/doctor", "运行诊断", false),
        ("/usage", "查询当前模型额度", false),
        ("/model status", "显示模型状态", false),
        ("/model list", "列出模型 profile", false),
        ("/model use ", "切换模型 profile", true),
        ("/model reset", "重置 session 模型 override", false),
        ("/skill list", "列出可用 skills", false),
        ("/skill status", "显示已读取 skills", false),
    ]
    .into_iter()
    .map(|(value, description, accepts_arguments)| CommandEntry {
        value: value.to_string(),
        description: description.to_string(),
        accepts_arguments,
        searchable: format!("{value} {description}"),
    })
    .collect()
}

#[derive(Clone)]
struct HistoryCell {
    kind: CellKind,
    title: String,
    body: String,
    meta: String,
    success: bool,
}

#[derive(Clone)]
enum CellKind {
    System,
    User,
    Assistant,
    Thinking,
    Tool { id: String },
    Error,
}

impl HistoryCell {
    fn system(text: impl Into<String>) -> Self {
        Self::new(CellKind::System, "system", text.into())
    }

    fn user(text: impl Into<String>) -> Self {
        Self::new(CellKind::User, "you", text.into())
    }

    fn assistant(text: impl Into<String>) -> Self {
        Self::new(CellKind::Assistant, "remi", text.into())
    }

    fn thinking(text: impl Into<String>) -> Self {
        Self::new(CellKind::Thinking, "thinking", text.into())
    }

    fn error(text: impl Into<String>) -> Self {
        Self::new(CellKind::Error, "error", text.into())
    }

    fn tool(id: String, name: String, body: String, meta: String, success: bool) -> Self {
        Self {
            kind: CellKind::Tool { id },
            title: format!("tool:{name}"),
            body,
            meta,
            success,
        }
    }

    fn new(kind: CellKind, title: impl Into<String>, body: String) -> Self {
        Self {
            kind,
            title: title.into(),
            body,
            meta: String::new(),
            success: true,
        }
    }

    fn append(&mut self, delta: &str) {
        self.body.push_str(delta);
    }

    fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let (prefix, title_style, body_style) = match self.kind {
            CellKind::User => (
                "›",
                Style::default().fg(CODEX_CYAN).add_modifier(Modifier::BOLD),
                Style::default().fg(Color::White),
            ),
            CellKind::Assistant => (
                "•",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(Color::White),
            ),
            CellKind::Thinking => (
                "thinking",
                Style::default().fg(CODEX_DIM),
                Style::default().fg(CODEX_DIM),
            ),
            CellKind::Tool { .. } => {
                if self.success {
                    (
                        "↳",
                        Style::default().fg(Color::Yellow),
                        Style::default().fg(CODEX_DIM),
                    )
                } else {
                    (
                        "↳",
                        Style::default().fg(Color::Red),
                        Style::default().fg(Color::Red),
                    )
                }
            }
            CellKind::Error => (
                "error",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                Style::default().fg(Color::Red),
            ),
            CellKind::System => (
                "note",
                Style::default().fg(CODEX_DIM),
                Style::default().fg(CODEX_DIM),
            ),
        };
        let title = if self.meta.is_empty() {
            self.title.clone()
        } else {
            format!("{} {}", self.title, self.meta)
        };
        lines.push(Line::from(vec![
            Span::styled(history_gutter(prefix), title_style),
            Span::styled(title, title_style),
        ]));
        let body = if self.body.trim().is_empty() {
            "(no content)"
        } else {
            self.body.trim_end()
        };
        let body_width = width.saturating_sub(HISTORY_GUTTER_WIDTH).max(16);
        for line in wrap_text(body, body_width) {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(HISTORY_GUTTER_WIDTH as usize)),
                Span::styled(line, body_style),
            ]));
        }
        lines.push(Line::from(""));
        lines
    }
}

fn history_gutter(prefix: &str) -> String {
    let used = UnicodeWidthStr::width(prefix) as u16;
    let spaces = HISTORY_GUTTER_WIDTH.saturating_sub(used).max(1);
    format!("{prefix}{}", " ".repeat(spaces as usize))
}

fn history_scroll_offset(total_lines: u16, visible_lines: u16, scroll_back: u16) -> u16 {
    let bottom_offset = total_lines.saturating_sub(visible_lines);
    bottom_offset.saturating_sub(scroll_back.min(bottom_offset))
}

fn wrap_text(text: &str, width: u16) -> Vec<String> {
    let width = width.max(16) as usize;
    text.lines()
        .flat_map(|line| {
            if line.is_empty() {
                vec![String::new()]
            } else {
                textwrap::wrap(line, width)
                    .into_iter()
                    .map(|line| line.into_owned())
                    .collect::<Vec<_>>()
            }
        })
        .collect()
}

fn wrap_line_count(text: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    text.lines()
        .map(|line| (UnicodeWidthStr::width(line) / width).max(1) as u16)
        .sum::<u16>()
        .max(1)
}

fn truncate_for_width(text: &str, width: u16) -> String {
    let width = width as usize;
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let mut output = String::new();
    let limit = width.saturating_sub(1);
    for ch in text.chars() {
        if UnicodeWidthStr::width(output.as_str()) + UnicodeWidthStr::width(ch.to_string().as_str())
            > limit
        {
            break;
        }
        output.push(ch);
    }
    output.push('…');
    output
}

fn short_session_label(session_id: &str) -> String {
    let width = UnicodeWidthStr::width(session_id);
    if width <= 12 {
        return session_id.to_string();
    }
    let prefix = session_id.chars().take(8).collect::<String>();
    format!("{prefix}…")
}

fn previous_char_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .char_indices()
        .last()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn next_char_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .char_indices()
        .nth(1)
        .map(|(index, _)| cursor + index)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_empty_input_to_one_row() {
        assert_eq!(wrap_line_count("", 80), 1);
    }

    #[test]
    fn static_commands_include_skill_commands() {
        let commands = static_commands();
        assert!(commands
            .iter()
            .any(|command| command.value == "/skill list"));
        assert!(commands
            .iter()
            .any(|command| command.value == "/model status"));
    }

    #[test]
    fn char_boundaries_handle_utf8() {
        let text = "a你b";
        let cursor = "a你".len();
        assert_eq!(previous_char_boundary(text, cursor), 1);
        assert_eq!(next_char_boundary(text, 1), cursor);
    }

    #[test]
    fn truncates_wide_text_with_ellipsis() {
        assert_eq!(truncate_for_width("abcdef", 4), "abc…");
        assert_eq!(truncate_for_width("你好世界", 5), "你好…");
    }

    #[test]
    fn history_scroll_is_bottom_anchored() {
        assert_eq!(history_scroll_offset(30, 10, 0), 20);
        assert_eq!(history_scroll_offset(30, 10, 6), 14);
        assert_eq!(history_scroll_offset(30, 10, 99), 0);
        assert_eq!(history_scroll_offset(5, 10, 0), 0);
    }

    #[test]
    fn history_gutter_has_stable_display_width() {
        for prefix in ["›", "•", "↳", "note", "thinking", "error"] {
            assert_eq!(UnicodeWidthStr::width(history_gutter(prefix).as_str()), 9);
        }
    }

    #[test]
    fn preprocessing_depth_constant_remains_available() {
        assert!(crate::MAX_COMMAND_PREPROCESS_DEPTH >= 1);
        assert_eq!(crate::CLI_CHANNEL, "cli");
        assert_eq!(crate::CLI_USERNAME, "local-user");
    }
}
