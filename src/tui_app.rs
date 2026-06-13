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
    CatEvent, Content, ContextCompactionEvent, ContextCompactionStatus, PrettyToolCall,
    PrettyToolStatus, StreamOptions, SupervisorTraceEvent, ThreadHistoryMessage,
    ToolApprovalDecision, ToolApprovalRequest, WorkflowReport,
};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
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
use remi_agentloop::prelude::{SubSessionEvent, SubSessionEventPayload};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;
use unicode_width::UnicodeWidthStr;

use crate::session::Session;
use crate::tui_markdown::{render_markdown_lines, MarkdownTheme};
use crate::workspace_files::{
    default_file_search_limit, search_workspace_files, WorkspaceFileMatch,
};
use crate::{
    process_runtime_commands, CliConfig, Runtime, RuntimeCommandPipelineResult,
    SESSION_INPUT_HISTORY_METADATA_KEY, SESSION_MODEL_PROFILE_METADATA_KEY,
};

const TUI_CHANNEL: &str = "tui";
const DEFAULT_TUI_SESSION: &str = "local-tui";
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

pub(crate) async fn run_tui(runtime: Rc<Runtime>, cli: CliConfig) -> anyhow::Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let session_id = if cli.resume {
        if let Some(selector) = cli.resume_session_id.as_deref() {
            resolve_resume_session_id(&runtime, selector).await?
        } else {
            select_resume_session_id(&runtime, &mut terminal.terminal).await?
        }
    } else {
        let session_channel = if cli.channel_id == crate::CLI_CHAT_ID {
            DEFAULT_TUI_SESSION
        } else {
            &cli.channel_id
        };
        runtime.sessions.lock().await.resolve_channel(
            TUI_CHANNEL,
            session_channel,
            &runtime.root_agent_id,
        )?
    };

    let mut app = TuiApp::new(runtime, cli, session_id).await;
    let result = app.run(&mut terminal.terminal).await;
    terminal.restore()?;
    result
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
            let title = session.title.as_deref().unwrap_or("untitled");
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
                Span::raw(truncate_for_width(title, 36)),
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
    let title = session.title.as_deref().unwrap_or("untitled");
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
        disable_raw_mode().context("disable raw mode")?;
        execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            DisableMouseCapture,
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputSegment {
    Text(String),
    Paste { text: String, label: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ComposerCursor {
    segment: usize,
    offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileMentionToken {
    start: usize,
    end: usize,
    query: String,
}

#[derive(Debug, Clone, Default)]
struct ComposerInput {
    segments: Vec<InputSegment>,
    cursor: ComposerCursor,
}

impl ComposerInput {
    fn is_empty(&self) -> bool {
        self.segments.iter().all(|segment| match segment {
            InputSegment::Text(text) => text.is_empty(),
            InputSegment::Paste { .. } => false,
        })
    }

    fn clear(&mut self) {
        self.segments.clear();
        self.cursor = ComposerCursor::default();
    }

    fn set_text(&mut self, text: String) {
        self.segments = if text.is_empty() {
            Vec::new()
        } else {
            vec![InputSegment::Text(text)]
        };
        self.cursor = self.end_cursor();
    }

    fn to_text(&self) -> String {
        let mut text = String::new();
        for segment in &self.segments {
            match segment {
                InputSegment::Text(value) => text.push_str(value),
                InputSegment::Paste { text: value, .. } => text.push_str(value),
            }
        }
        text
    }

    fn command_text(&self) -> Option<&str> {
        if self.segments.len() != 1 {
            return None;
        }
        match &self.segments[0] {
            InputSegment::Text(text) => Some(text),
            InputSegment::Paste { .. } => None,
        }
    }

    fn active_file_mention_token(&self) -> Option<FileMentionToken> {
        let text = self.to_text();
        let cursor = self.text_byte_for_cursor(self.cursor).min(text.len());
        active_file_mention_token(&text, cursor)
    }

    fn replace_text_range(&mut self, start: usize, end: usize, replacement: &str) {
        let start_cursor = self.cursor_from_text_byte(start.min(self.to_text().len()));
        let end_cursor = self.cursor_from_text_byte(end.min(self.to_text().len()));
        self.delete_range(start_cursor, end_cursor);
        self.cursor = start_cursor;
        self.insert_text(replacement);
    }

    fn display_text(&self) -> String {
        let mut text = String::new();
        for segment in &self.segments {
            match segment {
                InputSegment::Text(value) => text.push_str(value),
                InputSegment::Paste { label, .. } => text.push_str(label),
            }
        }
        text
    }

    fn display_lines(&self) -> Vec<Line<'static>> {
        let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
        for segment in &self.segments {
            match segment {
                InputSegment::Text(text) => {
                    for (index, part) in text.split('\n').enumerate() {
                        if index > 0 {
                            lines.push(Vec::new());
                        }
                        if !part.is_empty() {
                            lines
                                .last_mut()
                                .expect("composer line exists")
                                .push(Span::styled(
                                    part.to_string(),
                                    Style::default().fg(Color::White),
                                ));
                        }
                    }
                }
                InputSegment::Paste { label, .. } => {
                    lines
                        .last_mut()
                        .expect("composer line exists")
                        .push(Span::styled(
                            label.clone(),
                            Style::default().fg(CODEX_CYAN).add_modifier(Modifier::BOLD),
                        ));
                }
            }
        }
        lines
            .into_iter()
            .enumerate()
            .map(|(index, spans)| {
                let prefix = if index == 0 { "›  " } else { "   " };
                let mut line_spans = Vec::with_capacity(spans.len() + 1);
                line_spans.push(Span::styled(
                    prefix,
                    Style::default()
                        .fg(if index == 0 { CODEX_CYAN } else { CODEX_DIM })
                        .add_modifier(if index == 0 {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ));
                line_spans.extend(spans);
                Line::from(line_spans)
            })
            .collect()
    }

    fn insert_char(&mut self, ch: char) {
        let mut text = String::new();
        text.push(ch);
        self.insert_text(&text);
    }

    fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.insert_segment(InputSegment::Text(text.to_string()));
    }

    fn insert_paste(&mut self, text: String) {
        if should_collapse_paste(&text) {
            self.insert_segment(InputSegment::Paste {
                label: paste_chunk_label(&text),
                text,
            });
        } else {
            self.insert_text(&text);
        }
    }

    fn insert_segment(&mut self, segment: InputSegment) {
        let after = self.split_after_cursor();
        let insert_index = self.segments.len();
        self.segments.push(segment);
        self.cursor = self.cursor_after_segment(insert_index);
        self.segments.extend(after);
        self.merge_text_segments_around_cursor();
    }

    fn backspace(&mut self) {
        let previous = self.previous_cursor(self.cursor);
        if previous == self.cursor {
            return;
        }
        self.delete_range(previous, self.cursor);
        self.cursor = previous;
        self.merge_text_segments_around_cursor();
    }

    fn delete(&mut self) {
        let next = self.next_cursor(self.cursor);
        if next == self.cursor {
            return;
        }
        let current = self.cursor;
        self.delete_range(current, next);
        self.cursor = current;
        self.merge_text_segments_around_cursor();
    }

    fn move_left(&mut self) {
        self.cursor = self.previous_cursor(self.cursor);
    }

    fn move_right(&mut self) {
        self.cursor = self.next_cursor(self.cursor);
    }

    fn move_home(&mut self) {
        self.cursor = ComposerCursor::default();
    }

    fn move_end(&mut self) {
        self.cursor = self.end_cursor();
    }

    fn move_vertical(&mut self, direction: isize) {
        let display = self.display_text();
        let line_ranges = input_line_ranges(&display);
        if line_ranges.len() <= 1 {
            return;
        }
        let cursor_byte = self.cursor_display_byte();
        let current_line = line_ranges
            .iter()
            .position(|(start, end)| cursor_byte >= *start && cursor_byte <= *end)
            .unwrap_or_else(|| line_ranges.len().saturating_sub(1));
        let target_line = if direction < 0 {
            current_line.saturating_sub(1)
        } else {
            (current_line + 1).min(line_ranges.len().saturating_sub(1))
        };
        if target_line == current_line {
            return;
        }
        let (current_start, current_end) = line_ranges[current_line];
        let (target_start, target_end) = line_ranges[target_line];
        let desired_chars = display[current_start..cursor_byte.min(current_end)]
            .chars()
            .count();
        let target = byte_index_for_char_offset(&display, target_start, target_end, desired_chars);
        self.cursor = self.cursor_from_display_byte(target);
    }

    fn cursor_display_byte(&self) -> usize {
        let mut display_byte = 0usize;
        for (index, segment) in self.segments.iter().enumerate() {
            if index == self.cursor.segment {
                return match segment {
                    InputSegment::Text(_) => display_byte + self.cursor.offset,
                    InputSegment::Paste { label, .. } => {
                        display_byte
                            + if self.cursor.offset == 0 {
                                0
                            } else {
                                label.len()
                            }
                    }
                };
            }
            display_byte += segment.display_len();
        }
        display_byte
    }

    fn cursor_from_display_byte(&self, target: usize) -> ComposerCursor {
        let mut display_byte = 0usize;
        for (index, segment) in self.segments.iter().enumerate() {
            let next = display_byte + segment.display_len();
            if target <= next {
                return match segment {
                    InputSegment::Text(text) => ComposerCursor {
                        segment: index,
                        offset: target.saturating_sub(display_byte).min(text.len()),
                    },
                    InputSegment::Paste { label, .. } => ComposerCursor {
                        segment: index,
                        offset: usize::from(target.saturating_sub(display_byte) > label.len() / 2),
                    },
                };
            }
            display_byte = next;
        }
        self.end_cursor()
    }

    fn split_after_cursor(&mut self) -> Vec<InputSegment> {
        if self.cursor.segment >= self.segments.len() {
            return Vec::new();
        }
        match &mut self.segments[self.cursor.segment] {
            InputSegment::Text(text) => {
                let right = text.split_off(self.cursor.offset.min(text.len()));
                let mut after = self.segments.split_off(self.cursor.segment + 1);
                if !right.is_empty() {
                    after.insert(0, InputSegment::Text(right));
                }
                self.remove_empty_text_segments();
                self.cursor = self.end_cursor();
                after
            }
            InputSegment::Paste { .. } if self.cursor.offset == 0 => {
                let after = self.segments.split_off(self.cursor.segment);
                self.cursor = self.end_cursor();
                after
            }
            InputSegment::Paste { .. } => {
                let after = self.segments.split_off(self.cursor.segment + 1);
                self.cursor = self.end_cursor();
                after
            }
        }
    }

    fn delete_range(&mut self, start: ComposerCursor, end: ComposerCursor) {
        let (mut before, _) = split_segments_at(&self.segments, start);
        let (_, after) = split_segments_at(&self.segments, end);
        before.extend(after);
        self.segments = before;
        self.cursor = start;
    }

    fn text_byte_for_cursor(&self, cursor: ComposerCursor) -> usize {
        let mut byte = 0usize;
        for (index, segment) in self.segments.iter().enumerate() {
            if index == cursor.segment {
                return match segment {
                    InputSegment::Text(_) => byte + cursor.offset,
                    InputSegment::Paste { text, .. } => {
                        byte + if cursor.offset == 0 { 0 } else { text.len() }
                    }
                };
            }
            byte += segment.text_len();
        }
        byte
    }

    fn cursor_from_text_byte(&self, target: usize) -> ComposerCursor {
        let mut byte = 0usize;
        for (index, segment) in self.segments.iter().enumerate() {
            let next = byte + segment.text_len();
            if target <= next {
                return match segment {
                    InputSegment::Text(text) => ComposerCursor {
                        segment: index,
                        offset: target.saturating_sub(byte).min(text.len()),
                    },
                    InputSegment::Paste { text, .. } => ComposerCursor {
                        segment: index,
                        offset: usize::from(target.saturating_sub(byte) > text.len() / 2),
                    },
                };
            }
            byte = next;
        }
        self.end_cursor()
    }

    fn previous_cursor(&self, cursor: ComposerCursor) -> ComposerCursor {
        if self.segments.is_empty() {
            return cursor;
        }
        if cursor.segment >= self.segments.len() {
            return self.cursor_one_left_from_segment_end(self.segments.len() - 1);
        }
        match &self.segments[cursor.segment] {
            InputSegment::Text(text) if cursor.offset > 0 => ComposerCursor {
                segment: cursor.segment,
                offset: previous_char_boundary(text, cursor.offset.min(text.len())),
            },
            InputSegment::Paste { .. } if cursor.offset > 0 => ComposerCursor {
                segment: cursor.segment,
                offset: 0,
            },
            _ if cursor.segment > 0 => self.cursor_one_left_from_segment_end(cursor.segment - 1),
            _ => cursor,
        }
    }

    fn next_cursor(&self, cursor: ComposerCursor) -> ComposerCursor {
        if cursor.segment >= self.segments.len() {
            return cursor;
        }
        match &self.segments[cursor.segment] {
            InputSegment::Text(text) if cursor.offset < text.len() => ComposerCursor {
                segment: cursor.segment,
                offset: next_char_boundary(text, cursor.offset),
            },
            InputSegment::Paste { .. } if cursor.offset == 0 => ComposerCursor {
                segment: cursor.segment,
                offset: 1,
            },
            _ if cursor.segment + 1 < self.segments.len() => {
                self.cursor_one_right_from_segment_start(cursor.segment + 1)
            }
            _ => self.end_cursor(),
        }
    }

    fn cursor_one_right_from_segment_start(&self, segment: usize) -> ComposerCursor {
        match &self.segments[segment] {
            InputSegment::Text(text) => ComposerCursor {
                segment,
                offset: next_char_boundary(text, 0),
            },
            InputSegment::Paste { .. } => ComposerCursor { segment, offset: 1 },
        }
    }

    fn cursor_one_left_from_segment_end(&self, segment: usize) -> ComposerCursor {
        match &self.segments[segment] {
            InputSegment::Text(text) => ComposerCursor {
                segment,
                offset: previous_char_boundary(text, text.len()),
            },
            InputSegment::Paste { .. } => ComposerCursor { segment, offset: 0 },
        }
    }

    fn cursor_after_segment(&self, segment: usize) -> ComposerCursor {
        match &self.segments[segment] {
            InputSegment::Text(text) => ComposerCursor {
                segment,
                offset: text.len(),
            },
            InputSegment::Paste { .. } => ComposerCursor { segment, offset: 1 },
        }
    }

    fn end_cursor(&self) -> ComposerCursor {
        ComposerCursor {
            segment: self.segments.len(),
            offset: 0,
        }
    }

    fn remove_empty_text_segments(&mut self) {
        self.segments
            .retain(|segment| !matches!(segment, InputSegment::Text(text) if text.is_empty()));
    }

    fn merge_text_segments_around_cursor(&mut self) {
        self.remove_empty_text_segments();
        let text_byte = self.text_byte_for_cursor(self.cursor);
        let mut merged = Vec::new();
        for segment in std::mem::take(&mut self.segments) {
            match (merged.last_mut(), segment) {
                (Some(InputSegment::Text(left)), InputSegment::Text(right)) => {
                    left.push_str(&right);
                }
                (_, segment) => merged.push(segment),
            }
        }
        self.segments = merged;
        self.cursor = self.cursor_from_text_byte(text_byte);
    }
}

impl InputSegment {
    fn display_len(&self) -> usize {
        match self {
            InputSegment::Text(text) => text.len(),
            InputSegment::Paste { label, .. } => label.len(),
        }
    }

    fn text_len(&self) -> usize {
        match self {
            InputSegment::Text(text) => text.len(),
            InputSegment::Paste { text, .. } => text.len(),
        }
    }
}

fn split_segments_at(
    segments: &[InputSegment],
    cursor: ComposerCursor,
) -> (Vec<InputSegment>, Vec<InputSegment>) {
    let mut before = Vec::new();
    let mut after = Vec::new();
    for (index, segment) in segments.iter().cloned().enumerate() {
        if index < cursor.segment {
            before.push(segment);
            continue;
        }
        if index > cursor.segment {
            after.push(segment);
            continue;
        }
        match segment {
            InputSegment::Text(text) => {
                let split = cursor.offset.min(text.len());
                let (left, right) = text.split_at(split);
                if !left.is_empty() {
                    before.push(InputSegment::Text(left.to_string()));
                }
                if !right.is_empty() {
                    after.push(InputSegment::Text(right.to_string()));
                }
            }
            InputSegment::Paste { text, label } if cursor.offset == 0 => {
                after.push(InputSegment::Paste { text, label });
            }
            InputSegment::Paste { text, label } => {
                before.push(InputSegment::Paste { text, label });
            }
        }
    }
    (before, after)
}

fn should_collapse_paste(text: &str) -> bool {
    text.lines().count() > PASTE_CHUNK_LINE_THRESHOLD
        || text.chars().count() > PASTE_CHUNK_CHAR_THRESHOLD
}

fn paste_chunk_label(text: &str) -> String {
    let lines = text.lines().count().max(1);
    let bytes = text.len();
    let size = if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    };
    format!("[pasted {lines} lines, {size}]")
}

fn active_file_mention_token(input: &str, cursor: usize) -> Option<FileMentionToken> {
    let cursor = cursor.min(input.len());
    let before = &input[..cursor];
    let at = before.rfind('@')?;
    if at > 0 {
        let previous = before[..at].chars().next_back()?;
        if !previous.is_whitespace() {
            return None;
        }
    }
    let query = &before[at + 1..];
    if query.contains('@') || query.chars().any(char::is_whitespace) {
        return None;
    }
    Some(FileMentionToken {
        start: at,
        end: cursor,
        query: query.to_string(),
    })
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
    file_query: Option<String>,
    file_matches: Vec<WorkspaceFileMatch>,
    popup_selected: usize,
    show_shortcuts: bool,
    quit_hint_until: Option<Instant>,
    running: bool,
    run_started_at: Option<Instant>,
    cancel: Option<Arc<Notify>>,
    run_handle: Option<JoinHandle<()>>,
    queued_inputs: VecDeque<String>,
    input_history: Vec<String>,
    history_index: Option<usize>,
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
    supervisors: std::collections::HashMap<String, SupervisorUiState>,
    pending_approval: Option<ToolApprovalRequest>,
    approval_selected: usize,
    approval_state: &'static str,
    active_supervisor_id: Option<String>,
    last_todo_body: Option<String>,
    bot_tx: mpsc::UnboundedSender<BotEvent>,
    bot_rx: UnboundedReceiverStream<BotEvent>,
}

impl TuiApp {
    async fn new(runtime: Rc<Runtime>, cli: CliConfig, session_id: String) -> Self {
        let (bot_tx, bot_rx) = mpsc::unbounded_channel();
        let workspace_dir = current_workspace_dir(&runtime.data_dir);
        let workspace_root_label = current_workspace_root_label(&workspace_dir);
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
            file_query: None,
            file_matches: Vec::new(),
            popup_selected: 0,
            show_shortcuts: false,
            quit_hint_until: None,
            running: false,
            run_started_at: None,
            cancel: None,
            run_handle: None,
            queued_inputs: VecDeque::new(),
            input_history: Vec::new(),
            history_index: None,
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
            supervisors: std::collections::HashMap::new(),
            pending_approval: None,
            approval_selected: 0,
            approval_state: "waiting",
            active_supervisor_id: None,
            last_todo_body: None,
            bot_tx,
            bot_rx: UnboundedReceiverStream::new(bot_rx),
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
                } else {
                    self.move_input_cursor_vertical(-1);
                }
                Ok(false)
            }
            KeyCode::Down => {
                if self.popup_visible() {
                    self.move_popup_selection(1);
                } else {
                    self.move_input_cursor_vertical(1);
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
            KeyCode::Enter if self.popup_visible() => {
                match self.active_popup() {
                    Some(PopupKind::Command) if self.selected_command_accepts_arguments() => {
                        self.complete_selected_command();
                    }
                    Some(PopupKind::File) => self.complete_selected_file(),
                    _ => {
                        self.submit().await?;
                    }
                }
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

    async fn submit(&mut self) -> anyhow::Result<()> {
        let text = self.composer.to_text().trim().to_string();
        if text.is_empty() {
            return Ok(());
        }
        self.composer.clear();
        self.history_index = None;
        self.scroll = 0;
        self.popup_selected = 0;
        self.show_shortcuts = false;
        self.quit_hint_until = None;
        self.record_input_history(text.clone()).await;
        if is_tui_fork_command(&text) {
            self.start_fork_command();
            return Ok(());
        }
        if is_tui_new_command(&text) {
            self.start_new_command();
            return Ok(());
        }
        if self.running {
            self.queued_inputs.push_back(text);
            return Ok(());
        }
        self.start_turn(text);
        Ok(())
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
        self.status = StatusLine::default();
        self.active_tool_args.clear();
        self.active_tool_names.clear();
        self.active_tool_started_at.clear();
        self.sub_tool_args.clear();
        self.sub_tool_names.clear();
        self.sub_sessions.clear();
        self.supervisors.clear();
        self.pending_approval = None;
        self.approval_selected = 0;
        self.approval_state = "waiting";
        self.active_supervisor_id = None;
        self.last_todo_body = None;
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
            BotEvent::SupervisorProgress(progress) => self.upsert_supervisor_progress(progress),
            BotEvent::SupervisorReport(report) => self.upsert_supervisor_report(report),
            BotEvent::SubSession(event) => self.upsert_sub_session(event),
            BotEvent::ContextCompaction(event) => {
                upsert_context_compaction_cell(&mut self.cells, context_compaction_cell(event));
            }
            BotEvent::TodoState(body) => {
                if self.last_todo_body.as_deref() == Some(body.as_str()) {
                    return;
                }
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
            }
            BotEvent::Error(message) => {
                self.status.last_error = Some(message.clone());
                self.cells.push(HistoryCell::error(message));
                self.running = false;
                self.cancel = None;
                self.run_handle = None;
                self.sub_tool_args.clear();
                self.sub_tool_names.clear();
                self.sub_sessions.clear();
                self.supervisors.clear();
                self.active_supervisor_id = None;
                self.status.state = "error".to_string();
            }
            BotEvent::Done => {
                self.running = false;
                self.cancel = None;
                self.run_handle = None;
                self.sub_tool_args.clear();
                self.sub_tool_names.clear();
                self.sub_sessions.clear();
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

    fn render(&mut self, frame: &mut Frame<'_>) {
        let root = frame.area();
        let activity_height = self.activity_height();
        let footer_height = self.footer_height();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
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
        self.refresh_file_matches();
        match self.active_popup() {
            Some(PopupKind::Command) => self.render_command_popup(frame, chunks[4]),
            Some(PopupKind::File) => self.render_file_popup(frame, chunks[4]),
            None => {}
        }
    }

    fn render_status(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.flush_status_elapsed();
        let mut meta = Vec::new();
        if self.running {
            meta.push(format_elapsed(self.status.elapsed_ms));
        }
        let active_tools = self.active_tool_count();
        if active_tools > 0 {
            meta.push(format!("{active_tools} tools"));
        }
        if !self.queued_inputs.is_empty() {
            meta.push(format!("{} queued", self.queued_inputs.len()));
        }
        let error = self
            .status
            .last_error
            .as_deref()
            .map(|value| format!(" · {value}"))
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
            Span::styled(
                if meta.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", meta.join(" · "))
                },
                Style::default().fg(CODEX_DIM),
            ),
            Span::styled(error, Style::default().fg(Color::Red)),
        ]);
        let lines = vec![line, horizontal_rule(area.width)];
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn footer_context_line(&self) -> Line<'static> {
        let model_profile_id = self.runtime.sessions.try_lock().ok().and_then(|sessions| {
            sessions.metadata_string(&self.session_id, SESSION_MODEL_PROFILE_METADATA_KEY)
        });
        let effective_model = self
            .runtime
            .bot
            .effective_model_profile(model_profile_id.as_deref());
        let model = effective_model.profile.id.clone();
        let context_tokens = effective_model.profile.context_tokens;
        let session = short_session_label(&self.session_id);
        let mut spans = vec![
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
            Span::styled(format!("sid {session}"), Style::default().fg(CODEX_DIM)),
        ];
        if let Some(pct) = context_usage_percent(self.status.prompt_tokens, context_tokens) {
            spans.push(Span::styled(" · ", Style::default().fg(CODEX_DIM)));
            spans.push(Span::styled(
                format!("ctx {pct}%"),
                Style::default().fg(CODEX_DIM),
            ));
        }
        if self.status.model_elapsed_ms > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(CODEX_DIM)));
            spans.push(Span::styled(
                format!("model {}", format_elapsed(self.status.model_elapsed_ms)),
                Style::default().fg(CODEX_DIM),
            ));
        }
        Line::from(spans)
    }

    fn render_activity(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let mut lines = Vec::new();
        if self.running {
            let tool_count = self.active_tool_count();
            let mut detail = format!("{} elapsed", format_elapsed(self.status.elapsed_ms));
            if tool_count > 0 {
                detail.push_str(&format!(" · {tool_count} tools running"));
            }
            lines.push(Line::from(vec![
                Span::styled("  working", Style::default().fg(Color::Yellow)),
                Span::styled(" · ", Style::default().fg(CODEX_DIM)),
                Span::styled(detail, Style::default().fg(CODEX_DIM)),
                Span::styled(" · ", Style::default().fg(CODEX_DIM)),
                Span::styled("Ctrl+C to interrupt", Style::default().fg(Color::White)),
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
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        if self.show_shortcuts {
            let lines = vec![
                Line::from(Span::styled(
                    format!("{FOOTER_INDENT}shortcuts"),
                    Style::default().fg(CODEX_CYAN).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!(
                        "{FOOTER_INDENT}Enter send   Shift+Enter newline   Tab complete/queue   ? hide shortcuts"
                    ),
                    Style::default().fg(CODEX_DIM),
                )),
                Line::from(Span::styled(
                    format!(
                        "{FOOTER_INDENT}/ commands   Up/Down move input   Alt+Up/Down history   PgUp/PgDn or wheel scroll"
                    ),
                    Style::default().fg(CODEX_DIM),
                )),
            ];
            frame.render_widget(Paragraph::new(lines), area);
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
        frame.render_widget(Paragraph::new(hints).alignment(Alignment::Left), area);
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
        let start = history_scroll_offset(lines.len(), area.height as usize, self.scroll as usize);
        let visible_lines = lines
            .into_iter()
            .skip(start)
            .take(area.height as usize)
            .collect::<Vec<_>>();
        let paragraph = Paragraph::new(visible_lines).style(Style::default().fg(Color::Gray));
        frame.render_widget(Clear, area);
        frame.render_widget(paragraph, area);
    }

    fn render_composer(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        frame.render_widget(Paragraph::new(horizontal_rule(area.width)), area);
        let input_area = Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(1),
        );
        let lines = if self.composer.is_empty() {
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
            self.composer.display_lines()
        };
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(paragraph, input_area);
        if !self.running {
            let (x, y) = self.cursor_position(input_area);
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
                    Style::default().fg(CODEX_CYAN).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(CODEX_CYAN)
                };
                let description_style = Style::default().fg(CODEX_DIM);
                let marker = if selected { "›" } else { " " };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{marker} {:<22}", command.value.trim_end()),
                        command_style,
                    ),
                    Span::styled(command.description.clone(), description_style),
                ]))
            })
            .collect::<Vec<_>>();
        let list = List::new(rows).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" slash commands ")
                .border_style(Style::default().fg(CODEX_BORDER))
                .style(Style::default()),
        );
        frame.render_widget(list, area);
    }

    fn render_file_popup(&self, frame: &mut Frame<'_>, composer_area: Rect) {
        if self.file_matches.is_empty() {
            return;
        }
        let width = composer_area.width.min(88).max(44);
        let height = (self.file_matches.len() as u16 + 2).min(10);
        let x = composer_area.x + composer_area.width.saturating_sub(width);
        let y = composer_area.y.saturating_sub(height);
        let area = Rect::new(x, y, width, height);
        frame.render_widget(Clear, area);
        let rows = self
            .file_matches
            .iter()
            .take(height.saturating_sub(2) as usize)
            .enumerate()
            .map(|(index, file)| {
                let selected = index == self.popup_selected;
                let path_style = if selected {
                    Style::default().fg(CODEX_CYAN).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(CODEX_CYAN)
                };
                let marker = if selected { "›" } else { " " };
                let display = truncate_for_width(&format!("@{}", file.mention_path), 44);
                let relative = truncate_for_width(&file.display_path, 30);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{marker} {:<44}", display), path_style),
                    Span::styled(relative, Style::default().fg(CODEX_DIM)),
                ]))
            })
            .collect::<Vec<_>>();
        let list = List::new(rows).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" files ")
                .border_style(Style::default().fg(CODEX_BORDER))
                .style(Style::default()),
        );
        frame.render_widget(list, area);
    }

    fn insert_char(&mut self, ch: char) {
        self.composer.insert_char(ch);
        self.popup_selected = 0;
        self.history_index = None;
        self.quit_hint_until = None;
        self.refresh_file_matches();
    }

    fn insert_text(&mut self, text: &str) {
        self.composer.insert_text(text);
        self.popup_selected = 0;
        self.history_index = None;
        self.quit_hint_until = None;
        self.refresh_file_matches();
    }

    fn insert_paste(&mut self, text: String) {
        self.composer.insert_paste(text);
        self.popup_selected = 0;
        self.history_index = None;
        self.quit_hint_until = None;
        self.refresh_file_matches();
    }

    fn backspace(&mut self) {
        self.composer.backspace();
        self.popup_selected = 0;
        self.history_index = None;
        self.refresh_file_matches();
    }

    fn delete(&mut self) {
        self.composer.delete();
        self.popup_selected = 0;
        self.history_index = None;
        self.refresh_file_matches();
    }

    fn move_input_cursor_vertical(&mut self, direction: isize) {
        self.composer.move_vertical(direction);
        self.popup_selected = 0;
        self.history_index = None;
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
        let normalized = input.trim_start().trim_start_matches('/').to_lowercase();
        let terms = normalized.split_whitespace().collect::<Vec<_>>();
        self.command_catalog
            .iter()
            .filter(|command| {
                let searchable = command.searchable.to_lowercase();
                terms.iter().all(|term| searchable.contains(term))
            })
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
            if let Some(handle) = self.run_handle.take() {
                handle.abort();
            }
            self.cancel = None;
            self.running = false;
            self.status.state = "idle".to_string();
            self.active_tool_args.clear();
            self.active_tool_names.clear();
            self.active_tool_started_at.clear();
            self.sub_tool_args.clear();
            self.sub_tool_names.clear();
            self.sub_sessions.clear();
            self.supervisors.clear();
            self.pending_approval = None;
            self.approval_selected = 0;
            self.approval_state = "waiting";
            self.active_supervisor_id = None;
            self.cells.push(HistoryCell::system("Interrupt requested."));
            self.refresh_command_catalog();
            if let Some(next) = self.queued_inputs.pop_front() {
                self.start_turn(next);
            }
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
        let input = self
            .history_index
            .and_then(|index| self.input_history.get(index).cloned())
            .unwrap_or_default();
        self.composer.set_text(input);
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

    fn upsert_sub_session(&mut self, event: SubSessionEvent) {
        let id = sub_session_id(&event);
        let status = sub_session_status(&event.payload);
        let state = self
            .sub_sessions
            .entry(id.clone())
            .or_insert_with(|| SubSessionUiState::from_event(&event));
        state.update_context(&event);
        match &event.payload {
            SubSessionEventPayload::Start
            | SubSessionEventPayload::Delta { .. }
            | SubSessionEventPayload::ThinkingStart
            | SubSessionEventPayload::ThinkingEnd { .. }
            | SubSessionEventPayload::TurnStart { .. } => {}
            SubSessionEventPayload::ToolCallStart { id: call_id, name } => {
                self.sub_tool_names.insert(call_id.clone(), name.clone());
                self.sub_tool_args.entry(call_id.clone()).or_default();
                let pretty = PrettyToolCall::started(call_id, name, &empty_tool_args());
                state.upsert_tool(SubToolDisplay::from_pretty(call_id, &pretty, status));
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
                state.upsert_tool(SubToolDisplay::from_pretty(call_id, &pretty, status));
            }
            SubSessionEventPayload::ToolDelta {
                id: call_id,
                name,
                delta,
            } => {
                self.sub_tool_names.insert(call_id.clone(), name.clone());
                state.upsert_tool(SubToolDisplay {
                    id: call_id.clone(),
                    title: format!("调用 {name}"),
                    summary: truncate_chars(delta, MAX_TOOL_BODY_CHARS),
                    status,
                });
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
                state.upsert_tool(SubToolDisplay::from_pretty(
                    call_id,
                    &pretty,
                    ToolVisualStatus::from_success(success),
                ));
            }
            SubSessionEventPayload::Done { final_output } => {
                state.done = true;
                state.final_output = final_output
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
                    .map(|output| truncate_chars(&single_line(output), MAX_TOOL_BODY_CHARS));
            }
            SubSessionEventPayload::Error { message } => {
                state.failed = true;
                state.final_output =
                    Some(truncate_chars(&single_line(message), MAX_TOOL_BODY_CHARS));
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

    async fn load_thread_history(&mut self) {
        let history = self.runtime.bot.thread_history(&self.session_id).await;
        if history.is_empty() {
            self.cells.push(HistoryCell::system("no previous messages"));
            return;
        }
        let loaded = history.len();
        self.cells.push(HistoryCell::system(format!(
            "loaded {loaded} previous messages"
        )));
        self.cells
            .extend(history.into_iter().filter_map(history_cell));
        while self.cells.len() > MAX_HISTORY_CELLS {
            self.cells.remove(0);
        }
    }

    fn composer_height(&self, width: u16) -> u16 {
        let rows = wrap_line_count(&self.composer.display_text(), width.saturating_sub(3)).max(1);
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
        let before = &display[..cursor];
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
                    CatEvent::StateUpdate(user_state) => {
                        let items = bot_core::todo::todos_from_user_state(&user_state);
                        let _ = tx.send(BotEvent::TodoState(format_todo_state(&items)));
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
    match open_fork_in_new_pane(&fork_id, &cli) {
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
    match open_fork_in_new_pane(&session_id, &cli) {
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
    SubSession(SubSessionEvent),
    ContextCompaction(ContextCompactionEvent),
    SupervisorProgress(SupervisorTraceEvent),
    SupervisorReport(WorkflowReport),
    TodoState(String),
    ApprovalRequested(ToolApprovalRequest),
    ApprovalUpdated(ToolApprovalRequest),
    ApprovalResolved {
        request: ToolApprovalRequest,
        decision: ToolApprovalDecision,
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

struct StatusLine {
    state: String,
    prompt_tokens: u32,
    completion_tokens: u32,
    max_prompt_tokens: u32,
    model_elapsed_ms: u64,
    elapsed_ms: u64,
    last_error: Option<String>,
}

#[derive(Clone, Copy, Default)]
struct TokenStatsSnapshot {
    prompt_tokens: u32,
    completion_tokens: u32,
}

#[derive(Clone, Copy, Default)]
struct TokenDelta {
    prompt_tokens: u32,
    completion_tokens: u32,
}

impl TokenDelta {
    fn is_empty(self) -> bool {
        self.prompt_tokens == 0 && self.completion_tokens == 0
    }
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

#[derive(Clone, Default)]
struct SupervisorUiState {
    workflow_name: Option<String>,
    from_node: Option<String>,
    to_node: Option<String>,
    edge: Option<String>,
    status: Option<String>,
    reason: Option<String>,
    events: Vec<SupervisorEventDisplay>,
}

#[derive(Clone)]
struct SupervisorEventDisplay {
    kind: &'static str,
    label: String,
    body: String,
}

impl SupervisorUiState {
    fn push_event(&mut self, event: SupervisorEventDisplay) {
        if matches!(event.kind, "output") {
            if let Some(existing) = self
                .events
                .iter_mut()
                .rev()
                .find(|item| item.kind == "output")
            {
                if !existing.body.is_empty() && !event.body.is_empty() {
                    existing.body.push(' ');
                }
                existing.body.push_str(&event.body);
                existing.body = truncate_chars(&single_line(&existing.body), MAX_TOOL_BODY_CHARS);
                return;
            }
        }
        self.events.push(event);
        if self.events.len() > 3 {
            let overflow = self.events.len() - 3;
            self.events.drain(0..overflow);
        }
    }

    fn apply_report(&mut self, report: &WorkflowReport) {
        self.workflow_name = Some(report.workflow_name.clone());
        self.from_node = Some(report.from_node.clone());
        self.to_node = Some(report.to_node.clone());
        self.edge = report.edge.clone();
        self.status = Some(format!("{:?}", report.status));
        if !report.reason.trim().is_empty() {
            self.reason = Some(truncate_chars(
                &single_line(&report.reason),
                MAX_TOOL_BODY_CHARS,
            ));
        }
    }

    fn running_title(&self) -> String {
        let workflow = self.workflow_name.as_deref().unwrap_or("Supervisor");
        format!("supervisor · {workflow} · reviewing")
    }

    fn resolved_title(&self) -> String {
        let workflow = self.workflow_name.as_deref().unwrap_or("Supervisor");
        let from = self.from_node.as_deref().unwrap_or("?");
        let to = self.to_node.as_deref().unwrap_or("?");
        let status = self.status.as_deref().unwrap_or("done");
        format!("supervisor · {workflow} · {from} -> {to} · {status}")
    }

    fn meta(&self) -> String {
        let mut parts = Vec::new();
        if let (Some(from), Some(to)) = (&self.from_node, &self.to_node) {
            parts.push(format!("{from} -> {to}"));
        }
        if let Some(edge) = &self.edge {
            parts.push(format!("edge {edge}"));
        }
        if parts.is_empty() {
            "running".to_string()
        } else {
            parts.join(" · ")
        }
    }

    fn body(&self) -> String {
        let mut lines = Vec::new();
        if let (Some(from), Some(to)) = (&self.from_node, &self.to_node) {
            lines.push(format!("transition: {from} -> {to}"));
        }
        if let Some(reason) = &self.reason {
            lines.push(format!("reason: {reason}"));
        }
        for event in &self.events {
            lines.push(format!("{}: {}", event.label, event.body));
        }
        if lines.is_empty() {
            "reviewing workflow state".to_string()
        } else {
            lines.join("\n")
        }
    }
}

#[derive(Clone)]
struct SubSessionUiState {
    agent_name: String,
    input: Option<String>,
    parent_tool_call_id: String,
    thread_id: String,
    depth: u32,
    tools: Vec<SubToolDisplay>,
    done: bool,
    failed: bool,
    final_output: Option<String>,
}

impl SubSessionUiState {
    fn from_event(event: &SubSessionEvent) -> Self {
        Self {
            agent_name: event.agent_name.clone(),
            input: sub_session_input(event),
            parent_tool_call_id: event.parent_tool_call_id.clone(),
            thread_id: event.sub_thread_id.0.clone(),
            depth: event.depth,
            tools: Vec::new(),
            done: false,
            failed: false,
            final_output: None,
        }
    }

    fn update_context(&mut self, event: &SubSessionEvent) {
        self.agent_name = event.agent_name.clone();
        self.depth = event.depth;
        self.thread_id = event.sub_thread_id.0.clone();
        if !event.parent_tool_call_id.is_empty() {
            self.parent_tool_call_id = event.parent_tool_call_id.clone();
        }
        if self.input.is_none() {
            self.input = sub_session_input(event);
        }
    }

    fn upsert_tool(&mut self, tool: SubToolDisplay) {
        if let Some(existing) = self.tools.iter_mut().find(|item| item.id == tool.id) {
            *existing = tool;
        } else {
            self.tools.push(tool);
        }
    }

    fn title(&self) -> String {
        let indent = "  ".repeat(self.depth as usize);
        format!(
            "{indent}sub-agent · {} · calling {} tool{}",
            self.agent_name,
            self.tools.len(),
            if self.tools.len() == 1 { "" } else { "s" }
        )
    }

    fn meta(&self) -> String {
        let state = if self.failed {
            "failed"
        } else if self.done {
            "done"
        } else {
            "running"
        };
        format!(
            "depth {} · parent {} · thread {} · {state}",
            self.depth,
            short_session_id(&self.parent_tool_call_id),
            short_session_id(&self.thread_id)
        )
    }

    fn body(&self) -> String {
        let nested = "  ".repeat(self.depth.saturating_add(1) as usize);
        let mut lines = Vec::new();
        if let Some(input) = self
            .input
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            lines.push(format!("{nested}input: {input}"));
        }
        let recent_start = self.tools.len().saturating_sub(3);
        for tool in self.tools.iter().skip(recent_start) {
            let status = tool.status.label();
            let suffix = if status.is_empty() {
                String::new()
            } else {
                format!(" · {status}")
            };
            lines.push(format!("{nested}↳ {}{suffix}", tool.title));
            if !tool.summary.trim().is_empty() {
                lines.push(format!("{nested}  {}", tool.summary));
            }
        }
        if let Some(output) = self.final_output.as_deref() {
            lines.push(format!("{nested}final: {output}"));
        }
        if lines.is_empty() {
            lines.push(format!("{nested}waiting for sub-agent tools"));
        }
        lines.join("\n")
    }

    fn status(&self) -> ToolVisualStatus {
        if self.failed {
            ToolVisualStatus::Error
        } else if self.done {
            ToolVisualStatus::Success
        } else {
            ToolVisualStatus::Running
        }
    }
}

#[derive(Clone)]
struct SubToolDisplay {
    id: String,
    title: String,
    summary: String,
    status: ToolVisualStatus,
}

impl SubToolDisplay {
    fn from_pretty(id: &str, pretty: &PrettyToolCall, status: ToolVisualStatus) -> Self {
        Self {
            id: id.to_string(),
            title: pretty.title.clone(),
            summary: tool_body(pretty),
            status,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupKind {
    Command,
    File,
}

fn static_commands() -> Vec<CommandEntry> {
    [
        ("/fork", "fork 当前 session 到新 pane", false),
        ("/new", "创建空 session 到新 pane", false),
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneLaunchCommand {
    kind: &'static str,
    program: &'static str,
    args: Vec<OsString>,
}

fn open_fork_in_new_pane(
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

#[derive(Clone)]
struct HistoryCell {
    kind: CellKind,
    title: String,
    body: String,
    meta: String,
    status: ToolVisualStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolVisualStatus {
    Neutral,
    Running,
    Success,
    Error,
}

impl ToolVisualStatus {
    fn from_success(success: bool) -> Self {
        if success {
            Self::Success
        } else {
            Self::Error
        }
    }

    fn from_pretty(status: &PrettyToolStatus) -> Self {
        match status {
            PrettyToolStatus::Running => Self::Running,
            PrettyToolStatus::Success => Self::Success,
            PrettyToolStatus::Error => Self::Error,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Neutral => "",
            Self::Running => "running",
            Self::Success => "success",
            Self::Error => "failed",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Neutral => Style::default().fg(CODEX_DIM),
            Self::Running => Style::default().fg(Color::Yellow),
            Self::Success => Style::default().fg(CODEX_GREEN),
            Self::Error => Style::default().fg(Color::Red),
        }
    }
}

#[derive(Clone)]
enum CellKind {
    System,
    User,
    Assistant,
    Thinking,
    Tool { id: String },
    PatchDiff { id: String },
    Supervisor { id: String },
    SubSession { id: String },
    TodoState,
    Approval { id: String },
    ContextCompaction { id: String },
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

    fn todo_state(body: String) -> Self {
        Self::new(CellKind::TodoState, "todos", body)
    }

    fn approval_with_title(
        id: String,
        title: String,
        body: String,
        status: ToolVisualStatus,
    ) -> Self {
        Self {
            kind: CellKind::Approval { id },
            title,
            body,
            meta: String::new(),
            status,
        }
    }

    fn context_compaction(
        id: String,
        title: String,
        body: String,
        status: ToolVisualStatus,
    ) -> Self {
        Self {
            kind: CellKind::ContextCompaction { id },
            title,
            body,
            meta: String::new(),
            status,
        }
    }

    fn tool(
        id: String,
        name: String,
        body: String,
        meta: String,
        status: ToolVisualStatus,
    ) -> Self {
        Self {
            kind: CellKind::Tool { id },
            title: name,
            body,
            meta,
            status,
        }
    }

    fn patch_diff(id: String, patch: String, meta: String, status: ToolVisualStatus) -> Self {
        Self {
            kind: CellKind::PatchDiff { id },
            title: "patch diff".to_string(),
            body: patch,
            meta,
            status,
        }
    }

    fn supervisor(
        id: String,
        title: String,
        body: String,
        meta: String,
        status: ToolVisualStatus,
    ) -> Self {
        Self {
            kind: CellKind::Supervisor { id },
            title,
            body,
            meta,
            status,
        }
    }

    fn sub_session(
        id: String,
        title: String,
        body: String,
        meta: String,
        status: ToolVisualStatus,
    ) -> Self {
        Self {
            kind: CellKind::SubSession { id },
            title,
            body,
            meta,
            status,
        }
    }

    fn new(kind: CellKind, title: impl Into<String>, body: String) -> Self {
        Self {
            kind,
            title: title.into(),
            body,
            meta: String::new(),
            status: ToolVisualStatus::Neutral,
        }
    }

    fn append(&mut self, delta: &str) {
        self.body.push_str(delta);
    }

    fn tool_id(&self) -> Option<&str> {
        match &self.kind {
            CellKind::Tool { id } | CellKind::PatchDiff { id } => Some(id.as_str()),
            _ => None,
        }
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
                "?",
                Style::default().fg(CODEX_DIM),
                Style::default().fg(CODEX_DIM),
            ),
            CellKind::Tool { .. } => {
                if self.status == ToolVisualStatus::Success
                    || self.status == ToolVisualStatus::Running
                {
                    ("↳", self.status.style(), Style::default().fg(Color::White))
                } else {
                    (
                        "↳",
                        Style::default().fg(Color::Red),
                        Style::default().fg(Color::Red),
                    )
                }
            }
            CellKind::PatchDiff { .. } => (
                "Δ",
                Style::default().fg(CODEX_CYAN).add_modifier(Modifier::BOLD),
                Style::default().fg(CODEX_DIM),
            ),
            CellKind::Supervisor { .. } => (
                "⌁",
                self.status.style().add_modifier(Modifier::BOLD),
                Style::default().fg(Color::White),
            ),
            CellKind::SubSession { .. } => (
                "↘",
                self.status.style().add_modifier(Modifier::BOLD),
                Style::default().fg(Color::White),
            ),
            CellKind::TodoState => (
                "□",
                Style::default()
                    .fg(CODEX_GREEN)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(Color::White),
            ),
            CellKind::Approval { .. } => (
                "!",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(Color::White),
            ),
            CellKind::ContextCompaction { .. } => (
                "◇",
                self.status.style().add_modifier(Modifier::BOLD),
                Style::default().fg(Color::White),
            ),
            CellKind::Error => (
                "x",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                Style::default().fg(Color::Red),
            ),
            CellKind::System => (
                "·",
                Style::default().fg(CODEX_DIM),
                Style::default().fg(CODEX_DIM),
            ),
        };
        let title = if self.meta.is_empty() {
            self.title.clone()
        } else if self.status == ToolVisualStatus::Neutral {
            format!("{} {}", self.title, self.meta)
        } else {
            format!("{} · {} · {}", self.title, self.status.label(), self.meta)
        };
        lines.push(Line::from(vec![
            Span::styled(history_gutter(prefix), title_style),
            Span::styled(title, title_style),
        ]));
        if self.body.trim().is_empty()
            && matches!(
                self.kind,
                CellKind::Approval { .. } | CellKind::Supervisor { .. }
            )
        {
            lines.push(Line::from(""));
            return lines;
        }
        let body = if self.body.trim().is_empty() {
            "(no content)"
        } else {
            self.body.trim_end()
        };
        let body_width = width.saturating_sub(HISTORY_GUTTER_WIDTH).max(16);
        if matches!(self.kind, CellKind::PatchDiff { .. }) {
            for line in diff_lines(body, body_width) {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(HISTORY_GUTTER_WIDTH as usize)),
                    line,
                ]));
            }
        } else if matches!(self.kind, CellKind::Assistant) {
            let wrapped = render_markdown_lines(
                body,
                body_width,
                MarkdownTheme {
                    base: body_style,
                    dim: CODEX_DIM,
                    accent: CODEX_CYAN,
                    code: Color::Yellow,
                    quote: CODEX_DIM,
                },
            );
            let truncated = wrapped.len() > MAX_HISTORY_BODY_LINES;
            for line in wrapped.into_iter().take(MAX_HISTORY_BODY_LINES) {
                let mut spans = Vec::with_capacity(line.spans.len() + 1);
                spans.push(Span::raw(" ".repeat(HISTORY_GUTTER_WIDTH as usize)));
                spans.extend(line.spans);
                lines.push(Line::from(spans));
            }
            if truncated {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(HISTORY_GUTTER_WIDTH as usize)),
                    Span::styled("[truncated in TUI]", Style::default().fg(CODEX_DIM)),
                ]));
            }
        } else {
            let wrapped = wrap_text(body, body_width);
            let truncated = wrapped.len() > MAX_HISTORY_BODY_LINES;
            for line in wrapped.into_iter().take(MAX_HISTORY_BODY_LINES) {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(HISTORY_GUTTER_WIDTH as usize)),
                    Span::styled(line, body_style),
                ]));
            }
            if truncated {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(HISTORY_GUTTER_WIDTH as usize)),
                    Span::styled("[truncated in TUI]", Style::default().fg(CODEX_DIM)),
                ]));
            }
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

fn history_scroll_offset(total_lines: usize, visible_lines: usize, scroll_back: usize) -> usize {
    let bottom_offset = total_lines.saturating_sub(visible_lines);
    bottom_offset.saturating_sub(scroll_back.min(bottom_offset))
}

fn horizontal_rule(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width as usize),
        Style::default().fg(CODEX_BORDER),
    ))
}

