use std::io::{self, IsTerminal, Stdout, Write};

use anyhow::Context;
use crossterm::cursor::Show;
use crossterm::event::{read, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::style::ResetColor;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Color, Line, Modifier, Span, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use unicode_width::UnicodeWidthStr;

use crate::tui_theme;

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

struct FormTerminal {
    terminal: TuiTerminal,
    restored: bool,
}

impl FormTerminal {
    fn enter() -> anyhow::Result<Self> {
        enable_raw_mode().context("enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
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
            ResetColor,
            Show,
            EnableLineWrap,
            LeaveAlternateScreen
        )
        .context("restore terminal")?;
        disable_raw_mode().context("disable raw mode")?;
        self.terminal.show_cursor().context("show cursor")?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for FormTerminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub(crate) fn text_input(label: &str, default: &str, secret: bool) -> anyhow::Result<String> {
    if !io::stdout().is_terminal() {
        return fallback_text_input(label, default, secret);
    }
    let mut terminal = FormTerminal::enter()?;
    let mut value = String::new();
    loop {
        terminal
            .terminal
            .draw(|frame| render_text_input(frame, label, default, &value, secret))
            .context("draw setup input")?;
        match read().context("read setup input event")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter => {
                    terminal.restore()?;
                    let value = value.trim();
                    return Ok(if value.is_empty() {
                        default.to_string()
                    } else {
                        value.to_string()
                    });
                }
                KeyCode::Esc => {
                    terminal.restore()?;
                    anyhow::bail!("setup cancelled");
                }
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(ch) => value.push(ch),
                _ => {}
            },
            _ => {}
        }
    }
}

pub(crate) fn bool_input(label: &str, default: bool) -> anyhow::Result<bool> {
    let default_index = if default { 0 } else { 1 };
    let selected = select(label, &["yes".to_string(), "no".to_string()], default_index)?;
    Ok(selected == "yes")
}

pub(crate) fn select(
    label: &str,
    options: &[String],
    default_index: usize,
) -> anyhow::Result<String> {
    if options.is_empty() {
        anyhow::bail!("no options available for {label}");
    }
    if !io::stdout().is_terminal() {
        return fallback_select(label, options, default_index);
    }
    let mut terminal = FormTerminal::enter()?;
    let mut selected = default_index.min(options.len() - 1);
    loop {
        terminal
            .terminal
            .draw(|frame| render_select(frame, label, options, selected))
            .context("draw setup selector")?;
        match read().context("read setup selector event")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(options.len() - 1);
                }
                KeyCode::Home => selected = 0,
                KeyCode::End => selected = options.len() - 1,
                KeyCode::Enter => {
                    terminal.restore()?;
                    return Ok(options[selected].clone());
                }
                KeyCode::Esc => {
                    terminal.restore()?;
                    anyhow::bail!("setup cancelled");
                }
                KeyCode::Char(ch) if ch.is_ascii_digit() => {
                    if let Some(index) = ch.to_digit(10).and_then(|n| n.checked_sub(1)) {
                        let index = index as usize;
                        if index < options.len() {
                            terminal.restore()?;
                            return Ok(options[index].clone());
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn render_text_input(frame: &mut Frame<'_>, label: &str, default: &str, value: &str, secret: bool) {
    let area = centered(frame.area(), 72, 9);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" setup ")
        .borders(Borders::ALL)
        .border_style(tui_theme::border_style());
    frame.render_widget(block, area);
    let inner = inset(area, 2, 1);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                label.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  default: {}",
                    if secret && !default.is_empty() {
                        "stored secret"
                    } else {
                        default
                    }
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        chunks[0],
    );
    let display = if secret && !value.is_empty() {
        "*".repeat(value.chars().count())
    } else if secret && !default.is_empty() && value.is_empty() {
        "stored secret".to_string()
    } else if value.is_empty() {
        default.to_string()
    } else {
        value.to_string()
    };
    frame.render_widget(
        Paragraph::new(display.as_str())
            .block(Block::default().borders(Borders::ALL))
            .style(if value.is_empty() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            })
            .wrap(Wrap { trim: false }),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new("Enter confirm · Esc cancel · Backspace edit")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center),
        chunks[2],
    );
    let cursor_x = chunks[1].x
        + 1
        + display
            .width()
            .min(chunks[1].width.saturating_sub(2) as usize) as u16;
    frame.set_cursor_position((cursor_x, chunks[1].y + 1));
}

fn render_select(frame: &mut Frame<'_>, label: &str, options: &[String], selected: usize) {
    let height = (options.len() as u16 + 6).clamp(8, 18);
    let area = centered(frame.area(), 82, height);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" setup ")
        .borders(Borders::ALL)
        .border_style(tui_theme::border_style());
    frame.render_widget(block, area);
    let inner = inset(area, 2, 1);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(label.to_string()).style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[0],
    );
    let items = options
        .iter()
        .enumerate()
        .map(|(index, option)| {
            let marker = tui_theme::selection_marker(index == selected);
            let number = if index < 9 {
                format!("{} ", index + 1)
            } else {
                "  ".to_string()
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, tui_theme::accent_style(Modifier::empty())),
                Span::styled(number, Style::default().fg(Color::DarkGray)),
                Span::raw(option.clone()),
            ]))
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items), chunks[1]);
    frame.render_widget(
        Paragraph::new("Up/Down choose · Enter confirm · 1-9 quick select · Esc cancel")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center),
        chunks[2],
    );
}

fn fallback_text_input(label: &str, default: &str, _secret: bool) -> anyhow::Result<String> {
    print!("{label} [{default}]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let value = input.trim();
    Ok(if value.is_empty() {
        default.to_string()
    } else {
        value.to_string()
    })
}

fn fallback_select(
    label: &str,
    options: &[String],
    default_index: usize,
) -> anyhow::Result<String> {
    println!("{label}:");
    for (index, option) in options.iter().enumerate() {
        println!("  {}. {option}", index + 1);
    }
    let default = default_index.min(options.len() - 1);
    loop {
        print!("Enter number [{}]: ", default + 1);
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let value = input.trim();
        if value.is_empty() {
            return Ok(options[default].clone());
        }
        if let Ok(index) = value.parse::<usize>() {
            if (1..=options.len()).contains(&index) {
                return Ok(options[index - 1].clone());
            }
        }
        println!("Please enter a number from 1 to {}.", options.len());
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(20);
    let height = height.min(area.height.saturating_sub(2)).max(5);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn inset(area: Rect, x: u16, y: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(x),
        y: area.y.saturating_add(y),
        width: area.width.saturating_sub(x * 2),
        height: area.height.saturating_sub(y * 2),
    }
}
