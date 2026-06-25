use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use bot_core::{
    CatEvent, Content, ContextCompactionEvent, ContextCompactionStatus, Message, PrettyToolCall,
    PrettyToolStatus, StreamOptions, SupervisorTraceEvent, ThreadHistoryMessage, TokenUsage,
    ToolApprovalDecision, ToolApprovalRequest, UserQuestionRequest, UserQuestionResponse,
    UserQuestionStatus, WorkflowReport,
};
use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::style::ResetColor;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Color, Line, Modifier, Span, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use remi_agentloop::prelude::{SubSessionEvent, SubSessionEventPayload};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

mod components;
mod composer;
mod render;

use components::*;
use composer::*;

use crate::session::{ChannelBinding, Session, SubSessionKind};
use crate::tui_markdown::{render_markdown_lines, MarkdownTheme};
#[cfg(test)]
use crate::tui_text::contains_tui_control;
use crate::tui_text::sanitize_tui_text;
use crate::workspace_files::{
    default_file_search_limit, search_workspace_files, WorkspaceFileMatch,
};
use crate::{
    process_runtime_commands, CliConfig, Runtime, RuntimeCommandPipelineResult,
    SESSION_AGENT_ID_METADATA_KEY, SESSION_INPUT_HISTORY_METADATA_KEY,
    SESSION_MODEL_PROFILE_METADATA_KEY,
};

const TUI_CHANNEL: &str = "tui";
const MAX_HISTORY_CELLS: usize = 400;
const CODEX_CYAN: Color = Color::Rgb(109, 209, 255);
const CODEX_DIM: Color = Color::Rgb(118, 124, 134);
const CODEX_BORDER: Color = Color::Rgb(63, 68, 78);
const CODEX_GREEN: Color = Color::Rgb(122, 222, 151);
const FOOTER_INDENT: &str = "  ";
const HISTORY_GUTTER_WIDTH: u16 = 4;
const MAX_TOOL_BODY_CHARS: usize = 240;
const MAX_HISTORY_BODY_LINES: usize = 220;
const QUIT_HINT_TIMEOUT: Duration = Duration::from_secs(1);
const PASTE_CHUNK_LINE_THRESHOLD: usize = 3;
const PASTE_CHUNK_CHAR_THRESHOLD: usize = 1000;

type CrosstermTerminal = Terminal<CrosstermBackend<Stdout>>;

pub(crate) async fn run_tui(
    runtime: Rc<Runtime>,
    cli: CliConfig,
    trigger_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<crate::local_trigger_scheduler::LocalTriggerDispatch>,
    >,
) -> anyhow::Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let session_id = if cli.resume {
        if let Some(selector) = cli.resume_session_id.as_deref() {
            resolve_resume_session_id(&runtime, selector).await?
        } else {
            select_resume_session_id(&runtime, &mut terminal.terminal).await?
        }
    } else {
        let session_channel = tui_start_channel_id(&cli.channel_id);
        runtime
            .sessions
            .lock()
            .await
            .create_channel(TUI_CHANNEL, &session_channel, &runtime.root_agent_id, None)?
            .id
    };

    let mut app = TuiApp::new(runtime, cli, session_id, trigger_rx).await;
    let result = app.run(&mut terminal.terminal).await;
    terminal.restore()?;
    result
}

fn tui_start_channel_id(cli_channel_id: &str) -> String {
    if cli_channel_id == crate::CLI_CHAT_ID {
        format!("tui:{}", uuid::Uuid::new_v4())
    } else {
        cli_channel_id.to_string()
    }
}

async fn select_resume_session_id(
    runtime: &Rc<Runtime>,
    terminal: &mut CrosstermTerminal,
) -> anyhow::Result<String> {
    let mut sessions = runtime.sessions.lock().await.list();
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    if sessions.is_empty() {
        anyhow::bail!("no sessions available to resume");
    }

    let mut events = EventStream::new();
    let mut selected = 0usize;

    loop {
        terminal
            .draw(|frame| render_resume_selector(frame, &sessions, selected))
            .context("draw resume selector")?;

        let Some(event) = events.next().await else {
            anyhow::bail!("resume selection ended");
        };
        match event.context("read resume selector event")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                    anyhow::bail!("resume cancelled");
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(sessions.len() - 1);
                }
                KeyCode::PageUp => {
                    selected = selected.saturating_sub(10);
                }
                KeyCode::PageDown => {
                    selected = (selected + 10).min(sessions.len() - 1);
                }
                KeyCode::Home => selected = 0,
                KeyCode::End => selected = sessions.len() - 1,
                KeyCode::Enter => return Ok(sessions[selected].id.clone()),
                KeyCode::Char(ch) if ch.is_ascii_digit() => {
                    if let Some(number) = ch.to_digit(10).map(|value| value as usize) {
                        if number > 0 && number <= sessions.len().min(9) {
                            return Ok(sessions[number - 1].id.clone());
                        }
                    }
                }
                _ => {}
            },
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => selected = selected.saturating_sub(3),
                MouseEventKind::ScrollDown => selected = (selected + 3).min(sessions.len() - 1),
                _ => {}
            },
            _ => {}
        }
    }
}

fn render_resume_selector(frame: &mut Frame<'_>, sessions: &[Session], selected: usize) {
    let area = frame.area();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(4),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "Resume session",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "  choose a previous conversation",
                    Style::default().fg(CODEX_DIM),
                ),
            ]),
            Line::from(Span::styled(
                "Up/Down choose · Enter resume · 1-9 quick select · Esc cancel",
                Style::default().fg(CODEX_DIM),
            )),
        ]),
        layout[0],
    );

    let list_height = layout[1].height.saturating_sub(2).max(1) as usize;
    let offset = selected
        .saturating_sub(list_height.saturating_sub(1))
        .min(sessions.len().saturating_sub(list_height));
    let items = sessions
        .iter()
        .enumerate()
        .skip(offset)
        .take(list_height)
        .map(|(index, session)| {
            let title = sanitize_tui_text(session.title.as_deref().unwrap_or("untitled"));
            let marker = if index == selected { "›" } else { " " };
            let prefix = if index < 9 {
                format!("{marker} {}. ", index + 1)
            } else {
                format!("{marker}    ")
            };
            ListItem::new(Line::from(vec![
                Span::styled(prefix, Style::default().fg(CODEX_CYAN)),
                Span::styled(
                    short_session_id(&session.id),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    format!(
                        "{}:{}",
                        session.channel_binding.platform, session.channel_binding.channel_id
                    ),
                    Style::default().fg(CODEX_DIM),
                ),
                Span::raw("  "),
                Span::raw(truncate_for_width(&title, 36)),
                Span::styled(
                    format!("  {}", session.updated_at),
                    Style::default().fg(CODEX_DIM),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::TOP | Borders::BOTTOM)),
        layout[1],
    );

    let session = &sessions[selected];
    let title = sanitize_tui_text(session.title.as_deref().unwrap_or("untitled"));
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("selected ", Style::default().fg(CODEX_DIM)),
                Span::styled(&session.id, Style::default().fg(CODEX_CYAN)),
            ]),
            Line::from(format!(
                "{}:{} · {} · {}",
                session.channel_binding.platform,
                session.channel_binding.channel_id,
                title,
                session.updated_at
            )),
        ])
        .wrap(Wrap { trim: true }),
        layout[2],
    );
}

fn short_session_id(id: &str) -> String {
    if id.chars().count() <= 12 {
        id.to_string()
    } else {
        format!("{}…", id.chars().take(11).collect::<String>())
    }
}

async fn resolve_resume_session_id(
    runtime: &Rc<Runtime>,
    selector: &str,
) -> anyhow::Result<String> {
    let selector = selector.trim();
    if selector.is_empty() {
        anyhow::bail!("--resume requires a session id, unique prefix, title, or channel id");
    }

    let sessions = runtime.sessions.lock().await;
    if let Some(session) = sessions.get(selector) {
        return Ok(session.id);
    }

    let mut matches = sessions
        .list()
        .into_iter()
        .filter(|session| {
            session.id.starts_with(selector)
                || session.title.as_deref() == Some(selector)
                || session.channel_binding.channel_id == selector
        })
        .collect::<Vec<_>>();
    matches.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    match matches.as_slice() {
        [session] => Ok(session.id.clone()),
        [] => anyhow::bail!("no session matches resume selector `{selector}`"),
        _ => {
            let candidates = matches
                .iter()
                .take(5)
                .map(|session| {
                    let title = session.title.as_deref().unwrap_or("untitled");
                    format!(
                        "{}  {}:{}  {title}",
                        session.id,
                        session.channel_binding.platform,
                        session.channel_binding.channel_id
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!("resume selector `{selector}` is ambiguous:\n{candidates}");
        }
    }
}

struct TerminalGuard {
    terminal: CrosstermTerminal,
    restored: bool,
}

impl TerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        enable_raw_mode().context("enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )
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
        execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            DisableMouseCapture,
            ResetColor,
            Show,
            EnableLineWrap
        )
        .context("restore terminal modes")?;
        drain_terminal_events();
        disable_raw_mode().context("disable raw mode")?;
        execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            DisableMouseCapture,
            ResetColor,
            Show,
            EnableLineWrap,
            LeaveAlternateScreen
        )
        .context("leave alternate screen")?;
        self.terminal.show_cursor().context("show cursor")?;
        self.restored = true;
        Ok(())
    }
}

