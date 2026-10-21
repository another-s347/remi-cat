//! Native mention handling; never infer a recipient from a plain name.
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

pub(super) fn incoming_mention_text(text: &str, mentions: &[im_feishu::FeishuMention]) -> String {
    let mut replacements = Vec::new();
    for mention in mentions {
        let Some(name) = mention
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        if mention.key.is_empty() {
            continue;
        }
        for (start, _) in text.match_indices(&mention.key) {
            let end = start + mention.key.len();
            if text[end..]
                .chars()
                .next()
                .is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
            {
                continue;
            }
            replacements.push((start, end, format!("@{name}")));
        }
    }
    replacements.sort_by_key(|(start, _, _)| *start);
    replacements.dedup_by_key(|(start, _, _)| *start);
    let mut rendered = text.to_string();
    for (start, end, replacement) in replacements.into_iter().rev() {
        rendered.replace_range(start..end, &replacement);
    }
    rendered
}

/// Plain prose mentions are invalid IM output, even when a name is known.
/// Code is literal content; email addresses are not recipient mentions.
pub(super) fn has_plain_mentions(text: &str) -> bool {
    let mut code_depth = 0usize;
    let mut mention_link = false;
    let mut prose = String::new();
    for event in Parser::new_ext(text, Options::all()) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => code_depth += 1,
            Event::End(TagEnd::CodeBlock) => {
                code_depth = code_depth.saturating_sub(1);
                prose.push(' ');
            }
            Event::Start(Tag::Link { dest_url, .. }) if dest_url.starts_with("remi-mention:") => {
                mention_link = true;
                if prose.ends_with('@') {
                    prose.pop();
                }
                prose.push(' ');
            }
            Event::End(TagEnd::Link) => mention_link = false,
            Event::Text(value) if code_depth == 0 && !mention_link => prose.push_str(&value),
            Event::Code(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::End(TagEnd::Paragraph) => prose.push(' '),
            _ => {}
        }
    }
    // Inspect visible prose, so emphasis or escaping cannot disguise a bare mention.
    prose.match_indices('@').any(|(start, _)| {
        let previous = prose[..start].chars().next_back();
        let next = prose[start + 1..].chars().next();
        !previous.is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '/'))
            && next.is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_mentions_are_forbidden_without_resolving_names() {
        for text in [
            "@Alice 请查收",
            "@未知用户",
            "你好，@张三",
            "你好@张三",
            "@**Alice**",
            "@all",
            "@所有人",
            r"\@Alice",
            "> @Alice",
            "[联系 @Alice](https://example.test)",
        ] {
            assert!(has_plain_mentions(text), "{text}");
        }
    }

    #[test]
    fn structured_mentions_code_and_email_are_allowed() {
        for text in [
            "@[Alice](remi-mention:u0)",
            "@[所有人](remi-mention:all)",
            "`@Alice`",
            "```text\n@张三\n```",
            "alice@example.com",
            "https://example.test/@Alice",
        ] {
            assert!(!has_plain_mentions(text), "{text}");
        }
    }

    #[test]
    fn incoming_mention_names_preserve_placeholder_boundaries() {
        let mentions = vec![im_feishu::FeishuMention {
            key: "@_user_1".into(),
            name: Some("张三".into()),
            open_id: Some("ou_zhang".into()),
            user_id: None,
        }];
        assert_eq!(
            incoming_mention_text("@_user_10 @_user_1", &mentions),
            "@_user_10 @张三"
        );
    }

    #[test]
    fn text_fallback_preserves_native_tags_and_chunk_boundaries() {
        assert_eq!(
            card_mentions_for_text("<at id=ou_alice></at> `<at id=ou_code></at>`"),
            "<at user_id=\"ou_alice\"></at> `<at id=ou_code></at>`"
        );
        let prefix = "x".repeat(7_990);
        let tag = "<at user_id=\"ou_alice\"></at>";
        let input = format!("{prefix}{tag} end");
        assert_eq!(text_chunk_end(&input, 8_000), prefix.len());
        assert_eq!(text_chunk_end(&input[prefix.len()..], 8_000), tag.len() + 4);
    }
}

/// Text messages use user_id, unlike the id attribute in card Markdown.
/// Convert only complete native tags outside Markdown code.
pub(super) fn card_mentions_for_text(text: &str) -> String {
    let mut edits = Vec::new();
    for (event, range) in Parser::new_ext(text, Options::all()).into_offset_iter() {
        if !matches!(event, Event::InlineHtml(_) | Event::Html(_)) {
            continue;
        }
        for (offset, _) in text[range.clone()].match_indices("<at id=") {
            let start = range.start + offset;
            let rest = &text[start + "<at id=".len()..];
            let Some((id, _)) = rest.split_once("></at>") else {
                continue;
            };
            let id = id.trim_matches(['\'', '"']);
            if id.is_empty()
                || !id
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
            {
                continue;
            }
            let opening_end = start + text[start..].find('>').unwrap() + 1;
            edits.push((start, opening_end, format!("<at user_id=\"{id}\">")));
        }
    }
    let mut rendered = text.to_string();
    for (start, end, value) in edits.into_iter().rev() {
        rendered.replace_range(start..end, &value);
    }
    rendered
}

pub(super) fn text_chunk_end(text: &str, max_bytes: usize) -> usize {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    for (start, _) in text.match_indices("<at user_id=\"") {
        if start >= end {
            break;
        }
        if let Some(close) = text[start..].find("</at>") {
            // Valid provider IDs produce short tags. Never split one across
            // two messages; keep malformed oversized input making progress.
            if start > 0 && start + close + "</at>".len() > end {
                return start;
            }
        }
    }
    end
}
