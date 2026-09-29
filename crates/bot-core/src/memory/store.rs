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
//! named/
//!   <agent_id>/
//!     <name>.md             ← agent-maintained named memory
//! ```

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Utc};
use remi_agentloop::prelude::{AgentError, Message, Role};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::token_usage::estimate_memory_message_tokens;
use crate::tool_pretty::{tool_success, PrettyToolCall};
use crate::ContextCompactionEvent;

use super::compress::{compression_input_text, LlmCompressor};
use super::tier::{make_preview, MemoryEntry, MemoryIndex};
use crate::search_query::{tokenized_search_query, TokenizedSearchQuery};

// ── MemoryContext ─────────────────────────────────────────────────────────────

/// Loaded context for a single thread, ready for injection into the agent.
#[derive(Clone)]
pub struct MemoryContext {
    pub agent_md: Option<String>,
    pub soul_md: Option<String>,
    pub long_term: MemoryIndex,
    pub mid_term: MemoryIndex,
    pub short_term: Vec<Message>,
    /// Verbatim older user text retained separately from the raw exchange window.
    pub retained_user_inputs: Vec<Message>,
    /// Full text of the newest non-overlapping summary selected for direct
    /// context injection (mid-term preferred, long-term fallback).
    pub latest_summary: Option<String>,
    /// Persisted tool-managed state (todos, etc.) restored from disk.
    pub user_state: serde_json::Value,
}