fn drain_terminal_events() {
    while crossterm::event::poll(Duration::from_millis(0)).unwrap_or(false) {
        if crossterm::event::read().is_err() {
            break;
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn current_workspace_dir(data_dir: &std::path::Path) -> std::path::PathBuf {
    std::env::var_os("REMI_SANDBOX_HOST_DIR")
        .map(std::path::PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| data_dir.to_path_buf())
}

fn current_workspace_root_label(workspace_dir: &std::path::Path) -> String {
    let kind = std::env::var("REMI_SANDBOX_KIND")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase());
    if matches!(kind.as_deref(), Some("docker")) {
        std::env::var("REMI_SANDBOX_CONTAINER_DIR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "/workspace".to_string())
    } else {
        workspace_dir.display().to_string()
    }
}

fn compact_workspace_label(label: &str) -> String {
    let sanitized = sanitize_tui_text(label);
    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        return ".".to_string();
    }
    let path = std::path::Path::new(trimmed);
    if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
        if !name.trim().is_empty() {
            return name.to_string();
        }
    }
    trimmed.to_string()
}

fn current_git_branch(workspace_dir: &std::path::Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_dir)
        .arg("rev-parse")
        .arg("--abbrev-ref")
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

struct TuiApp {
    runtime: Rc<Runtime>,
    cli: CliConfig,
    session_id: String,
    cells: Vec<HistoryCell>,
    composer: ComposerInput,
    scroll: u16,
    command_catalog: Vec<CommandEntry>,
    workspace_dir: std::path::PathBuf,
    workspace_root_label: String,
    git_branch: Option<String>,
    file_query: Option<String>,
    file_matches: Vec<WorkspaceFileMatch>,
    popup_selected: usize,
    show_shortcuts: bool,
    quit_hint_until: Option<Instant>,
    running: bool,
    run_started_at: Option<Instant>,
    interrupt_requested: bool,
    cancel: Option<Arc<Notify>>,
    run_handle: Option<JoinHandle<()>>,
    queued_inputs: VecDeque<String>,
    input_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: Option<String>,
    status: StatusLine,
    last_stats_snapshot: TokenStatsSnapshot,
    pending_token_delta: TokenDelta,
    last_token_cell_index: Option<usize>,
    active_tool_args: std::collections::HashMap<String, String>,
    active_tool_names: std::collections::HashMap<String, String>,
    active_tool_started_at: std::collections::HashMap<String, Instant>,
    sub_tool_args: std::collections::HashMap<String, String>,
    sub_tool_names: std::collections::HashMap<String, String>,
    sub_sessions: std::collections::HashMap<String, SubSessionUiState>,
    opened_sub_session_panes: std::collections::HashSet<String>,
    supervisors: std::collections::HashMap<String, SupervisorUiState>,
    pending_approval: Option<ToolApprovalRequest>,
    approval_selected: usize,
    approval_state: &'static str,
    pending_user_question: Option<UserQuestionRequest>,
    user_question_selected: usize,
    user_question_state: &'static str,
    active_supervisor_id: Option<String>,
    last_todo_body: Option<String>,
    latest_active_todo_label: Option<String>,
    loaded_thread_history_len: usize,
    sub_session_event_log_lines: usize,
    bot_tx: mpsc::UnboundedSender<BotEvent>,
    bot_rx: UnboundedReceiverStream<BotEvent>,
    trigger_rx:
        Option<UnboundedReceiverStream<crate::local_trigger_scheduler::LocalTriggerDispatch>>,
}

impl TuiApp {
    async fn new(
        runtime: Rc<Runtime>,
        cli: CliConfig,
        session_id: String,
        trigger_rx: Option<
            tokio::sync::mpsc::UnboundedReceiver<
                crate::local_trigger_scheduler::LocalTriggerDispatch,
            >,
        >,
    ) -> Self {
        let (bot_tx, bot_rx) = mpsc::unbounded_channel();
        let workspace_dir = current_workspace_dir(&runtime.data_dir);
        let workspace_root_label = current_workspace_root_label(&workspace_dir);
        let git_branch = current_git_branch(&workspace_dir);
        let mut app = Self {
            runtime,
            cli,
            session_id,
            cells: Vec::new(),
            composer: ComposerInput::default(),
            scroll: 0,
            command_catalog: Vec::new(),
            workspace_dir,
            workspace_root_label,
            git_branch,
            file_query: None,
            file_matches: Vec::new(),
            popup_selected: 0,
            show_shortcuts: false,
            quit_hint_until: None,
            running: false,
            run_started_at: None,
            interrupt_requested: false,
            cancel: None,
            run_handle: None,
            queued_inputs: VecDeque::new(),
            input_history: Vec::new(),
            history_index: None,
            history_draft: None,
            status: StatusLine::default(),
            last_stats_snapshot: TokenStatsSnapshot::default(),
            pending_token_delta: TokenDelta::default(),
            last_token_cell_index: None,
            active_tool_args: std::collections::HashMap::new(),
            active_tool_names: std::collections::HashMap::new(),
            active_tool_started_at: std::collections::HashMap::new(),
            sub_tool_args: std::collections::HashMap::new(),
            sub_tool_names: std::collections::HashMap::new(),
            sub_sessions: std::collections::HashMap::new(),
            opened_sub_session_panes: std::collections::HashSet::new(),
            supervisors: std::collections::HashMap::new(),
            pending_approval: None,
            approval_selected: 0,
            approval_state: "waiting",
            pending_user_question: None,
            user_question_selected: 0,
            user_question_state: "waiting",
            active_supervisor_id: None,
            last_todo_body: None,
            latest_active_todo_label: None,
            loaded_thread_history_len: 0,
            sub_session_event_log_lines: 0,
            bot_tx,
            bot_rx: UnboundedReceiverStream::new(bot_rx),
            trigger_rx: trigger_rx.map(UnboundedReceiverStream::new),
        };
        app.refresh_command_catalog();
        app.load_input_history().await;
        app.cells.push(HistoryCell::system(
            "Remi Cat TUI ready. Type / for commands. Ctrl+C cancels a run or exits when idle.",
        ));
        app.cells.push(HistoryCell::system(format!(
            "session id: {}",
            app.session_id
        )));
        app.load_thread_history().await;
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
                    self.handle_bot_event(event).await;
                }
                maybe_trigger = next_trigger_dispatch(&mut self.trigger_rx) => {
                    if let Some(dispatch) = maybe_trigger {
                        self.start_trigger_turn(dispatch);
                    }
                }
                _ = tick.tick() => {
                    self.flush_status_elapsed();
                    self.poll_sub_session_history().await;
                }
            }
        }
        Ok(())
    }

    async fn handle_terminal_event(&mut self, event: Event) -> anyhow::Result<bool> {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key).await,
            Event::Paste(text) => {
                if self.composer.is_empty() && text == "?" {
                    self.show_shortcuts = !self.show_shortcuts;
                    return Ok(false);
                }
                self.insert_paste(text);
                Ok(false)
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.scroll = self.scroll.saturating_add(3);
                    Ok(false)
                }
                MouseEventKind::ScrollDown => {
                    self.scroll = self.scroll.saturating_sub(3);
                    Ok(false)
                }
                _ => Ok(false),
            },
            Event::Resize(_, _) => Ok(false),
            _ => Ok(false),
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        if self.pending_approval.is_some() {
            match key.code {
                KeyCode::Up => {
                    self.move_approval_selection(-1);
                    return Ok(false);
                }
                KeyCode::Down => {
                    self.move_approval_selection(1);
                    return Ok(false);
                }
                KeyCode::Enter => {
                    let decision = approval_option(self.approval_selected)
                        .map(|option| option.decision)
                        .unwrap_or(ToolApprovalDecision::Deny);
                    self.decide_pending_approval(decision).await;
                    return Ok(false);
                }
                KeyCode::Char('1') => {
                    self.decide_pending_approval(ToolApprovalDecision::AllowOnce)
                        .await;
                    return Ok(false);
                }
                KeyCode::Char('2') => {
                    self.decide_pending_approval(ToolApprovalDecision::AllowSession)
                        .await;
                    return Ok(false);
                }
                KeyCode::Char('3') => {
                    self.decide_pending_approval(ToolApprovalDecision::AllowSessionModelAuto)
                        .await;
                    return Ok(false);
                }
                KeyCode::Char('4') => {
                    self.decide_pending_approval(ToolApprovalDecision::Deny)
                        .await;
                    return Ok(false);
                }
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.decide_pending_approval(ToolApprovalDecision::AllowOnce)
                        .await;
                    return Ok(false);
                }
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    self.decide_pending_approval(ToolApprovalDecision::AllowSession)
                        .await;
                    return Ok(false);
                }
                KeyCode::Char('m') | KeyCode::Char('M') => {
                    self.decide_pending_approval(ToolApprovalDecision::AllowSessionModelAuto)
                        .await;
                    return Ok(false);
                }
                KeyCode::Char('p') | KeyCode::Char('P') => {
                    self.decide_pending_approval(ToolApprovalDecision::AllowSession)
                        .await;
                    return Ok(false);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.decide_pending_approval(ToolApprovalDecision::Deny)
                        .await;
                    return Ok(false);
                }
                _ => {}
            }
        }

        if self.pending_user_question.is_some() {
            match key.code {
                KeyCode::Up => {
                    self.move_user_question_selection(-1);
                    return Ok(false);
                }
                KeyCode::Down => {
                    self.move_user_question_selection(1);
                    return Ok(false);
                }
                KeyCode::Enter => {
                    self.answer_pending_user_question(false).await;
                    return Ok(false);
                }
                KeyCode::Esc => {
                    self.answer_pending_user_question(true).await;
                    return Ok(false);
                }
                KeyCode::Char(ch) if ch.is_ascii_digit() && self.composer.is_empty() => {
                    if let Some(index) = ch.to_digit(10).and_then(|digit| digit.checked_sub(1)) {
                        let index = index as usize;
                        if self
                            .pending_user_question
                            .as_ref()
                            .is_some_and(|request| index < request.options.len())
                        {
                            self.user_question_selected = index;
                            self.answer_pending_user_question(false).await;
                            return Ok(false);
                        }
                    }
                }
                _ => {}
            }
        }

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
            KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => {
                self.recall_history(-1);
                Ok(false)
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => {
                self.recall_history(1);
                Ok(false)
            }
            KeyCode::Esc => {
                match self.active_popup() {
                    Some(PopupKind::Command) => self.composer.clear(),
                    Some(PopupKind::File) => {
                        self.file_query = None;
                        self.file_matches.clear();
                    }
                    None => {}
                }
                self.show_shortcuts = false;
                self.quit_hint_until = None;
                self.popup_selected = 0;
                Ok(false)
            }
            KeyCode::Char('?') if self.composer.is_empty() => {
                self.show_shortcuts = !self.show_shortcuts;
                Ok(false)
            }
            KeyCode::Char('/')
                if self.composer.is_empty() && key.modifiers.contains(KeyModifiers::SHIFT) =>
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
            KeyCode::Up => {
                if self.popup_visible() {
                    self.move_popup_selection(-1);
                } else if self.composer.display_text().contains('\n') {
                    self.move_input_cursor_vertical(-1);
                } else {
                    self.recall_history(-1);
                }
                Ok(false)
            }
            KeyCode::Down => {
                if self.popup_visible() {
                    self.move_popup_selection(1);
                } else if self.composer.display_text().contains('\n') {
                    self.move_input_cursor_vertical(1);
                } else {
                    self.recall_history(1);
                }
                Ok(false)
            }
            KeyCode::Tab if self.popup_visible() => {
                match self.active_popup() {
                    Some(PopupKind::Command) => self.complete_selected_command(),
                    Some(PopupKind::File) => self.complete_selected_file(),
                    None => {}
                }
                Ok(false)
            }
            KeyCode::BackTab => {
                self.cycle_agent().await?;
                Ok(false)
            }
            KeyCode::Enter if self.popup_visible() => {
                match self.active_popup() {
                    Some(PopupKind::Command) if self.selected_command_accepts_arguments() => {
                        self.complete_selected_command();
                    }
                    Some(PopupKind::File) => self.complete_selected_file(),
                    _ => {
                        return self.submit().await;
                    }
                }
                Ok(false)
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_text("\n");
                Ok(false)
            }
            KeyCode::Enter => self.submit().await,
            KeyCode::Backspace => {
                self.backspace();
                Ok(false)
            }
            KeyCode::Delete => {
                self.delete();
                Ok(false)
            }
            KeyCode::Left => {
                self.composer.move_left();
                self.refresh_file_matches();
                Ok(false)
            }
            KeyCode::Right => {
                self.composer.move_right();
                self.refresh_file_matches();
                Ok(false)
            }
            KeyCode::Home => {
                self.composer.move_home();
                self.refresh_file_matches();
                Ok(false)
            }
            KeyCode::End => {
                self.composer.move_end();
                self.refresh_file_matches();
                Ok(false)
            }
            KeyCode::Char(ch) => {
                self.insert_char(ch);
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    async fn submit(&mut self) -> anyhow::Result<bool> {
        let text = self.composer.to_text().trim().to_string();
        if text.is_empty() {
            return Ok(false);
        }
        if self.pending_user_question.is_some() {
            self.answer_pending_user_question(false).await;
            return Ok(false);
        }
        self.composer.clear();
        self.history_index = None;
        self.history_draft = None;
        self.scroll = 0;
        self.popup_selected = 0;
        self.show_shortcuts = false;
        self.quit_hint_until = None;
        self.record_input_history(text.clone()).await;
        if is_tui_exit_command(&text) {
            self.cells.push(HistoryCell::user(text));
            return Ok(true);
        }
        if is_tui_fork_command(&text) {
            self.start_fork_command();
            return Ok(false);
        }
        if is_tui_new_command(&text) {
            self.start_new_command();
            return Ok(false);
        }
        if self.running {
            self.queued_inputs.push_back(text);
            return Ok(false);
        }
        self.start_turn(text);
        Ok(false)
    }

    fn start_fork_command(&mut self) {
        self.cells.push(HistoryCell::user("/fork".to_string()));
        if self.running {
            self.cells.push(HistoryCell::system(
                "当前 session 正在运行，结束或取消后再 fork。",
            ));
            return;
        }
        self.cells
            .push(HistoryCell::system("fork: 准备复制当前 session..."));
        let runtime = Rc::clone(&self.runtime);
        let session_id = self.session_id.clone();
        let cli = self.cli.clone();
        let tx = self.bot_tx.clone();
        tokio::task::spawn_local(async move {
            run_tui_fork_command(runtime, session_id, cli, tx).await;
        });
    }

    fn start_new_command(&mut self) {
        self.cells.push(HistoryCell::user("/new".to_string()));
        if self.running {
            self.cells.push(HistoryCell::system(
                "当前 session 正在运行，结束或取消后再创建新 session。",
            ));
            return;
        }
        self.cells
            .push(HistoryCell::system("new: 正在创建空 session..."));
        let runtime = Rc::clone(&self.runtime);
        let cli = self.cli.clone();
        let tx = self.bot_tx.clone();
        tokio::task::spawn_local(async move {
            run_tui_new_command(runtime, cli, tx).await;
        });
    }

    async fn cycle_agent(&mut self) -> anyhow::Result<()> {
        if self.running {
            self.cells.push(HistoryCell::system(
                "当前 session 正在运行，结束或取消后再切换 agent。",
            ));
            return Ok(());
        }
        let mut agents = self.runtime.bot.agent_profiles();
        agents.sort_by(|a, b| a.id.cmp(&b.id));
        if agents.is_empty() {
            self.cells.push(HistoryCell::system("没有可切换的 agent。"));
            return Ok(());
        }
        let current = self
            .runtime
            .sessions
            .lock()
            .await
            .metadata_string(&self.session_id, SESSION_AGENT_ID_METADATA_KEY);
        let effective = self
            .runtime
            .bot
            .effective_agent_profile(current.as_deref())
            .profile
            .id;
        let current_index = agents
            .iter()
            .position(|agent| agent.id == effective)
            .unwrap_or(0);
        let next = agents[(current_index + 1) % agents.len()].id.clone();
        self.runtime.sessions.lock().await.set_metadata_string(
            &self.session_id,
            SESSION_AGENT_ID_METADATA_KEY,
            &next,
        )?;
        self.cells.push(HistoryCell::system(format!(
            "已切换当前 session agent 为 `{next}`。"
        )));
        Ok(())
    }

    async fn replace_current_session(&mut self, session_id: String, message: String) {
        self.session_id = session_id;
        self.cells.clear();
        self.composer.clear();
        self.scroll = 0;
        self.popup_selected = 0;
        self.file_query = None;
        self.file_matches.clear();
        self.show_shortcuts = false;
        self.quit_hint_until = None;
        self.queued_inputs.clear();
        self.input_history.clear();
        self.history_index = None;
        self.history_draft = None;
        self.status = StatusLine::default();
        self.active_tool_args.clear();
        self.active_tool_names.clear();
        self.active_tool_started_at.clear();
        self.sub_tool_args.clear();
        self.sub_tool_names.clear();
        self.sub_sessions.clear();
        self.opened_sub_session_panes.clear();
        self.supervisors.clear();
        self.pending_approval = None;
        self.approval_selected = 0;
        self.approval_state = "waiting";
        self.active_supervisor_id = None;
        self.last_todo_body = None;
        self.latest_active_todo_label = None;
        self.loaded_thread_history_len = 0;
        self.sub_session_event_log_lines = 0;
        self.load_input_history().await;
        self.cells.push(HistoryCell::system(message));
        self.cells.push(HistoryCell::system(format!(
            "session id: {}",
            self.session_id
        )));
        self.load_thread_history().await;
    }

    fn start_turn(&mut self, text: String) {
        self.cells.push(HistoryCell::user(text.clone()));
        while self.cells.len() > MAX_HISTORY_CELLS {
            self.cells.remove(0);
        }

        let cancel = Arc::new(Notify::new());
        self.cancel = Some(Arc::clone(&cancel));
        self.running = true;
        self.run_started_at = Some(Instant::now());
        self.interrupt_requested = false;
        self.status.state = "running".to_string();
        self.status.last_error = None;
        self.last_stats_snapshot = TokenStatsSnapshot::default();
        self.pending_token_delta = TokenDelta::default();
        self.last_token_cell_index = None;
        self.active_supervisor_id = Some(format!("supervisor-{}", uuid::Uuid::new_v4()));

        let runtime = Rc::clone(&self.runtime);
        let session_id = self.session_id.clone();
        let sender_user_id = self.cli.user_id.clone();
        let sender_username = self.cli.username.clone();
        let tx = self.bot_tx.clone();
        self.run_handle = Some(tokio::task::spawn_local(async move {
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
        }));
    }

    fn start_trigger_turn(
        &mut self,
        dispatch: crate::local_trigger_scheduler::LocalTriggerDispatch,
    ) {
        self.cells.push(HistoryCell::system(format!(
            "触发器「{}」已触发，开始执行。",
            dispatch.trigger_name
        )));
        while self.cells.len() > MAX_HISTORY_CELLS {
            self.cells.remove(0);
        }

        let cancel = Arc::new(Notify::new());
        self.cancel = Some(Arc::clone(&cancel));
        self.running = true;
        self.run_started_at = Some(Instant::now());
        self.interrupt_requested = false;
        self.status.state = "running".to_string();
        self.status.last_error = None;
        self.last_stats_snapshot = TokenStatsSnapshot::default();
        self.pending_token_delta = TokenDelta::default();
        self.last_token_cell_index = None;
        self.active_supervisor_id = Some(format!("supervisor-{}", uuid::Uuid::new_v4()));

        let runtime = Rc::clone(&self.runtime);
        let tx = self.bot_tx.clone();
        self.run_handle = Some(tokio::task::spawn_local(async move {
            run_tui_trigger_turn(runtime, dispatch, cancel, tx).await;
        }));
    }

    async fn handle_bot_event(&mut self, event: BotEvent) {
        match event {
            BotEvent::Prefix(text) => {
                self.cells.push(HistoryCell::assistant(text));
                self.mark_token_cell(self.cells.len().saturating_sub(1));
            }
            BotEvent::Text(delta) => self.push_assistant_delta(&delta),
            BotEvent::Thinking(delta) => self.push_thinking_delta(&delta),
            BotEvent::ToolStart { id, name } => {
                self.active_tool_args.insert(id.clone(), String::new());
                self.active_tool_names.insert(id.clone(), name.clone());
                self.active_tool_started_at
                    .insert(id.clone(), Instant::now());
                let pretty = PrettyToolCall::started(&id, &name, &empty_tool_args());
                if name == "apply_patch" {
                    self.cells.push(HistoryCell::patch_diff(
                        id.clone(),
                        "waiting for patch...".to_string(),
                        format_elapsed(0),
                        ToolVisualStatus::Running,
                    ));
                } else {
                    let body = tool_body(&pretty);
                    self.cells.push(HistoryCell::tool(
                        id.clone(),
                        pretty.title,
                        body,
                        format_elapsed(0),
                        ToolVisualStatus::Running,
                    ));
                }
                if let Some(index) = self
                    .cells
                    .iter()
                    .rposition(|cell| cell.tool_id().is_some_and(|tool_id| tool_id == id))
                {
                    self.mark_token_cell(index);
                }
            }
            BotEvent::ToolArgs { id, delta } => {
                let args = self.active_tool_args.entry(id.clone()).or_default();
                args.push_str(&delta);
                let is_patch = self
                    .active_tool_names
                    .get(&id)
                    .is_some_and(|name| name == "apply_patch");
                if let Some(cell) = self
                    .cells
                    .iter_mut()
                    .rev()
                    .find(|cell| cell.tool_id().is_some_and(|tool_id| tool_id == id))
                {
                    if is_patch {
                        if let Some(patch) = extract_patch_arg(args) {
                            cell.body = patch;
                        }
                    } else {
                        if let (Some(name), Some(args_value)) =
                            (self.active_tool_names.get(&id), parse_tool_args(args))
                        {
                            let pretty = PrettyToolCall::started(&id, name, &args_value);
                            let body = tool_body(&pretty);
                            cell.title = pretty.title;
                            cell.body = body;
                        } else if cell.body.trim().is_empty() {
                            cell.body = "reading tool arguments...".to_string();
                        }
                    }
                }
            }
            BotEvent::ToolCall { id, name, args } => {
                self.active_tool_args.insert(id.clone(), args.clone());
                self.active_tool_names.insert(id.clone(), name.clone());
                self.active_tool_started_at
                    .entry(id.clone())
                    .or_insert_with(Instant::now);
                let args_value = parse_tool_args(&args).unwrap_or(serde_json::Value::Null);
                let pretty = PrettyToolCall::started(&id, &name, &args_value);
                let existing = self
                    .cells
                    .iter_mut()
                    .rev()
                    .find(|cell| cell.tool_id().is_some_and(|tool_id| tool_id == id));
                if name == "apply_patch" {
                    let body =
                        extract_patch_arg(&args).unwrap_or_else(|| "reading patch...".to_string());
                    if let Some(cell) = existing {
                        cell.body = body;
                    } else {
                        self.cells.push(HistoryCell::patch_diff(
                            id.clone(),
                            body,
                            format_elapsed(0),
                            ToolVisualStatus::Running,
                        ));
                    }
                } else if let Some(cell) = existing {
                    let body = tool_body(&pretty);
                    cell.title = pretty.title;
                    cell.body = body;
                } else {
                    let body = tool_body(&pretty);
                    self.cells.push(HistoryCell::tool(
                        id.clone(),
                        pretty.title,
                        body,
                        format_elapsed(0),
                        ToolVisualStatus::Running,
                    ));
                }
                if let Some(index) = self
                    .cells
                    .iter()
                    .rposition(|cell| cell.tool_id().is_some_and(|tool_id| tool_id == id))
                {
                    self.mark_token_cell(index);
                }
            }
            BotEvent::ToolDone {
                id,
                name,
                args,
                result,
                success,
                elapsed_ms,
            } => {
                let args = if args.trim().is_empty() {
                    self.active_tool_args.remove(&id).unwrap_or_default()
                } else {
                    self.active_tool_args.remove(&id);
                    args
                };
                self.active_tool_names.remove(&id);
                self.active_tool_started_at.remove(&id);
                let args_value = parse_tool_args(&args).unwrap_or(serde_json::Value::Null);
                let pretty = PrettyToolCall::completed(
                    &id,
                    &name,
                    &args_value,
                    &result,
                    success,
                    elapsed_ms,
                );
                if name == "apply_patch" {
                    let patch = extract_patch_arg(&args)
                        .unwrap_or_else(|| "patch arguments unavailable".to_string());
                    let meta = patch_tool_meta(&pretty);
                    if let Some(cell) = self
                        .cells
                        .iter_mut()
                        .rev()
                        .find(|cell| cell.tool_id().is_some_and(|tool_id| tool_id == id))
                    {
                        let meta = preserve_token_meta(meta, &cell.meta);
                        *cell = HistoryCell::patch_diff(
                            id.clone(),
                            patch,
                            meta,
                            ToolVisualStatus::from_success(success),
                        );
                    } else {
                        self.cells.push(HistoryCell::patch_diff(
                            id.clone(),
                            patch,
                            meta,
                            ToolVisualStatus::from_success(success),
                        ));
                    }
                    if let Some(index) = self
                        .cells
                        .iter()
                        .rposition(|cell| cell.tool_id().is_some_and(|tool_id| tool_id == id))
                    {
                        self.mark_token_cell(index);
                    }
                    return;
                }
                let status = ToolVisualStatus::from_pretty(&pretty.status);
                let meta = tool_meta(&pretty);
                if let Some(cell) = self
                    .cells
                    .iter_mut()
                    .rev()
                    .find(|cell| cell.tool_id().is_some_and(|tool_id| tool_id == id))
                {
                    let body = tool_body(&pretty);
                    let meta = preserve_token_meta(meta, &cell.meta);
                    cell.title = pretty.title;
                    cell.body = body;
                    cell.meta = meta;
                    cell.status = status;
                } else {
                    let body = tool_body(&pretty);
                    self.cells.push(HistoryCell::tool(
                        id.clone(),
                        pretty.title,
                        body,
                        meta,
                        status,
                    ));
                }
                if let Some(index) = self
                    .cells
                    .iter()
                    .rposition(|cell| cell.tool_id().is_some_and(|tool_id| tool_id == id))
                {
                    self.mark_token_cell(index);
                }
            }
            BotEvent::ToolDelta { id, name, delta } => {
                self.active_tool_names.insert(id.clone(), name.clone());
                self.active_tool_started_at
                    .entry(id.clone())
                    .or_insert_with(Instant::now);
                let meta = self
                    .active_tool_started_at
                    .get(&id)
                    .map(|started| format_elapsed(started.elapsed().as_millis() as u64))
                    .unwrap_or_else(|| format_elapsed(0));
                if let Some(cell) = self
                    .cells
                    .iter_mut()
                    .rev()
                    .find(|cell| cell.tool_id().is_some_and(|tool_id| tool_id == id))
                {
                    cell.title = format!("调用 {name}");
                    cell.body = truncate_chars(&delta, MAX_TOOL_BODY_CHARS);
                    cell.meta = preserve_token_meta(meta, &cell.meta);
                    cell.status = ToolVisualStatus::Running;
                } else {
                    self.cells.push(HistoryCell::tool(
                        id.clone(),
                        format!("调用 {name}"),
                        truncate_chars(&delta, MAX_TOOL_BODY_CHARS),
                        meta,
                        ToolVisualStatus::Running,
                    ));
                }
                if let Some(index) = self
                    .cells
                    .iter()
                    .rposition(|cell| cell.tool_id().is_some_and(|tool_id| tool_id == id))
                {
                    self.mark_token_cell(index);
                }
            }
            BotEvent::SupervisorProgress(progress) => self.upsert_supervisor_progress(progress),
            BotEvent::SupervisorReport(report) => self.upsert_supervisor_report(report),
            BotEvent::SubSession(event) => self.upsert_sub_session(event).await,
            BotEvent::ContextCompaction(event) => {
                upsert_context_compaction_cell(&mut self.cells, context_compaction_cell(event));
            }
            BotEvent::TodoState {
                body,
                latest_active,
            } => {
                if self.last_todo_body.as_deref() == Some(body.as_str()) {
                    self.latest_active_todo_label = latest_active;
                    return;
                }
                self.latest_active_todo_label = latest_active;
                self.last_todo_body = Some(body.clone());
                if let Some(cell) = self
                    .cells
                    .iter_mut()
                    .rev()
                    .find(|cell| matches!(cell.kind, CellKind::TodoState))
                {
                    *cell = HistoryCell::todo_state(body);
                } else {
                    self.cells.push(HistoryCell::todo_state(body));
                }
            }
            BotEvent::ApprovalRequested(request) => {
                self.pending_approval = Some(request.clone());
                self.approval_selected = 0;
                self.approval_state = "waiting";
                self.upsert_approval_cell(approval_cell(
                    &request,
                    self.approval_state,
                    0,
                    None,
                    None,
                ));
            }
            BotEvent::ApprovalUpdated(request) => {
                self.pending_approval = Some(request.clone());
                self.approval_selected = self.approval_selected.min(approval_options_len() - 1);
                self.approval_state = "reviewed";
                self.upsert_approval_cell(approval_cell(
                    &request,
                    self.approval_state,
                    self.approval_selected,
                    None,
                    None,
                ));
            }
            BotEvent::ApprovalResolved { request, decision } => {
                if self
                    .pending_approval
                    .as_ref()
                    .is_some_and(|pending| pending.id == request.id)
                {
                    self.pending_approval = None;
                }
                self.approval_state = "resolved";
                self.upsert_approval_cell(approval_cell(
                    &request,
                    "resolved",
                    self.approval_selected,
                    Some(format!("decision: {decision:?}")),
                    Some(decision),
                ));
            }
            BotEvent::UserQuestionRequested(request) => {
                self.pending_user_question = Some(request.clone());
                self.user_question_selected = 0;
                self.user_question_state = "waiting";
                self.upsert_user_question_cell(user_question_cell(
                    &request,
                    self.user_question_state,
                    self.user_question_selected,
                    None,
                    None,
                ));
            }
            BotEvent::UserQuestionUpdated(request) => {
                self.pending_user_question = Some(request.clone());
                self.user_question_selected = self
                    .user_question_selected
                    .min(request.options.len().saturating_sub(1));
                self.user_question_state = "waiting";
                self.upsert_user_question_cell(user_question_cell(
                    &request,
                    self.user_question_state,
                    self.user_question_selected,
                    None,
                    None,
                ));
            }
            BotEvent::UserQuestionResolved { request, response } => {
                if self
                    .pending_user_question
                    .as_ref()
                    .is_some_and(|pending| pending.id == request.id)
                {
                    self.pending_user_question = None;
                }
                self.user_question_state = "resolved";
                self.upsert_user_question_cell(user_question_cell(
                    &request,
                    "resolved",
                    self.user_question_selected,
                    response.answer_text.clone(),
                    Some(response.status),
                ));
            }
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
                self.update_cell_tokens_from_stats(prompt_tokens, completion_tokens);
            }
            BotEvent::ForkProgress(message) => {
                self.cells.push(HistoryCell::system(message));
            }
            BotEvent::SwitchToSession {
                session_id,
                message,
            } => {
                self.replace_current_session(session_id, message).await;
            }
            BotEvent::SessionCleared => {
                self.cells.retain(|cell| {
                    !matches!(cell.kind, CellKind::TodoState | CellKind::Supervisor { .. })
                });
                self.supervisors.clear();
                self.last_todo_body = None;
                self.latest_active_todo_label = None;
            }
            BotEvent::Error(message) => {
                self.status.last_error = Some(message.clone());
                self.cells.push(HistoryCell::error(message));
                self.running = false;
                self.interrupt_requested = false;
                self.cancel = None;
                self.run_handle = None;
                self.sub_tool_args.clear();
                self.sub_tool_names.clear();
                self.sub_sessions.clear();
                self.opened_sub_session_panes.clear();
                self.supervisors.clear();
                self.active_supervisor_id = None;
                self.status.state = "error".to_string();
            }
            BotEvent::Done => {
                self.running = false;
                self.interrupt_requested = false;
                self.cancel = None;
                self.run_handle = None;
                self.sub_tool_args.clear();
                self.sub_tool_names.clear();
                self.sub_sessions.clear();
                self.opened_sub_session_panes.clear();
                self.supervisors.clear();
                self.active_supervisor_id = None;
                self.status.state = "idle".to_string();
                self.refresh_command_catalog();
                if let Some(next) = self.queued_inputs.pop_front() {
                    self.start_turn(next);
                }
            }
        }
    }

    fn insert_char(&mut self, ch: char) {
        self.composer.insert_char(ch);
        self.popup_selected = 0;
        self.reset_history_navigation();
        self.quit_hint_until = None;
        self.refresh_file_matches();
    }

    fn insert_text(&mut self, text: &str) {
        self.composer.insert_text(text);
        self.popup_selected = 0;
        self.reset_history_navigation();
        self.quit_hint_until = None;
        self.refresh_file_matches();
    }

    fn insert_paste(&mut self, text: String) {
        self.composer.insert_paste(text);
        self.popup_selected = 0;
        self.reset_history_navigation();
        self.quit_hint_until = None;
        self.refresh_file_matches();
    }

    fn backspace(&mut self) {
        self.composer.backspace();
        self.popup_selected = 0;
        self.reset_history_navigation();
        self.refresh_file_matches();
    }

    fn delete(&mut self) {
        self.composer.delete();
        self.popup_selected = 0;
        self.reset_history_navigation();
        self.refresh_file_matches();
    }

    fn move_input_cursor_vertical(&mut self, direction: isize) {
        self.composer.move_vertical(direction);
        self.popup_selected = 0;
        self.reset_history_navigation();
        self.refresh_file_matches();
    }

    fn popup_visible(&self) -> bool {
        matches!(
            self.active_popup(),
            Some(PopupKind::Command | PopupKind::File)
        )
    }

    fn active_popup(&self) -> Option<PopupKind> {
        if self.composer.active_file_mention_token().is_some() && !self.file_matches.is_empty() {
            return Some(PopupKind::File);
        }
        let Some(input) = self.composer.command_text() else {
            return None;
        };
        let first_line = input.lines().next().unwrap_or("");
        if first_line.trim_start().starts_with('/') && !first_line.contains('\n') {
            Some(PopupKind::Command)
        } else {
            None
        }
    }

    fn filtered_commands(&self) -> Vec<&CommandEntry> {
        let Some(input) = self.composer.command_text() else {
            return Vec::new();
        };
        let terms = command_filter_terms(input);
        self.command_catalog
            .iter()
            .filter(|command| command_matches_filter(command, &terms))
            .collect()
    }

    fn refresh_file_matches(&mut self) {
        let Some(token) = self.composer.active_file_mention_token() else {
            self.file_query = None;
            self.file_matches.clear();
            return;
        };
        if self.file_query.as_deref() == Some(token.query.as_str()) {
            return;
        }
        self.file_query = Some(token.query.clone());
        self.file_matches = search_workspace_files(
            &self.workspace_dir,
            &self.workspace_root_label,
            &token.query,
            default_file_search_limit(Some(8)),
        )
        .unwrap_or_default();
        self.popup_selected = self
            .popup_selected
            .min(self.file_matches.len().saturating_sub(1));
    }

    fn complete_selected_command(&mut self) {
        let commands = self.filtered_commands();
        let Some(command) = commands.get(self.popup_selected).copied() else {
            return;
        };
        self.composer.set_text(command.value.clone());
    }

    fn selected_command_accepts_arguments(&self) -> bool {
        let commands = self.filtered_commands();
        commands
            .get(self.popup_selected)
            .is_some_and(|command| command.accepts_arguments)
    }

    fn complete_selected_file(&mut self) {
        let Some(token) = self.composer.active_file_mention_token() else {
            return;
        };
        let Some(file) = self.file_matches.get(self.popup_selected) else {
            return;
        };
        let replacement = format!("@{} ", file.mention_path);
        self.composer
            .replace_text_range(token.start, token.end, &replacement);
        self.file_query = None;
        self.file_matches.clear();
        self.popup_selected = 0;
    }

    fn move_popup_selection(&mut self, direction: isize) {
        let len = match self.active_popup() {
            Some(PopupKind::Command) => self.filtered_commands().len(),
            Some(PopupKind::File) => self.file_matches.len(),
            None => 0,
        };
        if len == 0 {
            self.popup_selected = 0;
        } else if direction < 0 {
            self.popup_selected = self.popup_selected.saturating_sub(1);
        } else {
            self.popup_selected = (self.popup_selected + 1).min(len - 1);
        }
    }

    fn handle_cancel_or_quit(&mut self) -> bool {
        if self.popup_visible() {
            self.composer.clear();
            self.popup_selected = 0;
            self.reset_history_navigation();
            return false;
        }
        if self.show_shortcuts {
            self.show_shortcuts = false;
            return false;
        }
        if self.running {
            if !push_interrupt_requested_once(&mut self.interrupt_requested, &mut self.cells) {
                self.status.state = "cancelling".to_string();
                return false;
            }
            if let Some(cancel) = &self.cancel {
                cancel.notify_waiters();
            }
            self.status.state = "cancelling".to_string();
            self.active_tool_args.clear();
            self.active_tool_names.clear();
            self.active_tool_started_at.clear();
            self.sub_tool_args.clear();
            self.sub_tool_names.clear();
            self.sub_sessions.clear();
            self.opened_sub_session_panes.clear();
            self.supervisors.clear();
            self.pending_approval = None;
            self.approval_selected = 0;
            self.approval_state = "waiting";
            self.active_supervisor_id = None;
            self.refresh_command_catalog();
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

    async fn load_input_history(&mut self) {
        let history = self
            .runtime
            .sessions
            .lock()
            .await
            .metadata_value(&self.session_id, SESSION_INPUT_HISTORY_METADATA_KEY)
            .and_then(|value| serde_json::from_value::<Vec<String>>(value).ok())
            .unwrap_or_default();
        self.input_history = normalize_input_history(history);
    }

    async fn record_input_history(&mut self, text: String) {
        self.input_history.push(text);
        self.input_history = normalize_input_history(std::mem::take(&mut self.input_history));
        let value = serde_json::json!(normalize_input_history(self.input_history.clone()));
        let _ = self.runtime.sessions.lock().await.set_metadata_value(
            &self.session_id,
            SESSION_INPUT_HISTORY_METADATA_KEY,
            value,
        );
    }

    fn recall_history(&mut self, direction: isize) {
        let (history_index, history_draft, input) = recall_input_history(
            &self.input_history,
            self.history_index,
            self.history_draft.take(),
            &self.composer.to_text(),
            direction,
        );
        self.history_index = history_index;
        self.history_draft = history_draft;
        self.composer.set_text(input);
        self.popup_selected = 0;
        self.refresh_file_matches();
    }

    fn reset_history_navigation(&mut self) {
        self.history_index = None;
        self.history_draft = None;
    }

    fn refresh_command_catalog(&mut self) {
        let mut commands = static_commands();
        if let Ok(mut workflows) = self.runtime.bot.workflow_definitions() {
            workflows.sort_by(|a, b| a.id.cmp(&b.id));
            commands.extend(
                workflows
                    .into_iter()
                    .filter(|workflow| workflow.id != "goal")
                    .map(|workflow| CommandEntry {
                        value: format!("/{} ", workflow.id),
                        description: format!("启动 workflow: {}", workflow.name),
                        accepts_arguments: true,
                        searchable: format!(
                            "/{} {} {} workflow {}",
                            workflow.id, workflow.name, workflow.description, workflow.id
                        ),
                    }),
            );
        }
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
            self.mark_token_cell(self.cells.len().saturating_sub(1));
        } else {
            self.cells.push(HistoryCell::assistant(delta.to_string()));
            self.mark_token_cell(self.cells.len().saturating_sub(1));
        }
    }

    fn push_thinking_delta(&mut self, delta: &str) {
        if let Some(cell) = self
            .cells
            .last_mut()
            .filter(|cell| matches!(cell.kind, CellKind::Thinking))
        {
            cell.append(delta);
            self.mark_token_cell(self.cells.len().saturating_sub(1));
        } else {
            self.cells.push(HistoryCell::thinking(delta.to_string()));
            self.mark_token_cell(self.cells.len().saturating_sub(1));
        }
    }

    fn mark_token_cell(&mut self, index: usize) {
        self.last_token_cell_index = Some(index);
        if !self.pending_token_delta.is_empty() {
            let delta = std::mem::take(&mut self.pending_token_delta);
            if let Some(cell) = self.cells.get_mut(index) {
                append_token_meta(cell, delta);
            }
        }
    }

    fn apply_token_delta(&mut self, delta: TokenDelta) {
        if delta.is_empty() {
            return;
        }
        if let Some(index) = self
            .last_token_cell_index
            .filter(|index| *index < self.cells.len())
        {
            if let Some(cell) = self.cells.get_mut(index) {
                append_token_meta(cell, delta);
                return;
            }
        }
        self.pending_token_delta.prompt_tokens = self
            .pending_token_delta
            .prompt_tokens
            .saturating_add(delta.prompt_tokens);
        self.pending_token_delta.completion_tokens = self
            .pending_token_delta
            .completion_tokens
            .saturating_add(delta.completion_tokens);
    }

    fn update_cell_tokens_from_stats(&mut self, prompt_tokens: u32, completion_tokens: u32) {
        let delta = TokenDelta {
            prompt_tokens: prompt_tokens.saturating_sub(self.last_stats_snapshot.prompt_tokens),
            completion_tokens: completion_tokens
                .saturating_sub(self.last_stats_snapshot.completion_tokens),
        };
        self.last_stats_snapshot = TokenStatsSnapshot {
            prompt_tokens,
            completion_tokens,
        };
        self.apply_token_delta(delta);
    }

    fn flush_status_elapsed(&mut self) {
        if let Some(started) = self.run_started_at {
            self.status.elapsed_ms = started.elapsed().as_millis() as u64;
        }
        for cell in &mut self.cells {
            if cell.status != ToolVisualStatus::Running {
                continue;
            }
            let Some(tool_id) = cell.tool_id() else {
                continue;
            };
            let Some(started) = self.active_tool_started_at.get(tool_id) else {
                continue;
            };
            cell.meta = format_elapsed(started.elapsed().as_millis() as u64);
        }
    }

    fn active_tool_count(&self) -> usize {
        self.active_tool_started_at.len()
    }

    async fn decide_pending_approval(&mut self, decision: ToolApprovalDecision) {
        let Some(request) = self.pending_approval.take() else {
            return;
        };
        let decided = self
            .runtime
            .bot
            .approval_manager()
            .decide(&request.id, decision)
            .await
            .is_some();
        tracing::info!(
            approval_id = %request.id,
            tool_name = %request.tool_name,
            session_id = %request.session_id,
            decision = ?decision,
            source = "tui",
            decided,
            "tool_approval.decision"
        );
        let status = if decided {
            format!("submitted decision: {:?}", decision)
        } else {
            "approval already resolved".to_string()
        };
        self.approval_state = "deciding";
        self.upsert_approval_cell(approval_cell(
            &request,
            self.approval_state,
            self.approval_selected,
            Some(status),
            None,
        ));
    }

    fn upsert_approval_cell(&mut self, cell: HistoryCell) {
        upsert_approval_cell(&mut self.cells, cell);
    }

    fn upsert_user_question_cell(&mut self, cell: HistoryCell) {
        upsert_user_question_cell(&mut self.cells, cell);
    }

    fn move_approval_selection(&mut self, direction: isize) {
        let len = approval_options_len();
        if len == 0 {
            return;
        }
        if direction < 0 {
            self.approval_selected = self.approval_selected.saturating_sub(1);
        } else {
            self.approval_selected = (self.approval_selected + 1).min(len - 1);
        }
        if let Some(request) = self.pending_approval.clone() {
            self.upsert_approval_cell(approval_cell(
                &request,
                self.approval_state,
                self.approval_selected,
                None,
                None,
            ));
        }
    }

    fn move_user_question_selection(&mut self, direction: isize) {
        let Some(request) = self.pending_user_question.clone() else {
            return;
        };
        let len = request.options.len();
        if len == 0 {
            return;
        }
        if direction < 0 {
            self.user_question_selected = self.user_question_selected.saturating_sub(1);
        } else {
            self.user_question_selected = (self.user_question_selected + 1).min(len - 1);
        }
        self.upsert_user_question_cell(user_question_cell(
            &request,
            self.user_question_state,
            self.user_question_selected,
            None,
            None,
        ));
    }

    async fn answer_pending_user_question(&mut self, cancel: bool) {
        let Some(request) = self.pending_user_question.take() else {
            return;
        };
        let free_text = self.composer.to_text().trim().to_string();
        self.composer.clear();
        let selected_option_ids = if !cancel && !request.options.is_empty() {
            request
                .options
                .get(self.user_question_selected)
                .map(|option| vec![option.id.clone()])
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let free_text = if !cancel && !free_text.is_empty() {
            Some(free_text)
        } else {
            None
        };
        let status = if cancel {
            UserQuestionStatus::Cancelled
        } else {
            UserQuestionStatus::Answered
        };
        let answer_text =
            build_tui_user_question_answer_text(&selected_option_ids, free_text.as_deref(), status);
        let response = UserQuestionResponse {
            question_id: request.id.clone(),
            status,
            selected_option_ids,
            free_text,
            answer_text: Some(answer_text.clone()),
            answered_at: None,
            source: Some("tui".to_string()),
        };
        let answered = self
            .runtime
            .bot
            .user_question_manager()
            .answer(&request.id, response)
            .await
            .is_some();
        tracing::info!(
            question_id = %request.id,
            session_id = %request.session_id,
            source = "tui",
            answered,
            "user_question.answer"
        );
        let status_text = if answered {
            answer_text
        } else {
            "question already resolved".to_string()
        };
        self.upsert_user_question_cell(user_question_cell(
            &request,
            "submitted",
            self.user_question_selected,
            Some(status_text),
            Some(status),
        ));
    }

    fn upsert_supervisor_progress(&mut self, progress: SupervisorTraceEvent) {
        let id = self
            .active_supervisor_id
            .clone()
            .unwrap_or_else(|| "supervisor".to_string());
        let event = supervisor_event_display(&progress);
        let state = self.supervisors.entry(id.clone()).or_default();
        state.push_event(event);
        let title = state.running_title();
        let body = state.body();
        let meta = state.meta();
        if let Some(cell) = self.cells.iter_mut().rev().find(
            |cell| matches!(&cell.kind, CellKind::Supervisor { id: cell_id } if cell_id == &id),
        ) {
            cell.title = title;
            cell.body = body;
            cell.status = ToolVisualStatus::Running;
            cell.meta = meta;
        } else {
            self.cells.push(HistoryCell::supervisor(
                id,
                title,
                body,
                meta,
                ToolVisualStatus::Running,
            ));
        }
    }

    fn upsert_supervisor_report(&mut self, report: WorkflowReport) {
        let id = self
            .active_supervisor_id
            .clone()
            .unwrap_or_else(|| "supervisor".to_string());
        let state = self.supervisors.entry(id.clone()).or_default();
        state.apply_report(&report);
        let title = state.resolved_title();
        let meta = state.meta();
        if let Some(cell) = self.cells.iter_mut().rev().find(
            |cell| matches!(&cell.kind, CellKind::Supervisor { id: cell_id } if cell_id == &id),
        ) {
            cell.title = title;
            cell.body = String::new();
            cell.meta = meta;
            cell.status = ToolVisualStatus::Success;
        } else {
            self.cells.push(HistoryCell::supervisor(
                id,
                title,
                String::new(),
                meta,
                ToolVisualStatus::Success,
            ));
        }
    }

    async fn upsert_sub_session(&mut self, event: SubSessionEvent) {
        self.render_sub_session_event(&event);
        self.persist_and_maybe_open_sub_session(event).await;
    }

    fn render_sub_session_event(&mut self, event: &SubSessionEvent) {
        let id = sub_session_id(event);
        let status = sub_session_status(&event.payload);
        let state = self
            .sub_sessions
            .entry(id.clone())
            .or_insert_with(|| SubSessionUiState::from_event(&event));
        state.update_context(&event);
        match &event.payload {
            SubSessionEventPayload::Start => {}
            SubSessionEventPayload::Delta { .. } => {}
            SubSessionEventPayload::ThinkingStart => {
                state.upsert_activity(SubSessionActivity::keyed(
                    "thinking",
                    "thinking",
                    "thinking...",
                    ToolVisualStatus::Running,
                ));
            }
            SubSessionEventPayload::ThinkingEnd { .. } => {
                state.upsert_activity(SubSessionActivity::keyed(
                    "thinking",
                    "thinking",
                    "thinking complete",
                    ToolVisualStatus::Running,
                ));
            }
            SubSessionEventPayload::TurnStart { turn } => {
                state.push_activity(SubSessionActivity::message(
                    "turn",
                    &format!("turn {turn}"),
                    ToolVisualStatus::Running,
                ));
            }
            SubSessionEventPayload::ToolCallStart { id: call_id, name } => {
                self.sub_tool_names.insert(call_id.clone(), name.clone());
                self.sub_tool_args.entry(call_id.clone()).or_default();
                let pretty = PrettyToolCall::started(call_id, name, &empty_tool_args());
                let tool = SubToolDisplay::from_pretty(call_id, &pretty, status);
                state.upsert_tool(tool.clone());
                state.upsert_activity(SubSessionActivity::from_tool(call_id, &tool));
            }
            SubSessionEventPayload::ToolCallArgumentsDelta { id: call_id, delta } => {
                let args = self.sub_tool_args.entry(call_id.clone()).or_default();
                args.push_str(delta);
                let name = self
                    .sub_tool_names
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| call_id.clone());
                let args_value = parse_tool_args(args).unwrap_or(serde_json::Value::Null);
                let pretty = PrettyToolCall::started(call_id, &name, &args_value);
                let tool = SubToolDisplay::from_pretty(call_id, &pretty, status);
                state.upsert_tool(tool.clone());
                state.upsert_activity(SubSessionActivity::from_tool(call_id, &tool));
            }
            SubSessionEventPayload::ToolDelta {
                id: call_id,
                name,
                delta,
            } => {
                self.sub_tool_names.insert(call_id.clone(), name.clone());
                let tool = SubToolDisplay {
                    id: call_id.clone(),
                    title: format!("调用 {name}"),
                    summary: truncate_chars(delta, MAX_TOOL_BODY_CHARS),
                    status,
                };
                state.upsert_tool(tool.clone());
                state.upsert_activity(SubSessionActivity::from_tool(call_id, &tool));
            }
            SubSessionEventPayload::ToolResult {
                id: call_id,
                name,
                result,
            } => {
                self.sub_tool_names.remove(call_id);
                let args = self.sub_tool_args.remove(call_id).unwrap_or_default();
                let args_value = parse_tool_args(&args).unwrap_or(serde_json::Value::Null);
                let success = bot_core::tool_success(result);
                let pretty =
                    PrettyToolCall::completed(call_id, name, &args_value, result, success, 0);
                let tool = SubToolDisplay::from_pretty(
                    call_id,
                    &pretty,
                    ToolVisualStatus::from_success(success),
                );
                state.upsert_tool(tool.clone());
                state.upsert_activity(SubSessionActivity::from_tool(call_id, &tool));
            }
            SubSessionEventPayload::Done { .. } => {
                state.done = true;
                state.final_output = None;
            }
            SubSessionEventPayload::Error { message } => {
                state.failed = true;
                state.final_output =
                    Some(truncate_chars(&single_line(message), MAX_TOOL_BODY_CHARS));
                state.push_activity(SubSessionActivity::message(
                    "error",
                    message,
                    ToolVisualStatus::Error,
                ));
            }
        }
        let title = state.title();
        let meta = state.meta();
        let body = state.body();
        let status = state.status();
        if let Some(cell) = self.cells.iter_mut().rev().find(
            |cell| matches!(&cell.kind, CellKind::SubSession { id: cell_id } if cell_id == &id),
        ) {
            cell.title = title;
            cell.body = body;
            cell.meta = meta;
            cell.status = status;
        } else {
            self.cells
                .push(HistoryCell::sub_session(id, title, body, meta, status));
        }
    }

    async fn persist_and_maybe_open_sub_session(&mut self, event: SubSessionEvent) {
        let kind = sub_session_kind(&event);
        let status = sub_session_status_label(&event.payload);
        if let Err(error) = self.runtime.sessions.lock().await.upsert_sub_session(
            &self.session_id,
            &event.sub_thread_id.0,
            kind,
            &event.agent_name,
            event.title.clone(),
            status,
        ) {
            self.cells.push(HistoryCell::error(format!(
                "sub-session 记录失败: {error:#}"
            )));
            return;
        }

        let child_session_id =
            match ensure_tui_sub_session_channel_session(&self.runtime, &self.session_id, &event)
                .await
            {
                Some(session_id) => session_id,
                None => return,
            };

        let messages = sub_session_history_messages(&event);
        if !messages.is_empty() {
            if let Err(error) = self
                .runtime
                .bot
                .append_thread_messages(&child_session_id, messages)
                .await
            {
                self.cells.push(HistoryCell::error(format!(
                    "sub-session 历史写入失败: {error:#}"
                )));
            }
        }
        if let Err(error) =
            append_sub_session_event_log(&self.runtime.data_dir, &child_session_id, &event).await
        {
            self.cells.push(HistoryCell::error(format!(
                "sub-session 事件写入失败: {error:#}"
            )));
        }

        if !matches!(event.payload, SubSessionEventPayload::Start) {
            return;
        }
        let pane_key = format!("{}:{}", self.session_id, event.sub_thread_id.0);
        if !self.opened_sub_session_panes.insert(pane_key) {
            return;
        }
        match open_tui_session_in_new_pane(&child_session_id, &self.cli) {
            Ok(Some(kind)) => self.cells.push(HistoryCell::system(format!(
                "sub-session 已在新的 {kind} pane 中打开。\nagent: {}\nsession: {}",
                event.agent_name, child_session_id
            ))),
            Ok(None) => self.cells.push(HistoryCell::system(format!(
                "sub-session 已创建独立 TUI session，可手动打开：\nremi-cat tui resume {}",
                child_session_id
            ))),
            Err(error) => self.cells.push(HistoryCell::system(format!(
                "sub-session 独立 TUI session 已创建，但自动打开 pane 失败。\nsession: {}\n错误: {error:#}",
                child_session_id
            ))),
        }
    }

    async fn load_thread_history(&mut self) {
        let history = self.runtime.bot.thread_history(&self.session_id).await;
        self.loaded_thread_history_len = history.len();
        if history.is_empty() {
            self.cells.push(HistoryCell::system("no previous messages"));
            return;
        }
        let loaded = history.len();
        self.cells.push(HistoryCell::system(format!(
            "loaded {loaded} previous messages"
        )));
        for message in history {
            self.append_history_message(message);
        }
        while self.cells.len() > MAX_HISTORY_CELLS {
            self.cells.remove(0);
        }
    }

    async fn poll_sub_session_history(&mut self) {
        if self.running {
            return;
        }
        let is_sub_session = self
            .runtime
            .sessions
            .lock()
            .await
            .metadata_string(&self.session_id, "sub_session_thread_id")
            .is_some();
        if !is_sub_session {
            return;
        }
        self.poll_sub_session_event_log().await;
        while self.cells.len() > MAX_HISTORY_CELLS {
            self.cells.remove(0);
        }
    }

    async fn poll_sub_session_event_log(&mut self) {
        let path = sub_session_event_log_path(&self.runtime.data_dir, &self.session_id);
        let Ok(content) = tokio::fs::read_to_string(path).await else {
            return;
        };
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() <= self.sub_session_event_log_lines {
            return;
        }
        let previous_lines = self.sub_session_event_log_lines;
        self.sub_session_event_log_lines = lines.len();
        for line in lines.into_iter().skip(previous_lines) {
            match serde_json::from_str::<SubSessionEvent>(line) {
                Ok(event) => self.apply_sub_session_event_as_session_view(event),
                Err(error) => self.cells.push(HistoryCell::error(format!(
                    "sub-session 事件解析失败: {error:#}"
                ))),
            }
        }
    }

    fn apply_sub_session_event_as_session_view(&mut self, event: SubSessionEvent) {
        match event.payload {
            SubSessionEventPayload::Start => {
                self.status.state = "running".to_string();
                self.status.elapsed_ms = 0;
                self.status.last_error = None;
            }
            SubSessionEventPayload::Delta { content } => {
                self.status.state = "running".to_string();
                self.push_assistant_delta(&content);
            }
            SubSessionEventPayload::ThinkingStart => {
                self.status.state = "thinking".to_string();
            }
            SubSessionEventPayload::ThinkingEnd { content } => {
                self.status.state = "running".to_string();
                if !content.trim().is_empty() {
                    self.push_thinking_delta(&content);
                }
            }
            SubSessionEventPayload::TurnStart { turn } => {
                self.status.state = format!("turn {turn}");
            }
            SubSessionEventPayload::ToolCallStart { id, name } => {
                self.status.state = "running".to_string();
                self.handle_bot_event_sync(BotEvent::ToolStart { id, name });
            }
            SubSessionEventPayload::ToolCallArgumentsDelta { id, delta } => {
                self.handle_bot_event_sync(BotEvent::ToolArgs { id, delta });
            }
            SubSessionEventPayload::ToolDelta { id, name, delta } => {
                self.status.state = "running".to_string();
                self.handle_bot_event_sync(BotEvent::ToolDelta { id, name, delta });
            }
            SubSessionEventPayload::ToolResult { id, name, result } => {
                self.handle_bot_event_sync(BotEvent::ToolDone {
                    id,
                    name,
                    args: String::new(),
                    success: bot_core::tool_success(&result),
                    result,
                    elapsed_ms: 0,
                });
            }
            SubSessionEventPayload::Done { final_output } => {
                if let Some(output) = final_output
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    let duplicate_last = self
                        .cells
                        .last()
                        .filter(|cell| matches!(cell.kind, CellKind::Assistant))
                        .is_some_and(|cell| cell.body.trim() == output);
                    if !duplicate_last {
                        self.push_assistant_delta(output);
                    }
                }
                self.status.state = "idle".to_string();
            }
            SubSessionEventPayload::Error { message } => {
                self.status.state = "error".to_string();
                self.status.last_error = Some(message.clone());
                self.cells.push(HistoryCell::error(message));
            }
        }
    }

    fn handle_bot_event_sync(&mut self, event: BotEvent) {
        match event {
            BotEvent::ToolStart { id, name } => {
                self.active_tool_args.insert(id.clone(), String::new());
                self.active_tool_names.insert(id.clone(), name.clone());
                self.active_tool_started_at
                    .insert(id.clone(), Instant::now());
                let pretty = PrettyToolCall::started(&id, &name, &empty_tool_args());
                let body = tool_body(&pretty);
                self.cells.push(HistoryCell::tool(
                    id.clone(),
                    pretty.title,
                    body,
                    format_elapsed(0),
                    ToolVisualStatus::Running,
                ));
                self.mark_token_cell(self.cells.len().saturating_sub(1));
            }
            BotEvent::ToolArgs { id, delta } => {
                let args = self.active_tool_args.entry(id.clone()).or_default();
                args.push_str(&delta);
                if let Some(cell) = self
                    .cells
                    .iter_mut()
                    .rev()
                    .find(|cell| cell.tool_id().is_some_and(|tool_id| tool_id == id))
                {
                    if let (Some(name), Some(args_value)) =
                        (self.active_tool_names.get(&id), parse_tool_args(args))
                    {
                        let pretty = PrettyToolCall::started(&id, name, &args_value);
                        let title = pretty.title.clone();
                        let body = tool_body(&pretty);
                        cell.title = title;
                        cell.body = body;
                    }
                }
            }
            BotEvent::ToolDelta { id, name, delta } => {
                self.active_tool_names.insert(id.clone(), name.clone());
                self.active_tool_started_at
                    .entry(id.clone())
                    .or_insert_with(Instant::now);
                let meta = self
                    .active_tool_started_at
                    .get(&id)
                    .map(|started| format_elapsed(started.elapsed().as_millis() as u64))
                    .unwrap_or_else(|| format_elapsed(0));
                if let Some(cell) = self
                    .cells
                    .iter_mut()
                    .rev()
                    .find(|cell| cell.tool_id().is_some_and(|tool_id| tool_id == id))
                {
                    cell.title = format!("调用 {name}");
                    cell.body = truncate_chars(&delta, MAX_TOOL_BODY_CHARS);
                    cell.meta = preserve_token_meta(meta, &cell.meta);
                    cell.status = ToolVisualStatus::Running;
                } else {
                    self.cells.push(HistoryCell::tool(
                        id,
                        format!("调用 {name}"),
                        truncate_chars(&delta, MAX_TOOL_BODY_CHARS),
                        meta,
                        ToolVisualStatus::Running,
                    ));
                }
            }
            BotEvent::ToolDone {
                id,
                name,
                args,
                result,
                success,
                elapsed_ms,
            } => {
                let args = if args.trim().is_empty() {
                    self.active_tool_args.remove(&id).unwrap_or_default()
                } else {
                    self.active_tool_args.remove(&id);
                    args
                };
                self.active_tool_names.remove(&id);
                self.active_tool_started_at.remove(&id);
                let args_value = parse_tool_args(&args).unwrap_or(serde_json::Value::Null);
                let pretty = PrettyToolCall::completed(
                    &id,
                    &name,
                    &args_value,
                    &result,
                    success,
                    elapsed_ms,
                );
                let status = ToolVisualStatus::from_pretty(&pretty.status);
                let meta = tool_meta(&pretty);
                if let Some(cell) = self
                    .cells
                    .iter_mut()
                    .rev()
                    .find(|cell| cell.tool_id().is_some_and(|tool_id| tool_id == id))
                {
                    let title = pretty.title.clone();
                    let body = tool_body(&pretty);
                    cell.title = title;
                    cell.body = body;
                    cell.meta = preserve_token_meta(meta, &cell.meta);
                    cell.status = status;
                } else {
                    let title = pretty.title.clone();
                    let body = tool_body(&pretty);
                    self.cells
                        .push(HistoryCell::tool(id, title, body, meta, status));
                }
            }
            _ => {}
        }
    }

    fn append_history_message(&mut self, message: ThreadHistoryMessage) {
        if message.role == "assistant" && message.pretty.is_none() {
            let text = message.text.trim();
            if text.is_empty() {
                return;
            }
            if let Some(cell) = self.cells.last_mut().filter(|cell| {
                matches!(cell.kind, CellKind::Assistant) && cell.status == ToolVisualStatus::Neutral
            }) {
                cell.body.push_str(&message.text);
                return;
            }
        }
        if let Some(cell) = history_cell(message) {
            self.cells.push(cell);
        }
    }

    fn composer_height(&self, width: u16) -> u16 {
        let rows = composer_visual_line_count(&self.composer, width).max(1);
        rows.clamp(1, 6).saturating_add(1)
    }

    fn activity_height(&self) -> u16 {
        u16::from(self.pending_approval.is_some())
            + u16::from(self.running)
            + u16::from(!self.queued_inputs.is_empty())
    }

    fn footer_height(&self) -> u16 {
        if self.show_shortcuts {
            3
        } else {
            1
        }
    }

    fn cursor_position(&self, area: Rect) -> (u16, u16) {
        let display = self.composer.display_text();
        let cursor = self.composer.cursor_display_byte().min(display.len());
        let before = expand_tabs(&sanitize_tui_text(&display[..cursor]));
        let (row, col) = composer_visual_position(&before, area.width);
        (
            area.x + col.min(area.width.saturating_sub(1)),
            area.y + row.min(area.height.saturating_sub(1)),
        )
    }
}

