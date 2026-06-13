use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::prelude::{Color, Line, Modifier, Span, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy)]
pub(crate) struct MarkdownTheme {
    pub(crate) base: Style,
    pub(crate) dim: Color,
    pub(crate) accent: Color,
    pub(crate) code: Color,
    pub(crate) quote: Color,
}

pub(crate) fn render_markdown_lines(
    input: &str,
    width: u16,
    theme: MarkdownTheme,
) -> Vec<Line<'static>> {
    let mut renderer = MarkdownRenderer::new(theme);
    renderer.render(input);
    wrap_styled_lines(renderer.finish(), width.max(16) as usize, theme.dim)
}

struct MarkdownRenderer {
    theme: MarkdownTheme,
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    inline_styles: Vec<Style>,
    list_stack: Vec<ListState>,
    quote_depth: usize,
    code_block: bool,
    active_links: Vec<String>,
    table: Option<TableState>,
}

#[derive(Clone, Copy)]
struct ListState {
    next: u64,
    ordered: bool,
}

#[derive(Default)]
struct TableState {
    rows: Vec<TableRow>,
    current_row: Option<TableRow>,
    current_cell: Option<TableCell>,
    header_rows: usize,
}

type TableRow = Vec<TableCell>;
type TableCell = Vec<Span<'static>>;

impl MarkdownRenderer {
    fn new(theme: MarkdownTheme) -> Self {
        Self {
            theme,
            lines: Vec::new(),
            current: Vec::new(),
            inline_styles: vec![theme.base],
            list_stack: Vec::new(),
            quote_depth: 0,
            code_block: false,
            active_links: Vec::new(),
            table: None,
        }
    }

    fn render(&mut self, input: &str) {
        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TASKLISTS);
        options.insert(Options::ENABLE_TABLES);
        let parser = Parser::new_ext(input, options);

