//! Filesystem-backed memory store with three tiers.
//!
//! Directory layout under `<data_dir>/memory/<thread_id>/`:
//!
//! ```text
//! short_term.jsonl          ← raw recent messages, one JSON per line
//! mid_term/
//!   index.json              ← [{uuid, created_at, preview}]
//!   <uuid>.md               ← compressed summary
//!   raw/
//!     <uuid>/               ← original messages before compression
//!       <RFC3339>.jsonl
//! long_term/
//!   index.json
//!   <uuid>.md               ← compressed summary
//!   raw/
//!     <long_uuid>/          ← merged raw archives from promoted mid-term entries
//!       <orig_mid_uuid>/
//!         <RFC3339>.jsonl
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use remi_agentloop::prelude::{AgentError, Message, Role};
use uuid::Uuid;

use super::compress::LlmCompressor;
use super::tier::{make_preview, MemoryEntry, MemoryIndex};

// ── MemoryContext ─────────────────────────────────────────────────────────────

/// Loaded context for a single thread, ready for injection into the agent.
pub struct MemoryContext {
    pub agent_md: Option<String>,
    pub soul_md: Option<String>,
    pub long_term: MemoryIndex,
    pub mid_term: MemoryIndex,
    pub short_term: Vec<Message>,
    /// Persisted tool-managed state (todos, etc.) restored from disk.
    pub user_state: serde_json::Value,
}

// ── MemoryStore ───────────────────────────────────────────────────────────────

pub struct MemoryStore {
    /// Root data dir (e.g. `.remi-cat`).  Agent.md / Soul.md are read from here.
    pub data_dir: PathBuf,
    pub compressor: LlmCompressor,
    /// Short-term token budget; overflow triggers compression to mid-term.
    pub short_term_tokens: usize,
    /// Mid-term entries older than this many days are promoted to long-term.
    pub memory_days: u64,
}

// ── Token estimation ──────────────────────────────────────────────────────────

fn token_estimate(msgs: &[Message]) -> usize {
    msgs.iter()
        .map(|m| m.content.text_content().len() / 4 + 10)
        .sum()
}

