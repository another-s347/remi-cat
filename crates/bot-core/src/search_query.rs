use std::collections::HashSet;

use regex::Regex;

#[derive(Debug, Clone)]
pub(crate) struct TokenizedSearchQuery {
    terms: Vec<String>,
    ripgrep_query: String,
    regex: Regex,
}

impl TokenizedSearchQuery {
    pub(crate) fn terms(&self) -> &[String] {
        &self.terms
    }

    pub(crate) fn ripgrep_query(&self) -> &str {
        &self.ripgrep_query
    }

    pub(crate) fn regex(&self) -> &Regex {
        &self.regex
    }
}

pub(crate) fn tokenized_search_query(query: &str) -> Option<TokenizedSearchQuery> {
    let terms = tokenize_search_query(query);
    let ripgrep_query = ripgrep_query_from_terms(&terms)?;
    let regex = Regex::new(&ripgrep_query).ok()?;
    Some(TokenizedSearchQuery {
        terms,
        ripgrep_query,
        regex,
    })
}

pub(crate) fn tokenize_search_query(query: &str) -> Vec<String> {
    #[derive(Clone, Copy, Eq, PartialEq)]
    enum Mode {
        Ascii,
        Cjk,
        Other,
    }

    let mut seen = HashSet::new();
    let mut terms = Vec::new();
    let mut buf = String::new();
    let mut mode: Option<Mode> = None;

    let flush = |buf: &mut String,
                 mode: &mut Option<Mode>,
                 terms: &mut Vec<String>,
                 seen: &mut HashSet<String>| {
        if buf.is_empty() {
            *mode = None;
            return;
        }
        match mode.take() {
            Some(Mode::Ascii) => push_ascii_terms(buf, terms, seen),
            Some(Mode::Cjk) => push_cjk_terms(buf, terms, seen),
            Some(Mode::Other) => push_generic_term(buf, terms, seen),
            None => {}
        }
        buf.clear();
    };

    for ch in query.chars() {
        let next_mode = if is_cjk_like(ch) {
            Some(Mode::Cjk)
        } else if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            Some(Mode::Ascii)
        } else if ch.is_alphanumeric() {
            Some(Mode::Other)
        } else {
            None
        };

        match (mode, next_mode) {
            (_, None) => flush(&mut buf, &mut mode, &mut terms, &mut seen),
            (Some(current), Some(next)) if current != next => {
                flush(&mut buf, &mut mode, &mut terms, &mut seen);
                buf.push(ch);
                mode = Some(next);
            }
            (_, Some(next)) => {
                buf.push(ch);
                mode = Some(next);
            }
        }
    }
    flush(&mut buf, &mut mode, &mut terms, &mut seen);

    terms
}

pub(crate) fn ripgrep_query_from_terms(terms: &[String]) -> Option<String> {
    if terms.is_empty() {
        return None;
    }
    let mut escaped = terms
        .iter()
        .map(|term| regex::escape(term))
        .collect::<Vec<_>>();
    escaped.sort_by(|a, b| {
        b.chars()
            .count()
            .cmp(&a.chars().count())
            .then_with(|| a.cmp(b))
    });
    let body = if escaped.len() == 1 {
        escaped.remove(0)
    } else {
        format!("(?:{})", escaped.join("|"))
    };
    Some(format!("(?i:{body})"))
}

fn push_ascii_terms(raw: &str, terms: &mut Vec<String>, seen: &mut HashSet<String>) {
    let normalized = raw
        .trim_matches(|ch| ch == '-' || ch == '_')
        .to_ascii_lowercase();
    push_ascii_candidate(&normalized, terms, seen);
    for part in normalized.split(['-', '_']) {
        push_ascii_candidate(part, terms, seen);
    }
}

fn push_ascii_candidate(candidate: &str, terms: &mut Vec<String>, seen: &mut HashSet<String>) {
    if candidate.len() >= 2 && !is_ascii_stopword(candidate) && seen.insert(candidate.to_string()) {
        terms.push(candidate.to_string());
    }
}

fn push_cjk_terms(raw: &str, terms: &mut Vec<String>, seen: &mut HashSet<String>) {
    let chars = raw.chars().collect::<Vec<_>>();
    match chars.len() {
        0 => {}
        1 => push_term(chars[0].to_string(), terms, seen),
        2 => push_term(chars.iter().collect::<String>(), terms, seen),
        _ => {
            for width in [3usize, 2] {
                if chars.len() < width {
                    continue;
                }
                for window in chars.windows(width) {
                    push_term(window.iter().collect::<String>(), terms, seen);
                }
            }
        }
    }
}

fn push_generic_term(raw: &str, terms: &mut Vec<String>, seen: &mut HashSet<String>) {
    let normalized = raw.to_lowercase();
    if normalized.chars().count() >= 2 {
        push_term(normalized, terms, seen);
    }
}

fn push_term(term: String, terms: &mut Vec<String>, seen: &mut HashSet<String>) {
    if !term.is_empty() && seen.insert(term.clone()) {
        terms.push(term);
    }
}

fn is_cjk_like(ch: char) -> bool {
    matches!(
        ch,
        '\u{3400}'..='\u{4dbf}'
            | '\u{4e00}'..='\u{9fff}'
            | '\u{f900}'..='\u{faff}'
            | '\u{20000}'..='\u{2a6df}'
            | '\u{2a700}'..='\u{2b73f}'
            | '\u{2b740}'..='\u{2b81f}'
            | '\u{2b820}'..='\u{2ceaf}'
            | '\u{3040}'..='\u{30ff}'
            | '\u{ac00}'..='\u{d7af}'
    )
}

fn is_ascii_stopword(term: &str) -> bool {
    matches!(
        term,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "by"
            | "can"
            | "could"
            | "did"
            | "do"
            | "does"
            | "for"
            | "from"
            | "had"
            | "has"
            | "have"
            | "how"
            | "i"
            | "in"
            | "is"
            | "it"
            | "me"
            | "my"
            | "of"
            | "on"
            | "or"
            | "that"
            | "the"
            | "this"
            | "to"
            | "was"
            | "were"
            | "what"
            | "when"
            | "where"
            | "which"
            | "who"
            | "why"
            | "with"
            | "you"
            | "your"
    )
}

#[cfg(test)]
mod tests {
    use super::{ripgrep_query_from_terms, tokenize_search_query, tokenized_search_query};

    #[test]
    fn tokenizer_filters_stopwords_and_keeps_keywords() {
        assert_eq!(
            tokenize_search_query("How long is my daily commute to work?"),
            vec!["long", "daily", "commute", "work"]
        );
    }

    #[test]
    fn tokenizer_generates_cjk_ngrams_for_natural_language_queries() {
        let terms = tokenize_search_query("我想找靠窗座位");
        assert!(terms.contains(&"靠窗".to_string()));
        assert!(terms.contains(&"座位".to_string()));
    }

    #[test]
    fn ripgrep_query_is_case_insensitive_escaped_alternation() {
        let query = ripgrep_query_from_terms(&["foo.bar".to_string(), "baz".to_string()])
            .expect("terms should produce query");
        assert_eq!(query, "(?i:(?:foo\\.bar|baz))");
    }

    #[test]
    fn tokenized_query_regex_matches_expected_text() {
        let query = tokenized_search_query("我想找靠窗座位").unwrap();
        assert!(query.regex().is_match("Istanbul 靠窗 seat"));
        assert!(query.ripgrep_query().contains("靠窗"));
    }
}