/// An uncommitted compression result. The runtime validates the rebuilt model
/// request before making this checkpoint visible to later requests.
pub struct CompactionDraft {
    pub summary: String,
    pub covered_messages: usize,
    pub retained_user_budget: u32,
    pub keep_recent_raw: bool,
    uuid: String,
    first_message_id: String,
    last_message_id: String,
    ledger_len: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ThreadHistoryMessage {
    pub id: String,
    pub role: String,
    pub text: String,
    pub timestamp: Option<String>,
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pretty: Option<PrettyToolCall>,
}

// ── MemoryStore ───────────────────────────────────────────────────────────────

pub struct MemoryStore {
    /// Root data dir (e.g. `.remi-cat`).  Soul.md and all thread data live here.
    pub data_dir: PathBuf,
    /// Exact directory containing per-thread and named memory data.
    /// Kept separate from `data_dir` so an application profile can reference
    /// shared or external memory without relocating its other state.
    pub memory_dir: PathBuf,
    /// If set, `Agent.md` is read from this exact path instead of
    /// `data_dir/Agent.md`.
    ///
    /// Use this to place `Agent.md` **outside** the agent's writable sandbox
    /// so only an admin / daemon can modify it.  Set via the `AGENT_MD_PATH`
    /// environment variable.
    pub agent_md_path: Option<PathBuf>,
    pub compressor: LlmCompressor,
    /// Short-term token budget; overflow triggers compression to mid-term.
    pub short_term_tokens: usize,
    /// When false, new turns are persisted without automatic short-term compaction.
    pub auto_compress: bool,
    /// Mid-term entries older than this many days are promoted to long-term.
    pub memory_days: u64,
}

#[derive(Debug, Clone)]
pub struct NamedMemoryWrite {
    pub name: String,
    pub path: PathBuf,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub bytes: usize,
}

#[derive(Debug, Clone)]
pub struct MemoryRecallResult {
    pub source: String,
    pub name: Option<String>,
    pub uuid: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub preview: String,
    pub snippet: String,
    pub score: usize,
}

pub type ContextCompactionSink<'a> = &'a mut dyn FnMut(ContextCompactionEvent);

const MEMORY_RECALL_SNIPPET_CHARS: usize = 2_000;
static SHORT_TERM_TOKEN_COUNTS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
static THREAD_LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();

fn short_term_token_counts() -> &'static Mutex<HashMap<String, usize>> {
    SHORT_TERM_TOKEN_COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn thread_lock(key: String) -> Arc<tokio::sync::Mutex<()>> {
    THREAD_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("memory thread lock map poisoned")
        .entry(key)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
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
    fn token_cache_key(&self, thread_id: &str) -> String {
        format!("{}\0{}", self.memory_dir.display(), sanitize_id(thread_id))
    }
    fn thread_dir(&self, thread_id: &str) -> PathBuf {
        self.memory_dir.join(sanitize_id(thread_id))
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

    fn named_memory_dir(&self, agent_id: &str) -> PathBuf {
        self.memory_dir.join("named").join(sanitize_id(agent_id))
    }

    // ── Index I/O ─────────────────────────────────────────────────────────────

    async fn read_index_checked(dir: &PathBuf) -> Result<MemoryIndex, AgentError> {
        let path = dir.join("index.json");
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => serde_json::from_str(&s).map_err(|err| {
                AgentError::other(format!("invalid memory index {}: {err}", path.display()))
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(MemoryIndex::default()),
            Err(err) => Err(AgentError::Io(format!("read {}: {err}", path.display()))),
        }
    }

    async fn write_index(dir: &PathBuf, idx: &MemoryIndex) -> Result<(), AgentError> {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        atomic_write(&dir.join("index.json"), idx.to_json().as_bytes()).await
    }

    // ── Short-term JSONL I/O ──────────────────────────────────────────────────

    async fn read_short_term_checked(path: &PathBuf) -> Result<Vec<Message>, AgentError> {
        let text = match tokio::fs::read_to_string(path).await {
            Ok(t) => t,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(err) => return Err(AgentError::Io(format!("read {}: {err}", path.display()))),
        };
        let blobs_dir = path
            .parent()
            .map(|p| p.join("blobs"))
            .unwrap_or_else(|| PathBuf::from("blobs"));
        let text = super::blob::restore_blobs(&text, &blobs_dir).await;
        text.lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| {
                serde_json::from_str(line).map_err(|err| {
                    AgentError::other(format!(
                        "invalid memory ledger {} line {}: {err}",
                        path.display(),
                        index + 1
                    ))
                })
            })
            .collect()
    }

    async fn read_short_term(path: &PathBuf) -> Vec<Message> {
        Self::read_short_term_checked(path)
            .await
            .unwrap_or_else(|err| {
                tracing::error!(error = %err, "memory ledger read failed");
                Vec::new()
            })
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
            .map(|m| {
                serde_json::to_string(m)
                    .map_err(|err| AgentError::other(format!("serialize memory message: {err}")))
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|l| l + "\n")
            .collect();
        let blobs_dir = path
            .parent()
            .map(|p| p.join("blobs"))
            .unwrap_or_else(|| PathBuf::from("blobs"));
        let lines = super::blob::extract_blobs(&lines, &blobs_dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        atomic_write(path, lines.as_bytes()).await
    }

    async fn append_short_term(path: &PathBuf, msgs: &[Message]) -> Result<(), AgentError> {
        if msgs.is_empty() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| AgentError::Io(e.to_string()))?;
        }
        let lines = msgs
            .iter()
            .map(|message| {
                serde_json::to_string(message)
                    .map_err(|err| AgentError::other(format!("serialize memory message: {err}")))
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|line| line + "\n")
            .collect::<String>();
        let blobs_dir = path
            .parent()
            .map(|parent| parent.join("blobs"))
            .unwrap_or_else(|| PathBuf::from("blobs"));
        let lines = super::blob::extract_blobs(&lines, &blobs_dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        file.write_all(lines.as_bytes())
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        file.sync_all()
            .await
            .map_err(|e| AgentError::Io(e.to_string()))
    }

    // ── Promotion: mid-term → long-term ──────────────────────────────────────

    /// Promote mid-term entries older than `memory_days` into a single long-term block.
    async fn maybe_promote(&self, thread_id: &str) -> Result<(), AgentError> {
        let mid_dir = self.mid_term_dir(thread_id);
        let long_dir = self.long_term_dir(thread_id);

        let mut mid_idx = Self::read_index_checked(&mid_dir).await?;
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
            let text = tokio::fs::read_to_string(&md_path)
                .await
                .map_err(|err| AgentError::Io(format!("read {}: {err}", md_path.display())))?;
            if !promoting_text.is_empty() {
                promoting_text.push_str("\n\n---\n\n");
            }
            promoting_text.push_str(&text);
        }

        // Collect ALL existing long-term summaries to merge with.
        let lt_idx_existing = Self::read_index_checked(&long_dir).await?;
        let mut existing_long_text = String::new();
        for entry in &lt_idx_existing.entries {
            let md_path = long_dir.join(format!("{}.md", entry.uuid));
            let text = tokio::fs::read_to_string(&md_path)
                .await
                .map_err(|err| AgentError::Io(format!("read {}: {err}", md_path.display())))?;
            if !existing_long_text.is_empty() {
                existing_long_text.push_str("\n\n---\n\n");
            }
            existing_long_text.push_str(&text);
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
        if long_summary.trim().is_empty() {
            return Err(AgentError::other(
                "long-term promotion compressor produced no committed summary",
            ));
        }

        let long_uuid = Uuid::new_v4().to_string();
        let preview = make_preview(&long_summary, 100);

        tokio::fs::create_dir_all(&long_dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;

        // Write merged long-term summary with timestamp header.
        let lt_path = long_dir.join(format!("{long_uuid}.md"));
        let ts_header = format!("<!-- created: {} -->\n\n", Utc::now().to_rfc3339());
        atomic_write(&lt_path, format!("{ts_header}{long_summary}").as_bytes()).await?;

        // Replace long-term index with single new entry.
        let new_lt_idx = MemoryIndex {
            entries: vec![MemoryEntry {
                uuid: long_uuid.clone(),
                created_at: Utc::now(),
                preview,
                first_message_id: to_promote.iter().find_map(|e| e.first_message_id.clone()),
                last_message_id: to_promote
                    .iter()
                    .rev()
                    .find_map(|e| e.last_message_id.clone()),
                message_count: Some(to_promote.iter().filter_map(|e| e.message_count).sum()),
                status: Some("committed".to_string()),
                retained_user_budget: to_promote
                    .last()
                    .and_then(|entry| entry.retained_user_budget),
                keep_recent_raw: to_promote.last().and_then(|entry| entry.keep_recent_raw),
            }],
        };
        Self::write_index(&long_dir, &new_lt_idx).await?;

        // Update mid-term index (remove promoted entries).
        mid_idx.entries = keep;
        Self::write_index(&mid_dir, &mid_idx).await?;

        // Both indexes now point to committed summaries.  Clean up old files
        // only after that point so an interrupted write cannot lose context.
        for entry in &to_promote {
            let src_raw = mid_dir.join("raw").join(&entry.uuid);
            if src_raw.exists() {
                let dst_raw = long_dir.join("raw").join(&long_uuid).join(&entry.uuid);
                if let Some(parent) = dst_raw.parent() {
                    if let Err(err) = tokio::fs::create_dir_all(parent).await {
                        tracing::warn!(error = %err, "could not prepare promoted raw archive directory");
                        continue;
                    }
                }
                if let Err(err) = move_dir(&src_raw, &dst_raw).await {
                    tracing::warn!(error = %err, "could not move promoted raw archive");
                }
            }
            let _ = tokio::fs::remove_file(mid_dir.join(format!("{}.md", entry.uuid))).await;
        }
        for entry in &lt_idx_existing.entries {
            let _ = tokio::fs::remove_file(long_dir.join(format!("{}.md", entry.uuid))).await;
        }

        Ok(())
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Load the full memory context for a thread.
    ///
    /// This also runs `maybe_promote` to age out stale mid-term entries.
    /// Promotion failures are non-fatal.
    pub async fn load_context(&self, thread_id: &str) -> Result<MemoryContext, AgentError> {
        self.load_context_with_user_budget(thread_id, 20_000).await
    }

    pub async fn load_context_with_user_budget(
        &self,
        thread_id: &str,
        user_budget: u32,
    ) -> Result<MemoryContext, AgentError> {
        self.migrate_legacy_raw_archives(thread_id).await?;
        // Attempt promotion first; ignore errors so a failing LLM call
        // does not block the main turn.
        let _ = self.maybe_promote(thread_id).await;

        let agent_md = match &self.agent_md_path {
            Some(p) => read_optional_file(p).await,
            None => read_optional_file(&self.data_dir.join("Agent.md")).await,
        };
        let soul_md = read_optional_file(&self.data_dir.join("Soul.md")).await;
        let long_term = Self::read_index_checked(&self.long_term_dir(thread_id)).await?;
        let mid_term = Self::read_index_checked(&self.mid_term_dir(thread_id)).await?;
        let ledger = Self::read_short_term_checked(&self.short_term_path(thread_id)).await?;
        let covered_position = last_covered_position_across_tiers(&ledger, &mid_term, &long_term);
        let uncovered_start = covered_position.map_or(0, |position| position + 1);
        let covered_id = covered_position
            .and_then(|position| ledger.get(position))
            .map(|message| message.id.to_string());
        let saved_window = mid_term
            .entries
            .iter()
            .chain(long_term.entries.iter())
            .filter(|entry| entry.status.as_deref() != Some("legacy"))
            .filter(|entry| entry.last_message_id == covered_id)
            .max_by_key(|entry| entry.created_at);
        let selected_budget = saved_window.as_ref().map_or(user_budget, |window| {
            user_budget.min(window.retained_user_budget.unwrap_or(20_000))
        });
        let keep_recent_raw = saved_window
            .as_ref()
            .is_none_or(|window| window.keep_recent_raw.unwrap_or(true));
        let (short_term, retained_user_inputs) = visible_ledger_messages(
            &ledger,
            covered_position,
            selected_budget.min(20_000),
            keep_recent_raw,
        );
        let uncovered_tokens = estimate_memory_message_tokens(&ledger[uncovered_start..]);
        short_term_token_counts()
            .lock()
            .expect("short-term token cache lock poisoned")
            .insert(self.token_cache_key(thread_id), uncovered_tokens);
        let user_state = self.load_user_state(thread_id).await;

        let latest_entry = mid_term.entries.last().or_else(|| long_term.entries.last());
        let latest_summary = match latest_entry {
            Some(entry) => {
                let dir = if mid_term.entries.iter().any(|e| e.uuid == entry.uuid) {
                    self.mid_term_dir(thread_id)
                } else {
                    self.long_term_dir(thread_id)
                };
                let path = dir.join(format!("{}.md", entry.uuid));
                Some(
                    tokio::fs::read_to_string(&path)
                        .await
                        .map_err(|err| AgentError::Io(format!("read {}: {err}", path.display())))?,
                )
            }
            None => None,
        };
        Ok(MemoryContext {
            agent_md,
            soul_md,
            long_term,
            mid_term,
            short_term,
            retained_user_inputs,
            latest_summary,
            user_state,
        })
    }

    async fn migrate_legacy_raw_archives(&self, thread_id: &str) -> Result<(), AgentError> {
        let thread_dir = self.thread_dir(thread_id);
        let marker = thread_dir.join(".ledger_migration_v1");
        if tokio::fs::metadata(&marker).await.is_ok() {
            return Ok(());
        }
        let lock = thread_lock(self.token_cache_key(thread_id));
        let _guard = lock.lock().await;
        let mut raw_files = Vec::new();
        for root in [
            self.mid_term_dir(thread_id).join("raw"),
            self.long_term_dir(thread_id).join("raw"),
        ] {
            collect_jsonl_files(root, &mut raw_files).await?;
        }
        if raw_files.is_empty() {
            atomic_write(&marker, b"complete\n").await?;
            return Ok(());
        }
        let short_path = self.short_term_path(thread_id);
        let mut merged = Self::read_short_term_checked(&short_path).await?;
        let mut ids = merged
            .iter()
            .map(|m| m.id.to_string())
            .collect::<std::collections::HashSet<_>>();
        for path in raw_files {
            let text = match tokio::fs::read_to_string(&path).await {
                Ok(v) => v,
                Err(_) => continue,
            };
            let blobs = thread_dir.join("blobs");
            let text = super::blob::restore_blobs(&text, &blobs).await;
            for message in text
                .lines()
                .filter_map(|line| serde_json::from_str::<Message>(line).ok())
            {
                if ids.insert(message.id.to_string()) {
                    merged.push(message);
                }
            }
        }
        merged.sort_by_key(message_timestamp);
        Self::write_short_term(&short_path, &merged).await?;
        let reparsed = Self::read_short_term_checked(&short_path).await?;
        if reparsed.len() != merged.len() {
            return Err(AgentError::Io(
                "legacy raw migration verification failed".to_string(),
            ));
        }
        atomic_write(&marker, b"complete\n").await?;
        for raw in [
            self.mid_term_dir(thread_id).join("raw"),
            self.long_term_dir(thread_id).join("raw"),
        ] {
            let _ = tokio::fs::remove_dir_all(raw).await;
        }
        Ok(())
    }

    pub async fn thread_history(&self, thread_id: &str) -> Vec<ThreadHistoryMessage> {
        let mut tool_calls =
            std::collections::HashMap::<String, (String, serde_json::Value)>::new();
        Self::read_short_term(&self.short_term_path(thread_id))
            .await
            .into_iter()
            .filter(|message| !matches!(message.role, Role::System))
            .map(|message| {
                if let Some(calls) = &message.tool_calls {
                    for call in calls {
                        let args = serde_json::from_str(&call.function.arguments)
                            .unwrap_or(serde_json::Value::String(call.function.arguments.clone()));
                        tool_calls.insert(call.id.clone(), (call.function.name.clone(), args));
                    }
                }
                let text = message.content.text_content();
                let pretty = match (&message.role, message.tool_call_id.as_deref()) {
                    (Role::Tool, Some(call_id)) => tool_calls.get(call_id).map(|(name, args)| {
                        let elapsed_ms = tool_elapsed_ms(&message);
                        let mut pretty = PrettyToolCall::completed(
                            call_id,
                            name,
                            args,
                            &text,
                            tool_success(&text),
                            elapsed_ms.unwrap_or(0),
                        );
                        if elapsed_ms.is_none() {
                            pretty.elapsed_ms = None;
                        }
                        pretty
                    }),
                    _ => None,
                };
                ThreadHistoryMessage {
                    id: message.id.0,
                    role: match message.role {
                        Role::System => "system",
                        Role::User
                            if message.metadata.as_ref().is_some_and(|metadata| {
                                metadata
                                    .get("internal_supervisor_message")
                                    .and_then(serde_json::Value::as_bool)
                                    .unwrap_or(false)
                            }) =>
                        {
                            "supervisor"
                        }
                        Role::User
                            if message.metadata.as_ref().is_some_and(|metadata| {
                                metadata
                                    .get("background_tool_task")
                                    .and_then(serde_json::Value::as_bool)
                                    .unwrap_or(false)
                            }) || message.content.text_content().starts_with(
                                "[Background tool task completed while this run was active]",
                            ) || message.content.text_content().starts_with(
                                "[Background tool tasks completed while this run was active]",
                            ) =>
                        {
                            "internal"
                        }
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::Tool => "tool",
                    }
                    .to_string(),
                    text,
                    timestamp: message
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get("timestamp"))
                        .and_then(serde_json::Value::as_str)
                        .map(ToOwned::to_owned),
                    tool_call_id: message.tool_call_id,
                    tool_calls: message
                        .tool_calls
                        .and_then(|calls| serde_json::to_value(calls).ok()),
                    pretty,
                }
            })
            .collect()
    }

    pub async fn delete_thread(&self, thread_id: &str) -> Result<(), AgentError> {
        let result = match tokio::fs::remove_dir_all(self.thread_dir(thread_id)).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(AgentError::Io(err.to_string())),
        };
        if result.is_ok() {
            remove_thread_caches(&self.token_cache_key(thread_id));
        }
        result
    }

    pub async fn fork_thread(
        &self,
        source_thread_id: &str,
        target_thread_id: &str,
    ) -> Result<(), AgentError> {
        let source = self.thread_dir(source_thread_id);
        let target = self.thread_dir(target_thread_id);
        if tokio::fs::metadata(&source).await.is_err() {
            return Ok(());
        }
        match tokio::fs::remove_dir_all(&target).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(AgentError::Io(err.to_string())),
        }
        copy_dir_all(source, target).await
    }

    /// Append new messages from a turn to short-term storage.
    ///
    /// Saving is append-only and never performs network I/O. Automatic
    /// compression is exclusively a pre-request operation.
    pub async fn save_turn(
        &self,
        thread_id: &str,
        new_msgs: Vec<Message>,
    ) -> Result<(), AgentError> {
        self.save_turn_with_compaction_events(thread_id, new_msgs, None)
            .await
    }

    /// Append a failed/preflight-only turn without triggering compaction.
    pub async fn append_failed_turn(
        &self,
        thread_id: &str,
        messages: Vec<Message>,
    ) -> Result<(), AgentError> {
        let lock = thread_lock(self.token_cache_key(thread_id));
        let _guard = lock.lock().await;
        Self::append_short_term(&self.short_term_path(thread_id), &messages).await?;
        remove_thread_caches(&self.token_cache_key(thread_id));
        Ok(())
    }

    pub async fn save_turn_with_compaction_events(
        &self,
        thread_id: &str,
        mut new_msgs: Vec<Message>,
        _compaction_sink: Option<ContextCompactionSink<'_>>,
    ) -> Result<(), AgentError> {
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
        let lock = thread_lock(self.token_cache_key(thread_id));
        let _guard = lock.lock().await;
        let short_path = self.short_term_path(thread_id);
        Self::append_short_term(&short_path, &new_msgs).await?;
        remove_thread_caches(&self.token_cache_key(thread_id));
        Ok(())
    }

    /// Compact the oldest uncovered complete exchanges for request preflight.
    /// The ledger remains append-only; the returned count is newly covered raw
    /// messages. Recent raw messages are retained whenever a safe boundary exists.
    pub async fn compact_for_request(&self, thread_id: &str) -> Result<usize, AgentError> {
        self.compact_for_request_with_compressor(thread_id, &self.compressor)
            .await
    }

    pub async fn compact_for_request_with_compressor(
        &self,
        thread_id: &str,
        compressor: &LlmCompressor,
    ) -> Result<usize, AgentError> {
        let draft = self.prepare_compaction(thread_id, compressor).await?;
        self.commit_compaction(thread_id, draft).await
    }

    pub async fn prepare_compaction(
        &self,
        thread_id: &str,
        compressor: &LlmCompressor,
    ) -> Result<CompactionDraft, AgentError> {
        self.prepare_compaction_with_recent(thread_id, compressor, false)
            .await
    }

    async fn prepare_compaction_with_recent(
        &self,
        thread_id: &str,
        compressor: &LlmCompressor,
        preserve_recent: bool,
    ) -> Result<CompactionDraft, AgentError> {
        let lock = thread_lock(self.token_cache_key(thread_id));
        let _guard = lock.lock().await;
        let ledger = Self::read_short_term_checked(&self.short_term_path(thread_id)).await?;
        let mid_idx = Self::read_index_checked(&self.mid_term_dir(thread_id)).await?;
        let long_idx = Self::read_index_checked(&self.long_term_dir(thread_id)).await?;
        let start = last_covered_position_across_tiers(&ledger, &mid_idx, &long_idx)
            .map_or(0, |position| position + 1);
        let end = if preserve_recent {
            start + recent_complete_start(&ledger[start..], 10)
        } else {
            ledger.len()
        };
        let active = &ledger[start..end];
        if active.is_empty() && preserve_recent {
            return Err(AgentError::other(
                "no uncovered history remains eligible for compression",
            ));
        }
        let mid_dir = self.mid_term_dir(thread_id);
        let long_dir = self.long_term_dir(thread_id);
        let previous = mid_idx
            .entries
            .last()
            .map(|e| (&mid_dir, e))
            .or_else(|| long_idx.entries.last().map(|e| (&long_dir, e)));
        let previous_summary = if let Some((dir, entry)) = previous {
            tokio::fs::read_to_string(dir.join(format!("{}.md", entry.uuid)))
                .await
                .map_err(|err| AgentError::Io(err.to_string()))?
        } else {
            String::new()
        };
        let source = compression_input_text(active);
        let summary = if active.is_empty() {
            if previous_summary.trim().is_empty() {
                return Err(AgentError::other(
                    "no uncovered history or summary remains eligible for compression",
                ));
            }
            let value = fold_compression_source(compressor, "", &previous_summary).await?;
            if crate::estimate_model_input_tokens(&value)
                >= crate::estimate_model_input_tokens(&previous_summary)
            {
                return Err(AgentError::other("existing summary did not shrink"));
            }
            value
        } else {
            if source.trim().is_empty() {
                return Err(AgentError::other(
                    "uncovered history has no text to summarize",
                ));
            }
            fold_compression_source(compressor, &previous_summary, &source).await?
        };
        let covered_id = ledger
            .get(start.saturating_sub(1))
            .map(|message| message.id.to_string());
        Ok(CompactionDraft {
            summary,
            covered_messages: active.len(),
            retained_user_budget: 20_000,
            keep_recent_raw: true,
            uuid: Uuid::new_v4().to_string(),
            first_message_id: active
                .first()
                .map(|message| message.id.to_string())
                .or_else(|| covered_id.clone())
                .expect("a previous summary covers a ledger message"),
            last_message_id: active
                .last()
                .map(|message| message.id.to_string())
                .or(covered_id)
                .expect("a previous summary covers a ledger message"),
            ledger_len: ledger.len(),
        })
    }

    pub async fn preview_compaction_context(
        &self,
        thread_id: &str,
        draft: &CompactionDraft,
        user_budget: u32,
    ) -> Result<MemoryContext, AgentError> {
        let mut context = self.load_context(thread_id).await?;
        let ledger = Self::read_short_term_checked(&self.short_term_path(thread_id)).await?;
        let covered = ledger
            .iter()
            .position(|message| message.id.to_string() == draft.last_message_id)
            .ok_or_else(|| {
                AgentError::other("compression draft coverage is no longer in the ledger")
            })?;
        let (short_term, retained_user_inputs) = visible_ledger_messages(
            &ledger,
            Some(covered),
            user_budget.min(draft.retained_user_budget).min(20_000),
            draft.keep_recent_raw,
        );
        context.short_term = short_term;
        context.retained_user_inputs = retained_user_inputs;
        context.latest_summary = Some(draft.summary.clone());
        context.mid_term.entries.push(MemoryEntry {
            uuid: draft.uuid.clone(),
            created_at: Utc::now(),
            preview: make_preview(&draft.summary, 100),
            first_message_id: Some(draft.first_message_id.clone()),
            last_message_id: Some(draft.last_message_id.clone()),
            message_count: Some(draft.covered_messages),
            status: Some("pending".to_string()),
            retained_user_budget: Some(draft.retained_user_budget),
            keep_recent_raw: Some(draft.keep_recent_raw),
        });
        Ok(context)
    }

    pub async fn commit_compaction(
        &self,
        thread_id: &str,
        draft: CompactionDraft,
    ) -> Result<usize, AgentError> {
        let lock = thread_lock(self.token_cache_key(thread_id));
        let _guard = lock.lock().await;
        let ledger = Self::read_short_term_checked(&self.short_term_path(thread_id)).await?;
        let mut mid_idx = Self::read_index_checked(&self.mid_term_dir(thread_id)).await?;
        let long_idx = Self::read_index_checked(&self.long_term_dir(thread_id)).await?;
        let start = last_covered_position_across_tiers(&ledger, &mid_idx, &long_idx)
            .map_or(0, |position| position + 1);
        let coverage_unchanged = if draft.covered_messages == 0 {
            ledger
                .get(start.saturating_sub(1))
                .map(|m| m.id.to_string())
                == Some(draft.last_message_id.clone())
        } else {
            ledger.get(start).map(|m| m.id.to_string()) == Some(draft.first_message_id.clone())
                && ledger
                    .get(start + draft.covered_messages - 1)
                    .map(|m| m.id.to_string())
                    == Some(draft.last_message_id.clone())
        };
        if ledger.len() != draft.ledger_len || !coverage_unchanged {
            return Err(AgentError::other(
                "compression draft is stale; coverage was not committed",
            ));
        }
        let uuid = draft.uuid.clone();
        let dir = self.mid_term_dir(thread_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        atomic_write(&dir.join(format!("{uuid}.md")), draft.summary.as_bytes()).await?;
        mid_idx.entries.push(MemoryEntry {
            uuid,
            created_at: Utc::now(),
            preview: make_preview(&draft.summary, 100),
            first_message_id: Some(draft.first_message_id),
            last_message_id: Some(draft.last_message_id),
            message_count: Some(draft.covered_messages),
            status: Some("committed".to_string()),
            retained_user_budget: Some(draft.retained_user_budget),
            keep_recent_raw: Some(draft.keep_recent_raw),
        });
        Self::write_index(&dir, &mid_idx).await?;
        remove_thread_caches(&self.token_cache_key(thread_id));
        Ok(draft.covered_messages)
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

    pub async fn load_user_state(&self, thread_id: &str) -> serde_json::Value {
        let path = self.user_state_path(thread_id);
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => serde_json::from_str(&s).unwrap_or(serde_json::Value::Null),
            Err(_) => serde_json::Value::Null,
        }
    }

    /// Force compression of uncovered complete exchanges while retaining the
    /// latest ten raw ledger messages for direct model context.
    ///
    /// - All existing mid-term `.md` files are read, merged, and re-compressed
    ///   together with the current short-term log.
    /// - The mid-term index is replaced with the single new entry.
    /// The append-only ledger is never truncated by this operation.
    pub async fn compact_now(&self, thread_id: &str) -> Result<usize, AgentError> {
        self.compact_now_with_compressor(thread_id, &self.compressor)
            .await
    }

    /// Commit a summary authored by the running agent. The append-only ledger
    /// remains intact; the summary covers every ledger message that predates
    /// the active run and replaces the current mid-term summary.
    pub async fn commit_agent_summary(
        &self,
        thread_id: &str,
        summary: &str,
    ) -> Result<usize, AgentError> {
        if summary.trim().is_empty() {
            return Err(AgentError::other("agent context summary must not be empty"));
        }
        let lock = thread_lock(self.token_cache_key(thread_id));
        let _guard = lock.lock().await;
        let ledger = Self::read_short_term_checked(&self.short_term_path(thread_id)).await?;
        if ledger.is_empty() {
            return Ok(0);
        }

        let mid_dir = self.mid_term_dir(thread_id);
        let old_index = Self::read_index_checked(&mid_dir).await?;
        let uuid = Uuid::new_v4().to_string();
        tokio::fs::create_dir_all(&mid_dir)
            .await
            .map_err(|error| AgentError::Io(error.to_string()))?;
        let body = format!(
            "<!-- created: {} -->\n\n{}",
            Utc::now().to_rfc3339(),
            summary.trim()
        );
        atomic_write(&mid_dir.join(format!("{uuid}.md")), body.as_bytes()).await?;

        let index = MemoryIndex {
            entries: vec![MemoryEntry {
                uuid: uuid.clone(),
                created_at: Utc::now(),
                preview: make_preview(summary.trim(), 100),
                first_message_id: ledger.first().map(|message| message.id.to_string()),
                last_message_id: ledger.last().map(|message| message.id.to_string()),
                message_count: Some(ledger.len()),
                status: Some("committed".to_string()),
                retained_user_budget: None,
                keep_recent_raw: None,
            }],
        };
        Self::write_index(&mid_dir, &index).await?;
        for entry in old_index.entries {
            if entry.uuid != uuid {
                let _ = tokio::fs::remove_file(mid_dir.join(format!("{}.md", entry.uuid))).await;
            }
        }
        remove_thread_caches(&self.token_cache_key(thread_id));
        Ok(ledger.len())
    }

    /// Compact using the model selected for the active session rather than
    /// the process-wide default compressor.
    pub async fn compact_now_with_compressor(
        &self,
        thread_id: &str,
        compressor: &LlmCompressor,
    ) -> Result<usize, AgentError> {
        let mut draft = match self
            .prepare_compaction_with_recent(thread_id, compressor, true)
            .await
        {
            Ok(draft) => draft,
            Err(err) if err.to_string().contains("no uncovered history") => return Ok(0),
            Err(err) => return Err(err),
        };
        let before = self.load_context(thread_id).await?;
        let mut after = self
            .preview_compaction_context(thread_id, &draft, 20_000)
            .await?;
        let tokens = |context: &MemoryContext| -> u32 {
            super::build_injected_history(context)
                .iter()
                .map(|message| crate::estimate_model_input_tokens(&message.content.text_content()))
                .sum()
        };
        while tokens(&after) >= tokens(&before) && !after.retained_user_inputs.is_empty() {
            after.retained_user_inputs.remove(0);
            draft.retained_user_budget = after
                .retained_user_inputs
                .iter()
                .map(|message| crate::estimate_model_input_tokens(&message.content.text_content()))
                .sum();
        }
        if tokens(&after) >= tokens(&before) {
            return Err(AgentError::other(
                "manual compaction did not reduce the injected context; coverage was not committed",
            ));
        }
        self.commit_compaction(thread_id, draft).await
    }

    /// Clear conversational history for a thread while preserving tool-managed
    /// `user_state.json` such as todos.
    pub async fn clear_thread(&self, thread_id: &str) -> Result<(), AgentError> {
        let lock = thread_lock(self.token_cache_key(thread_id));
        let _guard = lock.lock().await;
        let short_path = self.short_term_path(thread_id);
        Self::write_short_term(&short_path, &[]).await?;
        short_term_token_counts()
            .lock()
            .expect("short-term token cache lock poisoned")
            .insert(self.token_cache_key(thread_id), 0);

        for dir in [
            self.mid_term_dir(thread_id),
            self.long_term_dir(thread_id),
            self.thread_dir(thread_id).join("blobs"),
        ] {
            match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(AgentError::Io(err.to_string())),
            }
        }
        THREAD_LOCKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("memory thread lock map poisoned")
            .remove(&self.token_cache_key(thread_id));
        Ok(())
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
        if let Some(message) = Self::read_short_term_checked(&self.short_term_path(thread_id))
            .await?
            .into_iter()
            .find(|message| message.id.to_string() == uuid)
        {
            return serde_json::to_string_pretty(&message)
                .map(Some)
                .map_err(|e| AgentError::other(format!("serialize ledger message: {e}")));
        }
        Ok(None)
    }

    pub async fn upsert_named_memory(
        &self,
        agent_id: &str,
        name: &str,
        content: &str,
    ) -> Result<NamedMemoryWrite, AgentError> {
        let file_name = normalize_named_memory_name(name)?;
        let dir = self.named_memory_dir(agent_id);
        let path = dir.join(&file_name);
        let now = Utc::now();
        let created_at = read_named_memory_timestamps(&path)
            .await
            .map(|timestamps| timestamps.0)
            .unwrap_or(now);
        let body = format!(
            "<!-- created: {} -->\n<!-- updated: {} -->\n\n{}",
            created_at.to_rfc3339(),
            now.to_rfc3339(),
            content.trim()
        );
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        tokio::fs::write(&path, body.as_bytes())
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        Ok(NamedMemoryWrite {
            name: file_name,
            path,
            created_at,
            updated_at: now,
            bytes: body.len(),
        })
    }

    pub async fn get_named_memory(
        &self,
        agent_id: &str,
        name: &str,
    ) -> Result<Option<String>, AgentError> {
        let file_name = normalize_named_memory_name(name)?;
        let path = self.named_memory_dir(agent_id).join(file_name);
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => Ok(Some(text)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(AgentError::Io(err.to_string())),
        }
    }

    pub async fn recall(
        &self,
        agent_id: &str,
        thread_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecallResult>, AgentError> {
        let Some(query) = tokenized_search_query(query) else {
            return Ok(Vec::new());
        };
        tracing::debug!(
            token_count = query.terms().len(),
            ripgrep_query = query.ripgrep_query(),
            "memory.recall.query_tokenized"
        );
        let limit = limit.clamp(1, 20);
        let mut results = Vec::new();

        self.recall_named(agent_id, &query, &mut results).await?;
        self.recall_short_term(thread_id, &query, &mut results)
            .await?;
        self.recall_tier(thread_id, "mid_term", &query, &mut results)
            .await?;
        self.recall_tier(thread_id, "long_term", &query, &mut results)
            .await?;

        results.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| b.timestamp.cmp(&a.timestamp))
                .then_with(|| a.source.cmp(&b.source))
        });
        results.truncate(limit);
        Ok(results)
    }

    async fn recall_named(
        &self,
        agent_id: &str,
        query: &TokenizedSearchQuery,
        results: &mut Vec<MemoryRecallResult>,
    ) -> Result<(), AgentError> {
        let dir = self.named_memory_dir(agent_id);
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(AgentError::Io(err.to_string())),
        };
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?
        {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            let text = match tokio::fs::read_to_string(&path).await {
                Ok(text) => text,
                Err(_) => continue,
            };
            let body = strip_named_memory_header(&text);
            let score = match_score(&body, query);
            if score == 0 {
                continue;
            }
            let (_, updated_at) = parse_named_memory_timestamps(&text);
            results.push(MemoryRecallResult {
                source: "named".to_string(),
                name: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned()),
                uuid: None,
                timestamp: updated_at,
                preview: make_preview(&body, 100),
                snippet: make_snippet(&body, query, MEMORY_RECALL_SNIPPET_CHARS),
                score,
            });
        }
        Ok(())
    }

    async fn recall_short_term(
        &self,
        thread_id: &str,
        query: &TokenizedSearchQuery,
        results: &mut Vec<MemoryRecallResult>,
    ) -> Result<(), AgentError> {
        let messages = Self::read_short_term_checked(&self.short_term_path(thread_id)).await?;
        let excluded_tool_results = recursive_memory_search_result_ids(&messages);
        for msg in messages {
            if msg.role == Role::Tool
                && msg
                    .tool_call_id
                    .as_ref()
                    .is_some_and(|id| excluded_tool_results.contains(id))
            {
                continue;
            }
            let text = msg.content.text_content();
            let score = match_score(&text, query);
            if score == 0 {
                continue;
            }
            results.push(MemoryRecallResult {
                source: "short_term".to_string(),
                name: Some(format!("{:?}", msg.role)),
                uuid: Some(msg.id.to_string()),
                timestamp: message_timestamp(&msg),
                preview: make_preview(&text, 100),
                snippet: make_snippet(&text, query, MEMORY_RECALL_SNIPPET_CHARS),
                score,
            });
        }
        Ok(())
    }

    async fn recall_tier(
        &self,
        thread_id: &str,
        tier: &str,
        query: &TokenizedSearchQuery,
        results: &mut Vec<MemoryRecallResult>,
    ) -> Result<(), AgentError> {
        let (source, dir) = match tier {
            "mid_term" => ("mid_term", self.mid_term_dir(thread_id)),
            "long_term" => ("long_term", self.long_term_dir(thread_id)),
            _ => return Ok(()),
        };
        let idx = Self::read_index_checked(&dir).await?;
        for entry in idx.entries {
            let path = dir.join(format!("{}.md", entry.uuid));
            let text = match tokio::fs::read_to_string(&path).await {
                Ok(text) => text,
                Err(_) => continue,
            };
            let score = match_score(&text, query);
            if score == 0 {
                continue;
            }
            results.push(MemoryRecallResult {
                source: source.to_string(),
                name: None,
                uuid: Some(entry.uuid),
                timestamp: entry.created_at,
                preview: make_preview(&text, 100),
                snippet: make_snippet(&text, query, MEMORY_RECALL_SNIPPET_CHARS),
                score,
            });
        }
        Ok(())
    }
}

