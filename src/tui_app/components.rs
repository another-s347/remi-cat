use super::*;

pub(super) struct StatusLine {
    pub(super) state: String,
    pub(super) prompt_tokens: u32,
    pub(super) completion_tokens: u32,
    pub(super) max_prompt_tokens: u32,
    pub(super) model_elapsed_ms: u64,
    pub(super) elapsed_ms: u64,
    pub(super) last_error: Option<String>,
}

#[derive(Clone, Copy, Default)]
pub(super) struct TokenStatsSnapshot {
    pub(super) prompt_tokens: u32,
    pub(super) completion_tokens: u32,
}

#[derive(Clone, Copy, Default)]
pub(super) struct TokenDelta {
    pub(super) prompt_tokens: u32,
    pub(super) completion_tokens: u32,
}

impl TokenDelta {
    pub(super) fn is_empty(self) -> bool {
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
pub(super) struct SupervisorUiState {
    pub(super) workflow_name: Option<String>,
    pub(super) from_node: Option<String>,
    pub(super) to_node: Option<String>,
    pub(super) edge: Option<String>,
    pub(super) status: Option<String>,
    pub(super) reason: Option<String>,
    pub(super) events: Vec<SupervisorEventDisplay>,
}

#[derive(Clone)]
pub(super) struct SupervisorEventDisplay {
    pub(super) kind: &'static str,
    pub(super) label: String,
    pub(super) body: String,
}

impl SupervisorUiState {
    pub(super) fn push_event(&mut self, event: SupervisorEventDisplay) {
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

    pub(super) fn apply_report(&mut self, report: &WorkflowReport) {
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

    pub(super) fn running_title(&self) -> String {
        let workflow = self.workflow_name.as_deref().unwrap_or("Supervisor");
        format!("supervisor · {workflow} · reviewing")
    }

    pub(super) fn resolved_title(&self) -> String {
        let workflow = self.workflow_name.as_deref().unwrap_or("Supervisor");
        let from = self.from_node.as_deref().unwrap_or("?");
        let to = self.to_node.as_deref().unwrap_or("?");
        let status = self.status.as_deref().unwrap_or("done");
        format!("supervisor · {workflow} · {from} -> {to} · {status}")
    }

    pub(super) fn meta(&self) -> String {
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

    pub(super) fn body(&self) -> String {
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
pub(super) struct SubSessionUiState {
    pub(super) agent_name: String,
    pub(super) input: Option<String>,
    pub(super) parent_tool_call_id: String,
    pub(super) thread_id: String,
    pub(super) depth: u32,
    pub(super) tools: Vec<SubToolDisplay>,
    pub(super) activities: Vec<SubSessionActivity>,
    pub(super) done: bool,
    pub(super) failed: bool,
    pub(super) final_output: Option<String>,
}

impl SubSessionUiState {
    pub(super) fn from_event(event: &SubSessionEvent) -> Self {
        Self {
            agent_name: event.agent_name.clone(),
            input: sub_session_input(event),
            parent_tool_call_id: event.parent_tool_call_id.clone(),
            thread_id: event.sub_thread_id.0.clone(),
            depth: event.depth,
            tools: Vec::new(),
            activities: Vec::new(),
            done: false,
            failed: false,
            final_output: None,
        }
    }

    pub(super) fn update_context(&mut self, event: &SubSessionEvent) {
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

    pub(super) fn upsert_tool(&mut self, tool: SubToolDisplay) {
        if let Some(existing) = self.tools.iter_mut().find(|item| item.id == tool.id) {
            *existing = tool;
        } else {
            self.tools.push(tool);
        }
    }

    pub(super) fn push_activity(&mut self, activity: SubSessionActivity) {
        self.activities.push(activity);
    }

    pub(super) fn upsert_activity(&mut self, activity: SubSessionActivity) {
        if let Some(existing) = self
            .activities
            .iter_mut()
            .find(|item| item.key.as_deref() == activity.key.as_deref() && item.key.is_some())
        {
            *existing = activity;
        } else {
            self.activities.push(activity);
        }
    }

    pub(super) fn title(&self) -> String {
        let indent = "  ".repeat(self.depth as usize);
        format!(
            "{indent}sub-agent · {} · calling {} tool{}",
            self.agent_name,
            self.tools.len(),
            if self.tools.len() == 1 { "" } else { "s" }
        )
    }

    pub(super) fn meta(&self) -> String {
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

    pub(super) fn body(&self) -> String {
        let nested = "  ".repeat(self.depth.saturating_add(1) as usize);
        let mut lines = Vec::new();
        if let Some(input) = self
            .input
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            lines.push(format!("{nested}input: {input}"));
        }
        let recent_start = self.activities.len().saturating_sub(3);
        for activity in self.activities.iter().skip(recent_start) {
            let status = activity.status.label();
            let suffix = if status.is_empty() {
                String::new()
            } else {
                format!(" · {status}")
            };
            lines.push(format!("{nested}↳ {}{suffix}", activity.label));
            if !activity.body.trim().is_empty() {
                lines.push(format!("{nested}  {}", activity.body));
            }
        }
        if lines.is_empty() {
            lines.push(format!("{nested}waiting for sub-agent activity"));
        }
        lines.join("\n")
    }

    pub(super) fn status(&self) -> ToolVisualStatus {
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
pub(super) struct SubSessionActivity {
    pub(super) key: Option<String>,
    pub(super) label: String,
    pub(super) body: String,
    pub(super) status: ToolVisualStatus,
}

impl SubSessionActivity {
    pub(super) fn message(
        label: impl Into<String>,
        body: impl AsRef<str>,
        status: ToolVisualStatus,
    ) -> Self {
        Self {
            key: None,
            label: label.into(),
            body: truncate_chars(&single_line(body.as_ref()), MAX_TOOL_BODY_CHARS),
            status,
        }
    }

    pub(super) fn keyed(
        key: impl Into<String>,
        label: impl Into<String>,
        body: impl AsRef<str>,
        status: ToolVisualStatus,
    ) -> Self {
        Self {
            key: Some(key.into()),
            label: label.into(),
            body: truncate_chars(&single_line(body.as_ref()), MAX_TOOL_BODY_CHARS),
            status,
        }
    }

    pub(super) fn from_tool(id: &str, tool: &SubToolDisplay) -> Self {
        Self {
            key: Some(format!("tool:{id}")),
            label: tool.title.clone(),
            body: tool.summary.clone(),
            status: tool.status,
        }
    }
}

#[derive(Clone)]
pub(super) struct SubToolDisplay {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) summary: String,
    pub(super) status: ToolVisualStatus,
}

impl SubToolDisplay {
    pub(super) fn from_pretty(id: &str, pretty: &PrettyToolCall, status: ToolVisualStatus) -> Self {
        Self {
            id: id.to_string(),
            title: pretty.title.clone(),
            summary: tool_body(pretty),
            status,
        }
    }
}

#[derive(Clone)]
pub(super) struct HistoryCell {
    pub(super) kind: CellKind,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) meta: String,
    pub(super) status: ToolVisualStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ToolVisualStatus {
    Neutral,
    Running,
    Success,
    Error,
}

impl ToolVisualStatus {
    pub(super) fn from_success(success: bool) -> Self {
        if success {
            Self::Success
        } else {
            Self::Error
        }
    }

    pub(super) fn from_pretty(status: &PrettyToolStatus) -> Self {
        match status {
            PrettyToolStatus::Running => Self::Running,
            PrettyToolStatus::Success => Self::Success,
            PrettyToolStatus::Error => Self::Error,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Neutral => "",
            Self::Running => "running",
            Self::Success => "success",
            Self::Error => "failed",
        }
    }

    pub(super) fn style(self) -> Style {
        match self {
            Self::Neutral => Style::default().fg(CODEX_DIM),
            Self::Running => Style::default().fg(Color::Yellow),
            Self::Success => Style::default().fg(CODEX_GREEN),
            Self::Error => Style::default().fg(Color::Red),
        }
    }
}

#[derive(Clone)]
pub(super) enum CellKind {
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
    UserQuestion { id: String },
    ContextCompaction { id: String },
    Error,
}

impl HistoryCell {
    pub(super) fn system(text: impl Into<String>) -> Self {
        Self::new(CellKind::System, "system", text.into())
    }

    pub(super) fn user(text: impl Into<String>) -> Self {
        Self::new(CellKind::User, "you", text.into())
    }

    pub(super) fn assistant(text: impl Into<String>) -> Self {
        Self::new(CellKind::Assistant, "remi", text.into())
    }

    pub(super) fn thinking(text: impl Into<String>) -> Self {
        Self::new(CellKind::Thinking, "thinking", text.into())
    }

    pub(super) fn error(text: impl Into<String>) -> Self {
        Self::new(CellKind::Error, "error", text.into())
    }

    pub(super) fn todo_state(body: String) -> Self {
        Self::new(CellKind::TodoState, "todos", body)
    }

    pub(super) fn approval_with_title(
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

    pub(super) fn user_question_with_title(
        id: String,
        title: String,
        body: String,
        status: ToolVisualStatus,
    ) -> Self {
        Self {
            kind: CellKind::UserQuestion { id },
            title,
            body,
            meta: String::new(),
            status,
        }
    }

    pub(super) fn context_compaction(
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

    pub(super) fn tool(
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

    pub(super) fn patch_diff(
        id: String,
        patch: String,
        meta: String,
        status: ToolVisualStatus,
    ) -> Self {
        Self {
            kind: CellKind::PatchDiff { id },
            title: "patch diff".to_string(),
            body: patch,
            meta,
            status,
        }
    }

    pub(super) fn supervisor(
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

    pub(super) fn sub_session(
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

    pub(super) fn new(kind: CellKind, title: impl Into<String>, body: String) -> Self {
        Self {
            kind,
            title: title.into(),
            body,
            meta: String::new(),
            status: ToolVisualStatus::Neutral,
        }
    }

    pub(super) fn append(&mut self, delta: &str) {
        self.body.push_str(delta);
    }

    pub(super) fn tool_id(&self) -> Option<&str> {
        match &self.kind {
            CellKind::Tool { id } | CellKind::PatchDiff { id } => Some(id.as_str()),
            _ => None,
        }
    }

    pub(super) fn lines(&self, width: u16) -> Vec<Line<'static>> {
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
            CellKind::UserQuestion { .. } => (
                "?",
                Style::default().fg(CODEX_CYAN).add_modifier(Modifier::BOLD),
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
        let title = sanitize_tui_text(&title);
        lines.push(Line::from(vec![
            Span::styled(history_gutter(prefix), title_style),
            Span::styled(title, title_style),
        ]));
        if self.body.trim().is_empty()
            && matches!(
                self.kind,
                CellKind::Approval { .. }
                    | CellKind::UserQuestion { .. }
                    | CellKind::Supervisor { .. }
            )
        {
            lines.push(Line::from(""));
            return lines;
        }
        let raw_body = if self.body.trim().is_empty() {
            "(no content)"
        } else {
            self.body.trim_end()
        };
        let safe_body = sanitize_tui_text(raw_body);
        let body = safe_body.trim_end();
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

pub(super) fn history_gutter(prefix: &str) -> String {
    let used = UnicodeWidthStr::width(prefix) as u16;
    let spaces = HISTORY_GUTTER_WIDTH.saturating_sub(used).max(1);
    format!("{prefix}{}", " ".repeat(spaces as usize))
}

pub(super) fn horizontal_rule(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width as usize),
        Style::default().fg(CODEX_BORDER),
    ))
}

pub(super) fn upsert_approval_cell(cells: &mut Vec<HistoryCell>, next: HistoryCell) {
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

pub(super) fn upsert_user_question_cell(cells: &mut Vec<HistoryCell>, next: HistoryCell) {
    let next_id = match &next.kind {
        CellKind::UserQuestion { id } => id.clone(),
        _ => return,
    };
    if let Some(cell) = cells
        .iter_mut()
        .rev()
        .find(|cell| matches!(&cell.kind, CellKind::UserQuestion { id } if id == &next_id))
    {
        *cell = next;
    } else {
        cells.push(next);
    }
}

pub(super) fn upsert_context_compaction_cell(cells: &mut Vec<HistoryCell>, next: HistoryCell) {
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

pub(super) fn context_compaction_cell(event: ContextCompactionEvent) -> HistoryCell {
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

pub(super) fn history_cell(message: ThreadHistoryMessage) -> Option<HistoryCell> {
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

pub(super) fn extract_patch_arg(args: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(args).ok()?;
    value
        .get("patch")
        .and_then(|patch| patch.as_str())
        .map(ToOwned::to_owned)
}

pub(super) fn parse_tool_args(args: &str) -> Option<serde_json::Value> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Some(empty_tool_args());
    }
    serde_json::from_str::<serde_json::Value>(trimmed).ok()
}

pub(super) fn empty_tool_args() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

pub(super) fn tool_meta(pretty: &PrettyToolCall) -> String {
    pretty
        .elapsed_ms
        .map(format_elapsed)
        .unwrap_or_else(|| format_elapsed(0))
}

pub(super) fn append_token_meta(cell: &mut HistoryCell, delta: TokenDelta) {
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

pub(super) fn preserve_token_meta(mut meta: String, existing_meta: &str) -> String {
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

pub(super) fn patch_tool_meta(pretty: &PrettyToolCall) -> String {
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

pub(super) fn tool_body(pretty: &PrettyToolCall) -> String {
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

pub(super) fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut output = text.chars().take(keep).collect::<String>();
    output.push('…');
    output
}

#[cfg(test)]
pub(super) fn append_stream_event(body: &mut String, event: &str) {
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
pub(super) fn body_contains_stream_key(body: &str, event: &str) -> bool {
    let Some(key) = keyed_stream_event(event) else {
        return false;
    };
    body.split("\n\n")
        .any(|block| keyed_stream_event(block).as_deref() == Some(key.as_str()))
}

#[cfg(test)]
pub(super) fn mergeable_stream_event(event: &str) -> Option<(String, String)> {
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
pub(super) fn keyed_stream_event(event: &str) -> Option<String> {
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

pub(super) fn supervisor_event_display(event: &SupervisorTraceEvent) -> SupervisorEventDisplay {
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
pub(super) fn format_sub_session_title(event: &SubSessionEvent) -> String {
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
pub(super) fn format_sub_session_prefix(event: &SubSessionEvent) -> String {
    let title = event.agent_name.as_str();
    let indent = "  ".repeat(event.depth as usize);
    format!("{indent}sub-agent · {title}")
}

pub(super) fn sub_session_id(event: &SubSessionEvent) -> String {
    event.sub_thread_id.0.clone()
}

pub(super) fn sub_session_kind(event: &SubSessionEvent) -> SubSessionKind {
    if event.agent_name == "acp" {
        SubSessionKind::Acp
    } else {
        SubSessionKind::Agent
    }
}

pub(super) fn sub_session_status_label(payload: &SubSessionEventPayload) -> &'static str {
    match payload {
        SubSessionEventPayload::Done { .. } => "done",
        SubSessionEventPayload::Error { .. } => "error",
        _ => "running",
    }
}

pub(super) fn push_interrupt_requested_once(
    interrupt_requested: &mut bool,
    cells: &mut Vec<HistoryCell>,
) -> bool {
    if *interrupt_requested {
        return false;
    }
    *interrupt_requested = true;
    cells.push(HistoryCell::system("Interrupt requested."));
    true
}

pub(super) fn sub_session_input(event: &SubSessionEvent) -> Option<String> {
    event
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty() && *title != event.agent_name)
        .map(|title| truncate_chars(&single_line(title), 96))
}

pub(super) fn sub_session_history_messages(event: &SubSessionEvent) -> Vec<Message> {
    match &event.payload {
        SubSessionEventPayload::Start => {
            let input = event
                .title
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| {
                    format!(
                        "Sub-session started: {} / {}",
                        event.agent_name, event.sub_thread_id.0
                    )
                });
            vec![Message::user(input)]
        }
        _ => sub_session_event_text(event)
            .map(Message::assistant)
            .into_iter()
            .collect(),
    }
}

pub(super) fn sub_session_event_text(event: &SubSessionEvent) -> Option<String> {
    let text = match &event.payload {
        SubSessionEventPayload::Start => return None,
        SubSessionEventPayload::Delta { content } => content.clone(),
        SubSessionEventPayload::ThinkingStart => "Thinking...".to_string(),
        SubSessionEventPayload::ThinkingEnd { content } => {
            if content.trim().is_empty() {
                "Thinking complete.".to_string()
            } else {
                format!("Thinking:\n{content}")
            }
        }
        SubSessionEventPayload::TurnStart { turn } => format!("Turn {turn}"),
        SubSessionEventPayload::ToolCallStart { id, name } => {
            format!("Tool `{name}` started ({id}).")
        }
        SubSessionEventPayload::ToolCallArgumentsDelta { id, delta } => {
            format!("Tool arguments `{id}`:\n{delta}")
        }
        SubSessionEventPayload::ToolDelta { id, name, delta } => {
            format!("Tool `{name}` output ({id}):\n{delta}")
        }
        SubSessionEventPayload::ToolResult { id, name, result } => {
            format!("Tool `{name}` result ({id}):\n{result}")
        }
        SubSessionEventPayload::Done { final_output } => final_output
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "Done.".to_string()),
        SubSessionEventPayload::Error { message } => format!("Error: {message}"),
    };
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
pub(super) fn format_sub_session_meta(event: &SubSessionEvent) -> String {
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

pub(super) fn sub_session_status(payload: &SubSessionEventPayload) -> ToolVisualStatus {
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
pub(super) fn format_sub_session_event(event: &SubSessionEvent) -> Option<String> {
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
        SubSessionEventPayload::Done { .. } => Some("done".to_string()),
        SubSessionEventPayload::Error { message } => Some(format!(
            "error\n{}",
            truncate_chars(&single_line(message), MAX_TOOL_BODY_CHARS)
        )),
    }
}

pub(super) fn format_json_summary(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

pub(super) fn format_elapsed(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else if ms < 3_600_000 {
        let minutes = ms / 60_000;
        let seconds = (ms % 60_000) / 1_000;
        format!("{minutes}m{seconds:02}s")
    } else if ms < 86_400_000 {
        let hours = ms / 3_600_000;
        let minutes = (ms % 3_600_000) / 60_000;
        format!("{hours}h{minutes:02}m")
    } else {
        let days = ms / 86_400_000;
        let hours = (ms % 86_400_000) / 3_600_000;
        format!("{days}d{hours:02}h")
    }
}

pub(super) fn format_todo_state(items: &[bot_core::todo::TodoItem]) -> String {
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

pub(super) fn latest_active_todo_label(items: &[bot_core::todo::TodoItem]) -> Option<String> {
    let item = items.iter().rev().find(|item| !item.done)?;
    let mut label = format!("todo #{} {}", item.id, single_line(&item.content));
    if let Some(batch) = item
        .batch_title
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        label.push_str(&format!(" · {batch}"));
    }
    Some(truncate_chars(&label, 80))
}

#[derive(Clone, Copy)]
pub(super) struct ApprovalOption {
    pub(super) label: &'static str,
    pub(super) key: &'static str,
    pub(super) decision: ToolApprovalDecision,
}

pub(super) fn approval_options() -> [ApprovalOption; 4] {
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

pub(super) fn approval_options_len() -> usize {
    approval_options().len()
}

pub(super) fn approval_option(index: usize) -> Option<ApprovalOption> {
    approval_options().get(index).copied()
}

pub(super) fn approval_cell(
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

pub(super) fn user_question_cell(
    request: &UserQuestionRequest,
    state: &str,
    selected: usize,
    status: Option<String>,
    resolved_status: Option<UserQuestionStatus>,
) -> HistoryCell {
    if state == "resolved" {
        let label = match resolved_status {
            Some(UserQuestionStatus::Answered) => "answered",
            Some(UserQuestionStatus::Cancelled) => "cancelled",
            None => "resolved",
        };
        return HistoryCell::user_question_with_title(
            request.id.clone(),
            format!("question · {label}"),
            status.unwrap_or_default(),
            if matches!(resolved_status, Some(UserQuestionStatus::Cancelled)) {
                ToolVisualStatus::Error
            } else {
                ToolVisualStatus::Success
            },
        );
    }

    let mut lines = Vec::new();
    lines.push(request.question.clone());
    if let Some(reason) = request
        .reason
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        lines.push(format!("reason: {reason}"));
    }
    if !request.options.is_empty() {
        lines.push(String::new());
        for (index, option) in request.options.iter().enumerate() {
            let marker = if index == selected { ">" } else { " " };
            let default = if request.default_option_id.as_deref() == Some(option.id.as_str()) {
                " (recommended)"
            } else {
                ""
            };
            let mut line = format!("{marker} {}. {}{}", index + 1, option.label, default);
            if let Some(description) = option
                .description
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                line.push_str(&format!(" - {description}"));
            }
            lines.push(line);
        }
    }
    if request.allow_free_text {
        lines.push(String::new());
        lines.push(request.placeholder.clone().unwrap_or_else(|| {
            "Type an answer and press Enter. Use Shift+Enter for a newline.".to_string()
        }));
    }
    lines.push("Esc cancels; Up/Down changes selected option.".to_string());
    if let Some(status) = status {
        lines.push(String::new());
        lines.push(status);
    }
    HistoryCell::user_question_with_title(
        request.id.clone(),
        format!("question · {state}"),
        lines.join("\n"),
        ToolVisualStatus::Running,
    )
}

pub(super) fn build_tui_user_question_answer_text(
    selected_option_ids: &[String],
    free_text: Option<&str>,
    status: UserQuestionStatus,
) -> String {
    if status == UserQuestionStatus::Cancelled {
        return "User cancelled the question.".to_string();
    }
    let mut parts = Vec::new();
    if !selected_option_ids.is_empty() {
        parts.push(format!(
            "Selected option ids: {}",
            selected_option_ids.join(", ")
        ));
    }
    if let Some(text) = free_text {
        parts.push(format!("Free-text answer: {text}"));
    }
    if parts.is_empty() {
        "User answered without additional text.".to_string()
    } else {
        parts.join("\n")
    }
}

pub(super) fn diff_lines(text: &str, width: u16) -> Vec<Span<'static>> {
    let width = width.max(16);
    let text = sanitize_tui_text(text);
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

pub(super) fn diff_line_style(line: &str) -> Style {
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

pub(super) fn context_usage_percent(
    prompt_tokens: u32,
    completion_tokens: u32,
    max_prompt_tokens: u32,
    context_tokens: u32,
) -> Option<u32> {
    bot_core::ContextMetrics::from_usage(
        TokenUsage {
            prompt_tokens,
            completion_tokens,
            max_prompt_tokens,
        },
        context_tokens,
    )
    .rounded_percent()
}

pub(super) fn truncate_for_width(text: &str, width: u16) -> String {
    let width = width as usize;
    let text = sanitize_tui_text(text);
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text.as_str()) <= width {
        return text;
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

pub(super) fn short_session_label(session_id: &str) -> String {
    let width = UnicodeWidthStr::width(session_id);
    if width <= 12 {
        return session_id.to_string();
    }
    let prefix = session_id.chars().take(8).collect::<String>();
    format!("{prefix}…")
}