// ── Path helpers ──────────────────────────────────────────────────────────────

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl MemoryStore {
    fn thread_dir(&self, thread_id: &str) -> PathBuf {
        self.data_dir.join("memory").join(sanitize_id(thread_id))
    }

    fn short_term_path(&self, thread_id: &str) -> PathBuf {
        self.thread_dir(thread_id).join("short_term.jsonl")
    }

    fn user_state_path(&self, thread_id: &str) -> PathBuf {
        self.thread_dir(thread_id).join("user_state.json")
    }

    fn mid_term_dir(&self, thread_id: &str) -> PathBuf {
        self.thread_dir(thread_id).join("mid_term")
    }

    fn long_term_dir(&self, thread_id: &str) -> PathBuf {
        self.thread_dir(thread_id).join("long_term")
    }

    // ── Index I/O ─────────────────────────────────────────────────────────────

    async fn read_index(dir: &PathBuf) -> MemoryIndex {
        let path = dir.join("index.json");
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => MemoryIndex::from_json(&s),
            Err(_) => MemoryIndex::default(),
        }
    }

    async fn write_index(dir: &PathBuf, idx: &MemoryIndex) -> Result<(), AgentError> {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        let path = dir.join("index.json");
        tokio::fs::write(&path, idx.to_json())
            .await
            .map_err(|e| AgentError::Io(e.to_string()))
    }

    // ── Short-term JSONL I/O ──────────────────────────────────────────────────

    async fn read_short_term(path: &PathBuf) -> Vec<Message> {
        let text = match tokio::fs::read_to_string(path).await {
            Ok(t) => t,
            Err(_) => return vec![],
        };
        let blobs_dir = path
            .parent()
            .map(|p| p.join("blobs"))
            .unwrap_or_else(|| PathBuf::from("blobs"));
        let text = super::blob::restore_blobs(&text, &blobs_dir).await;
        let msgs: Vec<Message> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        // Guard: drop any leading Tool/Assistant messages (orphaned tool results
        // from a bad compression boundary).  The history passed to the API must
        // always start with a User or System message, otherwise the API returns
        // 400 "tool_call_id is not found".
        let start = msgs
            .iter()
            .position(|m| matches!(m.role, Role::User | Role::System))
            .unwrap_or(msgs.len());
        if start > 0 {
            tracing::warn!(
                "short_term starts with {} orphaned non-user message(s); dropping them",
                start
            );
        }
        msgs[start..].to_vec()
    }

    async fn write_short_term(path: &PathBuf, msgs: &[Message]) -> Result<(), AgentError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| AgentError::Io(e.to_string()))?;
        }
        tracing::debug!(
            path = %path.display(),
            count = msgs.len(),
            with_metadata = msgs.iter().filter(|m| m.metadata.is_some()).count(),
            "write_short_term: serializing messages"
        );
        let lines: String = msgs
            .iter()
            .filter_map(|m| {
                let json = serde_json::to_string(m).ok();
                tracing::debug!(role = ?m.role, has_metadata = m.metadata.is_some(), serialized_has_metadata = json.as_deref().map(|s| s.contains("\"metadata\"")).unwrap_or(false), "write_short_term: message");
                json
            })
            .map(|l| l + "\n")
            .collect();
        let blobs_dir = path
            .parent()
            .map(|p| p.join("blobs"))
            .unwrap_or_else(|| PathBuf::from("blobs"));
        let lines = super::blob::extract_blobs(&lines, &blobs_dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        tokio::fs::write(path, lines)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))
    }

    // ── Raw archive ───────────────────────────────────────────────────────────

    /// Write raw messages to `<tier_dir>/raw/<uuid>/<timestamp>.jsonl`
    /// **before** any compression is performed.
    async fn archive_raw(
        tier_dir: &PathBuf,
        uuid: &str,
        msgs: &[Message],
    ) -> Result<(), AgentError> {
        let raw_dir = tier_dir.join("raw").join(uuid);
        tokio::fs::create_dir_all(&raw_dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let path = raw_dir.join(format!("{ts}.jsonl"));
        let lines: String = msgs
            .iter()
            .filter_map(|m| serde_json::to_string(m).ok())
            .map(|l| l + "\n")
            .collect();
        // tier_dir is e.g. thread_dir/mid_term — blobs live in thread_dir/blobs
        let blobs_dir = tier_dir
            .parent()
            .map(|p| p.join("blobs"))
            .unwrap_or_else(|| tier_dir.join("blobs"));
        let lines = super::blob::extract_blobs(&lines, &blobs_dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        tokio::fs::write(&path, lines)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))
    }

    // ── Compression: short-term → mid-term ───────────────────────────────────

    /// Compress the oldest ~50 % of short-term messages into one mid-term block.
    /// Returns the remaining (newer) short-term messages to keep.
    async fn compress_to_mid_term(
        &self,
        thread_id: &str,
        msgs: Vec<Message>,
    ) -> Result<Vec<Message>, AgentError> {
        let desired = (msgs.len() / 2).max(1);
        let split = safe_split_point(&msgs, desired);
        let (oldest, remaining) = msgs.split_at(split);
        let oldest = oldest.to_vec();
        let remaining = remaining.to_vec();

        let new_uuid = Uuid::new_v4().to_string();
        let mid_dir = self.mid_term_dir(thread_id);

        // 1. Archive raw BEFORE compressing.
        Self::archive_raw(&mid_dir, &new_uuid, &oldest).await?;

        // 2. Compress.
        let summary = self.compressor.compress(&oldest).await?;
        let preview = make_preview(&summary, 100);

        // 3. Write summary with timestamp header.
        tokio::fs::create_dir_all(&mid_dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        let md_path = mid_dir.join(format!("{new_uuid}.md"));
        let ts_header = format!("<!-- created: {} -->\n\n", Utc::now().to_rfc3339());
        let summary_with_ts = format!("{ts_header}{summary}");
        tokio::fs::write(&md_path, &summary_with_ts)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;

        // 4. Update index.
        let mut idx = Self::read_index(&mid_dir).await;
        idx.entries.push(MemoryEntry {
            uuid: new_uuid,
            created_at: Utc::now(),
            preview,
        });
        Self::write_index(&mid_dir, &idx).await?;

        Ok(remaining)
    }

    // ── Promotion: mid-term → long-term ──────────────────────────────────────

    /// Promote mid-term entries older than `memory_days` into a single long-term block.
    async fn maybe_promote(&self, thread_id: &str) -> Result<(), AgentError> {
        let mid_dir = self.mid_term_dir(thread_id);
        let long_dir = self.long_term_dir(thread_id);

        let mut mid_idx = Self::read_index(&mid_dir).await;
        if mid_idx.entries.is_empty() {
            return Ok(());
        }

        let cutoff = Utc::now() - chrono::Duration::days(self.memory_days as i64);
        let (to_promote, keep): (Vec<_>, Vec<_>) = mid_idx
            .entries
            .drain(..)
            .partition(|e| e.created_at < cutoff);

        if to_promote.is_empty() {
            mid_idx.entries = keep;
            return Ok(());
        }

        // Collect summaries from all entries being promoted (mid-term).
        let mut promoting_text = String::new();
        for entry in &to_promote {
            let md_path = mid_dir.join(format!("{}.md", entry.uuid));
            if let Ok(text) = tokio::fs::read_to_string(&md_path).await {
                if !promoting_text.is_empty() {
                    promoting_text.push_str("\n\n---\n\n");
                }
                promoting_text.push_str(&text);
            }
        }

        // Collect ALL existing long-term summaries to merge with.
        let lt_idx_existing = Self::read_index(&long_dir).await;
        let mut existing_long_text = String::new();
        for entry in &lt_idx_existing.entries {
            let md_path = long_dir.join(format!("{}.md", entry.uuid));
            if let Ok(text) = tokio::fs::read_to_string(&md_path).await {
                if !existing_long_text.is_empty() {
                    existing_long_text.push_str("\n\n---\n\n");
                }
                existing_long_text.push_str(&text);
            }
        }

        // Build compress input: existing long-term (as context) + incoming mid-term.
        let mut compress_input = String::new();
        if !existing_long_text.is_empty() {
            compress_input.push_str("[已有长期记忆摘要]\n");
            compress_input.push_str(&existing_long_text);
            compress_input.push_str("\n\n---\n\n[待合并的中期记忆]\n");
        }
        compress_input.push_str(&promoting_text);

        // Always re-compress so existing long-term gets merged in.
        let long_summary = {
            let msgs = vec![Message::user(compress_input)];
            self.compressor.compress(&msgs).await?
        };

        let long_uuid = Uuid::new_v4().to_string();
        let preview = make_preview(&long_summary, 100);

        tokio::fs::create_dir_all(&long_dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;

        // Move each mid-term raw archive into long_term/raw/<long_uuid>/<orig_uuid>/.
        for entry in &to_promote {
            let src_raw = mid_dir.join("raw").join(&entry.uuid);
            if src_raw.exists() {
                let dst_raw = long_dir.join("raw").join(&long_uuid).join(&entry.uuid);
                if let Some(p) = dst_raw.parent() {
                    tokio::fs::create_dir_all(p)
                        .await
                        .map_err(|e| AgentError::Io(e.to_string()))?;
                }
                move_dir(&src_raw, &dst_raw).await?;
            }
            // Remove the mid-term summary file.
            let _ = tokio::fs::remove_file(mid_dir.join(format!("{}.md", entry.uuid))).await;
        }

        // Remove old long-term summary files (they are merged into the new one).
        for entry in &lt_idx_existing.entries {
            let _ = tokio::fs::remove_file(long_dir.join(format!("{}.md", entry.uuid))).await;
        }

        // Write merged long-term summary with timestamp header.
        let lt_path = long_dir.join(format!("{long_uuid}.md"));
        let ts_header = format!("<!-- created: {} -->\n\n", Utc::now().to_rfc3339());
        tokio::fs::write(&lt_path, format!("{ts_header}{long_summary}"))
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;

        // Replace long-term index with single new entry.
        let new_lt_idx = MemoryIndex {
            entries: vec![MemoryEntry {
                uuid: long_uuid,
                created_at: Utc::now(),
                preview,
            }],
        };
        Self::write_index(&long_dir, &new_lt_idx).await?;

        // Update mid-term index (remove promoted entries).
        mid_idx.entries = keep;
        Self::write_index(&mid_dir, &mid_idx).await?;

        Ok(())
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Load the full memory context for a thread.
    ///
    /// This also runs `maybe_promote` to age out stale mid-term entries.
    /// Promotion failures are non-fatal.
    pub async fn load_context(&self, thread_id: &str) -> Result<MemoryContext, AgentError> {
        // Attempt promotion first; ignore errors so a failing LLM call
        // does not block the main turn.
        let _ = self.maybe_promote(thread_id).await;

        let agent_md = read_optional_file(&self.data_dir.join("Agent.md")).await;
        let soul_md = read_optional_file(&self.data_dir.join("Soul.md")).await;
        let long_term = Self::read_index(&self.long_term_dir(thread_id)).await;
        let mid_term = Self::read_index(&self.mid_term_dir(thread_id)).await;
        let short_term = Self::read_short_term(&self.short_term_path(thread_id)).await;
        let user_state = self.load_user_state(thread_id).await;

        Ok(MemoryContext {
            agent_md,
            soul_md,
            long_term,
            mid_term,
            short_term,
            user_state,
        })
    }

    /// Append new messages from a turn to short-term storage.
    ///
    /// If the total token estimate exceeds `short_term_tokens`, the oldest
    /// chunk is compressed into a new mid-term block.  If compression fails,
    /// the oldest messages are simply dropped so the save never aborts.
    pub async fn save_turn(
        &self,
        thread_id: &str,
        mut new_msgs: Vec<Message>,
    ) -> Result<(), AgentError> {
        let short_path = self.short_term_path(thread_id);
        let mut all_msgs = Self::read_short_term(&short_path).await;

        // Attach turn timestamp to the first user message's metadata.
        let ts = Utc::now().to_rfc3339();
        if let Some(msg) = new_msgs.iter_mut().find(|m| matches!(m.role, Role::User)) {
            let meta = msg
                .metadata
                .get_or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let serde_json::Value::Object(map) = meta {
                map.entry("timestamp")
                    .or_insert(serde_json::Value::String(ts));
            }
        }
        all_msgs.append(&mut new_msgs);

        while token_estimate(&all_msgs) > self.short_term_tokens && all_msgs.len() > 1 {
            match self.compress_to_mid_term(thread_id, all_msgs.clone()).await {
                Ok(remaining) => {
                    all_msgs = remaining;
                }
                Err(e) => {
                    // Compression failed (e.g. LLM unavailable or empty response).
                    // Fall back to dropping the oldest half so we can still save.
                    tracing::warn!("compress_to_mid_term failed, dropping oldest messages: {e:#}");
                    let drop_n = (all_msgs.len() / 2).max(1);
                    all_msgs.drain(..drop_n);
                }
            }
        }

        Self::write_short_term(&short_path, &all_msgs).await
    }

    /// Persist tool-managed user_state (todos, etc.) to disk.
    pub async fn save_user_state(
        &self,
        thread_id: &str,
        user_state: &serde_json::Value,
    ) -> Result<(), AgentError> {
        // Only save when there's actual state (not null/empty object).
        if user_state.is_null() {
            return Ok(());
        }
        let path = self.user_state_path(thread_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| AgentError::Io(e.to_string()))?;
        }
        let json = serde_json::to_string_pretty(user_state)
            .map_err(|e| AgentError::other(format!("user_state serialise: {e}")))?;
        tokio::fs::write(&path, json)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))
    }

    async fn load_user_state(&self, thread_id: &str) -> serde_json::Value {
        let path = self.user_state_path(thread_id);
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => serde_json::from_str(&s).unwrap_or(serde_json::Value::Null),
            Err(_) => serde_json::Value::Null,
        }
    }

    /// Immediately compress **all** current short-term messages **and** all
    /// existing mid-term summaries into one single new mid-term block.
    ///
    /// - All existing mid-term `.md` files are read, merged, and re-compressed
    ///   together with the current short-term log.
    /// - The mid-term index is replaced with the single new entry.
    /// - Short-term is cleared.
    ///
    /// Returns the number of short-term messages that were included, or `0`
    /// if both short-term and mid-term were already empty.
    pub async fn compact_now(&self, thread_id: &str) -> Result<usize, AgentError> {
        let short_path = self.short_term_path(thread_id);
        let mid_dir = self.mid_term_dir(thread_id);

        let short_msgs = Self::read_short_term(&short_path).await;
        let short_count = short_msgs.len();

        // ── Collect existing mid-term summary texts ───────────────────────
        let mid_idx = Self::read_index(&mid_dir).await;
        let mut combined_text = String::new();
        for entry in &mid_idx.entries {
            let md_path = mid_dir.join(format!("{}.md", entry.uuid));
            if let Ok(text) = tokio::fs::read_to_string(&md_path).await {
                if !combined_text.is_empty() {
                    combined_text.push_str("\n\n---\n\n");
                }
                combined_text.push_str(&text);
            }
        }

        if short_count == 0 && combined_text.is_empty() {
            return Ok(0);
        }

        // ── Build input: existing mid-term summaries (as a system context)
        //    followed by the short-term messages ────────────────────────────
        let mut compress_msgs: Vec<remi_agentloop::prelude::Message> = Vec::new();
        if !combined_text.is_empty() {
            compress_msgs.push(remi_agentloop::prelude::Message::system(format!(
                "[已有中期记忆摘要]\n{combined_text}"
            )));
        }
        compress_msgs.extend(short_msgs.iter().cloned());

        let new_uuid = Uuid::new_v4().to_string();

        // ── Archive short-term raw before compressing ─────────────────────
        if !short_msgs.is_empty() {
            Self::archive_raw(&mid_dir, &new_uuid, &short_msgs).await?;
        }

        // ── Compress ──────────────────────────────────────────────────────
        let summary = self.compressor.compress(&compress_msgs).await?;
        let preview = make_preview(&summary, 100);

        // ── Write new summary ─────────────────────────────────────────────
        tokio::fs::create_dir_all(&mid_dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        let md_path = mid_dir.join(format!("{new_uuid}.md"));
        let ts_header = format!("<!-- created: {} -->\n\n", Utc::now().to_rfc3339());
        tokio::fs::write(&md_path, format!("{ts_header}{summary}"))
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;

        // ── Remove old mid-term .md files ─────────────────────────────────
        for entry in &mid_idx.entries {
            let _ = tokio::fs::remove_file(mid_dir.join(format!("{}.md", entry.uuid))).await;
        }

        // ── Replace mid-term index with single new entry ──────────────────
        let new_idx = super::tier::MemoryIndex {
            entries: vec![MemoryEntry {
                uuid: new_uuid,
                created_at: Utc::now(),
                preview,
            }],
        };
        Self::write_index(&mid_dir, &new_idx).await?;

        // ── Clear short-term ──────────────────────────────────────────────
        Self::write_short_term(&short_path, &[]).await?;

        Ok(short_count)
    }

    /// Return the full text of a memory block by UUID.
    ///
    /// Searches mid-term first, then long-term.
    pub async fn get_detail(
        &self,
        thread_id: &str,
        uuid: &str,
    ) -> Result<Option<String>, AgentError> {
        let safe = sanitize_id(uuid);
        for tier_dir in [self.mid_term_dir(thread_id), self.long_term_dir(thread_id)] {
            let path = tier_dir.join(format!("{safe}.md"));
            if let Ok(text) = tokio::fs::read_to_string(&path).await {
                return Ok(Some(text));
            }
        }
        Ok(None)
    }
}