/// Tool results from memory retrieval must not become retrieval candidates
/// themselves. Otherwise each recall can embed the previous recall payload,
/// causing recursive growth and eventually outranking the original memory.
fn recursive_memory_search_result_ids(messages: &[Message]) -> HashSet<String> {
    messages
        .iter()
        .filter_map(|message| message.tool_calls.as_ref())
        .flatten()
        .filter(|call| {
            if call.function.name == "memory__recall" {
                return true;
            }
            if call.function.name != "search" {
                return false;
            }
            serde_json::from_str::<serde_json::Value>(&call.function.arguments)
                .ok()
                .and_then(|arguments| {
                    arguments
                        .get("scope")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .is_some_and(|scope| scope.eq_ignore_ascii_case("memory"))
        })
        .map(|call| call.id.clone())
        .collect()
}

fn tool_elapsed_ms(message: &Message) -> Option<u64> {
    message.metadata.as_ref()?.get("tool_elapsed_ms")?.as_u64()
}

fn last_covered_position(messages: &[Message], index: &MemoryIndex) -> Option<usize> {
    index
        .entries
        .iter()
        .filter(|entry| entry.status.as_deref() != Some("legacy"))
        .filter_map(|entry| entry.last_message_id.as_deref())
        .filter_map(|id| {
            messages
                .iter()
                .position(|message| message.id.to_string() == id)
        })
        .max()
}

fn last_covered_position_across_tiers(
    messages: &[Message],
    mid_term: &MemoryIndex,
    long_term: &MemoryIndex,
) -> Option<usize> {
    last_covered_position(messages, mid_term)
        .into_iter()
        .chain(last_covered_position(messages, long_term))
        .max()
}

fn remove_thread_caches(key: &str) {
    short_term_token_counts()
        .lock()
        .expect("short-term token cache lock poisoned")
        .remove(key);
    THREAD_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("memory thread lock map poisoned")
        .remove(key);
}

fn recent_complete_start(messages: &[Message], max_messages: usize) -> usize {
    if messages.len() <= max_messages {
        return 0;
    }
    let desired = messages.len() - max_messages;
    (desired..messages.len())
        .find(|&i| {
            matches!(messages[i].role, Role::User | Role::System)
                && tool_protocol_closed(&messages[..i])
        })
        // Retaining extra raw history is safer than cutting through a tool
        // exchange when no closed boundary exists.
        .unwrap_or(0)
}

fn visible_ledger_messages(
    ledger: &[Message],
    covered_position: Option<usize>,
    user_budget: u32,
    keep_recent_raw: bool,
) -> (Vec<Message>, Vec<Message>) {
    let uncovered_start = covered_position.map_or(0, |position| position + 1);
    // The injected suffix can neutralize orphaned tool traffic locally, so it
    // only needs a user/system opening. Do not use the full-ledger protocol
    // closure scan here: an old missing tool result would turn its zero
    // fallback into a replay of all covered raw history.
    let recent_start = (ledger.len().saturating_sub(10)..ledger.len())
        .find(|&index| matches!(ledger[index].role, Role::User | Role::System))
        .unwrap_or(ledger.len());
    let raw_start = if keep_recent_raw && covered_position.is_some() {
        uncovered_start.min(recent_start)
    } else {
        uncovered_start
    };
    let short_term = protocol_safe_history(&ledger[raw_start..]);
    let mut retained_user_inputs = Vec::new();
    let mut used = 0_u32;
    for message in ledger[..raw_start]
        .iter()
        .rev()
        .filter(|m| m.role == Role::User)
    {
        let tokens = crate::estimate_model_input_tokens(&message.content.text_content());
        if used.saturating_add(tokens) > user_budget {
            break;
        }
        retained_user_inputs.push(message.clone());
        used = used.saturating_add(tokens);
    }
    retained_user_inputs.reverse();
    (short_term, retained_user_inputs)
}

async fn fold_compression_source(
    compressor: &LlmCompressor,
    previous_summary: &str,
    source: &str,
) -> Result<String, AgentError> {
    let mut summary = previous_summary.to_string();
    let mut remaining = source;
    let mut summary_reductions = 0_u8;
    while !remaining.is_empty() {
        let prefix = if summary.is_empty() {
            String::new()
        } else {
            format!("[Previous committed summary]\n{summary}\n\n[New source]\n")
        };
        if !compressor.input_text_fits(&format!("{prefix}x")) {
            if summary_reductions >= 8 {
                return Err(AgentError::other(
                    "compression summary still cannot admit more source after eight reductions",
                ));
            }
            let reduced = compressor.compress_text(&summary).await?;
            if crate::estimate_model_input_tokens(&reduced)
                >= crate::estimate_model_input_tokens(&summary)
            {
                return Err(AgentError::other(
                    "compression summary cannot be reduced enough to admit more source",
                ));
            }
            summary = reduced;
            summary_reductions += 1;
            continue;
        }
        summary_reductions = 0;
        let mut boundaries = remaining
            .char_indices()
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        boundaries.push(remaining.len());
        let mut low = 1_usize;
        let mut high = boundaries.len() - 1;
        let mut fitted = 0_usize;
        while low <= high {
            let middle = low + (high - low) / 2;
            let end = boundaries[middle];
            if compressor.input_text_fits(&format!("{prefix}{}", &remaining[..end])) {
                fitted = end;
                low = middle + 1;
            } else {
                high = middle.saturating_sub(1);
            }
        }
        if fitted == 0 {
            return Err(AgentError::other(
                "no source text fits the compressor context",
            ));
        }
        let input = format!("{prefix}{}", &remaining[..fitted]);
        let next = compressor.compress_text(&input).await?;
        if crate::estimate_model_input_tokens(&next) >= crate::estimate_model_input_tokens(&input) {
            return Err(AgentError::other(
                "compressor output did not reduce its input",
            ));
        }
        summary = next;
        remaining = &remaining[fitted..];
    }
    Ok(summary)
}

fn tool_protocol_closed(messages: &[Message]) -> bool {
    let calls = messages
        .iter()
        .filter_map(|message| message.tool_calls.as_ref())
        .flatten()
        .map(|call| call.id.as_str())
        .collect::<HashSet<_>>();
    let results = messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .filter_map(|message| message.tool_call_id.as_deref())
        .collect::<HashSet<_>>();
    calls == results
}

/// Return model-safe history without mutating the append-only ledger.
///
/// Providers require every assistant tool call to be followed by matching tool
/// results. Interrupted turns and legacy compression boundaries can leave a
/// partial chain in durable history. Preserve that evidence as a protocol-
/// neutral system record instead of repeatedly sending an invalid sequence.
fn protocol_safe_history(messages: &[Message]) -> Vec<Message> {
    let mut safe = Vec::with_capacity(messages.len());
    let mut i = 0;
    while i < messages.len() {
        let message = &messages[i];
        if let Some(calls) = message
            .tool_calls
            .as_ref()
            .filter(|calls| !calls.is_empty())
        {
            let expected = calls
                .iter()
                .map(|call| call.id.as_str())
                .collect::<HashSet<_>>();
            let mut found = HashSet::new();
            let mut j = i + 1;
            while j < messages.len() && messages[j].role == Role::Tool {
                if let Some(id) = messages[j].tool_call_id.as_deref() {
                    if expected.contains(id) {
                        found.insert(id);
                    }
                }
                j += 1;
            }
            if found.len() == expected.len() {
                safe.extend_from_slice(&messages[i..j]);
            } else {
                let call_list = calls
                    .iter()
                    .map(|call| {
                        format!(
                            "{}({}) id={}",
                            call.function.name, call.function.arguments, call.id
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let results = messages[i + 1..j]
                    .iter()
                    .map(|result| {
                        format!(
                            "id={} result={}",
                            result.tool_call_id.as_deref().unwrap_or("unknown"),
                            result.content.text_content()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                safe.push(Message::system(format!(
                    "[INCOMPLETE HISTORICAL TOOL ACTIVITY]\nassistant_text={}\ncalls={}\n{}",
                    message.content.text_content(),
                    call_list,
                    results
                )));
            }
            i = j;
            continue;
        }
        if message.role == Role::Tool {
            safe.push(Message::system(format!(
                "[ORPHANED HISTORICAL TOOL RESULT]\nid={}\n{}",
                message.tool_call_id.as_deref().unwrap_or("unknown"),
                message.content.text_content()
            )));
        } else {
            safe.push(message.clone());
        }
        i += 1;
    }
    safe
}

async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), AgentError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
    }
    let tmp = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)
        .await
        .map_err(|e| AgentError::Io(e.to_string()))?;
    file.write_all(bytes)
        .await
        .map_err(|e| AgentError::Io(e.to_string()))?;
    file.sync_all()
        .await
        .map_err(|e| AgentError::Io(e.to_string()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| AgentError::Io(e.to_string()))?;
    if let Some(parent) = path.parent() {
        if let Ok(dir) = tokio::fs::File::open(parent).await {
            let _ = dir.sync_all().await;
        }
    }
    Ok(())
}

// ── Filesystem helpers ────────────────────────────────────────────────────────

async fn read_optional_file(path: &PathBuf) -> Option<String> {
    tokio::fs::read_to_string(path).await.ok()
}

fn collect_jsonl_files(
    root: PathBuf,
    output: &mut Vec<PathBuf>,
) -> futures::future::BoxFuture<'_, Result<(), AgentError>> {
    Box::pin(async move {
        let mut entries = match tokio::fs::read_dir(&root).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(AgentError::Io(err.to_string())),
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?
        {
            let path = entry.path();
            let kind = entry
                .file_type()
                .await
                .map_err(|e| AgentError::Io(e.to_string()))?;
            if kind.is_dir() {
                collect_jsonl_files(path, output).await?;
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                output.push(path);
            }
        }
        Ok(())
    })
}

fn normalize_named_memory_name(name: &str) -> Result<String, AgentError> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || matches!(trimmed, "." | "..")
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('\0')
    {
        return Err(AgentError::tool(
            "memory__upsert_named",
            "name must be a simple file name",
        ));
    }
    let file_name = if trimmed.ends_with(".md") {
        trimmed.to_string()
    } else {
        format!("{trimmed}.md")
    };
    if file_name == ".md" || file_name == "..md" {
        return Err(AgentError::tool(
            "memory__upsert_named",
            "name must be a simple file name",
        ));
    }
    Ok(file_name)
}

async fn read_named_memory_timestamps(path: &Path) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    tokio::fs::read_to_string(path)
        .await
        .ok()
        .map(|text| parse_named_memory_timestamps(&text))
}

fn parse_named_memory_timestamps(text: &str) -> (DateTime<Utc>, DateTime<Utc>) {
    let created = parse_header_timestamp(text, "created").unwrap_or_else(epoch);
    let updated = parse_header_timestamp(text, "updated").unwrap_or(created);
    (created, updated)
}

fn parse_header_timestamp(text: &str, key: &str) -> Option<DateTime<Utc>> {
    let prefix = format!("<!-- {key}: ");
    text.lines().find_map(|line| {
        let raw = line.strip_prefix(&prefix)?.strip_suffix(" -->")?;
        chrono::DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    })
}

fn strip_named_memory_header(text: &str) -> String {
    text.lines()
        .skip_while(|line| line.trim().is_empty() || line.starts_with("<!-- "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn match_score(text: &str, query: &TokenizedSearchQuery) -> usize {
    let lower = text.to_lowercase();
    let unique_hits = query
        .terms()
        .iter()
        .filter(|term| lower.contains(term.as_str()))
        .count();
    let regex_hits = query.regex().find_iter(text).count();
    unique_hits.saturating_mul(10).saturating_add(regex_hits)
}

fn make_snippet(text: &str, query: &TokenizedSearchQuery, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let occurrences = query
        .regex()
        .find_iter(text)
        .map(|mat| mat.start())
        .collect::<Vec<_>>();
    if occurrences.is_empty() {
        return text.chars().take(max_chars).collect();
    }

    let window_chars = max_chars.clamp(120, 1_000);
    let prefix_chars = 40usize.min(window_chars / 3);
    let text_chars = text.chars().count();
    let mut candidates = Vec::new();
    for byte_idx in occurrences {
        let hit_char = text[..byte_idx].chars().count().min(text_chars);
        let start = hit_char.saturating_sub(prefix_chars);
        let end = (start + window_chars).min(text_chars);
        let window: String = text.chars().skip(start).take(end - start).collect();
        let window_lower = window.to_lowercase();
        let unique_terms = query
            .terms()
            .iter()
            .filter(|term| window_lower.contains(term.as_str()))
            .count();
        let total_matches = query.regex().find_iter(&window).count();
        candidates.push(SnippetWindow {
            start,
            end,
            unique_terms,
            total_matches,
        });
    }

    candidates.sort_by(|a, b| {
        b.unique_terms
            .cmp(&a.unique_terms)
            .then_with(|| b.total_matches.cmp(&a.total_matches))
            .then_with(|| a.start.cmp(&b.start))
    });

    let mut selected: Vec<SnippetWindow> = Vec::new();
    for candidate in candidates {
        if selected
            .iter()
            .all(|existing| !windows_overlap(existing, &candidate))
        {
            selected.push(candidate);
        }
        if selected.len() == 2 {
            break;
        }
    }
    selected.sort_by(|a, b| a.start.cmp(&b.start));

    let mut snippet = String::new();
    for window in selected {
        if !snippet.is_empty() {
            snippet.push_str(" ... ");
        }
        snippet.push_str(
            &text
                .chars()
                .skip(window.start)
                .take(window.end - window.start)
                .collect::<String>(),
        );
    }
    snippet.chars().take(max_chars).collect()
}

#[derive(Debug, Clone)]
struct SnippetWindow {
    start: usize,
    end: usize,
    unique_terms: usize,
    total_matches: usize,
}

fn windows_overlap(a: &SnippetWindow, b: &SnippetWindow) -> bool {
    a.start < b.end && b.start < a.end
}

fn message_timestamp(msg: &Message) -> DateTime<Utc> {
    msg.metadata
        .as_ref()
        .and_then(|metadata| metadata.get("timestamp"))
        .and_then(|value| value.as_str())
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(epoch)
}

fn epoch() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(0, 0).expect("unix epoch should be valid")
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

#[cfg(test)]
mod tests {
    use super::*;
    use remi_agentloop::types::{FunctionCall, ToolCallMessage};
    use serde_json::json;

    fn test_store(data_dir: PathBuf) -> MemoryStore {
        MemoryStore {
            memory_dir: data_dir.join("memory"),
            data_dir,
            agent_md_path: None,
            compressor: LlmCompressor::new(
                "test-key".to_string(),
                None,
                "gpt-4o-mini".to_string(),
                128_000,
                4096,
                serde_json::Map::new(),
            ),
            short_term_tokens: 8192,
            auto_compress: false,
            memory_days: 7,
        }
    }

    fn dt(raw: &str) -> DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn coverage_uses_furthest_committed_entry_across_mid_and_long_term() {
        let messages = vec![
            Message::user("one"),
            Message::assistant("two"),
            Message::user("three"),
        ];
        let entry = |last_message_id: String| MemoryEntry {
            uuid: Uuid::new_v4().to_string(),
            created_at: Utc::now(),
            preview: String::new(),
            first_message_id: None,
            last_message_id: Some(last_message_id),
            message_count: None,
            status: Some("committed".to_string()),
            retained_user_budget: None,
            keep_recent_raw: None,
        };
        let mid = MemoryIndex {
            entries: vec![entry(messages[0].id.to_string())],
        };
        let long = MemoryIndex {
            entries: vec![entry(messages[1].id.to_string())],
        };

        assert_eq!(
            last_covered_position_across_tiers(&messages, &mid, &long),
            Some(1)
        );
    }

    #[test]
    fn covered_history_does_not_reenter_when_recent_tool_chain_is_incomplete() {
        let mut messages = (0..170)
            .map(|index| {
                if index % 2 == 0 {
                    Message::user(format!("exact original user input {index}"))
                } else {
                    Message::assistant(format!("answer {index}"))
                }
            })
            .collect::<Vec<_>>();
        let mut incomplete = Message::assistant("unfinished tool call");
        incomplete.tool_calls = Some(vec![ToolCallMessage {
            id: "missing-result".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "lookup".to_string(),
                arguments: "{}".to_string(),
            },
        }]);
        messages.push(incomplete);
        for index in 171..186 {
            messages.push(Message::user(format!("exact original user input {index}")));
        }
        assert_eq!(messages.len(), 186);
        assert_eq!(recent_complete_start(&messages, 10), 0);
        let (after_first, _) = visible_ledger_messages(&messages, Some(0), 20_000, true);
        let (after_second, _) = visible_ledger_messages(&messages, Some(26), 20_000, true);
        assert_eq!(after_first.len(), 185);
        assert_eq!(after_second.len(), 159);
        let (recent_only, _) = visible_ledger_messages(&messages, Some(185), 20_000, true);
        assert_eq!(recent_only.len(), 10);
        let (fully_covered, retained_users) =
            visible_ledger_messages(&messages, Some(185), 20_000, false);
        assert!(fully_covered.is_empty());
        assert_eq!(
            retained_users.last().unwrap().content.text_content(),
            "exact original user input 185"
        );
    }

    #[tokio::test]
    async fn committed_context_window_survives_reload_without_reinjecting_covered_raw() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());
        let thread = "window-checkpoint-restart";
        let messages = (0..20)
            .map(|index| {
                if index % 2 == 0 {
                    Message::user(format!("original input {index}"))
                } else {
                    Message::assistant(format!("answer {index}"))
                }
            })
            .collect::<Vec<_>>();
        MemoryStore::append_short_term(&store.short_term_path(thread), &messages)
            .await
            .unwrap();
        let mid_dir = store.mid_term_dir(thread);
        tokio::fs::create_dir_all(&mid_dir).await.unwrap();
        tokio::fs::write(mid_dir.join("checkpoint.md"), "summary")
            .await
            .unwrap();
        MemoryStore::write_index(
            &mid_dir,
            &MemoryIndex {
                entries: vec![MemoryEntry {
                    uuid: "checkpoint".to_string(),
                    created_at: Utc::now(),
                    preview: "summary".to_string(),
                    first_message_id: Some(messages[0].id.to_string()),
                    last_message_id: Some(messages[19].id.to_string()),
                    message_count: Some(20),
                    status: Some("committed".to_string()),
                    retained_user_budget: Some(0),
                    keep_recent_raw: Some(false),
                }],
            },
        )
        .await
        .unwrap();
        let restored_store = test_store(tmp.path().to_path_buf());
        let restored = restored_store.load_context(thread).await.unwrap();
        assert!(restored.short_term.is_empty());
        assert!(restored.retained_user_inputs.is_empty());
        assert_eq!(restored.latest_summary.as_deref(), Some("summary"));
        assert_eq!(restored_store.thread_history(thread).await.len(), 20);
    }

    #[tokio::test]
    async fn save_turn_keeps_recent_messages_in_append_only_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = test_store(tmp.path().to_path_buf());
        store.auto_compress = true;
        store.short_term_tokens = 15;
        let mut events = Vec::new();
        let mut sink = |event| events.push(event);

        store
            .save_turn_with_compaction_events(
                "thread-1",
                vec![
                    Message::tool_result("call-1", ""),
                    Message::tool_result("call-2", ""),
                ],
                Some(&mut sink),
            )
            .await
            .unwrap();

        assert!(events.is_empty());
        assert_eq!(store.thread_history("thread-1").await.len(), 2);
    }

    #[tokio::test]
    async fn save_turn_never_runs_post_turn_compression() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = test_store(tmp.path().to_path_buf());
        store.auto_compress = true;
        store.short_term_tokens = 1;
        let messages = (0..20)
            .map(|index| {
                if index % 2 == 0 {
                    Message::user(format!("user-{index} {}", "large ".repeat(100)))
                } else {
                    Message::assistant(format!("assistant-{index} {}", "large ".repeat(100)))
                }
            })
            .collect::<Vec<_>>();

        tokio::time::timeout(
            // `save_turn` fsyncs the append-only ledger. That can exceed a
            // sub-second deadline on CI or virtualized filesystems even though
            // no compressor/network work is performed.
            std::time::Duration::from_secs(2),
            store.save_turn("append-only-save", messages),
        )
        .await
        .expect("save_turn must not wait for the compressor")
        .unwrap();

        assert_eq!(store.thread_history("append-only-save").await.len(), 20);
        assert!(
            MemoryStore::read_index_checked(&store.mid_term_dir("append-only-save"))
                .await
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[tokio::test]
    async fn unavailable_compaction_target_does_not_drop_recent_ledger_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = test_store(tmp.path().to_path_buf());
        store.auto_compress = true;
        store.short_term_tokens = 15;
        let mid_dir = store.mid_term_dir("thread-1");
        tokio::fs::create_dir_all(mid_dir.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&mid_dir, "not a directory").await.unwrap();
        let mut events = Vec::new();
        let mut sink = |event| events.push(event);

        store
            .save_turn_with_compaction_events(
                "thread-1",
                vec![
                    Message::tool_result("call-1", ""),
                    Message::tool_result("call-2", ""),
                ],
                Some(&mut sink),
            )
            .await
            .unwrap();

        assert!(events.is_empty());
        assert_eq!(store.thread_history("thread-1").await.len(), 2);
    }

    #[tokio::test]
    async fn load_context_keeps_uncovered_messages_before_recent_window() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());
        let thread_id = "thread-no-coverage-gap";
        let messages = (0..20)
            .map(|i| {
                if i % 2 == 0 {
                    Message::user(format!("user-{i}"))
                } else {
                    Message::assistant(format!("assistant-{i}"))
                }
            })
            .collect::<Vec<_>>();
        MemoryStore::append_short_term(&store.short_term_path(thread_id), &messages)
            .await
            .unwrap();

        let entry = MemoryEntry {
            uuid: "summary-1".to_string(),
            created_at: Utc::now(),
            preview: "covered first six".to_string(),
            first_message_id: Some(messages[0].id.to_string()),
            last_message_id: Some(messages[5].id.to_string()),
            message_count: Some(6),
            status: Some("committed".to_string()),
            retained_user_budget: None,
            keep_recent_raw: None,
        };
        let mid_dir = store.mid_term_dir(thread_id);
        tokio::fs::create_dir_all(&mid_dir).await.unwrap();
        tokio::fs::write(mid_dir.join("summary-1.md"), "committed summary")
            .await
            .unwrap();
        MemoryStore::write_index(
            &mid_dir,
            &MemoryIndex {
                entries: vec![entry],
            },
        )
        .await
        .unwrap();

        let context = store.load_context(thread_id).await.unwrap();
        assert_eq!(context.short_term.len(), 14);
        assert_eq!(context.short_term[0].id, messages[6].id);
        assert_eq!(context.short_term.last().unwrap().id, messages[19].id);
        assert_eq!(context.latest_summary.as_deref(), Some("committed summary"));

        // A summary boundary may overlap the complete-exchange expansion of
        // the recent window.  The overlap must not suppress the whole summary,
        // because it also carries facts from older, non-overlapping messages.
        let overlapping_entry = MemoryEntry {
            uuid: "summary-1".to_string(),
            created_at: Utc::now(),
            preview: "covered first twelve".to_string(),
            first_message_id: Some(messages[0].id.to_string()),
            last_message_id: Some(messages[11].id.to_string()),
            message_count: Some(12),
            status: Some("committed".to_string()),
            retained_user_budget: None,
            keep_recent_raw: None,
        };
        MemoryStore::write_index(
            &mid_dir,
            &MemoryIndex {
                entries: vec![overlapping_entry],
            },
        )
        .await
        .unwrap();
        let overlapping = store.load_context(thread_id).await.unwrap();
        assert_eq!(overlapping.short_term.len(), 10);
        assert_eq!(overlapping.short_term[0].id, messages[10].id);
        assert_eq!(
            overlapping.latest_summary.as_deref(),
            Some("committed summary")
        );
    }

    #[test]
    fn protocol_safe_history_preserves_complete_tool_chain() {
        let mut assistant = Message::assistant("");
        assistant.tool_calls = Some(vec![ToolCallMessage {
            id: "call-1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "lookup".to_string(),
                arguments: "{\"id\":1}".to_string(),
            },
        }]);
        let result = Message::tool_result("call-1", "found");

        let safe = protocol_safe_history(&[assistant.clone(), result.clone()]);
        assert_eq!(safe.len(), 2);
        assert_eq!(safe[0].id, assistant.id);
        assert_eq!(safe[1].id, result.id);
        assert_eq!(safe[1].role, Role::Tool);
    }

    #[test]
    fn protocol_safe_history_neutralizes_incomplete_and_orphaned_tool_messages() {
        let mut assistant = Message::assistant("working");
        assistant.tool_calls = Some(vec![ToolCallMessage {
            id: "call-1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "lookup".to_string(),
                arguments: "{}".to_string(),
            },
        }]);
        let wrong_result = Message::tool_result("other-call", "orphan data");

        let safe = protocol_safe_history(&[assistant, wrong_result]);
        assert_eq!(safe.len(), 1);
        assert_eq!(safe[0].role, Role::System);
        let text = safe[0].content.text_content();
        assert!(text.contains("INCOMPLETE HISTORICAL TOOL ACTIVITY"));
        assert!(text.contains("lookup({}) id=call-1"));
        assert!(text.contains("orphan data"));

        let orphan = protocol_safe_history(&[Message::tool_result("call-2", "standalone")]);
        assert_eq!(orphan.len(), 1);
        assert_eq!(orphan[0].role, Role::System);
        assert!(orphan[0]
            .content
            .text_content()
            .contains("ORPHANED HISTORICAL TOOL RESULT"));
    }

    #[tokio::test]
    async fn supervisor_workflow_state_round_trips_with_thread_user_state() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());
        let instance = crate::supervisor_workflow::WorkflowInstance {
            definition: crate::supervisor_workflow::embedded_goal_definition(),
            context: json!({"goal": "finish the release"}),
            current_node: "review".to_string(),
            incoming_edge: None,
            node_message: None,
            status: crate::supervisor_workflow::WorkflowStatus::Paused,
            pause_reason: None,
            ask_user_for_help: None,
            max_rounds: crate::supervisor_workflow::WorkflowMaxRounds::Limited(20),
            last_report: None,
            updated_at: Utc::now(),
        };
        let mut user_state = json!({"__todos": []});
        crate::supervisor_workflow::set_instance_in_user_state(&mut user_state, &instance).unwrap();
        store
            .save_user_state("session-1", &user_state)
            .await
            .unwrap();

        let restored_state = test_store(tmp.path().to_path_buf())
            .load_user_state("session-1")
            .await;
        let restored =
            crate::supervisor_workflow::instance_from_user_state(&restored_state).unwrap();
        assert_eq!(restored, instance);
        assert_eq!(restored_state["__todos"], json!([]));
    }

    #[tokio::test]
    async fn background_task_completion_stays_in_context_but_is_internal_ui_history() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());
        let mut message = Message::user("Background tool task completed.");
        message.metadata = Some(serde_json::json!({"background_tool_task": true}));
        let legacy_message = Message::user(
            "[Background tool task completed while this run was active]\nlegacy completion",
        );
        store
            .save_turn("thread-background", vec![message, legacy_message])
            .await
            .unwrap();

        let context = store.load_context("thread-background").await.unwrap();
        assert_eq!(context.short_term.len(), 2);
        assert_eq!(
            context.short_term[0].content.text_content(),
            "Background tool task completed."
        );
        let history = store.thread_history("thread-background").await;
        assert_eq!(history.len(), 2);
        assert!(history.iter().all(|message| message.role == "internal"));
    }

    #[test]
    fn snippet_includes_later_relevant_window_for_repeated_terms() {
        let text = "\
**user:** I've been enjoying audiobooks a lot lately, especially during my daily commute.

**assistant:** Here are several fiction audiobook recommendations with strong narration.

**user:** Those worked well. I've been listening to audiobooks during my daily commute, which takes 45 minutes each way.
";

        let query = tokenized_search_query("How long is my daily commute to work?").unwrap();
        let snippet = make_snippet(text, &query, MEMORY_RECALL_SNIPPET_CHARS);
        assert!(snippet.contains("daily commute"));
        assert!(snippet.contains("45 minutes each way"));

        let query = tokenized_search_query("commute work minutes").unwrap();
        let snippet = make_snippet(text, &query, MEMORY_RECALL_SNIPPET_CHARS);
        assert!(snippet.contains("45 minutes each way"));
    }

    #[test]
    fn snippet_handles_lowercase_expansion_without_slicing_original_at_lower_byte_offset() {
        let text = "记忆：İstanbul 的偏好是坐在靠窗位置。";
        let query = tokenized_search_query("istanbul 靠窗").unwrap();

        let snippet = make_snippet(text, &query, MEMORY_RECALL_SNIPPET_CHARS);

        assert!(snippet.contains("İstanbul"));
        assert!(snippet.contains("靠窗"));
    }

    #[tokio::test]
    async fn recall_tokenizes_cjk_natural_language_query() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());
        store
            .upsert_named_memory("default", "travel", "Istanbul 的偏好是坐在靠窗位置。")
            .await
            .unwrap();

        let results = store
            .recall("default", "thread-1", "我想找靠窗座位", 8)
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].snippet.contains("靠窗"));
    }

    #[tokio::test]
    async fn named_memory_create_update_and_preserve_created() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());

        let first = store
            .upsert_named_memory("default", "project-pref", "Prefer concise updates.")
            .await
            .unwrap();
        assert_eq!(first.name, "project-pref.md");
        assert!(first.path.ends_with("memory/named/default/project-pref.md"));

        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let second = store
            .upsert_named_memory("default", "project-pref", "Prefer concise Chinese updates.")
            .await
            .unwrap();
        assert_eq!(second.created_at, first.created_at);
        assert!(second.updated_at >= first.updated_at);

        let text = tokio::fs::read_to_string(second.path).await.unwrap();
        assert!(text.contains("<!-- created: "));
        assert!(text.contains("<!-- updated: "));
        assert!(text.contains("Prefer concise Chinese updates."));
    }

    #[tokio::test]
    async fn named_memory_rejects_unsafe_names() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());
        for name in ["", ".", "..", "../x", "a/b", "a\\b"] {
            assert!(
                store
                    .upsert_named_memory("default", name, "content")
                    .await
                    .is_err(),
                "{name:?} should be rejected"
            );
        }
    }

    #[tokio::test]
    async fn named_memory_is_agent_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());
        store
            .upsert_named_memory("planner", "prefs", "Planner remembers blue widgets.")
            .await
            .unwrap();

        let planner = store
            .recall("planner", "thread-1", "blue widgets", 8)
            .await
            .unwrap();
        let coder = store
            .recall("coder", "thread-1", "blue widgets", 8)
            .await
            .unwrap();
        assert_eq!(planner.len(), 1);
        assert!(coder.is_empty());
    }

    #[tokio::test]
    async fn recall_searches_named_short_mid_and_long_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());
        store
            .upsert_named_memory("default", "preferences", "User prefers rust memory recall.")
            .await
            .unwrap();

        let mut msg = Message::user("Short term says recall keyword lives here.");
        msg.metadata = Some(json!({ "timestamp": "2026-05-01T00:00:00Z" }));
        store.save_turn("thread-1", vec![msg]).await.unwrap();

        let mid_dir = store.mid_term_dir("thread-1");
        let long_dir = store.long_term_dir("thread-1");
        tokio::fs::create_dir_all(&mid_dir).await.unwrap();
        tokio::fs::create_dir_all(&long_dir).await.unwrap();
        let mid_uuid = Uuid::new_v4().to_string();
        let long_uuid = Uuid::new_v4().to_string();
        tokio::fs::write(
            mid_dir.join(format!("{mid_uuid}.md")),
            "Mid summary contains rust recall details.",
        )
        .await
        .unwrap();
        tokio::fs::write(
            long_dir.join(format!("{long_uuid}.md")),
            "Long summary contains old recall details.",
        )
        .await
        .unwrap();
        MemoryStore::write_index(
            &mid_dir,
            &MemoryIndex {
                entries: vec![MemoryEntry {
                    uuid: mid_uuid,
                    created_at: dt("2026-05-02T00:00:00Z"),
                    preview: "mid".to_string(),
                    first_message_id: None,
                    last_message_id: None,
                    message_count: None,
                    status: None,
                    retained_user_budget: None,
                    keep_recent_raw: None,
                }],
            },
        )
        .await
        .unwrap();
        MemoryStore::write_index(
            &long_dir,
            &MemoryIndex {
                entries: vec![MemoryEntry {
                    uuid: long_uuid,
                    created_at: dt("2026-05-03T00:00:00Z"),
                    preview: "long".to_string(),
                    first_message_id: None,
                    last_message_id: None,
                    message_count: None,
                    status: None,
                    retained_user_budget: None,
                    keep_recent_raw: None,
                }],
            },
        )
        .await
        .unwrap();

        let sources: std::collections::BTreeSet<_> = store
            .recall("default", "thread-1", "recall", 8)
            .await
            .unwrap()
            .into_iter()
            .map(|result| result.source)
            .collect();
        assert!(sources.contains("named"));
        assert!(sources.contains("short_term"));
        assert!(sources.contains("mid_term"));
        assert!(sources.contains("long_term"));
    }

    #[tokio::test]
    async fn recall_excludes_recursive_memory_search_tool_results() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());
        let tool_call = |id: &str, name: &str, arguments: serde_json::Value| {
            let mut message = Message::assistant("");
            message.tool_calls = Some(vec![ToolCallMessage {
                id: id.to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: name.to_string(),
                    arguments: arguments.to_string(),
                },
            }]);
            message
        };

        let original = Message::user("recursive-target original user fact");
        let recall_result =
            Message::tool_result("recall-1", "recursive-target nested memory__recall payload");
        let recall_result_id = recall_result.id.to_string();
        let search_result =
            Message::tool_result("search-1", "recursive-target nested search payload");
        let search_result_id = search_result.id.to_string();
        let ordinary_result =
            Message::tool_result("search-2", "recursive-target ordinary skills payload");
        let ordinary_result_id = ordinary_result.id.to_string();

        store
            .save_turn(
                "thread-1",
                vec![
                    original,
                    tool_call("recall-1", "memory__recall", json!({"query": "target"})),
                    recall_result,
                    tool_call(
                        "search-1",
                        "search",
                        json!({"query": "target", "scope": "memory"}),
                    ),
                    search_result,
                    tool_call(
                        "search-2",
                        "search",
                        json!({"query": "target", "scope": "skills"}),
                    ),
                    ordinary_result,
                ],
            )
            .await
            .unwrap();

        let results = store
            .recall("default", "thread-1", "recursive-target", 20)
            .await
            .unwrap();
        let ids = results
            .iter()
            .filter_map(|result| result.uuid.as_deref())
            .collect::<HashSet<_>>();

        assert!(!ids.contains(recall_result_id.as_str()));
        assert!(!ids.contains(search_result_id.as_str()));
        assert!(ids.contains(ordinary_result_id.as_str()));
        assert!(results
            .iter()
            .any(|result| result.name.as_deref() == Some("User")));
    }

    #[tokio::test]
    async fn recall_orders_by_score_then_timestamp_and_caps_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());

        let mut older_high = Message::user("alpha alpha alpha");
        older_high.metadata = Some(json!({ "timestamp": "2026-05-01T00:00:00Z" }));
        let mut newer_low = Message::user("alpha");
        newer_low.metadata = Some(json!({ "timestamp": "2026-05-03T00:00:00Z" }));
        let mut newer_same = Message::user("alpha alpha alpha");
        newer_same.metadata = Some(json!({ "timestamp": "2026-05-02T00:00:00Z" }));
        store
            .save_turn("thread-1", vec![older_high, newer_low, newer_same])
            .await
            .unwrap();

        for i in 0..25 {
            store
                .upsert_named_memory("default", &format!("extra-{i}"), "alpha")
                .await
                .unwrap();
        }

        let results = store
            .recall("default", "thread-1", "alpha", 100)
            .await
            .unwrap();
        assert_eq!(results.len(), 20);
        assert_eq!(results[0].score, 13);
        assert_eq!(results[0].timestamp, dt("2026-05-02T00:00:00Z"));
        assert_eq!(results[1].score, 13);
        assert_eq!(results[1].timestamp, dt("2026-05-01T00:00:00Z"));
    }

    #[tokio::test]
    async fn corrupt_ledger_or_index_stops_context_load_without_discarding_history() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());
        let thread = "corrupt-thread";
        store
            .save_turn(thread, vec![Message::user("retain this message")])
            .await
            .unwrap();
        let ledger_path = store.short_term_path(thread);
        let original_ledger = tokio::fs::read(&ledger_path).await.unwrap();

        tokio::fs::write(&ledger_path, "{invalid json}\n")
            .await
            .unwrap();
        let err = store.load_context(thread).await.err().unwrap();
        assert!(err.to_string().contains("invalid memory ledger"));
        let err = store.compact_for_request(thread).await.unwrap_err();
        assert!(err.to_string().contains("invalid memory ledger"));
        tokio::fs::write(&ledger_path, &original_ledger)
            .await
            .unwrap();

        let mid_dir = store.mid_term_dir(thread);
        tokio::fs::create_dir_all(&mid_dir).await.unwrap();
        tokio::fs::write(mid_dir.join("index.json"), "{invalid json}")
            .await
            .unwrap();
        let err = store.load_context(thread).await.err().unwrap();
        assert!(err.to_string().contains("invalid memory index"));
        assert_eq!(
            tokio::fs::read(&ledger_path).await.unwrap(),
            original_ledger
        );

        let missing_id = Uuid::new_v4().to_string();
        MemoryStore::write_index(
            &mid_dir,
            &MemoryIndex {
                entries: vec![MemoryEntry {
                    uuid: missing_id,
                    created_at: Utc::now(),
                    preview: "missing".to_string(),
                    first_message_id: None,
                    last_message_id: None,
                    message_count: None,
                    status: Some("committed".to_string()),
                    retained_user_budget: None,
                    keep_recent_raw: None,
                }],
            },
        )
        .await
        .unwrap();
        let err = store.load_context(thread).await.err().unwrap();
        assert!(err.to_string().contains(".md"));
    }
}
