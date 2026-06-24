use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use aho_corasick::{AhoCorasick, MatchKind};

/// Redacts secret values from tool output text using Aho-Corasick multi-pattern
/// search.  Thread-safe; intended to be held behind `Arc<RwLock<_>>` and rebuilt
/// when the secret set changes.
pub struct SecretRedactor {
    /// Built automaton, `None` when there are no secrets.
    ac: Option<AhoCorasick>,
    /// Replacement labels — index-aligned with the AhoCorasick patterns.
    labels: Vec<String>,
}

impl SecretRedactor {
    /// Construct a redactor with no patterns (identity transform).
    pub fn empty() -> Self {
        Self {
            ac: None,
            labels: Vec::new(),
        }
    }

    /// (Re-)build from a `key → value` map.
    ///
    /// Only non-empty values are included; values shorter than 4 bytes are
    /// skipped to avoid spurious matches of very short strings.
    pub fn from_entries(entries: &HashMap<String, String>) -> Self {
        let mut patterns: Vec<&str> = Vec::new();
        let mut labels: Vec<String> = Vec::new();

        for (key, val) in entries {
            if val.len() < 4 {
                continue;
            }
            patterns.push(val);
            labels.push(key.clone());
        }

        if patterns.is_empty() {
            return Self::empty();
        }

        let ac = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(patterns)
            .expect("AhoCorasick build failed");

        Self {
            ac: Some(ac),
            labels,
        }
    }

    /// Replace all secret values in `text` with `[REDACTED:<KEY>]`.
    /// Returns the original string unchanged when no secrets are configured.
    pub fn redact(&self, text: &str) -> String {
        let Some(ac) = &self.ac else {
            return text.to_string();
        };

        let mut result = String::with_capacity(text.len());
        let mut last = 0usize;

        for m in ac.find_iter(text) {
            result.push_str(&text[last..m.start()]);
            result.push_str("[REDACTED:");
            result.push_str(&self.labels[m.pattern().as_usize()]);
            result.push(']');
            last = m.end();
        }
        result.push_str(&text[last..]);
        result
    }
}

/// Convenience alias used by `CatBot`.
pub type SharedRedactor = Arc<RwLock<SecretRedactor>>;