fn upsert_approval_cell(cells: &mut Vec<HistoryCell>, next: HistoryCell) {
    let next_id = match &next.kind {
        CellKind::Approval { id } => id.clone(),
        _ => return,
    };
    if let Some(cell) = cells
        .iter_mut()
        .rev()
        .find(|cell| matches!(&cell.kind, CellKind::Approval { id } if id == &next_id))
    {
        *cell = next;
    } else {
        cells.push(next);
    }
}

fn upsert_context_compaction_cell(cells: &mut Vec<HistoryCell>, next: HistoryCell) {
    let next_id = match &next.kind {
        CellKind::ContextCompaction { id } => id.clone(),
        _ => return,
    };
    if let Some(cell) = cells
        .iter_mut()
        .rev()
        .find(|cell| matches!(&cell.kind, CellKind::ContextCompaction { id } if id == &next_id))
    {
        *cell = next;
    } else {
        cells.push(next);
    }
}

fn context_compaction_cell(event: ContextCompactionEvent) -> HistoryCell {
    let (title, status) = match event.status {
        ContextCompactionStatus::Started => ("compressing context", ToolVisualStatus::Running),
        ContextCompactionStatus::Completed => ("context compressed", ToolVisualStatus::Success),
        ContextCompactionStatus::Failed => ("context compression failed", ToolVisualStatus::Error),
    };
    let body = if matches!(event.status, ContextCompactionStatus::Failed) {
        event
            .error
            .unwrap_or_else(|| "context compression failed".to_string())
    } else {
        format!(
            "compacted {} messages; remaining {} messages",
            event.compacted_messages, event.remaining_messages
        )
    };
    HistoryCell::context_compaction(event.id, title.to_string(), body, status)
}

