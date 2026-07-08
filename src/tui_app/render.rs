use super::*;
use crate::tui_theme;

impl TuiApp {
    pub(super) fn render(&mut self, frame: &mut Frame<'_>) {
        let root = frame.area();
        let activity_height = self.activity_height();
        let action_height = self.action_panel_height(root.height);
        let footer_height = self.footer_height();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(4),
                Constraint::Length(activity_height),
                Constraint::Length(action_height),
                Constraint::Length(footer_height),
                Constraint::Length(self.composer_height(root.width)),
            ])
            .split(root);

        self.render_status(frame, chunks[0]);
        self.render_history(frame, chunks[1]);
        self.render_activity(frame, chunks[2]);
        self.render_action_panel(frame, chunks[3]);
        self.render_footer(frame, chunks[4]);
        self.render_composer(frame, chunks[5]);
        self.refresh_file_matches();
        match self.active_popup() {
            Some(PopupKind::Command) => self.render_command_popup(frame, chunks[5]),
            Some(PopupKind::File) => self.render_file_popup(frame, chunks[5]),
            None => {}
        }
    }

    fn render_status(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.flush_status_elapsed();
        let active_tools = self.active_tool_count();
        let status_busy = self.running
            || active_tools > 0
            || matches!(self.status.state.as_str(), "running" | "thinking")
            || self.status.state.starts_with("turn ");
        let meta = self.status_primary_parts(active_tools);
        let error = self
            .status
            .last_error
            .as_deref()
            .map(|value| format!(" · {}", sanitize_tui_text(value)))
            .unwrap_or_default();
        let primary = Line::from(vec![
            accent_span(" Remi Cat", Modifier::BOLD),
            dim_span(" · "),
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
        let context = self.status_context_line(area.width);
        let lines = vec![primary, context, horizontal_rule(area.width)];
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn status_primary_parts(&self, active_tools: usize) -> Vec<String> {
        let mut meta = Vec::new();
        meta.push(self.supervisor_status_label());
        meta.push(
            self.latest_active_todo_label
                .clone()
                .unwrap_or_else(|| "todo none".to_string()),
        );
        if self.running {
            meta.push(format_elapsed(self.status.elapsed_ms));
        }
        if active_tools > 0 {
            meta.push(format!("{active_tools} tools"));
        }
        if !self.queued_inputs.is_empty() {
            meta.push(format!("{} queued", self.queued_inputs.len()));
        }
        meta
    }

    fn status_context_line(&self, width: u16) -> Line<'static> {
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
        let mut parts = vec![
            format!(
                "{}/{}",
                effective_agent.profile.id, effective_model.profile.id
            ),
            format!(
                "{}+{} tokens",
                self.status.prompt_tokens, self.status.completion_tokens
            ),
            format!("sid {}", short_session_id(&self.session_id)),
            format!(
                "cwd {}",
                compact_workspace_label(&self.workspace_root_label)
            ),
        ];
        if let Some(pct) = context_usage_percent(
            self.status.prompt_tokens,
            self.status.completion_tokens,
            self.status.max_prompt_tokens,
            effective_model.profile.context_tokens,
        ) {
            parts.insert(2, format!("ctx {pct}%"));
        }
        if self.status.model_elapsed_ms > 0 {
            parts.push(format!(
                "model {}",
                format_elapsed(self.status.model_elapsed_ms)
            ));
        }
        if let Some(branch) = &self.git_branch {
            parts.push(format!("git {branch}"));
        }

        let text = truncate_for_width(&parts.join(" · "), width.saturating_sub(1));
        Line::from(vec![
            dim_span(" "),
            Span::styled(text, tui_theme::dim_style()),
        ])
    }

    fn supervisor_status_label(&self) -> String {
        let Some(id) = self.active_supervisor_id.as_ref() else {
            return "supervisor idle".to_string();
        };
        let Some(state) = self.supervisors.get(id) else {
            return "supervisor running".to_string();
        };
        if let Some(edge) = state.edge.as_deref().filter(|value| !value.is_empty()) {
            return format!("supervisor {edge}");
        }
        if let Some(status) = state.status.as_deref().filter(|value| !value.is_empty()) {
            return format!("supervisor {status}");
        }
        if let Some(event) = state.events.last() {
            return format!("supervisor {}", event.label);
        }
        "supervisor running".to_string()
    }

    fn render_activity(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let mut lines = Vec::new();
        if let Some(line) = self.current_activity_line(area.width) {
            lines.push(line);
        }
        if let Some(next) = self.queued_inputs.front() {
            lines.push(Line::from(vec![
                accent_span("  queued", Modifier::empty()),
                dim_span(" · "),
                Span::styled(
                    truncate_for_width(&next.display_text, area.width.saturating_sub(14)),
                    Style::default().fg(CODEX_DIM),
                ),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_action_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let Some((status, lines)) = self.pending_action_panel_lines(area.width.saturating_sub(2))
        else {
            return;
        };
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" action required ")
            .border_style(status.style().add_modifier(Modifier::BOLD));
        let inner = inset_rect(area, 1, 1);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let mut lines = lines;
        lines.truncate(inner.height as usize);
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn pending_action_panel_lines(
        &self,
        width: u16,
    ) -> Option<(ToolVisualStatus, Vec<Line<'static>>)> {
        if let Some(request) = &self.pending_approval {
            return Some((
                ToolVisualStatus::Running,
                self.approval_panel_lines(request, width),
            ));
        }
        self.pending_user_question.as_ref().map(|request| {
            (
                ToolVisualStatus::Running,
                self.user_question_panel_lines(request, width),
            )
        })
    }

    fn approval_panel_lines(
        &self,
        request: &ToolApprovalRequest,
        width: u16,
    ) -> Vec<Line<'static>> {
        let risk = request
            .review
            .as_ref()
            .map(|review| format!("{:?}", review.risk))
            .unwrap_or_else(|| format!("{:?}", request.risk));
        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    "approval",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                dim_span(" · "),
                Span::styled(
                    truncate_for_width(&request.tool_name, width.saturating_sub(18)),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(vec![dim_span(format!(
                "state {} · risk {} · id {}",
                self.approval_state,
                risk,
                short_session_id(&request.id)
            ))]),
        ];
        if let Some(review) = &request.review {
            if !review.reason.trim().is_empty() {
                lines.push(Line::from(dim_span(truncate_for_width(
                    &format!("review: {}", review.reason.trim()),
                    width,
                ))));
            }
            for concern in review.concerns.iter().take(2) {
                lines.push(Line::from(dim_span(truncate_for_width(
                    &format!("- {concern}"),
                    width,
                ))));
            }
        }
        if !request.args_summary.trim().is_empty() {
            lines.push(Line::from(dim_span(truncate_for_width(
                &format!("args: {}", single_line(&request.args_summary)),
                width,
            ))));
        }
        lines.push(Line::from(""));
        for (index, option) in approval_options().iter().enumerate() {
            lines.push(option_line(
                index == self.approval_selected,
                index + 1,
                option.label,
                Some(option.key),
                None,
                width,
            ));
        }
        lines.push(Line::from(dim_span(
            "Up/Down choose · Enter confirm · Esc deny",
        )));
        lines
    }

    fn user_question_panel_lines(
        &self,
        request: &UserQuestionRequest,
        width: u16,
    ) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(vec![
            accent_span("question", Modifier::BOLD),
            dim_span(" · "),
            Span::styled(
                truncate_for_width(&request.question, width.saturating_sub(12)),
                Style::default().fg(Color::White),
            ),
        ])];
        if let Some(reason) = request
            .reason
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            lines.push(Line::from(dim_span(truncate_for_width(
                &format!("reason: {reason}"),
                width,
            ))));
        }
        if !request.options.is_empty() {
            lines.push(Line::from(""));
            for (index, option) in request.options.iter().enumerate() {
                let recommended = request.default_option_id.as_deref() == Some(option.id.as_str());
                lines.push(option_line(
                    index == self.user_question_selected,
                    index + 1,
                    &option.label,
                    None,
                    option
                        .description
                        .as_deref()
                        .or(recommended.then_some("recommended")),
                    width,
                ));
            }
        }
        if request.allow_free_text {
            let placeholder = request
                .placeholder
                .as_deref()
                .unwrap_or("Type an answer and press Enter.");
            lines.push(Line::from(dim_span(truncate_for_width(placeholder, width))));
        }
        lines.push(Line::from(dim_span(
            "Up/Down choose · Enter answer · Esc cancel",
        )));
        lines
    }

    fn current_activity_line(&self, width: u16) -> Option<Line<'static>> {
        if !self.running || self.active_tool_count() == 0 {
            return None;
        }
        let cell = self.cells.iter().rev().find(|cell| {
            matches!(
                cell.kind,
                CellKind::Tool { .. } | CellKind::PatchDiff { .. }
            ) && cell.status == ToolVisualStatus::Running
        })?;
        let mut detail = sanitize_tui_text(&cell.title);
        if !cell.meta.trim().is_empty() {
            detail.push_str(" · ");
            detail.push_str(&sanitize_tui_text(&cell.meta));
        }
        Some(Line::from(vec![
            Span::styled("  working", Style::default().fg(Color::Yellow)),
            dim_span(" · "),
            Span::styled(
                truncate_for_width(&detail, width.saturating_sub(14)),
                Style::default().fg(CODEX_DIM),
            ),
        ]))
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
        frame.render_widget(Paragraph::new(hints).alignment(Alignment::Left), area);
    }

    fn base_footer_line(&self) -> Line<'static> {
        let running_hint = if self.running {
            "Ctrl+C cancel"
        } else {
            "Ctrl+C exit"
        };
        let focus_hint = self.input_focus_label();
        let hints = Line::from(vec![
            dim_span(FOOTER_INDENT),
            dim_span("focus "),
            accent_span(focus_hint, Modifier::empty()),
            dim_span(" · "),
            key_span("?"),
            dim_span(" shortcuts"),
            dim_span(" · "),
            key_span("Enter"),
            dim_span(" send"),
            dim_span(" · "),
            key_span("/"),
            dim_span(" commands"),
            dim_span(" · "),
            key_span(running_hint),
        ]);
        hints
    }

    fn input_focus_label(&self) -> &'static str {
        if self.pending_approval.is_some() {
            "approval"
        } else if self.pending_user_question.is_some() {
            "question"
        } else if self.history_browsing {
            "input/history"
        } else {
            match self.active_popup() {
                Some(PopupKind::Command) => "command menu",
                Some(PopupKind::File) => "file menu",
                None => "input",
            }
        }
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
            if self.is_pending_action_history_cell(cell) {
                continue;
            }
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

    fn is_pending_action_history_cell(&self, cell: &HistoryCell) -> bool {
        match &cell.kind {
            CellKind::Approval { id } => self
                .pending_approval
                .as_ref()
                .is_some_and(|request| &request.id == id),
            CellKind::UserQuestion { id } => self
                .pending_user_question
                .as_ref()
                .is_some_and(|request| &request.id == id),
            _ => false,
        }
    }

    fn render_composer(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        frame.render_widget(Paragraph::new(self.composer_focus_rule(area.width)), area);
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

    fn composer_focus_rule(&self, width: u16) -> Line<'static> {
        if width == 0 {
            return Line::default();
        }
        horizontal_rule(width)
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
                let is_selected = index == self.popup_selected;
                let command_style = if is_selected {
                    Style::default().fg(CODEX_CYAN).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(CODEX_CYAN)
                };
                let description_style = Style::default().fg(CODEX_DIM);
                let marker = tui_theme::selection_marker(is_selected);
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{marker} {:<22}", command.value.trim_end()),
                        command_style,
                    ),
                    Span::styled(sanitize_tui_text(&command.description), description_style),
                ]))
            })
            .collect::<Vec<_>>();
        let list = List::new(rows).block(self.popup_block(" slash commands ", PopupKind::Command));
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
                let is_selected = index == self.popup_selected;
                let path_style = if is_selected {
                    Style::default().fg(CODEX_CYAN).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(CODEX_CYAN)
                };
                let marker = tui_theme::selection_marker(is_selected);
                let display = truncate_for_width(&format!("@{}", file.mention_path), 44);
                let relative = truncate_for_width(&file.display_path, 30);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{marker} {:<44}", display), path_style),
                    Span::styled(relative, Style::default().fg(CODEX_DIM)),
                ]))
            })
            .collect::<Vec<_>>();
        let list = List::new(rows).block(self.popup_block(" files ", PopupKind::File));
        frame.render_widget(list, area);
    }

    fn popup_block(&self, title: &'static str, popup: PopupKind) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(self.popup_border_style(popup))
            .style(Style::default())
    }

    fn popup_border_style(&self, popup: PopupKind) -> Style {
        if !self.history_browsing && self.active_popup() == Some(popup) {
            Style::default().fg(CODEX_CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(CODEX_BORDER)
        }
    }
}

fn dim_span(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), tui_theme::dim_style())
}

fn accent_span(text: impl Into<String>, modifier: Modifier) -> Span<'static> {
    Span::styled(text.into(), tui_theme::accent_style(modifier))
}

fn key_span(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(Color::White))
}

fn option_line(
    selected: bool,
    number: usize,
    label: &str,
    key: Option<&str>,
    detail: Option<&str>,
    width: u16,
) -> Line<'static> {
    let marker = tui_theme::selection_marker(selected);
    let mut text = format!("{marker} {number}. {label}");
    if let Some(key) = key {
        text.push_str(&format!(" ({key})"));
    }
    if let Some(detail) = detail.filter(|value| !value.trim().is_empty()) {
        text.push_str(" · ");
        text.push_str(detail);
    }
    let style = if selected {
        tui_theme::accent_style(Modifier::BOLD)
    } else {
        tui_theme::dim_style()
    };
    Line::from(Span::styled(truncate_for_width(&text, width), style))
}

fn inset_rect(area: Rect, x: u16, y: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(x),
        y: area.y.saturating_add(y),
        width: area.width.saturating_sub(x.saturating_mul(2)),
        height: area.height.saturating_sub(y.saturating_mul(2)),
    }
}
