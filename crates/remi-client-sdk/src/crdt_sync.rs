use anyhow::{Context, Result};
use automerge::AutoCommit;
use automerge::sync::{self, SyncDoc};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

/// Trait describing the domain-specific parts of client-side CRDT usage.
///
/// The sync handshake is handled by `AutomergeSyncSession` and the helper functions in this
/// module. Each syncable domain (preferences, things, etc.) implements this trait to describe:
/// - how to initialize an Automerge document
/// - how to extract domain entries from the document
/// - how to apply local mutations (upsert/delete)
/// - what magic/version to use for persisted local state
pub trait CrdtModel {
    /// Stable persisted-file magic prefix for this domain.
    const PERSIST_MAGIC: &'static [u8];
    /// Persisted-file version for this domain.
    const PERSIST_VERSION: u8;

    type Entry;
    type Upsert;
    type Key: ?Sized;

    fn init_doc(entries: Vec<Self::Entry>) -> Result<Vec<u8>>;
    fn extract(doc_bytes: &[u8]) -> Result<Vec<Self::Entry>>;
    fn apply_upsert(doc_bytes: &[u8], upsert: Self::Upsert) -> Result<Vec<u8>>;
    fn apply_delete(doc_bytes: &[u8], key: &Self::Key) -> Result<Vec<u8>>;
}

pub fn device_actor_id(device_id: &str) -> automerge::ActorId {
    // Actor IDs must be stable per device and unique across devices.
    // Using the device_id bytes is deterministic and avoids inheriting the server actor
    // when bootstrapping from a server-provided snapshot.
    let bytes = if device_id.is_empty() {
        b"remi-device".to_vec()
    } else {
        device_id.as_bytes().to_vec()
    };
    automerge::ActorId::from(bytes)
}

/// Ensure the document uses a device-specific actor id.
///
/// Important when bootstrapping from a server snapshot: the snapshot may carry the server actor.
pub fn set_doc_actor(doc_bytes: &[u8], device_id: &str) -> Result<Vec<u8>> {
    let mut doc = AutoCommit::load(doc_bytes).context("Failed to load document")?;
    doc.set_actor(device_actor_id(device_id));
    Ok(doc.save())
}

/// Initialize a new automerge sync state (encoded bytes).
///
/// This uses `automerge::sync::State::encode()`, which persists only the state that should be
/// reused across connections (per Automerge docs).
pub fn init_sync_state() -> Vec<u8> {
    sync::State::new().encode()
}

pub fn decode_sync_state(sync_state_bytes: &[u8]) -> sync::State {
    if sync_state_bytes.is_empty() {
        return sync::State::new();
    }
    sync::State::decode(sync_state_bytes).unwrap_or_else(|_| sync::State::new())
}

/// Apply a server sync message to the client doc/state.
///
/// Returns updated `(doc_bytes, sync_state_bytes)`.
pub fn receive_sync_message(
    doc_bytes: &[u8],
    sync_state_bytes: &[u8],
    server_message_bytes: &[u8],
) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut doc = AutoCommit::load(doc_bytes).context("Failed to load document")?;
    let mut state = decode_sync_state(sync_state_bytes);

    if !server_message_bytes.is_empty() {
        let msg = sync::Message::decode(server_message_bytes)
            .context("Failed to decode server sync message")?;
        doc.sync()
            .receive_sync_message(&mut state, msg)
            .context("Failed to apply server sync message")?;
    }

    Ok((doc.save(), state.encode()))
}

/// Apply a batch of server sync messages to the client doc/state.
///
/// Returns updated `(doc_bytes, sync_state_bytes)`.
pub fn receive_sync_messages(
    doc_bytes: &[u8],
    sync_state_bytes: &[u8],
    server_messages: &[Vec<u8>],
) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut doc = AutoCommit::load(doc_bytes).context("Failed to load document")?;
    let mut state = decode_sync_state(sync_state_bytes);

    for bytes in server_messages {
        if bytes.is_empty() {
            continue;
        }
        let msg = sync::Message::decode(bytes).context("Failed to decode server sync message")?;
        doc.sync()
            .receive_sync_message(&mut state, msg)
            .context("Failed to apply server sync message")?;
    }

    Ok((doc.save(), state.encode()))
}

/// Generate the next client->server sync message.
///
/// Returns `(maybe_message_bytes, updated_sync_state_bytes)`.
pub fn generate_sync_message(
    doc_bytes: &[u8],
    sync_state_bytes: &[u8],
) -> Result<(Option<Vec<u8>>, Vec<u8>)> {
    let mut doc = AutoCommit::load(doc_bytes).context("Failed to load document")?;
    let mut state = decode_sync_state(sync_state_bytes);

    let msg = doc
        .sync()
        .generate_sync_message(&mut state)
        .map(|m| m.encode());

    Ok((msg, state.encode()))
}

/// Convenience for one unary-RPC round-trip style sync:
/// 1) receive optional server message
/// 2) generate next client message
///
/// Returns `(updated_doc_bytes, updated_sync_state_bytes, next_client_message_bytes)`.
pub fn sync_step(
    doc_bytes: &[u8],
    sync_state_bytes: &[u8],
    server_message_bytes: &[u8],
) -> Result<(Vec<u8>, Vec<u8>, Option<Vec<u8>>)> {
    let (doc_bytes, sync_state_bytes) =
        receive_sync_message(doc_bytes, sync_state_bytes, server_message_bytes)?;
    let (next_msg, sync_state_bytes) = generate_sync_message(&doc_bytes, &sync_state_bytes)?;
    Ok((doc_bytes, sync_state_bytes, next_msg))
}

