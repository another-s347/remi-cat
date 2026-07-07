use crate::error::AgentError;
use crate::types::{Message, MessageId, RunId, ThreadId};
use std::rc::Rc;
use std::sync::Arc;

/// Persistent storage for conversation threads and messages.
///
/// Implement this trait to back conversation history with any storage system
/// (in-memory, SQLite, PostgreSQL, Redis, …).  The framework ships
/// [`InMemoryStore`] for development and testing, and [`NoStore`] for
/// stateless operation.
///
/// All methods are async and use RPITIT — no `Send` bound, compatible with
/// single-threaded `wasm32` targets.
///
/// # Example — custom store
///
/// ```ignore
/// use remi_agentloop_core::context::ContextStore;
/// use remi_agentloop_core::error::AgentError;
/// use remi_agentloop_core::types::*;
///
/// struct MyStore { /* … */ }
///
/// impl ContextStore for MyStore {
///     async fn create_thread(&self) -> Result<ThreadId, AgentError> {
///         Ok(ThreadId::new())
///     }
///     // … implement remaining methods
/// }
/// ```
pub trait ContextStore {
    async fn create_thread(&self) -> Result<ThreadId, AgentError>;
    async fn get_messages(&self, thread_id: &ThreadId) -> Result<Vec<Message>, AgentError>;
    async fn get_recent_messages(
        &self,
        thread_id: &ThreadId,
        limit: usize,
    ) -> Result<Vec<Message>, AgentError>;
    async fn append_message(
        &self,
        thread_id: &ThreadId,
        message: Message,
    ) -> Result<MessageId, AgentError>;
    async fn append_messages(
        &self,
        thread_id: &ThreadId,
        messages: Vec<Message>,
    ) -> Result<Vec<MessageId>, AgentError>;
    async fn delete_thread(&self, thread_id: &ThreadId) -> Result<(), AgentError>;
    async fn create_run(&self, thread_id: &ThreadId) -> Result<RunId, AgentError>;
    async fn complete_run(&self, run_id: &RunId) -> Result<(), AgentError>;
}

// ── NoStore —— placeholder when no store is needed ───────────────────────────

/// Marker: no context store — operates in fully stateless mode.
///
/// When the builder uses `NoStore`, `create_thread` always returns a fresh
/// [`ThreadId`] and message persistence is a no-op.  Use this when you only
/// need the agent loop without persistent history.
pub struct NoStore;

impl ContextStore for NoStore {
    async fn create_thread(&self) -> Result<ThreadId, AgentError> {
        Ok(ThreadId::new())
    }
    async fn get_messages(&self, _: &ThreadId) -> Result<Vec<Message>, AgentError> {
        Ok(vec![])
    }
    async fn get_recent_messages(
        &self,
        _: &ThreadId,
        _: usize,
    ) -> Result<Vec<Message>, AgentError> {
        Ok(vec![])
    }
    async fn append_message(&self, _: &ThreadId, msg: Message) -> Result<MessageId, AgentError> {
        Ok(msg.id.clone())
    }
    async fn append_messages(
        &self,
        _: &ThreadId,
        msgs: Vec<Message>,
    ) -> Result<Vec<MessageId>, AgentError> {
        Ok(msgs.iter().map(|m| m.id.clone()).collect())
    }
    async fn delete_thread(&self, _: &ThreadId) -> Result<(), AgentError> {
        Ok(())
    }
    async fn create_run(&self, _: &ThreadId) -> Result<RunId, AgentError> {
        Ok(RunId::new())
    }
    async fn complete_run(&self, _: &RunId) -> Result<(), AgentError> {
        Ok(())
    }
}

// ── InMemoryStore ─────────────────────────────────────────────────────────────

use std::collections::HashMap;
use std::sync::Mutex;

/// In-process, heap-backed context store for development and testing.
///
/// Thread-safe (backed by `Arc<Mutex<…>>`) but not durable — all state is
/// lost when the process exits.  Suitable for unit tests, examples, and
/// single-process servers that don't require cross-restart history.
///
/// # Example
///
/// ```ignore
/// use remi_agentloop_core::prelude::*;
///
/// let agent = AgentBuilder::new()
///     .model(oai)
///     .context_store(InMemoryStore::new())
///     .build();
///
/// let tid = agent.create_thread().await?;
/// ```
#[derive(Debug, Default, Clone)]
pub struct InMemoryStore {
    inner: Arc<Mutex<InMemoryStoreInner>>,
}