async fn ensure_tui_sub_session_channel_session(
    runtime: &Rc<Runtime>,
    parent_session_id: &str,
    event: &SubSessionEvent,
) -> Option<String> {
    let title = event.title.clone().or_else(|| {
        Some(format!(
            "{} {}",
            event.agent_name,
            event.sub_thread_id.0.trim()
        ))
    });
    let existing_binding = runtime
        .sessions
        .lock()
        .await
        .sub_session_channel_binding(parent_session_id, &event.sub_thread_id.0);
    let (session_platform, session_channel_id) = existing_binding
        .as_ref()
        .map(|binding| (binding.platform.as_str(), binding.channel_id.as_str()))
        .unwrap_or((TUI_CHANNEL, event.sub_thread_id.0.as_str()));

    let session = runtime.sessions.lock().await.create_channel(
        session_platform,
        session_channel_id,
        &runtime.root_agent_id,
        title,
    );
    let session = match session {
        Ok(session) => session,
        Err(_) => return None,
    };

    let mut sessions = runtime.sessions.lock().await;
    if existing_binding.is_none() {
        let _ = sessions.bind_sub_session_channel(
            parent_session_id,
            &event.sub_thread_id.0,
            ChannelBinding {
                platform: TUI_CHANNEL.to_string(),
                channel_id: event.sub_thread_id.0.clone(),
            },
        );
    }
    let _ = sessions.set_metadata_string(
        &session.id,
        "sub_session_parent_session_id",
        parent_session_id,
    );
    let _ =
        sessions.set_metadata_string(&session.id, "sub_session_thread_id", &event.sub_thread_id.0);
    let _ = sessions.set_metadata_string(&session.id, "sub_session_agent", &event.agent_name);
    drop(sessions);
    if event.agent_name == "acp" {
        if let Some(acp_session_id) = runtime
            .bot
            .acp_session_id_for_sub_session(&event.sub_thread_id.0)
            .await
        {
            let _ = runtime.sessions.lock().await.set_metadata_string(
                &session.id,
                "sub_session_acp_session_id",
                &acp_session_id,
            );
        }
    }
    Some(session.id)
}

