pub(crate) fn sanitize_tui_text(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => skip_escape_sequence(&mut chars),
            '\u{0090}' | '\u{0098}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => {
                skip_string_control(&mut chars)
            }
            '\u{009b}' => skip_csi(&mut chars),
            '\n' | '\t' => output.push(ch),
            '\r' => output.push('\n'),
            _ if is_unsafe_control(ch) || is_bidi_control(ch) => {}
            _ => output.push(ch),
        }
    }
    output
}

#[cfg(test)]
pub(crate) fn contains_tui_control(input: &str) -> bool {
    input.chars().any(|ch| {
        ch == '\x1b'
            || ch == '\r'
            || is_unsafe_control(ch)
            || is_bidi_control(ch)
            || matches!(
                ch,
                '\u{0090}' | '\u{0098}' | '\u{009b}' | '\u{009d}' | '\u{009e}' | '\u{009f}'
            )
    })
}

fn skip_escape_sequence<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    let Some(ch) = chars.next() else {
        return;
    };
    match ch {
        '[' => skip_csi(chars),
        ']' | 'P' | '^' | '_' | 'X' => skip_string_control(chars),
        '(' | ')' | '*' | '+' | '-' | '.' | '/' => {
            let _ = chars.next();
        }
        _ => {}
    }
}

fn skip_csi<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    for ch in chars.by_ref() {
        if ('\u{0040}'..='\u{007e}').contains(&ch) {
            break;
        }
    }
}

fn skip_string_control<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    while let Some(ch) = chars.next() {
        if ch == '\u{0007}' || matches!(ch, '\u{009c}') {
            break;
        }
        if ch == '\x1b' && chars.peek().is_some_and(|next| *next == '\\') {
            let _ = chars.next();
            break;
        }
    }
}

fn is_unsafe_control(ch: char) -> bool {
    (ch.is_control() && !matches!(ch, '\n' | '\t' | '\r'))
        || ('\u{0080}'..='\u{009f}').contains(&ch)
}

fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_csi_sequences() {
        assert_eq!(sanitize_tui_text("a\x1b[31mred\x1b[0mz"), "aredz");
        assert_eq!(sanitize_tui_text("a\x1b[?7lb"), "ab");
    }

    #[test]
    fn strips_osc_and_string_controls() {
        assert_eq!(sanitize_tui_text("a\x1b]0;bad\x07b"), "ab");
        assert_eq!(
            sanitize_tui_text("a\x1b]8;;https://x\x1b\\link\x1b]8;;\x1b\\b"),
            "alinkb"
        );
        assert_eq!(sanitize_tui_text("a\x1bPpayload\x1b\\b"), "ab");
    }

    #[test]
    fn strips_control_and_bidi_chars_but_preserves_text_layout_chars() {
        assert_eq!(sanitize_tui_text("a\rb\x08c\x07d"), "a\nbcd");
        assert_eq!(sanitize_tui_text("a\u{202e}b\u{2069}c"), "abc");
        assert_eq!(sanitize_tui_text("你\t好\n🙂"), "你\t好\n🙂");
    }

    #[test]
    fn detects_controls_without_flagging_plain_text() {
        assert!(contains_tui_control("\x1b[31mred"));
        assert!(contains_tui_control("a\rb"));
        assert!(contains_tui_control("a\u{202e}b"));
        assert!(!contains_tui_control("plain\n中文\t🙂"));
    }
}
