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
                Constraint::Min(4),
                Constraint::Length(action_height),
                Constraint::Length(activity_height),
                Constraint::Length(self.composer_height(root.width)),
                Constraint::Length(1),
                Constraint::Length(footer_height),
            ])
            .split(root);

        self.render_history(frame, chunks[0]);
        self.render_action_panel(frame, chunks[1]);
        self.render_activity(frame, chunks[2]);
        self.render_composer(frame, chunks[3]);
        self.render_status_separator(frame, chunks[4]);
        self.render_footer(frame, chunks[5]);
        self.refresh_file_matches();
        match self.active_popup() {
            Some(PopupKind::Command) => self.render_command_popup(frame, chunks[3]),
            Some(PopupKind::File) => self.render_file_popup(frame, chunks[3]),
            None => {}
        }
    }

    fn render_status_bar(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.flush_status_elapsed();
        let active_tools = self.active_tool_count();
        let status = self.execution_status_label();
        let status_busy = status != "idle" && !status.contains("error");
        let meta = self.status_metadata_parts(active_tools);
        let context = self.status_context_parts();
        let detail = [meta, context].concat().join(" · ");
        let detail = truncate_for_width(&detail, area.width.saturating_sub(4));
        let line = Line::from(vec![
            dim_span(FOOTER_INDENT),
            Span::styled(
                status,
                Style::default().fg(if status_busy {
                    Color::Yellow
                } else {
                    CODEX_GREEN
                }),
            ),
            Span::styled(
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" · {detail}")
                },
                Style::default().fg(CODEX_DIM),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_status_separator(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(Paragraph::new(horizontal_rule(area.width)), area);
    }

    /// A single, user-facing execution status. Multiple active owners share
    /// the same suffix so `agent, supervisor running` reads as one status
    /// instead of two competing status fields.
    fn execution_status_label(&self) -> String {
        let mut owners: Vec<(String, &'static str)> = Vec::new();
        let supervisor_phase = self.active_supervisor_phase();

        if supervisor_phase.is_none()
            && !self.awaiting_background_tasks
            && (self.running || !matches!(self.status.state.as_str(), "idle"))
        {
            let phase = match self.status.state.as_str() {
                "thinking" => "thinking",
                "cancelling" => "cancelling",
                "error" => "error",
                _ => "running",
            };
            owners.push(("agent".to_string(), phase));
        }
        // A sub-session is a background task only after the foreground agent
        // has finished. While the agent is still running, its sub-sessions are
        // part of the agent's own execution rather than a separate status.
        let background_task_count = self.background_task_count;
        if (!self.running || self.awaiting_background_tasks) && background_task_count > 0 {
            let label = format!("{background_task_count} background tasks");
            owners.push((label, "running"));
        }
        if let Some(phase) = supervisor_phase {
            owners.push(("supervisor".to_string(), phase));
        }

        if owners.is_empty() && !self.compressing_memory {
            return "idle".to_string();
        }

        let mut labels = Vec::new();
        if self.compressing_memory {
            labels.push("Compressing memory".to_string());
        }
        for phase in ["running", "thinking", "cancelling", "paused", "error"] {
            let names: Vec<&str> = owners
                .iter()
                .filter_map(|(name, owner_phase)| (*owner_phase == phase).then_some(name.as_str()))
                .collect();
            if !names.is_empty() {
                labels.push(format!("{} {phase}", names.join(", ")));
            }
        }
        labels.join(" · ")
    }

    /// Returns a supervisor phase only after a real workflow report exists.
    /// `active_supervisor_id` is allocated for every turn, so it is not by
    /// itself evidence that the supervisor is doing work.
    fn active_supervisor_phase(&self) -> Option<&'static str> {
        let id = self.active_supervisor_id.as_ref()?;
        let state = self.supervisors.get(id)?;
        if state.executing {
            return Some("running");
        }
        match state.status.as_deref()?.to_ascii_lowercase().as_str() {
            "paused" => Some("paused"),
            "error" => Some("error"),
            _ => None,
        }
    }

    fn status_metadata_parts(&self, active_tools: usize) -> Vec<String> {
        let mut meta = Vec::new();
        if let Some(todo) = &self.latest_active_todo_label {
            meta.push(todo.clone());
        }
        if let Some(node) = self.current_supervisor_node_label() {
            meta.push(format!("supervisor {node}"));
        }
        if self.running {
            meta.push(format!("turn {}", format_elapsed(self.status.elapsed_ms)));
        }
        if active_tools > 0 {
            meta.push(format!("{active_tools} tools"));
        }
        meta
    }

    fn current_supervisor_node_label(&self) -> Option<&str> {
        let id = self.active_supervisor_id.as_ref()?;
        self.supervisors
            .get(id)?
            .to_node
            .as_deref()
            .filter(|node| !node.trim().is_empty())
    }

    fn status_context_parts(&self) -> Vec<String> {
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
                "model {}/{}",
                effective_agent.profile.id, effective_model.profile.id
            ),
            self.git_status
                .as_ref()
                .map(|status| format!("git {status}"))
                .unwrap_or_default(),
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
            parts.insert(1, format!("ctx {pct}%"));
        }
        if self.status.model_elapsed_ms > 0 {
            parts.push(format!(
                "model {}",
                format_elapsed(self.status.model_elapsed_ms)
            ));
        }
        parts.retain(|part| !part.is_empty());
        parts
    }

    fn render_activity(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let mut lines = Vec::new();
        if let Some(line) = self.current_activity_line(area.width) {
            lines.push(line);
        }
        let queued_total = self.queued_inputs.len() + self.pending_steers.len();
        if queued_total > 0 {
            lines.push(Line::from(vec![accent_span(
                format!("  queued {queued_total}"),
                Modifier::empty(),
            )]));
            for input in self
                .queued_inputs
                .iter()
                .take(area.height.saturating_sub(lines.len() as u16) as usize)
            {
                lines.push(Line::from(vec![
                    dim_span("  ↳ next · "),
                    Span::styled(
                        truncate_for_width(&input.display_text, area.width.saturating_sub(12)),
                        Style::default().fg(CODEX_DIM),
                    ),
                ]));
            }
            for steer in self
                .pending_steers
                .iter()
                .take(area.height.saturating_sub(lines.len() as u16) as usize)
            {
                let label = if steer.next_turn {
                    "  ↳ next · "
                } else {
                    "  ↳ steer · "
                };
                lines.push(Line::from(vec![
                    dim_span(label),
                    Span::styled(
                        truncate_for_width(
                            &steer.display_text,
                            area.width.saturating_sub(label.len() as u16),
                        ),
                        Style::default().fg(CODEX_DIM),
                    ),
                ]));
            }
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

    fn render_footer(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let status_area = Rect::new(area.x, area.y, area.width, 1);
        self.render_status_bar(frame, status_area);
        if self.show_shortcuts {
            let shortcuts_area = Rect::new(
                area.x,
                area.y.saturating_add(1),
                area.width,
                area.height.saturating_sub(1),
            );
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
            frame.render_widget(Paragraph::new(lines), shortcuts_area);
        }
    }

    fn render_history(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let visible_lines = self.visible_history_lines(area);
        let paragraph = Paragraph::new(visible_lines).style(Style::default().fg(Color::Gray));
        frame.render_widget(Clear, area);
        frame.render_widget(paragraph, area);
    }

    fn visible_history_lines(&mut self, area: Rect) -> Vec<Line<'static>> {
        let height = area.height as usize;
        if height == 0 {
            return Vec::new();
        }
        let needed = height.saturating_add(self.scroll as usize);
        let mut lines = Vec::with_capacity(needed.min(height.saturating_mul(2).max(1)));
        let mut visited_cache_indices = Vec::new();
        self.history_line_cache
            .retain(|index, _| *index < self.cells.len());
        for index in (0..self.cells.len()).rev() {
            let cell = &self.cells[index];
            if self.is_pending_action_history_cell(cell) {
                continue;
            }
            let fingerprint = Self::history_cell_fingerprint(cell);
            let cell_lines = match self.history_line_cache.get(&index) {
                Some(cached) if cached.width == area.width && cached.fingerprint == fingerprint => {
                    cached.lines.clone()
                }
                _ => {
                    let lines = cell.lines(area.width);
                    self.history_line_cache.insert(
                        index,
                        CachedHistoryCell {
                            width: area.width,
                            fingerprint,
                            lines: lines.clone(),
                        },
                    );
                    lines
                }
            };
            visited_cache_indices.push(index);
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
        // Keep the cache proportional to the viewport rather than to every
        // cell a user has scrolled through in a long-running session.
        let retained_indices = visited_cache_indices
            .into_iter()
            .rev()
            .take(MAX_HISTORY_LINE_CACHE_CELLS)
            .collect::<std::collections::HashSet<_>>();
        self.history_line_cache
            .retain(|index, _| retained_indices.contains(index));
        lines.reverse();
        lines.into_iter().take(height).collect()
    }

    fn history_cell_fingerprint(cell: &HistoryCell) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::mem::discriminant(&cell.kind).hash(&mut hasher);
        cell.title.hash(&mut hasher);
        cell.body.hash(&mut hasher);
        cell.meta.hash(&mut hasher);
        cell.status.hash(&mut hasher);
        hasher.finish()
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
        // A running turn still accepts a draft (it will be queued as steer),
        // so keep the terminal cursor anchored to the composer once the user
        // starts typing. Otherwise every streaming redraw leaves the cursor at
        // the terminal's previous position and makes the input look unfocused.
        if !self.running || self.pending_user_question.is_some() || !self.composer.is_empty() {
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