fn history_cell(message: ThreadHistoryMessage) -> Option<HistoryCell> {
    match message.role.as_str() {
        "user" => Some(HistoryCell::user(message.text)),
        "assistant" => {
            if message.text.trim().is_empty() {
                None
            } else {
                Some(HistoryCell::assistant(message.text))
            }
        }
        "tool" => {
            if let Some(pretty) = message.pretty {
                if pretty.tool_name == "apply_patch" {
                    let patch = pretty
                        .request
                        .get("patch")
                        .and_then(|patch| patch.as_str())
                        .unwrap_or("")
                        .to_string();
                    let meta = patch_tool_meta(&pretty);
                    let status = ToolVisualStatus::from_pretty(&pretty.status);
                    Some(HistoryCell::patch_diff(pretty.id, patch, meta, status))
                } else {
                    let status = ToolVisualStatus::from_pretty(&pretty.status);
                    let meta = tool_meta(&pretty);
                    let body = tool_body(&pretty);
                    Some(HistoryCell::tool(
                        pretty.id,
                        pretty.title,
                        body,
                        meta,
                        status,
                    ))
                }
            } else {
                Some(HistoryCell::tool(
                    message.tool_call_id.unwrap_or(message.id),
                    "tool result".to_string(),
                    truncate_chars(&single_line(&message.text), MAX_TOOL_BODY_CHARS),
                    String::new(),
                    ToolVisualStatus::Neutral,
                ))
            }
        }
        _ => None,
    }
}

