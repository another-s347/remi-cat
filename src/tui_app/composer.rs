use super::*;

#[derive(Debug, Clone)]
pub(super) enum InputSegment {
    Text(String),
    Paste { text: String, label: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct ComposerCursor {
    segment: usize,
    offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileMentionToken {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) query: String,
}

#[derive(Debug, Clone, Default)]
pub(super) struct ComposerInput {
    segments: Vec<InputSegment>,
    pub(super) cursor: ComposerCursor,
}

impl ComposerInput {
    pub(super) fn is_empty(&self) -> bool {
        self.segments.iter().all(|segment| match segment {
            InputSegment::Text(text) => text.is_empty(),
            InputSegment::Paste { .. } => false,
        })
    }

    pub(super) fn clear(&mut self) {
        self.segments.clear();
        self.cursor = ComposerCursor::default();
    }

    pub(super) fn set_text(&mut self, text: String) {
        self.segments = if text.is_empty() {
            Vec::new()
        } else {
            vec![InputSegment::Text(text)]
        };
        self.cursor = self.end_cursor();
    }

    pub(super) fn to_text(&self) -> String {
        let mut text = String::new();
        for segment in &self.segments {
            match segment {
                InputSegment::Text(value) => text.push_str(value),
                InputSegment::Paste { text: value, .. } => text.push_str(value),
            }
        }
        text
    }

    pub(super) fn command_text(&self) -> Option<&str> {
        if self.segments.len() != 1 {
            return None;
        }
        match &self.segments[0] {
            InputSegment::Text(text) => Some(text),
            InputSegment::Paste { .. } => None,
        }
    }

    pub(super) fn active_file_mention_token(&self) -> Option<FileMentionToken> {
        let text = self.to_text();
        let cursor = self.text_byte_for_cursor(self.cursor).min(text.len());
        active_file_mention_token(&text, cursor)
    }

    pub(super) fn replace_text_range(&mut self, start: usize, end: usize, replacement: &str) {
        let start_cursor = self.cursor_from_text_byte(start.min(self.to_text().len()));
        let end_cursor = self.cursor_from_text_byte(end.min(self.to_text().len()));
        self.delete_range(start_cursor, end_cursor);
        self.cursor = start_cursor;
        self.insert_text(replacement);
    }

    pub(super) fn display_text(&self) -> String {
        let mut text = String::new();
        for segment in &self.segments {
            match segment {
                InputSegment::Text(value) => text.push_str(value),
                InputSegment::Paste { label, .. } => text.push_str(label),
            }
        }
        text
    }

    pub(super) fn display_lines(&self) -> Vec<Line<'static>> {
        let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
        for segment in &self.segments {
            match segment {
                InputSegment::Text(text) => {
                    let safe_text = sanitize_tui_text(text);
                    for (index, part) in safe_text.split('\n').enumerate() {
                        if index > 0 {
                            lines.push(Vec::new());
                        }
                        let part = expand_tabs(part);
                        if !part.is_empty() {
                            lines
                                .last_mut()
                                .expect("composer line exists")
                                .push(Span::styled(part, Style::default().fg(Color::White)));
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

    pub(super) fn insert_char(&mut self, ch: char) {
        let mut text = String::new();
        text.push(ch);
        self.insert_text(&text);
    }

    pub(super) fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.insert_segment(InputSegment::Text(text.to_string()));
    }

    pub(super) fn insert_paste(&mut self, text: String) {
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

    pub(super) fn backspace(&mut self) {
        let previous = self.previous_cursor(self.cursor);
        if previous == self.cursor {
            return;
        }
        self.delete_range(previous, self.cursor);
        self.cursor = previous;
        self.merge_text_segments_around_cursor();
    }

    pub(super) fn delete(&mut self) {
        let next = self.next_cursor(self.cursor);
        if next == self.cursor {
            return;
        }
        let current = self.cursor;
        self.delete_range(current, next);
        self.cursor = current;
        self.merge_text_segments_around_cursor();
    }

    pub(super) fn move_left(&mut self) {
        self.cursor = self.previous_cursor(self.cursor);
    }

    pub(super) fn move_right(&mut self) {
        self.cursor = self.next_cursor(self.cursor);
    }

    pub(super) fn move_home(&mut self) {
        self.cursor = ComposerCursor::default();
    }

    pub(super) fn move_end(&mut self) {
        self.cursor = self.end_cursor();
    }

    pub(super) fn move_vertical(&mut self, direction: isize) {
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

    pub(super) fn cursor_display_byte(&self) -> usize {
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

    pub(super) fn delete_range(&mut self, start: ComposerCursor, end: ComposerCursor) {
        let (mut before, _) = split_segments_at(&self.segments, start);
        let (_, after) = split_segments_at(&self.segments, end);
        before.extend(after);
        self.segments = before;
        self.cursor = start;
    }

    pub(super) fn text_byte_for_cursor(&self, cursor: ComposerCursor) -> usize {
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

pub(super) fn composer_visual_lines(composer: &ComposerInput, width: u16) -> Vec<Line<'static>> {
    wrap_composer_lines(composer.display_lines(), width)
}

pub(super) fn composer_visual_line_count(composer: &ComposerInput, width: u16) -> u16 {
    composer_visual_lines(composer, width).len().max(1) as u16
}

fn wrap_composer_lines(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(4);
    let mut wrapped = Vec::new();
    for line in lines {
        let mut current = Vec::new();
        let mut current_width = 0_u16;
        let mut continuation = false;

        for span in line.spans {
            for ch in span.content.chars() {
                if current.is_empty() {
                    let prefix = if continuation { "   " } else { "" };
                    if !prefix.is_empty() {
                        push_styled_text(&mut current, prefix, Style::default().fg(CODEX_DIM));
                        current_width = current_width.saturating_add(3);
                    }
                }

                let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
                if current_width > 0 && ch_width > 0 && current_width + ch_width > width {
                    wrapped.push(Line::from(std::mem::take(&mut current)));
                    current_width = 0;
                    continuation = true;
                    push_styled_text(&mut current, "   ", Style::default().fg(CODEX_DIM));
                    current_width = current_width.saturating_add(3);
                }
                push_styled_char(&mut current, ch, span.style);
                current_width = current_width.saturating_add(ch_width);
            }
        }
        wrapped.push(Line::from(current));
    }
    if wrapped.is_empty() {
        vec![Line::from("")]
    } else {
        wrapped
    }
}

fn push_styled_text(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.style == style {
            last.content.to_mut().push_str(text);
            return;
        }
    }
    spans.push(Span::styled(text.to_string(), style));
}

fn push_styled_char(spans: &mut Vec<Span<'static>>, ch: char, style: Style) {
    let mut text = String::new();
    text.push(ch);
    push_styled_text(spans, &text, style);
}

pub(super) fn composer_visual_position(text_before_cursor: &str, width: u16) -> (u16, u16) {
    let width = width.max(4);
    let mut row = 0_u16;
    let mut col = 3_u16;
    for ch in text_before_cursor.chars() {
        if ch == '\n' {
            row = row.saturating_add(1);
            col = 3;
            continue;
        }
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if ch_width > 0 && col + ch_width > width {
            row = row.saturating_add(1);
            col = 3;
        }
        col = col.saturating_add(ch_width);
    }
    (row, col)
}

pub(super) fn expand_tabs(text: &str) -> String {
    if !text.contains('\t') {
        return text.to_string();
    }
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '\t' {
            output.push_str("    ");
        } else {
            output.push(ch);
        }
    }
    output
}

pub(super) fn previous_char_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .char_indices()
        .last()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

pub(super) fn next_char_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .char_indices()
        .nth(1)
        .map(|(index, _)| cursor + index)
        .unwrap_or(text.len())
}

pub(super) fn input_line_ranges(text: &str) -> Vec<(usize, usize)> {
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

pub(super) fn byte_index_for_char_offset(
    text: &str,
    start: usize,
    end: usize,
    desired_chars: usize,
) -> usize {
    text[start..end]
        .char_indices()
        .nth(desired_chars)
        .map(|(index, _)| start + index)
        .unwrap_or(end)
}
