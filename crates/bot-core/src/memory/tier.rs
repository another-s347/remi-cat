//! Memory tier types: `MemoryEntry`, `MemoryIndex`, and context message builders.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use remi_agentloop::prelude::Message;

// ── MemoryEntry / MemoryIndex ─────────────────────────────────────────────────

/// One compressed memory block stored in mid-term or long-term storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub uuid: String,
    pub created_at: DateTime<Utc>,
    /// Single-line preview shown in the injected context index message.
    pub preview: String,
}

/// The `index.json` file for one memory tier.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryIndex {
    pub entries: Vec<MemoryEntry>,
}

impl MemoryIndex {
    pub fn from_json(s: &str) -> Self {
        serde_json::from_str(s).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| r#"{"entries":[]}"#.to_string())
    }
}

// ── Context message builders ──────────────────────────────────────────────────

/// Build a system message that lists all entries in one memory tier.
///
/// The agent can call `memory__get_detail { uuid }` to read the full block.
pub fn build_tier_msg(tier: &str, entries: &[MemoryEntry]) -> Message {
    let rows: String = entries
        .iter()
        .map(|e| {
            format!(
                "  [{}]  {}  —  {}",
                e.uuid,
                e.created_at.format("%Y-%m-%d"),
                e.preview,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    Message::system(format!(
        "[{tier} MEMORY]\n\
         Use memory__get_detail{{uuid}} to read the full content of any block.\n\
         {rows}"
    ))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Take the first non-empty line and truncate to `max_chars`.
pub fn make_preview(text: &str, max_chars: usize) -> String {
    text.trim()
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(max_chars)
        .collect()
}