fn extract_patch_arg(args: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(args).ok()?;
    value
        .get("patch")
        .and_then(|patch| patch.as_str())
        .map(ToOwned::to_owned)
}

fn parse_tool_args(args: &str) -> Option<serde_json::Value> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Some(empty_tool_args());
    }
    serde_json::from_str::<serde_json::Value>(trimmed).ok()
}

fn empty_tool_args() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

fn tool_meta(pretty: &PrettyToolCall) -> String {
    pretty
        .elapsed_ms
        .map(format_elapsed)
        .unwrap_or_else(|| format_elapsed(0))
}

fn append_token_meta(cell: &mut HistoryCell, delta: TokenDelta) {
    if delta.is_empty() {
        return;
    }
    let token_meta = format!(
        "tokens +{}p/+{}c",
        delta.prompt_tokens, delta.completion_tokens
    );
    if cell.meta.is_empty() {
        cell.meta = token_meta;
    } else {
        cell.meta.push_str(" · ");
        cell.meta.push_str(&token_meta);
    }
}

fn preserve_token_meta(mut meta: String, existing_meta: &str) -> String {
    for part in existing_meta
        .split(" · ")
        .filter(|part| part.starts_with("tokens +"))
    {
        if !meta.is_empty() {
            meta.push_str(" · ");
        }
        meta.push_str(part);
    }
    meta
}