/// In-memory sync session which keeps `sync::State` across multiple rounds.
///
/// Note: `sync::State::encode()` is intended for persistence between sessions.
/// For multi-round convergence loops, keep the `State` in memory and only
/// encode it when you want to persist.
pub struct AutomergeSyncSession {
    doc: AutoCommit,
    state: sync::State,
}

impl AutomergeSyncSession {
    pub fn new(doc_bytes: &[u8], sync_state_bytes: &[u8]) -> Result<Self> {
        let doc = AutoCommit::load(doc_bytes).context("Failed to load document")?;
        let state = decode_sync_state(sync_state_bytes);
        Ok(Self { doc, state })
    }

    pub fn new_with_device_id(
        doc_bytes: &[u8],
        sync_state_bytes: &[u8],
        device_id: &str,
    ) -> Result<Self> {
        let mut doc = AutoCommit::load(doc_bytes).context("Failed to load document")?;
        doc.set_actor(device_actor_id(device_id));
        let state = decode_sync_state(sync_state_bytes);
        Ok(Self { doc, state })
    }

    pub fn apply_server_messages(&mut self, server_messages: &[Vec<u8>]) -> Result<()> {
        for bytes in server_messages {
            if bytes.is_empty() {
                continue;
            }
            let msg =
                sync::Message::decode(bytes).context("Failed to decode server sync message")?;
            self.doc
                .sync()
                .receive_sync_message(&mut self.state, msg)
                .context("Failed to apply server sync message")?;
        }
        Ok(())
    }

    pub fn generate_client_message(&mut self) -> Option<Vec<u8>> {
        self.doc
            .sync()
            .generate_sync_message(&mut self.state)
            .map(|m| m.encode())
    }

    pub fn doc_bytes(&mut self) -> Vec<u8> {
        self.doc.save()
    }

    pub fn sync_state_bytes(&self) -> Vec<u8> {
        self.state.encode()
    }
}

pub struct PersistedSyncState {
    pub device_id: String,
    pub automerge_doc: Vec<u8>,
    pub sync_state: Vec<u8>,
}

/// Save `(device_id, automerge_doc, sync_state)` to a local file.
///
/// Format: magic + version + len-prefixed device_id/doc/state (u32 LE).
pub fn save_persisted_state(
    path: &Path,
    magic: &[u8],
    version: u8,
    device_id: &str,
    automerge_doc: &[u8],
    sync_state: &[u8],
) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(magic);
    buf.push(version);

    write_len_prefixed(&mut buf, device_id.as_bytes());
    write_len_prefixed(&mut buf, automerge_doc);
    write_len_prefixed(&mut buf, sync_state);

    let mut file = fs::File::create(path).with_context(|| {
        format!(
            "Failed to create persisted sync state file at '{}'",
            path.display()
        )
    })?;
    file.write_all(&buf).with_context(|| {
        format!(
            "Failed to write persisted sync state file at '{}'",
            path.display()
        )
    })?;
    Ok(())
}

/// Load `(device_id, automerge_doc, sync_state)` from a local file.
pub fn load_persisted_state(
    path: &Path,
    magic: &[u8],
    version: u8,
) -> Result<Option<PersistedSyncState>> {
    if !path.exists() {
        return Ok(None);
    }

    let mut file = fs::File::open(path).with_context(|| {
        format!(
            "Failed to open persisted sync state file at '{}'",
            path.display()
        )
    })?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).with_context(|| {
        format!(
            "Failed to read persisted sync state file at '{}'",
            path.display()
        )
    })?;

    if buf.len() < magic.len() + 1 {
        return Ok(None);
    }
    if &buf[..magic.len()] != magic {
        return Ok(None);
    }
    let got_version = buf[magic.len()];
    if got_version != version {
        return Ok(None);
    }

    let mut offset = magic.len() + 1;
    let device_id =
        read_len_prefixed(&buf, &mut offset).context("Failed to decode persisted device_id")?;
    let doc =
        read_len_prefixed(&buf, &mut offset).context("Failed to decode persisted automerge_doc")?;
    let state =
        read_len_prefixed(&buf, &mut offset).context("Failed to decode persisted sync_state")?;

    let device_id =
        String::from_utf8(device_id).context("Persisted device_id is not valid UTF-8")?;
    Ok(Some(PersistedSyncState {
        device_id,
        automerge_doc: doc,
        sync_state: state,
    }))
}

fn write_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
}

fn read_len_prefixed(buf: &[u8], offset: &mut usize) -> Result<Vec<u8>> {
    if *offset + 4 > buf.len() {
        anyhow::bail!("unexpected EOF reading length");
    }
    let len = u32::from_le_bytes(buf[*offset..*offset + 4].try_into().unwrap()) as usize;
    *offset += 4;
    if *offset + len > buf.len() {
        anyhow::bail!("unexpected EOF reading bytes");
    }
    let out = buf[*offset..*offset + len].to_vec();
    *offset += len;
    Ok(out)
}