// ── Filesystem helpers ────────────────────────────────────────────────────────

async fn read_optional_file(path: &PathBuf) -> Option<String> {
    tokio::fs::read_to_string(path).await.ok()
}

/// Move a directory: try atomic rename, fall back to recursive copy + delete.
async fn move_dir(src: &PathBuf, dst: &PathBuf) -> Result<(), AgentError> {
    if tokio::fs::rename(src, dst).await.is_ok() {
        return Ok(());
    }
    copy_dir_all(src.clone(), dst.clone()).await?;
    tokio::fs::remove_dir_all(src)
        .await
        .map_err(|e| AgentError::Io(e.to_string()))
}

fn copy_dir_all(
    src: PathBuf,
    dst: PathBuf,
) -> futures::future::BoxFuture<'static, Result<(), AgentError>> {
    Box::pin(async move {
        tokio::fs::create_dir_all(&dst)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        let mut rd = tokio::fs::read_dir(&src)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?
        {
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            let ft = entry
                .file_type()
                .await
                .map_err(|e| AgentError::Io(e.to_string()))?;
            if ft.is_dir() {
                copy_dir_all(src_path, dst_path).await?;
            } else {
                tokio::fs::copy(&src_path, &dst_path)
                    .await
                    .map_err(|e| AgentError::Io(e.to_string()))?;
            }
        }
        Ok(())
    })
}

/// Find a safe split point for compressing short-term messages.
///
/// Starting at `desired`, walks backwards until `msgs[split]` is a `User` or
/// `System` message.  This guarantees the "remaining" half always starts at a
/// clean exchange boundary and never begins with an orphaned `Tool` result
/// (which would cause the API to reject the history with "tool_call_id not found").
fn safe_split_point(msgs: &[Message], desired: usize) -> usize {
    let mut i = desired.min(msgs.len().saturating_sub(1));
    while i > 0 {
        if matches!(msgs[i].role, Role::User | Role::System) {
            break;
        }
        i -= 1;
    }
    i.max(1)
}
