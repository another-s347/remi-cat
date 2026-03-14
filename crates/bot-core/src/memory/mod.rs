//! 3-tier memory system for 7×24 operation.
//!
//! ## Tiers
//!
//! | Tier       | Storage               | Context slot              |
//! |------------|-----------------------|---------------------------|
//! | Short-term | `short_term.jsonl`    | Raw messages in history   |
//! | Mid-term   | `mid_term/<uuid>.md`  | One summarised index msg  |
//! | Long-term  | `long_term/<uuid>.md` | One summarised index msg  |
//!
//! ## Injection order (every turn)
//!
//! ```text
//! [System: Agent.md]      (if file exists)
//! [System: Soul.md]       (if file exists)
//! [System: current DM user]   (single-chat only; injected per turn)
//! [System: LONG-TERM index]  (if any entries)
//! [System: MID-TERM index]   (if any entries)
//! ... short-term messages ...
//! [User: new message]
//! ```
//!
//! ## Raw archive preservation
//!
//! Before any compression or promotion, the original messages are written to:
//! - `mid_term/raw/<uuid>/<timestamp>.jsonl`
//! - `long_term/raw/<long_uuid>/<orig_mid_uuid>/<timestamp>.jsonl`
//!
//! These files are never loaded into context but are accessible for future
//! filesystem-mount tools.

pub mod blob;
pub mod compress;
pub mod store;
pub mod tier;
pub mod tool;

pub use compress::LlmCompressor;
pub use store::{MemoryContext, MemoryStore};
pub use tier::{build_tier_msg, make_preview, MemoryEntry, MemoryIndex};
pub use tool::MemoryGetDetailTool;

use remi_agentloop::prelude::Message;

/// Build the full injected message list from a loaded `MemoryContext`.
///
/// Returns messages in the canonical order:
/// `agent_md → soul_md → long_term index → mid_term index → short_term messages`
///
/// The length of the returned `Vec` is used by `CatBot::stream()` to strip
/// these injected messages before saving the new turn to disk.
pub fn build_injected_history(ctx: &MemoryContext) -> Vec<Message> {
    let mut msgs: Vec<Message> = Vec::new();

    if let Some(text) = &ctx.agent_md {
        msgs.push(Message::system(text.clone()));
    }
    if let Some(text) = &ctx.soul_md {
        msgs.push(Message::system(text.clone()));
    }
    if !ctx.long_term.entries.is_empty() {
        msgs.push(build_tier_msg("LONG-TERM", &ctx.long_term.entries));
    }
    if !ctx.mid_term.entries.is_empty() {
        msgs.push(build_tier_msg("MID-TERM", &ctx.mid_term.entries));
    }
    msgs.extend(ctx.short_term.clone());
    msgs
}
