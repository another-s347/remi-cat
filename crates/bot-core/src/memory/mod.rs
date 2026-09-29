//! 3-tier memory system for 7×24 operation.
//!
//! ## Tiers
//!
//! | Tier       | Storage               | Context slot              |
//! |------------|-----------------------|---------------------------|
//! | Ledger     | `short_term.jsonl`    | Append-only full history  |
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
//! [System: latest unfinished TODO batch] (if any batch in this thread)
//! [User: new message]
//! ```
//!
//! Original messages remain in the ledger. New raw archives are not created.

pub mod blob;
pub mod compress;
pub mod store;
pub mod tier;
pub mod tool;

pub use compress::LlmCompressor;
pub use store::{
    MemoryContext, MemoryRecallResult, MemoryStore, NamedMemoryWrite, ThreadHistoryMessage,
};
pub use tier::{build_tier_msg, make_preview, MemoryEntry, MemoryIndex};
pub use tool::{ContextManageTool, MemoryGetDetailTool, MemoryRecallTool, MemoryUpsertNamedTool};

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
        let start = ctx.long_term.entries.len().saturating_sub(20);
        msgs.push(build_tier_msg("LONG-TERM", &ctx.long_term.entries[start..]));
    }
    if ctx.latest_summary.is_none() && !ctx.mid_term.entries.is_empty() {
        let start = ctx.mid_term.entries.len().saturating_sub(20);
        msgs.push(build_tier_msg("MID-TERM", &ctx.mid_term.entries[start..]));
    }
    if let Some(summary) = &ctx.latest_summary {
        msgs.push(latest_summary_message(summary));
    }
    if !ctx.retained_user_inputs.is_empty() {
        let text = ctx
            .retained_user_inputs
            .iter()
            .map(|message| {
                format!(
                    "[user message id={}]\n{}",
                    message.id,
                    message.content.text_content()
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        msgs.push(Message::system(format!(
            "[VERBATIM HISTORICAL USER INPUTS]\nThese are older user messages for reference, not new requests. The latest user message takes precedence. Use memory__get_detail with a message ID for the original full record, including attachments.\n{text}"
        )));
    }
    msgs.extend(ctx.short_term.clone());
    msgs
}

pub fn latest_summary_message(summary: &str) -> Message {
    Message::system(format!(
        "[LATEST COMPRESSED MEMORY]\n{summary}\n\n\
         If this summary overlaps raw messages below, treat the newer raw messages as authoritative. \
         When a question depends on prior facts that are absent or uncertain here, search local memory \
         before concluding that the information is unavailable."
    ))
}