fn patch_tool_meta(pretty: &PrettyToolCall) -> String {
    let elapsed = tool_meta(pretty);
    if pretty.summary.trim().is_empty() {
        elapsed
    } else {
        format!(
            "{elapsed} · {}",
            truncate_chars(&single_line(&pretty.summary), MAX_TOOL_BODY_CHARS)
        )
    }
}

fn tool_body(pretty: &PrettyToolCall) -> String {
    let summary = if matches!(pretty.tool_name.as_str(), "bash" | "workspace_bash") {
        pretty.summary.trim().to_string()
    } else {
        single_line(&pretty.summary)
    };
    if summary.trim().is_empty() {
        return match pretty.status {
            PrettyToolStatus::Running => "running".to_string(),
            PrettyToolStatus::Success => "completed".to_string(),
            PrettyToolStatus::Error => "failed".to_string(),
        };
    }
    truncate_chars(&summary, MAX_TOOL_BODY_CHARS)
}

fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut output = text.chars().take(keep).collect::<String>();
    output.push('…');
    output
}

#[cfg(test)]
fn append_stream_event(body: &mut String, event: &str) {
    if event.trim().is_empty() {
        return;
    }
    if let Some((key, payload)) = mergeable_stream_event(event) {
        if let Some(index) = body.rfind("\n\n") {
            if mergeable_stream_event(&body[index + 2..])
                .is_some_and(|(last_key, _)| last_key == key)
            {
                body.push_str(&payload);
                return;
            }
        } else if mergeable_stream_event(body).is_some_and(|(last_key, _)| last_key == key) {
            body.push_str(&payload);
            return;
        }
    }
    if keyed_stream_event(event).is_some() && body_contains_stream_key(body, event) {
        return;
    }
    if !body.trim().is_empty() {
        body.push_str("\n\n");
    }
    if mergeable_stream_event(event).is_some() {
        body.push_str(event);
    } else {
        body.push_str(event.trim_end());
    }
}