#[derive(Debug, Default)]
struct InMemoryStoreInner {
    threads: HashMap<String, Vec<Message>>,
    runs: HashMap<String, String>, // run_id → thread_id
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ContextStore for InMemoryStore {
    async fn create_thread(&self) -> Result<ThreadId, AgentError> {
        let tid = ThreadId::new();
        self.inner
            .lock()
            .unwrap()
            .threads
            .insert(tid.0.clone(), vec![]);
        Ok(tid)
    }

    async fn get_messages(&self, thread_id: &ThreadId) -> Result<Vec<Message>, AgentError> {
        self.inner
            .lock()
            .unwrap()
            .threads
            .get(&thread_id.0)
            .cloned()
            .ok_or_else(|| AgentError::ThreadNotFound(thread_id.clone()))
    }

    async fn get_recent_messages(
        &self,
        thread_id: &ThreadId,
        limit: usize,
    ) -> Result<Vec<Message>, AgentError> {
        let guard = self.inner.lock().unwrap();
        let msgs = guard
            .threads
            .get(&thread_id.0)
            .ok_or_else(|| AgentError::ThreadNotFound(thread_id.clone()))?;
        let skip = msgs.len().saturating_sub(limit);
        Ok(msgs[skip..].to_vec())
    }

    async fn append_message(
        &self,
        thread_id: &ThreadId,
        message: Message,
    ) -> Result<MessageId, AgentError> {
        let mut guard = self.inner.lock().unwrap();
        let msgs = guard.threads.entry(thread_id.0.clone()).or_default();
        let id = message.id.clone();
        msgs.push(message);
        Ok(id)
    }

    async fn append_messages(
        &self,
        thread_id: &ThreadId,
        messages: Vec<Message>,
    ) -> Result<Vec<MessageId>, AgentError> {
        let mut guard = self.inner.lock().unwrap();
        let msgs = guard.threads.entry(thread_id.0.clone()).or_default();
        let ids: Vec<_> = messages.iter().map(|m| m.id.clone()).collect();
        msgs.extend(messages);
        Ok(ids)
    }

    async fn delete_thread(&self, thread_id: &ThreadId) -> Result<(), AgentError> {
        self.inner.lock().unwrap().threads.remove(&thread_id.0);
        Ok(())
    }

    async fn create_run(&self, thread_id: &ThreadId) -> Result<RunId, AgentError> {
        let rid = RunId::new();
        self.inner
            .lock()
            .unwrap()
            .runs
            .insert(rid.0.clone(), thread_id.0.clone());
        Ok(rid)
    }

    async fn complete_run(&self, run_id: &RunId) -> Result<(), AgentError> {
        self.inner.lock().unwrap().runs.remove(&run_id.0);
        Ok(())
    }
}

// ── Rc<S> blanket impl — enables shared store for Sub-Agent pattern ──────────

impl<S: ContextStore> ContextStore for Rc<S> {
    async fn create_thread(&self) -> Result<ThreadId, AgentError> {
        (**self).create_thread().await
    }
    async fn get_messages(&self, thread_id: &ThreadId) -> Result<Vec<Message>, AgentError> {
        (**self).get_messages(thread_id).await
    }
    async fn get_recent_messages(
        &self,
        thread_id: &ThreadId,
        limit: usize,
    ) -> Result<Vec<Message>, AgentError> {
        (**self).get_recent_messages(thread_id, limit).await
    }
    async fn append_message(
        &self,
        thread_id: &ThreadId,
        message: Message,
    ) -> Result<MessageId, AgentError> {
        (**self).append_message(thread_id, message).await
    }
    async fn append_messages(
        &self,
        thread_id: &ThreadId,
        messages: Vec<Message>,
    ) -> Result<Vec<MessageId>, AgentError> {
        (**self).append_messages(thread_id, messages).await
    }
    async fn delete_thread(&self, thread_id: &ThreadId) -> Result<(), AgentError> {
        (**self).delete_thread(thread_id).await
    }
    async fn create_run(&self, thread_id: &ThreadId) -> Result<RunId, AgentError> {
        (**self).create_run(thread_id).await
    }
    async fn complete_run(&self, run_id: &RunId) -> Result<(), AgentError> {
        (**self).complete_run(run_id).await
    }
}

// ── Arc<S> blanket impl — enables shared store across Send boundaries ────────

impl<S: ContextStore> ContextStore for Arc<S> {
    async fn create_thread(&self) -> Result<ThreadId, AgentError> {
        (**self).create_thread().await
    }
    async fn get_messages(&self, thread_id: &ThreadId) -> Result<Vec<Message>, AgentError> {
        (**self).get_messages(thread_id).await
    }
    async fn get_recent_messages(
        &self,
        thread_id: &ThreadId,
        limit: usize,
    ) -> Result<Vec<Message>, AgentError> {
        (**self).get_recent_messages(thread_id, limit).await
    }
    async fn append_message(
        &self,
        thread_id: &ThreadId,
        message: Message,
    ) -> Result<MessageId, AgentError> {
        (**self).append_message(thread_id, message).await
    }
    async fn append_messages(
        &self,
        thread_id: &ThreadId,
        messages: Vec<Message>,
    ) -> Result<Vec<MessageId>, AgentError> {
        (**self).append_messages(thread_id, messages).await
    }
    async fn delete_thread(&self, thread_id: &ThreadId) -> Result<(), AgentError> {
        (**self).delete_thread(thread_id).await
    }
    async fn create_run(&self, thread_id: &ThreadId) -> Result<RunId, AgentError> {
        (**self).create_run(thread_id).await
    }
    async fn complete_run(&self, run_id: &RunId) -> Result<(), AgentError> {
        (**self).complete_run(run_id).await
    }
}

// ── ContextStoreExt — convenience methods ────────────────────────────────────

/// Extension trait for [`ContextStore`] — provides convenience methods
/// like conversation forking.
pub trait ContextStoreExt: ContextStore {
    /// Fork a thread — copy messages up to (and including) `up_to_message`
    /// into a new thread.
    ///
    /// The new thread shares history up to the fork point and then evolves
    /// independently. Each copied message gets a fresh [`MessageId`].
    async fn fork_thread(
        &self,
        source: &ThreadId,
        up_to_message: &MessageId,
    ) -> Result<ThreadId, AgentError> {
        let messages = self.get_messages(source).await?;
        let idx = messages
            .iter()
            .position(|m| m.id == *up_to_message)
            .ok_or(AgentError::MessageNotFound(up_to_message.clone()))?;
        let forked: Vec<Message> = messages[..=idx]
            .iter()
            .map(|m| Message {
                id: MessageId::new(),
                ..m.clone()
            })
            .collect();
        let new_thread = self.create_thread().await?;
        self.append_messages(&new_thread, forked).await?;
        Ok(new_thread)
    }

    /// Clone messages up to and including `up_to_index` into a new thread.
    ///
    /// This is useful for replay/debug flows that restart from a historical
    /// point in the conversation while preserving the original thread.
    async fn fork_thread_at_index(
        &self,
        source: &ThreadId,
        up_to_index: usize,
    ) -> Result<ThreadId, AgentError> {
        let messages = self.get_messages(source).await?;
        if up_to_index >= messages.len() {
            return Err(AgentError::ReplayIndexOutOfBounds {
                thread_id: source.clone(),
                requested: up_to_index,
                available: messages.len(),
            });
        }

        let forked: Vec<Message> = messages[..=up_to_index]
            .iter()
            .map(|m| Message {
                id: MessageId::new(),
                ..m.clone()
            })
            .collect();
        let new_thread = self.create_thread().await?;
        self.append_messages(&new_thread, forked).await?;
        Ok(new_thread)
    }
}

// blanket impl — every ContextStore gets ContextStoreExt for free
impl<S: ContextStore> ContextStoreExt for S {}
