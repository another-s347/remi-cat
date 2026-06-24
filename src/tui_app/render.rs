use super::*;

impl TuiApp {
    pub(super) fn render(&mut self, frame: &mut Frame<'_>) {
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
        let active_tools = self.active_tool_count();
        let status_busy = self.running
            || active_tools > 0
            || matches!(self.status.state.as_str(), "running" | "thinking")
            || self.status.state.starts_with("turn ");
        if self.running {
            meta.push(format_elapsed(self.status.elapsed_ms));
        }
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
            .map(|value| format!(" · {}", sanitize_tui_text(value)))
            .unwrap_or_default();
        let line = Line::from(vec![
            Span::styled(
                " Remi Cat",
                Style::default().fg(CODEX_CYAN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", Style::default().fg(CODEX_DIM)),
            Span::styled(
                self.status.state.clone(),
                Style::default().fg(if status_busy {
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
        let (model_profile_id, agent_id) = self
            .runtime
            .sessions
            .try_lock()
            .ok()
            .map(|sessions| {
                (
                    sessions.metadata_string(&self.session_id, SESSION_MODEL_PROFILE_METADATA_KEY),
                    sessions.metadata_string(&self.session_id, SESSION_AGENT_ID_METADATA_KEY),
                )
            })
            .unwrap_or((None, None));
        let effective_agent = self
            .runtime
            .bot
            .effective_agent_profile(agent_id.as_deref());
        let effective_model = self.runtime.bot.effective_model_profile_for_agent(
            model_profile_id.as_deref(),
            &effective_agent.profile,
        );
        let agent = effective_agent.profile.id.clone();
        let model = effective_model.profile.id.clone();
        let context_tokens = effective_model.profile.context_tokens;
        let session = short_session_label(&self.session_id);
        let mut spans = vec![
            Span::styled(format!("{agent}/{model} "), Style::default().fg(CODEX_DIM)),
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
        if let Some(pct) = context_usage_percent(
            self.status.prompt_tokens,
            self.status.completion_tokens,
            self.status.max_prompt_tokens,
            context_tokens,
        ) {
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
        let visible_lines = self.visible_history_lines(area);
        let paragraph = Paragraph::new(visible_lines).style(Style::default().fg(Color::Gray));
        frame.render_widget(Clear, area);
        frame.render_widget(paragraph, area);
    }

    fn visible_history_lines(&self, area: Rect) -> Vec<Line<'static>> {
        let height = area.height as usize;
        if height == 0 {
            return Vec::new();
        }
        let needed = height.saturating_add(self.scroll as usize);
        let mut lines = Vec::with_capacity(needed.min(height.saturating_mul(2).max(1)));
        for cell in self.cells.iter().rev() {
            let cell_lines = cell.lines(area.width);
            for line in cell_lines.into_iter().rev() {
                lines.push(line);
                if lines.len() >= needed {
                    break;
                }
            }
            if lines.len() >= needed {
                break;
            }
        }
        lines.reverse();
        lines.into_iter().take(height).collect()
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
            composer_visual_lines(&self.composer, input_area.width)
        };
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, input_area);
        if !self.running || self.pending_user_question.is_some() {
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
                    Span::styled(sanitize_tui_text(&command.description), description_style),
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
}