#[cfg(test)]
fn body_contains_stream_key(body: &str, event: &str) -> bool {
    let Some(key) = keyed_stream_event(event) else {
        return false;
    };
    body.split("\n\n")
        .any(|block| keyed_stream_event(block).as_deref() == Some(key.as_str()))
}

#[cfg(test)]
fn mergeable_stream_event(event: &str) -> Option<(String, String)> {
    if let Some(payload) = event.strip_prefix("output\n") {
        return Some(("output".to_string(), payload.to_string()));
    }
    if let Some(payload) = event.strip_prefix("tool args: ") {
        let (id, rest) = payload.split_once('\n')?;
        return Some((format!("tool args: {id}"), rest.to_string()));
    }
    if let Some(payload) = event.strip_prefix("tool delta: ") {
        let (name, rest) = payload.split_once('\n')?;
        let (call_id, rest) = rest.split_once('\n')?;
        return Some((format!("tool delta: {name}\n{call_id}"), rest.to_string()));
    }
    None
}

#[cfg(test)]
fn keyed_stream_event(event: &str) -> Option<String> {
    if let Some(payload) = event.strip_prefix("tool call: ") {
        let (_name, rest) = payload.split_once('\n')?;
        let id = rest.strip_prefix("call_id: ")?;
        return Some(format!("tool call: {id}"));
    }
    if let Some(payload) = event.strip_prefix("tool result: ") {
        let (_name, rest) = payload.split_once('\n')?;
        let (call_id, _rest) = rest.split_once('\n')?;
        return Some(format!("tool result: {call_id}"));
    }
    None
}