        for event in parser {
            match event {
                Event::Start(tag) => self.start_tag(tag),
                Event::End(tag) => self.end_tag(tag),
                Event::Text(text) => self.push_text(text.as_ref(), self.current_style()),
                Event::Code(code) => self.push_text(code.as_ref(), self.code_style()),
                Event::InlineMath(math) => self.push_text(math.as_ref(), self.code_style()),
                Event::DisplayMath(math) => self.push_code_lines(math.as_ref()),
                Event::Html(html) | Event::InlineHtml(html) => {
                    self.push_text(html.as_ref(), self.dim_style())
                }
                Event::FootnoteReference(reference) => {
                    self.push_text(&format!("[{reference}]"), self.dim_style())
                }
                Event::SoftBreak => self.push_text(" ", self.current_style()),
                Event::HardBreak => self.finish_current_line(),
                Event::Rule => {
                    self.finish_current_line();
                    self.push_line(Line::styled("─".repeat(24), self.dim_style()));
                    self.finish_current_line();
                }
                Event::TaskListMarker(done) => {
                    let marker = if done { "[x] " } else { "[ ] " };
                    self.push_text(marker, self.dim_style());
                }
            }
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.finish_current_line();
        trim_outer_blank_lines(&mut self.lines);
        self.lines
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.table.is_none() {
                    self.ensure_block_prefix();
                }
            }
            Tag::Heading { level, .. } => {
                self.finish_current_line();
                self.ensure_quote_prefix();
                let marker = "#".repeat(heading_level(level) as usize);
                self.push_span(format!("{marker} "), self.heading_style(level));
                self.push_inline_style(self.heading_style(level));
            }
            Tag::BlockQuote(_) => {
                self.finish_current_line();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.finish_current_line();
                self.code_block = true;
                if let CodeBlockKind::Fenced(info) = kind {
                    let lang = info.split_whitespace().next().unwrap_or_default();
                    if !lang.is_empty() {
                        self.push_line(Line::styled(format!("`{lang}`"), self.dim_style()));
                    }
                }
            }
            Tag::List(start) => {
                self.finish_current_line();
                self.list_stack.push(ListState {
                    next: start.unwrap_or(1),
                    ordered: start.is_some(),
                });
            }
            Tag::Item => {
                self.finish_current_line();
                self.ensure_quote_prefix();
                let depth = self.list_stack.len().saturating_sub(1);
                if depth > 0 {
                    self.push_span("  ".repeat(depth * 2), self.dim_style());
                }
                let marker = if let Some(state) = self.list_stack.last_mut() {
                    if state.ordered {
                        let marker = format!("{}. ", state.next);
                        state.next += 1;
                        marker
                    } else {
                        "- ".to_string()
                    }
                } else {
                    "- ".to_string()
                };
                self.push_span(marker, self.dim_style());
            }
            Tag::Emphasis => self.push_inline_modifier(Modifier::ITALIC),
            Tag::Strong => self.push_inline_modifier(Modifier::BOLD),
            Tag::Strikethrough => self.push_inline_modifier(Modifier::CROSSED_OUT),
            Tag::Link { dest_url, .. } => {
                self.active_links.push(dest_url.to_string());
                self.push_inline_style(
                    self.current_style()
                        .fg(self.theme.accent)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::Image { dest_url, .. } => {
                self.push_text("[image", self.dim_style());
                if !dest_url.is_empty() {
                    self.push_text(&format!(": {dest_url}"), self.dim_style());
                }
                self.push_text("] ", self.dim_style());
            }
            Tag::Table(_) => {
                self.finish_current_line();
                self.table = Some(TableState::default());
            }
            Tag::TableHead => {}
            Tag::TableRow => self.start_table_row(),
            Tag::TableCell => self.start_table_cell(),
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_)
            | Tag::Superscript
            | Tag::Subscript => self.ensure_block_prefix(),
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph
            | TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => {
                if self.table.is_none() {
                    self.finish_current_line();
                }
            }
            TagEnd::Heading(_) => {
                self.finish_current_line();
                self.pop_inline_style();
            }
            TagEnd::BlockQuote(_) => {
                self.finish_current_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.code_block = false;
                self.finish_current_line();
            }
            TagEnd::List(_) => {
                self.finish_current_line();
                self.list_stack.pop();
            }
            TagEnd::Item => self.finish_current_line(),
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript => self.pop_inline_style(),
            TagEnd::Link => {
                self.pop_inline_style();
                if let Some(link) = self.active_links.pop().filter(|link| !link.is_empty()) {
                    self.push_text(&format!(" ({link})"), self.dim_style());
                }
            }
            TagEnd::TableCell => self.finish_table_cell(),
            TagEnd::TableRow => self.finish_table_row(),
            TagEnd::TableHead => {
                self.finish_table_row();
                if let Some(table) = &mut self.table {
                    table.header_rows = table.rows.len().max(1);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.push_table(table);
                }
            }
            TagEnd::Image => {}
            TagEnd::DefinitionList => self.finish_current_line(),
        }
    }

    fn ensure_block_prefix(&mut self) {
        if self.current.is_empty() {
            self.ensure_quote_prefix();
        }
    }

    fn ensure_quote_prefix(&mut self) {
        if self.current.is_empty() && self.quote_depth > 0 {
            self.push_span(
                format!("{} ", ">".repeat(self.quote_depth)),
                self.quote_style(),
            );
        }
    }

    fn push_text(&mut self, text: &str, style: Style) {
        if self.code_block {
            self.push_code_lines(text);
            return;
        }
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.finish_current_line();
            }
            if !part.is_empty() {
                self.ensure_block_prefix();
                self.push_span(part.to_string(), style);
            }
        }
    }

    fn push_code_lines(&mut self, text: &str) {
        for line in text.split_inclusive('\n') {
            let content = line.strip_suffix('\n').unwrap_or(line);
            let mut spans = Vec::new();
            if self.quote_depth > 0 {
                spans.push(Span::styled(
                    format!("{} ", ">".repeat(self.quote_depth)),
                    self.quote_style(),
                ));
            }
            spans.push(Span::styled(format!("  {content}"), self.code_style()));
            self.push_line(Line::from(spans));
        }
    }

    fn push_span(&mut self, content: impl Into<String>, style: Style) {
        let content = content.into();
        if !content.is_empty() {
            if let Some(cell) = self
                .table
                .as_mut()
                .and_then(|table| table.current_cell.as_mut())
            {
                cell.push(Span::styled(content, style));
            } else {
                self.current.push(Span::styled(content, style));
            }
        }
    }

    fn push_line(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }

    fn finish_current_line(&mut self) {
        if self.table.is_none() && !self.current.is_empty() {
            self.lines
                .push(Line::from(std::mem::take(&mut self.current)));
        }
    }

    fn start_table_row(&mut self) {
        if let Some(table) = &mut self.table {
            table.current_row = Some(Vec::new());
        }
    }

    fn finish_table_row(&mut self) {
        self.finish_table_cell();
        if let Some(table) = &mut self.table {
            if let Some(row) = table.current_row.take() {
                table.rows.push(row);
            }
        }
    }

    fn start_table_cell(&mut self) {
        if let Some(table) = &mut self.table {
            table.current_cell = Some(Vec::new());
        }
    }

    fn finish_table_cell(&mut self) {
        if let Some(table) = &mut self.table {
            if let Some(cell) = table.current_cell.take() {
                table.current_row.get_or_insert_with(Vec::new).push(cell);
            }
        }
    }

    fn push_table(&mut self, table: TableState) {
        if table.rows.is_empty() {
            return;
        }
        let column_count = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if column_count == 0 {
            return;
        }

        let mut widths = vec![1usize; column_count];
        for row in &table.rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(spans_width(cell));
            }
        }

        for (row_index, row) in table.rows.into_iter().enumerate() {
            self.lines
                .push(table_row_line(row, &widths, self.dim_style()));
            if row_index + 1 == table.header_rows {
                self.lines
                    .push(table_separator_line(&widths, self.dim_style()));
            }
        }
    }

    fn current_style(&self) -> Style {
        self.inline_styles
            .last()
            .copied()
            .unwrap_or(self.theme.base)
    }

    fn push_inline_modifier(&mut self, modifier: Modifier) {
        self.push_inline_style(self.current_style().add_modifier(modifier));
    }

    fn push_inline_style(&mut self, style: Style) {
        self.inline_styles.push(style);
    }

    fn pop_inline_style(&mut self) {
        if self.inline_styles.len() > 1 {
            self.inline_styles.pop();
        }
    }

    fn heading_style(&self, level: HeadingLevel) -> Style {
        let modifier = if heading_level(level) <= 2 {
            Modifier::BOLD
        } else {
            Modifier::BOLD | Modifier::ITALIC
        };
        self.theme.base.fg(self.theme.accent).add_modifier(modifier)
    }

    fn code_style(&self) -> Style {
        self.theme.base.fg(self.theme.code)
    }

    fn quote_style(&self) -> Style {
        self.theme.base.fg(self.theme.quote)
    }

    fn dim_style(&self) -> Style {
        self.theme.base.fg(self.theme.dim)
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn trim_outer_blank_lines(lines: &mut Vec<Line<'static>>) {
    while lines
        .first()
        .is_some_and(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
    {
        lines.remove(0);
    }
    while lines
        .last()
        .is_some_and(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
    {
        lines.pop();
    }
}

fn table_row_line(row: TableRow, widths: &[usize], border_style: Style) -> Line<'static> {
    let mut spans = vec![Span::styled("│ ", border_style)];
    for (index, width) in widths.iter().enumerate() {
        let cell = row.get(index).cloned().unwrap_or_default();
        let cell_width = spans_width(&cell);
        spans.extend(cell);
        if *width > cell_width {
            spans.push(Span::raw(" ".repeat(width - cell_width)));
        }
        spans.push(Span::styled(" │", border_style));
        if index + 1 < widths.len() {
            spans.push(Span::raw(" "));
        }
    }
    Line::from(spans)
}

fn table_separator_line(widths: &[usize], style: Style) -> Line<'static> {
    let mut text = String::from("├");
    for (index, width) in widths.iter().enumerate() {
        text.push_str(&"─".repeat(width + 2));
        text.push(if index + 1 == widths.len() {
            '┤'
        } else {
            '┼'
        });
    }
    Line::styled(text, style)
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

fn wrap_styled_lines(lines: Vec<Line<'static>>, width: usize, dim: Color) -> Vec<Line<'static>> {
    if lines.is_empty() {
        return vec![Line::from("")];
    }
    lines
        .into_iter()
        .flat_map(|line| wrap_styled_line(line, width, dim))
        .collect()
}

fn wrap_styled_line(line: Line<'static>, width: usize, dim: Color) -> Vec<Line<'static>> {
    let width = width.max(1);
    let continuation = continuation_indent(&line);
    let mut wrapped = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0usize;
    let mut at_line_start = true;

    for span in line.spans {
        for token in styled_tokens(span.content.as_ref(), span.style) {
            if token.text == "\n" {
                wrapped.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
                at_line_start = true;
                continue;
            }

            let token_width = UnicodeWidthStr::width(token.text.as_str());
            if token.text.trim().is_empty() {
                if at_line_start {
                    continue;
                }
                if current_width + token_width > width {
                    wrapped.push(Line::from(std::mem::take(&mut current)));
                    current_width = 0;
                    at_line_start = true;
                    continue;
                }
                current.push(Span::styled(token.text, token.style));
                current_width += token_width;
                at_line_start = false;
                continue;
            }

            if !at_line_start && current_width + token_width > width {
                wrapped.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
                at_line_start = true;
            }

            if at_line_start && !wrapped.is_empty() && !continuation.is_empty() {
                current.push(Span::styled(continuation.clone(), Style::default().fg(dim)));
                current_width = UnicodeWidthStr::width(continuation.as_str());
                at_line_start = false;
            }

            if token_width > width.saturating_sub(current_width).max(1) {
                for ch in token.text.chars() {
                    let ch_width = ch.width().unwrap_or(0);
                    if !at_line_start && current_width + ch_width > width {
                        wrapped.push(Line::from(std::mem::take(&mut current)));
                        current_width = 0;
                        if !continuation.is_empty() {
                            current
                                .push(Span::styled(continuation.clone(), Style::default().fg(dim)));
                            current_width = UnicodeWidthStr::width(continuation.as_str());
                        }
                    }
                    current.push(Span::styled(ch.to_string(), token.style));
                    current_width += ch_width;
                    at_line_start = false;
                }
            } else {
                current.push(Span::styled(token.text, token.style));
                current_width += token_width;
                at_line_start = false;
            }
        }
    }

    wrapped.push(Line::from(current));
    wrapped
}

#[derive(Clone)]
struct StyledToken {
    text: String,
    style: Style,
}

fn styled_tokens(text: &str, style: Style) -> Vec<StyledToken> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut current_whitespace = None;

    for ch in text.chars() {
        if ch == '\n' {
            if !current.is_empty() {
                tokens.push(StyledToken {
                    text: std::mem::take(&mut current),
                    style,
                });
            }
            tokens.push(StyledToken {
                text: "\n".to_string(),
                style,
            });
            current_whitespace = None;
            continue;
        }

        let is_whitespace = ch.is_whitespace();
        match current_whitespace {
            Some(kind) if kind == is_whitespace => current.push(ch),
            Some(_) => {
                tokens.push(StyledToken {
                    text: std::mem::take(&mut current),
                    style,
                });
                current.push(ch);
                current_whitespace = Some(is_whitespace);
            }
            None => {
                current.push(ch);
                current_whitespace = Some(is_whitespace);
            }
        }
    }

    if !current.is_empty() {
        tokens.push(StyledToken {
            text: current,
            style,
        });
    }
    tokens
}

fn continuation_indent(line: &Line<'static>) -> String {
    let raw = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    let leading = raw.chars().take_while(|ch| ch.is_whitespace()).count();
    let trimmed = raw.trim_start();
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        return " ".repeat(leading + 2);
    }
    if let Some(dot_index) = trimmed.find(". ") {
        if trimmed[..dot_index].chars().all(|ch| ch.is_ascii_digit()) {
            return " ".repeat(leading + dot_index + 2);
        }
    }
    if trimmed.starts_with("> ") {
        return "> ".to_string();
    }
    " ".repeat(leading)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> MarkdownTheme {
        MarkdownTheme {
            base: Style::default().fg(Color::White),
            dim: Color::DarkGray,
            accent: Color::Cyan,
            code: Color::Yellow,
            quote: Color::Green,
        }
    }

    #[test]
    fn renders_inline_markdown_styles() {
        let lines = render_markdown_lines("hello **bold** *em* `code`", 80, theme());
        let spans = &lines[0].spans;
        assert!(spans.iter().any(|span| {
            span.content.as_ref() == "bold" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(spans.iter().any(|span| {
            span.content.as_ref() == "em" && span.style.add_modifier.contains(Modifier::ITALIC)
        }));
        assert!(spans
            .iter()
            .any(|span| span.content.as_ref() == "code" && span.style.fg == Some(Color::Yellow)));
    }

    #[test]
    fn wraps_list_items_with_continuation_indent() {
        let lines = render_markdown_lines(
            "- a very long list item that needs to wrap after the marker",
            24,
            theme(),
        );
        assert!(lines.len() > 1);
        let second = lines[1]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(second.starts_with("  "));
    }

    #[test]
    fn renders_partial_streaming_markdown_without_panicking() {
        let partial = render_markdown_lines("hello **bo", 80, theme());
        let complete = render_markdown_lines("hello **bold**", 80, theme());
        assert!(!partial.is_empty());
        assert!(complete[0].spans.iter().any(|span| {
            span.content.as_ref() == "bold" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    #[test]
    fn renders_code_fences_as_code_lines() {
        let lines = render_markdown_lines("```rust\nfn main() {}\n```\nafter", 80, theme());
        assert!(lines.iter().any(|line| {
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            text.contains("fn main")
                && line
                    .spans
                    .iter()
                    .any(|span| span.style.fg == Some(Color::Yellow))
        }));
        assert!(lines.iter().any(|line| line
            .spans
            .iter()
            .any(|span| span.content.as_ref() == "after")));
    }

    #[test]
    fn renders_markdown_tables() {
        let lines = render_markdown_lines(
            "| Name | Value |\n| --- | --- |\n| **CPU** | `42%` |\n",
            80,
            theme(),
        );
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(rendered.iter().any(|line| line.contains("│ Name")));
        assert!(rendered.iter().any(|line| line.contains("├")));
        assert!(rendered.iter().any(|line| line.contains("CPU")));
        assert!(lines.iter().any(|line| line.spans.iter().any(|span| {
            span.content.as_ref() == "CPU" && span.style.add_modifier.contains(Modifier::BOLD)
        })));
        assert!(lines.iter().any(|line| line.spans.iter().any(|span| {
            span.content.as_ref() == "42%" && span.style.fg == Some(Color::Yellow)
        })));
    }
}