async fn next_trigger_dispatch(
    trigger_rx: &mut Option<
        UnboundedReceiverStream<crate::local_trigger_scheduler::LocalTriggerDispatch>,
    >,
) -> Option<crate::local_trigger_scheduler::LocalTriggerDispatch> {
    match trigger_rx {
        Some(rx) => rx.next().await,
        None => std::future::pending().await,
    }
}

fn sub_session_event_log_path(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir
        .join("tui-subsession-events")
        .join(format!("{session_id}.jsonl"))
}

async fn append_sub_session_event_log(
    data_dir: &Path,
    session_id: &str,
    event: &SubSessionEvent,
) -> anyhow::Result<()> {
    let path = sub_session_event_log_path(data_dir, session_id);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(serde_json::to_string(event)?.as_bytes())
        .await?;
    file.write_all(b"\n").await?;
    Ok(())
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
    let (stream_thread_id, text, skill_injections, persist_agent_child_turn) =
        match crate::sub_session_input_target(&runtime, &session_id).await {
            Some(crate::SubSessionInputTarget::Acp { acp_session_id, .. }) => {
                match runtime
                    .bot
                    .acp_bound_message(&acp_session_id, text.trim())
                    .await
                {
                    Ok(reply) => {
                        crate::append_direct_sub_session_turn(
                            &runtime,
                            &session_id,
                            text.trim(),
                            &reply,
                        )
                        .await;
                        let _ = tx.send(BotEvent::Text(reply));
                    }
                    Err(error) => {
                        let _ = tx.send(BotEvent::Error(error.to_string()));
                    }
                }
                let _ = tx.send(BotEvent::Done);
                return;
            }
            Some(crate::SubSessionInputTarget::Agent { sub_thread_id, .. }) => {
                (sub_thread_id, text, Vec::new(), true)
            }
            None => match process_runtime_commands(&runtime, &session_id, text.trim()).await {
                Ok(RuntimeCommandPipelineResult::Reply(reply)) => {
                    if text.trim() == "/clear" {
                        let _ = tx.send(BotEvent::SessionCleared);
                    }
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
                    (session_id.clone(), text, skill_injections, false)
                }
                Err(error) => {
                    let _ = tx.send(BotEvent::Error(error.to_string()));
                    let _ = tx.send(BotEvent::Done);
                    return;
                }
            },
        };

    {
        let (model_profile_id, agent_id) = {
            let sessions = runtime.sessions.lock().await;
            (
                sessions.metadata_string(&session_id, SESSION_MODEL_PROFILE_METADATA_KEY),
                sessions.metadata_string(&session_id, SESSION_AGENT_ID_METADATA_KEY),
            )
        };
        let opts = StreamOptions {
            model_profile_id,
            agent_id,
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
        let user_text_for_history = text.clone();
        let mut assistant_text_for_history = String::new();
        let mut stream = std::pin::pin!(runtime.bot.stream_with_options(
            &stream_thread_id,
            Content::text(text),
            opts
        ));
        while let Some(event) = stream.next().await {
            match event {
                CatEvent::Text(delta) => {
                    if persist_agent_child_turn {
                        assistant_text_for_history.push_str(&delta);
                    }
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
                CatEvent::ToolCall { id, name, args } => {
                    let _ = tx.send(BotEvent::ToolCall {
                        id,
                        name,
                        args: args.to_string(),
                    });
                }
                CatEvent::ToolCallResult {
                    id,
                    name,
                    args,
                    result,
                    success,
                    elapsed_ms,
                } => {
                    let _ = tx.send(BotEvent::ToolDone {
                        id,
                        name,
                        args: args.to_string(),
                        result,
                        success,
                        elapsed_ms,
                    });
                }
                CatEvent::SubSession(event) => {
                    let _ = tx.send(BotEvent::SubSession(event));
                }
                CatEvent::ContextCompaction(event) => {
                    let _ = tx.send(BotEvent::ContextCompaction(event));
                }
                CatEvent::Supervisor(report) => {
                    let _ = tx.send(BotEvent::SupervisorReport(report));
                }
                CatEvent::SupervisorProgress(progress) => {
                    let _ = tx.send(BotEvent::SupervisorProgress(progress));
                }
                CatEvent::ToolApprovalRequested(request) => {
                    let _ = tx.send(BotEvent::ApprovalRequested(request));
                }
                CatEvent::ToolApprovalUpdated(request) => {
                    let _ = tx.send(BotEvent::ApprovalUpdated(request));
                }
                CatEvent::ToolApprovalResolved { request, decision } => {
                    let _ = tx.send(BotEvent::ApprovalResolved { request, decision });
                }
                CatEvent::UserQuestionRequested(request) => {
                    let _ = tx.send(BotEvent::UserQuestionRequested(request));
                }
                CatEvent::UserQuestionUpdated(request) => {
                    let _ = tx.send(BotEvent::UserQuestionUpdated(request));
                }
                CatEvent::UserQuestionResolved { request, response } => {
                    let _ = tx.send(BotEvent::UserQuestionResolved { request, response });
                }
                CatEvent::StateUpdate(user_state) => {
                    let items = bot_core::todo::todos_from_user_state(&user_state);
                    let _ = tx.send(BotEvent::TodoState {
                        body: format_todo_state(&items),
                        latest_active: latest_active_todo_label(&items),
                    });
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
        if persist_agent_child_turn {
            crate::append_direct_sub_session_turn(
                &runtime,
                &session_id,
                user_text_for_history.trim(),
                &assistant_text_for_history,
            )
            .await;
        }
        let _ = tx.send(BotEvent::Done);
    }
}

async fn run_tui_trigger_turn(
    runtime: Rc<Runtime>,
    dispatch: crate::local_trigger_scheduler::LocalTriggerDispatch,
    cancel: Arc<Notify>,
    tx: mpsc::UnboundedSender<BotEvent>,
) {
    let (model_profile_id, agent_id) = {
        let sessions = runtime.sessions.lock().await;
        (
            sessions.metadata_string(&dispatch.thread_id, SESSION_MODEL_PROFILE_METADATA_KEY),
            sessions.metadata_string(&dispatch.thread_id, SESSION_AGENT_ID_METADATA_KEY),
        )
    };
    let opts = StreamOptions {
        model_profile_id,
        agent_id,
        sender_user_id: Some(dispatch.owner_user_id),
        sender_username: dispatch.owner_username,
        message_id: Some(format!("trigger-{}", uuid::Uuid::new_v4())),
        chat_type: dispatch.chat_type.or_else(|| Some("p2p".to_string())),
        platform: dispatch.platform.or_else(|| Some(TUI_CHANNEL.to_string())),
        todo_create_via_sdk: true,
        trigger_tools_enabled: false,
        trigger_run: true,
        cancel: Some(cancel),
        ..StreamOptions::default()
    };
    let mut stream = std::pin::pin!(runtime.bot.stream_with_options(
        &dispatch.thread_id,
        Content::text(dispatch.request),
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
            CatEvent::ToolCall { id, name, args } => {
                let _ = tx.send(BotEvent::ToolCall {
                    id,
                    name,
                    args: args.to_string(),
                });
            }
            CatEvent::ToolCallResult {
                id,
                name,
                args,
                result,
                success,
                elapsed_ms,
            } => {
                let _ = tx.send(BotEvent::ToolDone {
                    id,
                    name,
                    args: args.to_string(),
                    result,
                    success,
                    elapsed_ms,
                });
            }
            CatEvent::SubSession(event) => {
                let _ = tx.send(BotEvent::SubSession(event));
            }
            CatEvent::ContextCompaction(event) => {
                let _ = tx.send(BotEvent::ContextCompaction(event));
            }
            CatEvent::Supervisor(report) => {
                let _ = tx.send(BotEvent::SupervisorReport(report));
            }
            CatEvent::SupervisorProgress(progress) => {
                let _ = tx.send(BotEvent::SupervisorProgress(progress));
            }
            CatEvent::ToolApprovalRequested(request) => {
                let _ = tx.send(BotEvent::ApprovalRequested(request));
            }
            CatEvent::ToolApprovalUpdated(request) => {
                let _ = tx.send(BotEvent::ApprovalUpdated(request));
            }
            CatEvent::ToolApprovalResolved { request, decision } => {
                let _ = tx.send(BotEvent::ApprovalResolved { request, decision });
            }
            CatEvent::UserQuestionRequested(request) => {
                let _ = tx.send(BotEvent::UserQuestionRequested(request));
            }
            CatEvent::UserQuestionUpdated(request) => {
                let _ = tx.send(BotEvent::UserQuestionUpdated(request));
            }
            CatEvent::UserQuestionResolved { request, response } => {
                let _ = tx.send(BotEvent::UserQuestionResolved { request, response });
            }
            CatEvent::StateUpdate(user_state) => {
                let items = bot_core::todo::todos_from_user_state(&user_state);
                let _ = tx.send(BotEvent::TodoState {
                    body: format_todo_state(&items),
                    latest_active: latest_active_todo_label(&items),
                });
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

async fn run_tui_fork_command(
    runtime: Rc<Runtime>,
    source_session_id: String,
    cli: CliConfig,
    tx: mpsc::UnboundedSender<BotEvent>,
) {
    let send_progress = |message: String| {
        let _ = tx.send(BotEvent::ForkProgress(message));
    };

    if runtime.bot.is_thread_running(&source_session_id).await {
        send_progress("当前 session 正在运行，结束或取消后再 fork。".to_string());
        return;
    }

    let channel_id = format!("fork:{}", uuid::Uuid::new_v4());
    send_progress("fork: 正在创建新 session...".to_string());
    let fork = match runtime.sessions.lock().await.fork_session(
        &source_session_id,
        TUI_CHANNEL,
        &channel_id,
        None,
    ) {
        Ok(Some(fork)) => fork,
        Ok(None) => {
            send_progress(format!(
                "fork 失败: source session `{source_session_id}` not found"
            ));
            return;
        }
        Err(error) => {
            send_progress(format!("fork 失败: {error:#}"));
            return;
        }
    };

    let fork_title = fork.title.as_deref().unwrap_or("新对话").to_string();
    send_progress(format!(
        "fork: 已创建新 session。\n新 session: {}\n标题: {}\n正在复制上下文...",
        fork.id, fork_title
    ));
    if let Err(error) = runtime
        .bot
        .fork_thread_data(&source_session_id, &fork.id, Some(&cli.user_id))
        .await
    {
        let _ = runtime.sessions.lock().await.delete(&fork.id);
        send_progress(format!("fork 失败，已清理新 session。\n错误: {error:#}"));
        return;
    }

    send_progress("fork: 上下文复制完成，正在打开新 pane...".to_string());
    let fork_id = fork.id.clone();
    match open_tui_session_in_new_pane(&fork_id, &cli) {
        Ok(Some(kind)) => {
            send_progress(format!(
                "已 fork 当前 session，并在新的 {kind} pane 中打开。\n新 session: {}\n标题: {}",
                fork_id, fork_title
            ));
        }
        Ok(None) => {
            let _ = tx.send(BotEvent::SwitchToSession {
                session_id: fork_id.clone(),
                message: format!(
                    "已 fork 当前 session，并在当前 pane 切换到新 session。\n新 session: {}\n标题: {}",
                    fork_id, fork_title
                ),
            });
        }
        Err(error) => {
            let message = error.to_string();
            let _ = tx.send(BotEvent::SwitchToSession {
                session_id: fork_id.clone(),
                message: format!(
                    "自动打开新 pane 失败，已在当前 pane 切换到 fork session。\n新 session: {}\n标题: {}\n错误: {message}",
                    fork_id, fork_title
                ),
            });
            send_progress(format!(
                "自动打开新 pane 失败，已在当前 pane 切换到 fork session。\n错误: {message}"
            ));
        }
    }
}

async fn run_tui_new_command(
    runtime: Rc<Runtime>,
    cli: CliConfig,
    tx: mpsc::UnboundedSender<BotEvent>,
) {
    let send_progress = |message: String| {
        let _ = tx.send(BotEvent::ForkProgress(message));
    };

    let channel_id = format!("new:{}", uuid::Uuid::new_v4());
    let title = "新对话".to_string();
    let session = match runtime.sessions.lock().await.create_channel(
        TUI_CHANNEL,
        &channel_id,
        &runtime.root_agent_id,
        Some(title.clone()),
    ) {
        Ok(session) => session,
        Err(error) => {
            send_progress(format!("new 失败: {error:#}"));
            return;
        }
    };
    let shown_title = session.title.as_deref().unwrap_or(&title).to_string();
    send_progress(format!(
        "new: 已创建空 session。\n新 session: {}\n标题: {}\n正在打开新 pane...",
        session.id, shown_title
    ));
    let session_id = session.id.clone();
    match open_tui_session_in_new_pane(&session_id, &cli) {
        Ok(Some(kind)) => {
            send_progress(format!(
                "已创建空 session，并在新的 {kind} pane 中打开。\n新 session: {}\n标题: {}",
                session_id, shown_title
            ));
        }
        Ok(None) => {
            let _ = tx.send(BotEvent::SwitchToSession {
                session_id: session_id.clone(),
                message: format!(
                    "已创建空 session，并在当前 pane 切换过去。\n新 session: {}\n标题: {}",
                    session_id, shown_title
                ),
            });
        }
        Err(error) => {
            let message = error.to_string();
            let _ = tx.send(BotEvent::SwitchToSession {
                session_id: session_id.clone(),
                message: format!(
                    "自动打开新 pane 失败，已在当前 pane 切换到新 session。\n新 session: {}\n标题: {}\n错误: {message}",
                    session_id, shown_title
                ),
            });
            send_progress(format!(
                "自动打开新 pane 失败，已在当前 pane 切换到新 session。\n错误: {message}"
            ));
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
    ToolCall {
        id: String,
        name: String,
        args: String,
    },
    ToolDone {
        id: String,
        name: String,
        args: String,
        result: String,
        success: bool,
        elapsed_ms: u64,
    },
    ToolDelta {
        id: String,
        name: String,
        delta: String,
    },
    SubSession(SubSessionEvent),
    ContextCompaction(ContextCompactionEvent),
    SupervisorProgress(SupervisorTraceEvent),
    SupervisorReport(WorkflowReport),
    TodoState {
        body: String,
        latest_active: Option<String>,
    },
    ApprovalRequested(ToolApprovalRequest),
    ApprovalUpdated(ToolApprovalRequest),
    ApprovalResolved {
        request: ToolApprovalRequest,
        decision: ToolApprovalDecision,
    },
    UserQuestionRequested(UserQuestionRequest),
    UserQuestionUpdated(UserQuestionRequest),
    UserQuestionResolved {
        request: UserQuestionRequest,
        response: UserQuestionResponse,
    },
    Stats {
        prompt_tokens: u32,
        completion_tokens: u32,
        max_prompt_tokens: u32,
        elapsed_ms: u64,
    },
    ForkProgress(String),
    SwitchToSession {
        session_id: String,
        message: String,
    },
    SessionCleared,
    Error(String),
    Done,
}

#[derive(Clone)]
struct CommandEntry {
    value: String,
    description: String,
    accepts_arguments: bool,
    searchable: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupKind {
    Command,
    File,
}

fn static_commands() -> Vec<CommandEntry> {
    [
        ("/fork", "fork 当前 session 到新 pane", false),
        ("/new", "创建空 session 到新 pane", false),
        ("/exit", "退出 TUI", false),
        ("/quit", "退出 TUI", false),
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
        ("/agent status", "显示当前 agent 状态", false),
        ("/agent list", "列出可切换 agents", false),
        ("/agent use ", "切换当前 session agent", true),
        ("/agent reset", "重置 session agent override", false),
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

fn command_filter_terms(input: &str) -> Vec<String> {
    input
        .trim_start()
        .trim_start_matches('/')
        .to_lowercase()
        .split(|ch: char| ch.is_whitespace() || ch == ':')
        .filter(|term| !term.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn command_matches_filter(command: &CommandEntry, terms: &[String]) -> bool {
    let searchable = command.searchable.to_lowercase();
    terms.iter().all(|term| searchable.contains(term))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneLaunchCommand {
    kind: &'static str,
    program: &'static str,
    args: Vec<OsString>,
}

fn open_tui_session_in_new_pane(
    session_id: &str,
    cli: &CliConfig,
) -> anyhow::Result<Option<&'static str>> {
    let tmux = std::env::var_os("TMUX").is_some();
    let zellij = std::env::var_os("ZELLIJ").is_some();
    let warp = std::env::var_os("WARP_TERMINAL_SESSION_UUID").is_some()
        || std::env::var_os("WARP_FOCUS_URL").is_some()
        || std::env::var("TERM_PROGRAM").is_ok_and(|value| value == "WarpTerminal");
    let exe = std::env::current_exe()?;

    if !tmux && !zellij && warp {
        let config_name = warp_tab_config_name(session_id);
        let config_dir = warp_tab_config_dir()?;
        std::fs::create_dir_all(&config_dir)
            .with_context(|| format!("creating {}", config_dir.display()))?;
        let config_path = config_dir.join(format!("{config_name}.toml"));
        std::fs::write(
            &config_path,
            warp_tab_config_toml(&config_name, &exe, session_id, cli),
        )
        .with_context(|| format!("writing {}", config_path.display()))?;

        let command = build_warp_open_command(&config_name);
        let status = Command::new(command.program)
            .args(&command.args)
            .status()
            .with_context(|| format!("opening Warp tab config {config_name}"))?;
        if status.success() {
            return Ok(Some(command.kind));
        }
        anyhow::bail!("{} exited with status {}", command.program, status);
    }

    let Some(command) = build_pane_launch_command(tmux, zellij, &exe, session_id, cli) else {
        return Ok(None);
    };
    let status = Command::new(command.program)
        .args(&command.args)
        .status()
        .with_context(|| format!("launching {} pane", command.kind))?;
    if status.success() {
        Ok(Some(command.kind))
    } else {
        anyhow::bail!("{} exited with status {}", command.program, status);
    }
}

fn build_pane_launch_command(
    tmux: bool,
    zellij: bool,
    exe: &Path,
    session_id: &str,
    cli: &CliConfig,
) -> Option<PaneLaunchCommand> {
    if tmux {
        let mut args = vec![OsString::from("split-window"), OsString::from("-h")];
        args.extend(tui_resume_command_args(exe, session_id, cli));
        return Some(PaneLaunchCommand {
            kind: "tmux",
            program: "tmux",
            args,
        });
    }
    if zellij {
        let mut args = vec![
            OsString::from("action"),
            OsString::from("new-pane"),
            OsString::from("--direction"),
            OsString::from("right"),
            OsString::from("--"),
        ];
        args.extend(tui_resume_command_args(exe, session_id, cli));
        return Some(PaneLaunchCommand {
            kind: "Zellij",
            program: "zellij",
            args,
        });
    }
    None
}

fn build_warp_open_command(config_name: &str) -> PaneLaunchCommand {
    let uri = format!("warp://tab_config/{config_name}");
    if cfg!(target_os = "macos") {
        PaneLaunchCommand {
            kind: "Warp",
            program: "open",
            args: vec![OsString::from(uri)],
        }
    } else if cfg!(target_os = "windows") {
        PaneLaunchCommand {
            kind: "Warp",
            program: "cmd",
            args: vec![
                OsString::from("/C"),
                OsString::from("start"),
                OsString::from(""),
                OsString::from(uri),
            ],
        }
    } else {
        PaneLaunchCommand {
            kind: "Warp",
            program: "xdg-open",
            args: vec![OsString::from(uri)],
        }
    }
}

fn tui_resume_command_args(exe: &Path, session_id: &str, cli: &CliConfig) -> Vec<OsString> {
    vec![
        exe.as_os_str().to_os_string(),
        OsString::from("tui"),
        OsString::from("resume"),
        OsString::from(session_id),
        OsString::from("--user"),
        OsString::from(cli.user_id.clone()),
        OsString::from("--name"),
        OsString::from(cli.username.clone()),
    ]
}

fn warp_tab_config_dir() -> anyhow::Result<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        return Ok(PathBuf::from(home).join(".warp").join("tab_configs"));
    }
    if cfg!(target_os = "windows") {
        let appdata = std::env::var_os("APPDATA").context("APPDATA is not set")?;
        return Ok(PathBuf::from(appdata)
            .join("warp")
            .join("Warp")
            .join("data")
            .join("tab_configs"));
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .context("XDG_DATA_HOME and HOME are not set")?;
    Ok(data_home.join("warp-terminal").join("tab_configs"))
}

fn warp_tab_config_name(session_id: &str) -> String {
    let suffix = session_id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(12)
        .collect::<String>();
    if suffix.is_empty() {
        "remi_cat_fork".to_string()
    } else {
        format!("remi_cat_fork_{suffix}")
    }
}

fn warp_tab_config_toml(
    config_name: &str,
    exe: &Path,
    session_id: &str,
    cli: &CliConfig,
) -> String {
    let command = shell_join_command(&tui_resume_command_args(exe, session_id, cli));
    format!(
        "name = {}\ntitle = {}\n\n[[panes]]\nid = \"main\"\ntype = \"terminal\"\ncommands = [{}]\nis_focused = true\n",
        toml_string(&format!("Remi fork {config_name}")),
        toml_string("Remi fork"),
        toml_string(&command)
    )
}

fn shell_join_command(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| shell_quote(&arg.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '='))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn toml_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

fn is_tui_fork_command(command: &str) -> bool {
    command == "/fork"
}

fn is_tui_new_command(command: &str) -> bool {
    command == "/new"
}

fn is_tui_exit_command(command: &str) -> bool {
    matches!(command, "/exit" | "/quit")
}

fn wrap_text(text: &str, width: u16) -> Vec<String> {
    let width = width.max(16) as usize;
    let text = sanitize_tui_text(text);
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

fn normalize_input_history(items: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for item in items {
        let item = item.trim().to_string();
        if item.is_empty() || normalized.last() == Some(&item) {
            continue;
        }
        normalized.push(item);
    }
    if normalized.len() > 100 {
        normalized.split_off(normalized.len() - 100)
    } else {
        normalized
    }
}

fn recall_input_history(
    input_history: &[String],
    history_index: Option<usize>,
    history_draft: Option<String>,
    current_input: &str,
    direction: isize,
) -> (Option<usize>, Option<String>, String) {
    if input_history.is_empty() {
        return (history_index, history_draft, current_input.to_string());
    }

    let draft = if direction < 0 && history_index.is_none() {
        Some(current_input.to_string())
    } else {
        history_draft
    };
    let current = history_index.unwrap_or(input_history.len());
    let next = if direction < 0 {
        current.saturating_sub(1)
    } else {
        (current + 1).min(input_history.len())
    };
    if next < input_history.len() {
        (
            Some(next),
            draft,
            input_history.get(next).cloned().unwrap_or_default(),
        )
    } else {
        (None, None, draft.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_visual_lines_wrap_to_available_width() {
        let mut input = ComposerInput::default();
        input.insert_text("abcdef");

        let lines = composer_visual_lines(&input, 6);
        assert_eq!(rendered_lines(&lines), vec!["›  abc", "   def"]);
        assert_eq!(composer_visual_line_count(&input, 6), 2);
        assert_eq!(composer_visual_position("abcdef", 6), (1, 6));
    }

    #[test]
    fn composer_visual_position_tracks_newlines_and_wide_chars() {
        assert_eq!(composer_visual_position("你a\nb", 7), (1, 4));
        assert_eq!(composer_visual_position("你好吗", 7), (1, 5));
    }

    #[test]
    fn composer_display_expands_tabs() {
        let mut input = ComposerInput::default();
        input.insert_text("a\tb");

        assert_eq!(rendered_lines(&input.display_lines()), vec!["›  a    b"]);
        assert_eq!(composer_visual_position(&expand_tabs("a\tb"), 20), (0, 9));
    }

    #[test]
    fn static_commands_include_skill_commands() {
        let commands = static_commands();
        assert!(commands.iter().any(|command| command.value == "/fork"));
        assert!(commands.iter().any(|command| command.value == "/new"));
        assert!(commands.iter().any(|command| command.value == "/exit"));
        assert!(commands
            .iter()
            .any(|command| command.value == "/skill list"));
        assert!(commands
            .iter()
            .any(|command| command.value == "/model status"));
    }

    #[test]
    fn tui_exit_command_accepts_exit_and_quit() {
        assert!(is_tui_exit_command("/exit"));
        assert!(is_tui_exit_command("/quit"));
        assert!(!is_tui_exit_command("/exit now"));
    }

    #[test]
    fn command_filter_matches_skill_prefix_syntax() {
        let command = CommandEntry {
            value: "/skill:code-review ".to_string(),
            description: "Review code".to_string(),
            accepts_arguments: true,
            searchable: "skill code-review 技能 .agents/skills".to_string(),
        };

        let colon_terms = command_filter_terms("/skill:code");
        assert_eq!(colon_terms, vec!["skill", "code"]);
        assert!(command_matches_filter(&command, &colon_terms));

        let space_terms = command_filter_terms("/skill code");
        assert_eq!(space_terms, vec!["skill", "code"]);
        assert!(command_matches_filter(&command, &space_terms));
    }

    #[test]
    fn command_filter_matches_workflow_names() {
        let command = CommandEntry {
            value: "/review-loop ".to_string(),
            description: "启动 workflow: Review Loop".to_string(),
            accepts_arguments: true,
            searchable: "/review-loop Review Loop Verify changes workflow review-loop".to_string(),
        };

        let id_terms = command_filter_terms("/review");
        assert!(command_matches_filter(&command, &id_terms));

        let name_terms = command_filter_terms("/Review Loop");
        assert_eq!(name_terms, vec!["review", "loop"]);
        assert!(command_matches_filter(&command, &name_terms));
    }

    #[test]
    fn parses_tui_fork_command() {
        assert!(is_tui_fork_command("/fork"));
        assert!(!is_tui_fork_command("/fork now"));
        assert!(!is_tui_fork_command("/new"));
    }

    #[test]
    fn parses_tui_new_command() {
        assert!(is_tui_new_command("/new"));
        assert!(!is_tui_new_command("/new now"));
        assert!(!is_tui_new_command("/fork"));
    }

    #[test]
    fn tmux_pane_launch_command_resumes_session() {
        let cli = test_cli_config();
        let command =
            build_pane_launch_command(true, true, Path::new("/tmp/remi-cat"), "session-1", &cli)
                .expect("tmux should be preferred");

        assert_eq!(command.kind, "tmux");
        assert_eq!(command.program, "tmux");
        assert_eq!(
            os_args_to_strings(&command.args),
            vec![
                "split-window",
                "-h",
                "/tmp/remi-cat",
                "tui",
                "resume",
                "session-1",
                "--user",
                "u1",
                "--name",
                "Alice",
            ]
        );
    }

    #[test]
    fn zellij_pane_launch_command_resumes_session() {
        let cli = test_cli_config();
        let command =
            build_pane_launch_command(false, true, Path::new("/tmp/remi-cat"), "session-1", &cli)
                .expect("zellij should be supported");

        assert_eq!(command.kind, "Zellij");
        assert_eq!(command.program, "zellij");
        assert_eq!(
            os_args_to_strings(&command.args),
            vec![
                "action",
                "new-pane",
                "--direction",
                "right",
                "--",
                "/tmp/remi-cat",
                "tui",
                "resume",
                "session-1",
                "--user",
                "u1",
                "--name",
                "Alice",
            ]
        );
    }

    #[test]
    fn pane_launch_command_is_absent_without_supported_multiplexer() {
        let cli = test_cli_config();
        assert!(build_pane_launch_command(
            false,
            false,
            Path::new("/tmp/remi-cat"),
            "session-1",
            &cli
        )
        .is_none());
    }

    #[test]
    fn warp_tab_config_runs_resume_command() {
        let cli = test_cli_config();
        let toml = warp_tab_config_toml(
            "remi_cat_fork_session1",
            Path::new("/tmp/remi cat"),
            "session-1",
            &cli,
        );

        assert!(toml.contains("name = \"Remi fork remi_cat_fork_session1\""));
        assert!(toml.contains("type = \"terminal\""));
        assert!(toml.contains("is_focused = true"));
        assert!(toml.contains("'/tmp/remi cat' tui resume session-1 --user u1 --name Alice"));
    }

    #[test]
    fn input_history_recall_preserves_current_draft() {
        let history = vec![
            "/model status".to_string(),
            "plain message".to_string(),
            "/skill list".to_string(),
        ];
        let (index, draft, input) = recall_input_history(&history, None, None, "draft", -1);
        assert_eq!(index, Some(2));
        assert_eq!(draft.as_deref(), Some("draft"));
        assert_eq!(input, "/skill list");

        let (index, draft, input) = recall_input_history(&history, index, draft, "", -1);
        assert_eq!(index, Some(1));
        assert_eq!(draft.as_deref(), Some("draft"));
        assert_eq!(input, "plain message");

        let (index, draft, input) = recall_input_history(&history, index, draft, "", 1);
        assert_eq!(index, Some(2));
        assert_eq!(draft.as_deref(), Some("draft"));
        assert_eq!(input, "/skill list");

        let (index, draft, input) = recall_input_history(&history, index, draft, "", 1);
        assert_eq!(index, None);
        assert_eq!(draft, None);
        assert_eq!(input, "draft");
    }

    #[test]
    fn normalizes_command_history_like_other_input() {
        let history = normalize_input_history(vec![
            " /model status ".to_string(),
            "/model status".to_string(),
            "/skill list".to_string(),
        ]);
        assert_eq!(history, vec!["/model status", "/skill list"]);
    }

    #[test]
    fn char_boundaries_handle_utf8() {
        let text = "a你b";
        let cursor = "a你".len();
        assert_eq!(previous_char_boundary(text, cursor), 1);
        assert_eq!(next_char_boundary(text, 1), cursor);
    }

    #[test]
    fn input_line_ranges_and_char_offsets_handle_utf8() {
        let text = "ab\n你cd\nz";
        let ranges = input_line_ranges(text);
        assert_eq!(ranges, vec![(0, 2), (3, 8), (9, 10)]);
        assert_eq!(
            byte_index_for_char_offset(text, ranges[1].0, ranges[1].1, 0),
            3
        );
        assert_eq!(
            byte_index_for_char_offset(text, ranges[1].0, ranges[1].1, 1),
            6
        );
        assert_eq!(
            byte_index_for_char_offset(text, ranges[1].0, ranges[1].1, 99),
            8
        );
    }

    #[test]
    fn composer_collapses_large_paste_but_submits_full_text() {
        let paste = (0..10)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut input = ComposerInput::default();
        input.insert_text("before ");
        input.insert_paste(paste.clone());
        input.insert_text(" after");

        assert_eq!(input.to_text(), format!("before {paste} after"));
        assert!(input.display_text().contains("[pasted 10 lines,"));
        assert!(!input.display_text().contains("line 9"));
    }

    #[test]
    fn composer_keeps_short_paste_editable() {
        let mut input = ComposerInput::default();
        input.insert_paste("a\nb".to_string());

        assert_eq!(input.to_text(), "a\nb");
        assert_eq!(input.display_text(), "a\nb");
    }

    #[test]
    fn composer_collapses_more_than_three_pasted_lines() {
        let mut input = ComposerInput::default();
        input.insert_paste("a\nb\nc\nd".to_string());

        assert_eq!(input.to_text(), "a\nb\nc\nd");
        assert!(input.display_text().starts_with("[pasted 4 lines,"));
    }

    #[test]
    fn composer_moves_across_paste_as_atomic_chunk() {
        let paste = "x".repeat(PASTE_CHUNK_CHAR_THRESHOLD + 1);
        let mut input = ComposerInput::default();
        input.insert_text("a");
        input.insert_paste(paste);
        input.insert_text("b");

        input.move_home();
        input.move_right();
        let before_chunk = input.cursor;
        input.move_right();
        let after_chunk = input.cursor;
        input.move_right();

        assert_ne!(before_chunk, after_chunk);
        assert_eq!(input.cursor_display_byte(), input.display_text().len());
    }

    #[test]
    fn composer_deletes_paste_as_atomic_chunk() {
        let paste = "x".repeat(PASTE_CHUNK_CHAR_THRESHOLD + 1);
        let mut input = ComposerInput::default();
        input.insert_text("a");
        input.insert_paste(paste);
        input.insert_text("b");

        input.move_home();
        input.move_right();
        input.delete();

        assert_eq!(input.to_text(), "ab");
        assert_eq!(input.display_text(), "ab");
    }

    #[test]
    fn composer_backspace_deletes_paste_as_atomic_chunk() {
        let paste = "x".repeat(PASTE_CHUNK_CHAR_THRESHOLD + 1);
        let mut input = ComposerInput::default();
        input.insert_text("a");
        input.insert_paste(paste);
        input.insert_text("b");

        input.move_end();
        input.move_left();
        input.backspace();

        assert_eq!(input.to_text(), "ab");
        assert_eq!(input.display_text(), "ab");
    }

    #[test]
    fn truncates_wide_text_with_ellipsis() {
        assert_eq!(truncate_for_width("abcdef", 4), "abc…");
        assert_eq!(truncate_for_width("你好世界", 5), "你好…");
    }

    #[test]
    fn history_gutter_has_stable_display_width() {
        for prefix in ["›", "•", "↳", "·", "?", "x", "!", "□", "Δ"] {
            assert_eq!(UnicodeWidthStr::width(history_gutter(prefix).as_str()), 4);
        }
    }

    #[test]
    fn context_usage_uses_model_context_window() {
        assert_eq!(context_usage_percent(16_000, 0, 16_000, 128_000), Some(13));
        assert_eq!(context_usage_percent(260_000, 0, 64_000, 128_000), Some(50));
        assert_eq!(
            context_usage_percent(130_000, 0, 130_000, 128_000),
            Some(100)
        );
        assert_eq!(context_usage_percent(1, 0, 1, 0), None);
    }

    #[test]
    fn extracts_apply_patch_argument() {
        let args = serde_json::json!({
            "patch": "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n"
        })
        .to_string();
        let patch = extract_patch_arg(&args).expect("patch argument should parse");
        assert!(patch.contains("@@ -1 +1 @@"));
        assert!(patch.contains("+new"));
    }

    #[test]
    fn patch_diff_cell_renders_colored_diff_lines() {
        let cell = HistoryCell::patch_diff(
            "tool-1".to_string(),
            "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
            "12ms".to_string(),
            ToolVisualStatus::Success,
        );
        let lines = cell.lines(80);
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.as_ref() == "-old" && span.style.fg == Some(Color::Red))
        }));
        assert!(lines.iter().any(|line| line.spans.iter().any(|span| {
            span.content.as_ref() == "+new" && span.style.fg == Some(CODEX_GREEN)
        })));
        assert!(lines.iter().any(|line| line.spans.iter().any(|span| {
            span.content.as_ref() == "@@ -1 +1 @@" && span.style.fg == Some(CODEX_CYAN)
        })));
    }

    #[test]
    fn assistant_cell_renders_markdown_styles() {
        let cell = HistoryCell::assistant("hello **bold** and `code`");
        let lines = cell.lines(80);
        assert!(lines.iter().any(|line| line.spans.iter().any(|span| {
            span.content.as_ref() == "bold" && span.style.add_modifier.contains(Modifier::BOLD)
        })));
        assert!(lines.iter().any(|line| line.spans.iter().any(|span| {
            span.content.as_ref() == "code" && span.style.fg == Some(Color::Yellow)
        })));
    }

    #[test]
    fn history_cells_strip_terminal_control_sequences() {
        let cells = [
            HistoryCell::user("hello\x1b[?7l wrapped"),
            HistoryCell::assistant("**ok**\x1b]0;bad\x07\u{202e}"),
            HistoryCell::tool(
                "tool-1".to_string(),
                "tool\x1b[31m".to_string(),
                "result\rb\x08\x07".to_string(),
                "meta\x1b[0m".to_string(),
                ToolVisualStatus::Success,
            ),
            HistoryCell::patch_diff(
                "patch-1".to_string(),
                "--- a\n+++ b\n+\x1b[32mnew\x1b[0m\n".to_string(),
                "1ms".to_string(),
                ToolVisualStatus::Success,
            ),
        ];

        for cell in cells {
            let rendered = rendered_text(&cell.lines(80));
            assert!(
                !contains_tui_control(&rendered),
                "rendered cell still contains controls: {rendered:?}"
            );
        }
    }

    #[test]
    fn composer_display_strips_controls_but_keeps_submitted_text() {
        let raw = "show\x1b[31m red\x1b[0m\nnext\x07";
        let mut input = ComposerInput::default();
        input.insert_paste(raw.to_string());

        assert_eq!(input.to_text(), raw);
        let rendered = rendered_text(&input.display_lines());
        assert!(!contains_tui_control(&rendered));
        assert!(rendered.contains("show red"));
        assert!(rendered.contains("next"));
    }

    #[test]
    fn approval_updates_reuse_one_history_cell() {
        let mut cells = Vec::new();
        upsert_approval_cell(
            &mut cells,
            HistoryCell::approval_with_title(
                "approval-1".to_string(),
                "approval · waiting".to_string(),
                "waiting".to_string(),
                ToolVisualStatus::Running,
            ),
        );
        upsert_approval_cell(
            &mut cells,
            HistoryCell::approval_with_title(
                "approval-1".to_string(),
                "approval · resolved".to_string(),
                String::new(),
                ToolVisualStatus::Success,
            ),
        );
        upsert_approval_cell(
            &mut cells,
            HistoryCell::approval_with_title(
                "approval-2".to_string(),
                "approval · waiting".to_string(),
                "other".to_string(),
                ToolVisualStatus::Running,
            ),
        );

        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].title, "approval · resolved");
        assert!(cells[0].body.is_empty());
        assert_eq!(cells[1].body, "other");
    }

    #[test]
    fn context_compaction_updates_reuse_one_history_cell() {
        let mut cells = Vec::new();
        let started = ContextCompactionEvent {
            id: "compact-1".to_string(),
            thread_id: "thread-1".to_string(),
            status: ContextCompactionStatus::Started,
            source: bot_core::ContextCompactionSource::Auto,
            compacted_messages: 4,
            remaining_messages: 3,
            error: None,
        };
        let mut completed = started.clone();
        completed.status = ContextCompactionStatus::Completed;
        completed.remaining_messages = 2;
        upsert_context_compaction_cell(&mut cells, context_compaction_cell(started));
        upsert_context_compaction_cell(&mut cells, context_compaction_cell(completed));

        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].title, "context compressed");
        assert_eq!(cells[0].body, "compacted 4 messages; remaining 2 messages");
        assert_eq!(cells[0].status, ToolVisualStatus::Success);
    }

    #[test]
    fn resolved_approval_cell_collapses_to_title_only() {
        let request = ToolApprovalRequest {
            id: "approval-1".to_string(),
            session_id: "session-1".to_string(),
            run_id: "run-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "fs_remove".to_string(),
            risk: bot_core::ToolRiskLevel::Medium,
            args_summary: "{\"path\":\"target/tmp\"}".to_string(),
            platform: Some("tui".to_string()),
            review: Some(bot_core::ToolRiskReview {
                risk: bot_core::ToolRiskLevel::Medium,
                reason: "mutates files".to_string(),
                concerns: vec!["Deletes local data".to_string()],
            }),
        };
        let pending = approval_cell(&request, "waiting", 1, None, None);
        assert!(pending.body.contains("› 2."));
        assert!(pending.body.contains("review: mutates files"));

        let resolved = approval_cell(
            &request,
            "resolved",
            1,
            None,
            Some(ToolApprovalDecision::AllowOnce),
        );
        let lines = resolved.lines(100);
        assert!(resolved.body.is_empty());
        assert_eq!(lines.len(), 2);
        assert!(lines[0].spans.iter().any(|span| {
            span.content
                .contains("approval · resolved · fs_remove · AllowOnce")
        }));
    }

    #[test]
    fn supervisor_state_keeps_recent_events_and_report_summary() {
        let mut state = SupervisorUiState::default();
        for index in 0..5 {
            state.push_event(SupervisorEventDisplay {
                kind: "tool_call",
                label: format!("calling tool{index}"),
                body: format!("args{index}"),
            });
        }
        assert_eq!(state.events.len(), 3);
        assert_eq!(state.events[0].label, "calling tool2");

        let report = WorkflowReport {
            workflow_id: "goal".to_string(),
            workflow_name: "Goal".to_string(),
            from_node: "review".to_string(),
            edge: Some("continue".to_string()),
            to_node: "work".to_string(),
            status: bot_core::supervisor_workflow::WorkflowStatus::Active,
            reason: "Need another step".to_string(),
            agent_message: None,
            next_node_message: None,
            supervisor_trace: Vec::new(),
            round: 2,
            max_rounds: bot_core::supervisor_workflow::WorkflowMaxRounds::Limited(5),
            error: None,
        };
        state.apply_report(&report);
        assert_eq!(
            state.resolved_title(),
            "supervisor · Goal · review -> work · Active"
        );
        assert!(state.body().contains("transition: review -> work"));
    }

    #[test]
    fn resolved_supervisor_cell_collapses_to_title_only() {
        let cell = HistoryCell::supervisor(
            "supervisor-1".to_string(),
            "supervisor · Goal · review -> work · Active".to_string(),
            String::new(),
            "review -> work".to_string(),
            ToolVisualStatus::Success,
        );
        let lines = cell.lines(100);
        assert_eq!(lines.len(), 2);
        assert!(lines[0]
            .spans
            .iter()
            .any(|span| span.content.contains("supervisor · Goal")));
    }

    #[test]
    fn parses_and_formats_tool_metadata() {
        assert_eq!(format_elapsed(42), "42ms");
        assert_eq!(format_elapsed(1250), "1.2s");
        assert_eq!(format_elapsed(90_000), "1m30s");
        assert_eq!(format_elapsed(970_000), "16m10s");
        assert_eq!(format_elapsed(5_400_000), "1h30m");
        assert_eq!(format_elapsed(172_800_000), "2d00h");
        assert_eq!(parse_tool_args("").unwrap(), empty_tool_args());
        assert_eq!(
            parse_tool_args("{\"command\":\"cargo test\"}")
                .unwrap()
                .get("command")
                .and_then(|value| value.as_str()),
            Some("cargo test")
        );
    }

    #[test]
    fn tool_body_truncates_response_summaries() {
        let result = "x".repeat(MAX_TOOL_BODY_CHARS + 80);
        let pretty = PrettyToolCall::completed(
            "call-1",
            "unknown_tool",
            &empty_tool_args(),
            &result,
            true,
            10,
        );
        let body = tool_body(&pretty);
        assert!(body.chars().count() <= MAX_TOOL_BODY_CHARS);
        assert!(body.ends_with('…'));
        assert_ne!(body, result);
    }

    #[test]
    fn tool_body_preserves_bash_summary_lines() {
        let pretty = PrettyToolCall::completed(
            "call-1",
            "bash",
            &serde_json::json!({"command":"cargo test"}),
            "one\ntwo\nthree\nfour\n",
            true,
            10,
        );
        let body = tool_body(&pretty);

        assert!(body.contains("$ cargo test\n输出 4 行:\none\ntwo\nthree"));
        assert!(!body.contains("four"));
    }

    #[test]
    fn stream_events_merge_consecutive_output_blocks() {
        let mut body = String::new();
        append_stream_event(&mut body, "output\nhello ");
        append_stream_event(&mut body, "output\nworld");
        append_stream_event(&mut body, "thinking\nnext");

        assert_eq!(body, "output\nhello world\n\nthinking\nnext");
    }

    #[test]
    fn stream_events_merge_tool_argument_deltas_by_call_id() {
        let mut body = String::new();
        append_stream_event(&mut body, "tool args: call_1\n{");
        append_stream_event(&mut body, "tool args: call_1\n\"");
        append_stream_event(&mut body, "tool args: call_1\npath");
        append_stream_event(&mut body, "tool args: call_1\n\":\"");
        append_stream_event(&mut body, "tool args: call_1\nmemory");
        append_stream_event(&mut body, "tool args: call_1\n\"}");
        append_stream_event(&mut body, "tool call: fs_ls\ncall_id: call_2");

        assert_eq!(
            body,
            "tool args: call_1\n{\"path\":\"memory\"}\n\ntool call: fs_ls\ncall_id: call_2"
        );
    }

    #[test]
    fn stream_events_do_not_duplicate_tool_call_start() {
        let mut body = String::new();
        append_stream_event(&mut body, "tool call: fs_ls\ncall_id: call_1");
        append_stream_event(&mut body, "tool args: call_1\n{\"path\":\".\"}");
        append_stream_event(&mut body, "tool call: fs_ls\ncall_id: call_1");
        append_stream_event(&mut body, "tool result: fs_ls\ncall_id: call_1\nok");

        assert_eq!(body.matches("tool call: fs_ls").count(), 1);
        assert_eq!(
            body,
            "tool call: fs_ls\ncall_id: call_1\n\ntool args: call_1\n{\"path\":\".\"}\n\ntool result: fs_ls\ncall_id: call_1\nok"
        );
    }

    #[test]
    fn sub_session_format_preserves_hierarchy_context() {
        let event = SubSessionEvent::new(
            "parent-tool-call",
            remi_agentloop::prelude::ThreadId("thread-1234567890".to_string()),
            remi_agentloop::prelude::RunId("run-1234567890".to_string()),
            "coder",
            Some("Coder Agent".to_string()),
            2,
            SubSessionEventPayload::Start,
        );

        assert_eq!(format_sub_session_title(&event), "    sub-agent · coder");
        let meta = format_sub_session_meta(&event);
        assert!(meta.contains("depth 2"));
        assert!(meta.contains("parent parent-tool…"));
        assert!(meta.contains("thread thread-1234…"));
        assert_eq!(sub_session_input(&event).as_deref(), Some("Coder Agent"));
        assert!(format_sub_session_event(&event).is_none());
    }

    #[test]
    fn sub_session_cell_id_is_stable_across_poll_runs() {
        let base = |run_id: &str| {
            SubSessionEvent::new(
                "parent-tool-call",
                remi_agentloop::prelude::ThreadId("thread-1".to_string()),
                remi_agentloop::prelude::RunId(run_id.to_string()),
                "acp",
                Some("Codex ACP".to_string()),
                1,
                SubSessionEventPayload::Start,
            )
        };

        assert_eq!(
            sub_session_id(&base("run-1")),
            sub_session_id(&base("run-2"))
        );
    }

    #[test]
    fn sub_session_status_and_kind_are_persistable() {
        let acp = SubSessionEvent::new(
            "parent-tool-call",
            remi_agentloop::prelude::ThreadId("thread-1".to_string()),
            remi_agentloop::prelude::RunId("run-1".to_string()),
            "acp",
            Some("Codex ACP".to_string()),
            1,
            SubSessionEventPayload::Start,
        );
        let agent = SubSessionEvent::new(
            "parent-tool-call",
            remi_agentloop::prelude::ThreadId("thread-2".to_string()),
            remi_agentloop::prelude::RunId("run-1".to_string()),
            "coder",
            None,
            1,
            SubSessionEventPayload::Error {
                message: "failed".to_string(),
            },
        );

        assert_eq!(sub_session_kind(&acp), SubSessionKind::Acp);
        assert_eq!(sub_session_kind(&agent), SubSessionKind::Agent);
        assert_eq!(sub_session_status_label(&acp.payload), "running");
        assert_eq!(sub_session_status_label(&agent.payload), "error");
    }

    #[test]
    fn sub_session_events_convert_to_child_history_messages() {
        let base = |payload| {
            SubSessionEvent::new(
                "parent-tool-call",
                remi_agentloop::prelude::ThreadId("thread-1".to_string()),
                remi_agentloop::prelude::RunId("run-1".to_string()),
                "coder",
                Some("Investigate issue".to_string()),
                1,
                payload,
            )
        };

        let start_messages = sub_session_history_messages(&base(SubSessionEventPayload::Start));
        assert_eq!(start_messages.len(), 1);
        assert_eq!(
            start_messages[0].content.text_content(),
            "Investigate issue"
        );

        let output_messages = sub_session_history_messages(&base(SubSessionEventPayload::Delta {
            content: "progress".to_string(),
        }));
        assert_eq!(output_messages.len(), 1);
        assert_eq!(output_messages[0].content.text_content(), "progress");

        let done_messages = sub_session_history_messages(&base(SubSessionEventPayload::Done {
            final_output: None,
        }));
        assert_eq!(done_messages[0].content.text_content(), "Done.");
    }

    #[tokio::test]
    async fn acp_sub_session_event_log_round_trips_structured_events() {
        let dir = std::env::temp_dir().join(format!("remi-tui-acp-log-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let event = SubSessionEvent::new(
            "call-acp",
            remi_agentloop::prelude::ThreadId("acp-sub-1".to_string()),
            remi_agentloop::prelude::RunId("run-acp-1".to_string()),
            "acp",
            Some("Codex ACP".to_string()),
            1,
            SubSessionEventPayload::ToolCallStart {
                id: "tool-1".to_string(),
                name: "bash".to_string(),
            },
        );

        append_sub_session_event_log(&dir, "child-session", &event)
            .await
            .unwrap();
        let raw = tokio::fs::read_to_string(sub_session_event_log_path(&dir, "child-session"))
            .await
            .unwrap();
        let restored: SubSessionEvent = serde_json::from_str(raw.trim()).unwrap();

        assert_eq!(restored.agent_name, "acp");
        assert_eq!(restored.sub_thread_id.0, "acp-sub-1");
        assert_eq!(restored.sub_run_id.0, "run-acp-1");
        assert!(matches!(
            restored.payload,
            SubSessionEventPayload::ToolCallStart { ref name, .. } if name == "bash"
        ));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn interrupt_requested_system_cell_is_not_duplicated() {
        let mut requested = false;
        let mut cells = Vec::new();

        assert!(push_interrupt_requested_once(&mut requested, &mut cells));
        assert!(!push_interrupt_requested_once(&mut requested, &mut cells));

        let count = cells
            .iter()
            .filter(|cell| cell.title == "system" && cell.body == "Interrupt requested.")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn sub_session_parent_summary_hides_model_output() {
        let base = |payload| {
            SubSessionEvent::new(
                "parent-tool-call",
                remi_agentloop::prelude::ThreadId("thread-1".to_string()),
                remi_agentloop::prelude::RunId("run-1".to_string()),
                "coder",
                None,
                1,
                payload,
            )
        };

        assert!(
            format_sub_session_event(&base(SubSessionEventPayload::Delta {
                content: "streaming text".to_string(),
            }))
            .is_none()
        );
        assert!(format_sub_session_event(&base(SubSessionEventPayload::ThinkingStart)).is_none());
        assert!(
            format_sub_session_event(&base(SubSessionEventPayload::ToolCallStart {
                id: "call-1".to_string(),
                name: "fs_ls".to_string(),
            }))
            .is_some_and(|body| body.contains("tool call: fs_ls"))
        );
        assert_eq!(
            format_sub_session_event(&base(SubSessionEventPayload::Done {
                final_output: Some("final answer".to_string()),
            }))
            .as_deref(),
            Some("done")
        );
    }

    #[test]
    fn sub_session_state_prints_input_and_latest_three_tools() {
        let mut state = SubSessionUiState::from_event(&SubSessionEvent::new(
            "parent-tool-call",
            remi_agentloop::prelude::ThreadId("thread-1".to_string()),
            remi_agentloop::prelude::RunId("run-1".to_string()),
            "explorer",
            Some("Explore this workspace in detail".to_string()),
            1,
            SubSessionEventPayload::Start,
        ));
        for index in 0..5 {
            state.upsert_tool(SubToolDisplay {
                id: format!("call-{index}"),
                title: format!("查看目录 path-{index}"),
                summary: format!("列出 {index} 项"),
                status: ToolVisualStatus::Success,
            });
            let tool = state.tools.last().expect("tool just inserted").clone();
            state.upsert_activity(SubSessionActivity::from_tool(
                &format!("call-{index}"),
                &tool,
            ));
        }

        assert_eq!(state.title(), "  sub-agent · explorer · calling 5 tools");
        let body = state.body();
        assert!(body.contains("  input: Explore this workspace in detail"));
        assert!(!body.contains("path-1"));
        assert!(body.contains("path-2"));
        assert!(body.contains("path-4"));
    }

    #[test]
    fn sub_session_parent_body_does_not_print_final_output() {
        let mut state = SubSessionUiState::from_event(&SubSessionEvent::new(
            "parent-tool-call",
            remi_agentloop::prelude::ThreadId("thread-1".to_string()),
            remi_agentloop::prelude::RunId("run-1".to_string()),
            "explorer",
            Some("Explore".to_string()),
            0,
            SubSessionEventPayload::Start,
        ));
        state.done = true;
        state.final_output = Some("final answer".to_string());

        let body = state.body();
        assert!(!body.contains("final answer"));
        assert!(body.contains("input: Explore"));
    }

    #[test]
    fn formats_todo_state_for_tui_card() {
        let items = vec![
            bot_core::todo::TodoItem {
                id: 1,
                content: "Draft release notes".to_string(),
                description: Some("Include migration notes".to_string()),
                done: false,
                batch_id: Some(7),
                batch_title: Some("Release".to_string()),
                batch_index: Some(0),
                storage_kind: Default::default(),
                collection_uuid: None,
                thing_uuid: None,
            },
            bot_core::todo::TodoItem {
                id: 2,
                content: "Ship".to_string(),
                description: None,
                done: true,
                batch_id: None,
                batch_title: None,
                batch_index: None,
                storage_kind: Default::default(),
                collection_uuid: None,
                thing_uuid: None,
            },
        ];
        let rendered = format_todo_state(&items);
        assert!(rendered.contains("[ ] #1 Draft release notes · Release"));
        assert!(rendered.contains("Include migration notes"));
        assert!(rendered.contains("[x] #2 Ship"));
    }

    #[test]
    fn latest_active_todo_label_uses_last_unfinished_item() {
        let items = vec![
            bot_core::todo::TodoItem {
                id: 1,
                content: "Draft release notes".to_string(),
                description: None,
                done: false,
                batch_id: Some(7),
                batch_title: Some("Release".to_string()),
                batch_index: Some(0),
                storage_kind: Default::default(),
                collection_uuid: None,
                thing_uuid: None,
            },
            bot_core::todo::TodoItem {
                id: 2,
                content: "Ship".to_string(),
                description: None,
                done: true,
                batch_id: None,
                batch_title: None,
                batch_index: None,
                storage_kind: Default::default(),
                collection_uuid: None,
                thing_uuid: None,
            },
            bot_core::todo::TodoItem {
                id: 3,
                content: "Verify install".to_string(),
                description: None,
                done: false,
                batch_id: Some(7),
                batch_title: Some("Release".to_string()),
                batch_index: Some(1),
                storage_kind: Default::default(),
                collection_uuid: None,
                thing_uuid: None,
            },
        ];

        assert_eq!(
            latest_active_todo_label(&items).as_deref(),
            Some("todo #3 Verify install · Release")
        );
    }

    #[test]
    fn compact_workspace_label_uses_final_path_component() {
        assert_eq!(compact_workspace_label("/home/skye/remi-cat"), "remi-cat");
        assert_eq!(compact_workspace_label("/"), "/");
        assert_eq!(compact_workspace_label(""), ".");
    }

    #[test]
    fn appends_token_meta_to_history_cell() {
        let mut cell = HistoryCell::assistant("hello");
        append_token_meta(
            &mut cell,
            TokenDelta {
                prompt_tokens: 12,
                completion_tokens: 3,
            },
        );

        assert_eq!(cell.meta, "tokens +12p/+3c");
    }

    #[test]
    fn preserves_token_meta_when_replacing_tool_meta() {
        let meta = preserve_token_meta("120ms".to_string(), "tokens +12p/+3c");
        assert_eq!(meta, "120ms · tokens +12p/+3c");
    }

    #[test]
    fn preprocessing_depth_constant_remains_available() {
        assert!(crate::MAX_COMMAND_PREPROCESS_DEPTH >= 1);
        assert_eq!(crate::CLI_CHANNEL, "cli");
        assert_eq!(crate::CLI_USERNAME, "local-user");
    }

    #[test]
    fn default_tui_start_uses_fresh_channel_id() {
        let first = tui_start_channel_id(crate::CLI_CHAT_ID);
        let second = tui_start_channel_id(crate::CLI_CHAT_ID);

        assert!(first.starts_with("tui:"));
        assert!(second.starts_with("tui:"));
        assert_ne!(first, second);
        assert_eq!(tui_start_channel_id("desk"), "desk");
    }

    fn test_cli_config() -> CliConfig {
        CliConfig {
            enabled: true,
            tui: true,
            resume: false,
            resume_session_id: None,
            once: None,
            pure_prompt: false,
            admin_only: false,
            channel_id: "desk".to_string(),
            user_id: "u1".to_string(),
            username: "Alice".to_string(),
        }
    }

    fn os_args_to_strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn rendered_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered_lines(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }
}