fn supervisor_event_display(event: &SupervisorTraceEvent) -> SupervisorEventDisplay {
    match event {
        SupervisorTraceEvent::Thinking { content } => SupervisorEventDisplay {
            kind: "thinking",
            label: "thinking".to_string(),
            body: truncate_chars(&single_line(content), MAX_TOOL_BODY_CHARS),
        },
        SupervisorTraceEvent::ToolCall { name, args } => SupervisorEventDisplay {
            kind: "tool_call",
            label: format!("calling {name}"),
            body: truncate_chars(&format_json_summary(args), MAX_TOOL_BODY_CHARS),
        },
        SupervisorTraceEvent::ToolResult { name, result } => SupervisorEventDisplay {
            kind: "tool_result",
            label: format!("{name} result"),
            body: truncate_chars(&single_line(result), MAX_TOOL_BODY_CHARS),
        },
        SupervisorTraceEvent::OutputDelta { content }
        | SupervisorTraceEvent::Output { content } => SupervisorEventDisplay {
            kind: "output",
            label: "output".to_string(),
            body: truncate_chars(&single_line(content), MAX_TOOL_BODY_CHARS),
        },
    }
}

#[cfg(test)]
fn format_sub_session_title(event: &SubSessionEvent) -> String {
    let prefix = format_sub_session_prefix(event);
    match &event.payload {
        SubSessionEventPayload::ToolCallStart { name, .. }
        | SubSessionEventPayload::ToolDelta { name, .. }
        | SubSessionEventPayload::ToolResult { name, .. } => {
            format!("{prefix} · {name}")
        }
        SubSessionEventPayload::ToolCallArgumentsDelta { id, .. } => {
            format!("{prefix} · {id}")
        }
        SubSessionEventPayload::Done { .. } => format!("{prefix} · final"),
        SubSessionEventPayload::Error { .. } => format!("{prefix} · error"),
        _ => prefix,
    }
}

#[cfg(test)]
fn format_sub_session_prefix(event: &SubSessionEvent) -> String {
    let title = event.agent_name.as_str();
    let indent = "  ".repeat(event.depth as usize);
    format!("{indent}sub-agent · {title}")
}

fn sub_session_id(event: &SubSessionEvent) -> String {
    format!("{}:{}", event.sub_thread_id.0, event.sub_run_id.0)
}

fn sub_session_input(event: &SubSessionEvent) -> Option<String> {
    event
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty() && *title != event.agent_name)
        .map(|title| truncate_chars(&single_line(title), 96))
}

#[cfg(test)]
fn format_sub_session_meta(event: &SubSessionEvent) -> String {
    let state = match &event.payload {
        SubSessionEventPayload::Start => "started".to_string(),
        SubSessionEventPayload::Delta { .. } => "streaming".to_string(),
        SubSessionEventPayload::ThinkingStart => "thinking".to_string(),
        SubSessionEventPayload::ThinkingEnd { .. } => "thinking done".to_string(),
        SubSessionEventPayload::ToolCallStart { name, .. } => format!("{name} · running"),
        SubSessionEventPayload::ToolCallArgumentsDelta { id, .. } => {
            format!("{id} · args")
        }
        SubSessionEventPayload::ToolDelta { name, .. } => format!("{name} · streaming"),
        SubSessionEventPayload::ToolResult { name, .. } => format!("{name} · done"),
        SubSessionEventPayload::TurnStart { turn } => format!("turn {turn}"),
        SubSessionEventPayload::Done { .. } => "done".to_string(),
        SubSessionEventPayload::Error { .. } => "failed".to_string(),
    };
    format!(
        "depth {} · parent {} · thread {} · {}",
        event.depth,
        short_session_id(&event.parent_tool_call_id),
        short_session_id(&event.sub_thread_id.0),
        state
    )
}

fn sub_session_status(payload: &SubSessionEventPayload) -> ToolVisualStatus {
    match payload {
        SubSessionEventPayload::ToolResult { result, .. } => {
            ToolVisualStatus::from_success(bot_core::tool_success(result))
        }
        SubSessionEventPayload::Done { .. } => ToolVisualStatus::Success,
        SubSessionEventPayload::Error { .. } => ToolVisualStatus::Error,
        _ => ToolVisualStatus::Running,
    }
}

#[cfg(test)]
fn format_sub_session_event(event: &SubSessionEvent) -> Option<String> {
    match &event.payload {
        SubSessionEventPayload::Start
        | SubSessionEventPayload::Delta { .. }
        | SubSessionEventPayload::ThinkingStart
        | SubSessionEventPayload::ThinkingEnd { .. }
        | SubSessionEventPayload::TurnStart { .. } => None,
        SubSessionEventPayload::ToolCallStart { id, name } => {
            Some(format!("tool call: {name}\ncall_id: {id}"))
        }
        SubSessionEventPayload::ToolCallArgumentsDelta { id, delta } => Some(format!(
            "tool args: {id}\n{}",
            truncate_chars(delta, MAX_TOOL_BODY_CHARS)
        )),
        SubSessionEventPayload::ToolDelta { id, name, delta } => Some(format!(
            "tool delta: {name}\ncall_id: {id}\n{}",
            truncate_chars(delta, MAX_TOOL_BODY_CHARS)
        )),
        SubSessionEventPayload::ToolResult { id, name, result } => Some(format!(
            "tool result: {name}\ncall_id: {id}\n{}",
            truncate_chars(&single_line(result), MAX_TOOL_BODY_CHARS)
        )),
        SubSessionEventPayload::Done { final_output } => {
            if let Some(output) = final_output
                .as_deref()
                .filter(|text| !text.trim().is_empty())
            {
                Some(format!(
                    "done\n{}",
                    truncate_chars(&single_line(output), MAX_TOOL_BODY_CHARS)
                ))
            } else {
                Some("done".to_string())
            }
        }
        SubSessionEventPayload::Error { message } => Some(format!(
            "error\n{}",
            truncate_chars(&single_line(message), MAX_TOOL_BODY_CHARS)
        )),
    }
}

fn format_json_summary(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn format_elapsed(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

fn format_todo_state(items: &[bot_core::todo::TodoItem]) -> String {
    if items.is_empty() {
        return "no todos".to_string();
    }
    let mut lines = Vec::with_capacity(items.len());
    for item in items {
        let status = if item.done { "[x]" } else { "[ ]" };
        let batch = item
            .batch_title
            .as_deref()
            .map(|title| format!(" · {title}"))
            .unwrap_or_default();
        lines.push(format!("{status} #{} {}{batch}", item.id, item.content));
        if let Some(description) = item
            .description
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            lines.push(format!("    {description}"));
        }
    }
    lines.join("\n")
}

#[derive(Clone, Copy)]
struct ApprovalOption {
    label: &'static str,
    key: &'static str,
    decision: ToolApprovalDecision,
}

fn approval_options() -> [ApprovalOption; 4] {
    [
        ApprovalOption {
            label: "Yes, proceed",
            key: "y",
            decision: ToolApprovalDecision::AllowOnce,
        },
        ApprovalOption {
            label: "Yes, and don't ask again in this session",
            key: "s/p",
            decision: ToolApprovalDecision::AllowSession,
        },
        ApprovalOption {
            label: "Yes, and let the model auto-approve medium-risk tools this session",
            key: "m",
            decision: ToolApprovalDecision::AllowSessionModelAuto,
        },
        ApprovalOption {
            label: "No, and tell Remi what to do differently",
            key: "esc",
            decision: ToolApprovalDecision::Deny,
        },
    ]
}

fn approval_options_len() -> usize {
    approval_options().len()
}

fn approval_option(index: usize) -> Option<ApprovalOption> {
    approval_options().get(index).copied()
}

fn approval_cell(
    request: &bot_core::ToolApprovalRequest,
    state: &str,
    selected: usize,
    status: Option<String>,
    decision: Option<ToolApprovalDecision>,
) -> HistoryCell {
    if state == "resolved" {
        let decision = decision
            .map(|decision| format!("{decision:?}"))
            .or(status)
            .unwrap_or_else(|| "resolved".to_string());
        return HistoryCell::approval_with_title(
            request.id.clone(),
            format!(
                "approval · resolved · {} · {}",
                request.tool_name,
                decision.replace("decision: ", "")
            ),
            String::new(),
            ToolVisualStatus::Success,
        );
    }

    let mut lines = Vec::new();
    let risk = request
        .review
        .as_ref()
        .map(|review| format!("{:?}", review.risk))
        .unwrap_or_else(|| format!("{:?}", request.risk));
    lines.push(format!("{} · risk {} · {}", state, risk, request.tool_name));
    if let Some(platform) = request
        .platform
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        lines.push(format!(
            "from {platform} · id {}",
            short_session_id(&request.id)
        ));
    } else {
        lines.push(format!("id {}", short_session_id(&request.id)));
    }
    if let Some(review) = &request.review {
        if !review.reason.trim().is_empty() {
            lines.push(format!("review: {}", review.reason.trim()));
        }
        for concern in review.concerns.iter().take(3) {
            lines.push(format!("  - {concern}"));
        }
    }
    if !request.args_summary.trim().is_empty() {
        lines.push(format!(
            "args: {}",
            truncate_chars(&single_line(&request.args_summary), MAX_TOOL_BODY_CHARS)
        ));
    }
    lines.push(String::new());
    for (index, option) in approval_options().iter().enumerate() {
        let marker = if index == selected { "›" } else { " " };
        lines.push(format!(
            "{marker} {}. {} ({})",
            index + 1,
            option.label,
            option.key
        ));
    }
    lines.push(String::new());
    lines.push("↑/↓ choose · Enter confirm · Esc deny".to_string());
    if let Some(status) = status {
        lines.push(status);
    }
    HistoryCell::approval_with_title(
        request.id.clone(),
        format!("approval · {state} · {}", request.tool_name),
        lines.join("\n"),
        ToolVisualStatus::Running,
    )
}

fn diff_lines(text: &str, width: u16) -> Vec<Span<'static>> {
    let width = width.max(16);
    text.lines()
        .flat_map(|line| {
            let style = diff_line_style(line);
            let chunks = if line.is_empty() {
                vec![String::new()]
            } else {
                wrap_text(line, width)
            };
            chunks
                .into_iter()
                .map(move |chunk| Span::styled(chunk, style))
        })
        .collect()
}

fn diff_line_style(line: &str) -> Style {
    if line.starts_with("@@") {
        Style::default().fg(CODEX_CYAN).add_modifier(Modifier::BOLD)
    } else if line.starts_with('+') && !line.starts_with("+++") {
        Style::default().fg(CODEX_GREEN)
    } else if line.starts_with('-') && !line.starts_with("---") {
        Style::default().fg(Color::Red)
    } else if line.starts_with("diff --git ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("*** ")
    {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(CODEX_DIM)
    }
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

fn context_usage_percent(prompt_tokens: u32, context_tokens: u32) -> Option<u32> {
    if context_tokens == 0 {
        return None;
    }
    let used = prompt_tokens.min(context_tokens);
    Some((used as f64 / context_tokens as f64 * 100.0).round() as u32)
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

fn input_line_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for (index, ch) in text.char_indices() {
        if ch == '\n' {
            ranges.push((start, index));
            start = index + ch.len_utf8();
        }
    }
    ranges.push((start, text.len()));
    ranges
}

fn byte_index_for_char_offset(text: &str, start: usize, end: usize, desired_chars: usize) -> usize {
    text[start..end]
        .char_indices()
        .nth(desired_chars)
        .map(|(index, _)| start + index)
        .unwrap_or(end)
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
        assert!(commands.iter().any(|command| command.value == "/fork"));
        assert!(commands.iter().any(|command| command.value == "/new"));
        assert!(commands
            .iter()
            .any(|command| command.value == "/skill list"));
        assert!(commands
            .iter()
            .any(|command| command.value == "/model status"));
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
    fn history_scroll_is_bottom_anchored() {
        assert_eq!(history_scroll_offset(30, 10, 0), 20);
        assert_eq!(history_scroll_offset(30, 10, 6), 14);
        assert_eq!(history_scroll_offset(30, 10, 99), 0);
        assert_eq!(history_scroll_offset(5, 10, 0), 0);
        assert_eq!(history_scroll_offset(70_000, 20, 0), 69_980);
    }

    #[test]
    fn history_gutter_has_stable_display_width() {
        for prefix in ["›", "•", "↳", "·", "?", "x", "!", "□", "Δ"] {
            assert_eq!(UnicodeWidthStr::width(history_gutter(prefix).as_str()), 4);
        }
    }

    #[test]
    fn context_usage_uses_model_context_window() {
        assert_eq!(context_usage_percent(16_000, 128_000), Some(13));
        assert_eq!(context_usage_percent(128_000, 128_000), Some(100));
        assert_eq!(context_usage_percent(130_000, 128_000), Some(100));
        assert_eq!(context_usage_percent(1, 0), None);
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
    fn sub_session_only_renders_tools_and_final_output() {
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
        assert!(
            format_sub_session_event(&base(SubSessionEventPayload::Done {
                final_output: Some("final answer".to_string()),
            }))
            .is_some_and(|body| body.contains("final answer"))
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
        }

        assert_eq!(state.title(), "  sub-agent · explorer · calling 5 tools");
        let body = state.body();
        assert!(body.contains("  input: Explore this workspace in detail"));
        assert!(!body.contains("path-1"));
        assert!(body.contains("path-2"));
        assert!(body.contains("path-4"));
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
}
