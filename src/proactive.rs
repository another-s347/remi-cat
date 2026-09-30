//! Durable, no-model assistant messages delivered to an existing IM session.
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use im_feishu::FeishuGateway;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

use crate::core::Runtime;

const MAX_TEXT_BYTES: usize = 20_000;
const MAX_WIRE_BYTES: usize = 128_000;
const DEDUPE_WINDOW: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DeliveryRequest {
    pub session_id: String,
    pub text: String,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DeliveryReceipt {
    pub session_id: String,
    pub delivery_id: String,
    pub idempotency_key: String,
    pub state: String,
    pub platform_message_id: Option<String>,
    pub error: Option<String>,
}

impl DeliveryReceipt {
    pub(crate) fn complete(&self) -> bool {
        self.state == "complete"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OutboxRecord {
    receipt: DeliveryReceipt,
    text: String,
    platform: String,
    channel_id: String,
    provider_uuid: String,
    first_attempt_unix: Option<u64>,
}

fn ensure_same_delivery(
    record: &OutboxRecord,
    text: &str,
    platform: &str,
    channel_id: &str,
) -> Result<()> {
    if record.text != text || record.platform != platform || record.channel_id != channel_id {
        bail!("idempotency key already belongs to different content or channel binding");
    }
    Ok(())
}

fn unconfirmed_retry_window_expired(record: &OutboxRecord, now: u64) -> bool {
    matches!(record.receipt.state.as_str(), "uncertain" | "sending")
        && record
            .first_attempt_unix
            .is_some_and(|started| now.saturating_sub(started) >= DEDUPE_WINDOW.as_secs())
}

pub(crate) struct ProfileLock(File);

impl ProfileLock {
    pub(crate) fn try_acquire(data_dir: &Path) -> Result<Option<Self>> {
        std::fs::create_dir_all(data_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(data_dir.join("proactive-runtime.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ControlDescriptor {
    port: u16,
    token: String,
    pid: u32,
}

#[derive(Debug, Serialize, Deserialize)]
enum ControlOperation {
    Send(DeliveryRequest),
    Status {
        session_id: String,
        idempotency_key: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct WireRequest {
    token: String,
    operation: ControlOperation,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireResponse {
    receipt: Option<DeliveryReceipt>,
    error: Option<String>,
}

pub(crate) struct ControlCommand {
    operation: ControlOperation,
    response: oneshot::Sender<WireResponse>,
}

pub(crate) struct ControlGuard {
    path: PathBuf,
    token: String,
}

impl Drop for ControlGuard {
    fn drop(&mut self) {
        let current = std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ControlDescriptor>(&bytes).ok());
        if current
            .as_ref()
            .is_some_and(|value| value.token == self.token)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn descriptor_path(data_dir: &Path) -> PathBuf {
    data_dir.join("proactive-control.json")
}

fn record_path(data_dir: &Path, session_id: &str, key: &str) -> PathBuf {
    let session_hash = hex::encode(Sha256::digest(session_id.as_bytes()));
    let key_hash = hex::encode(Sha256::digest(key.as_bytes()));
    data_dir
        .join("proactive-outbox")
        .join(session_hash)
        .join(format!("{key_hash}.json"))
}

fn topic_reply_anchor<'a>(
    channel_id: &str,
    metadata: &'a serde_json::Map<String, serde_json::Value>,
) -> Result<Option<&'a str>> {
    let Some((chat_id, thread_id)) = channel_id.rsplit_once(":thread:") else {
        if !channel_id.starts_with("oc_") {
            bail!("invalid Feishu chat binding");
        }
        return Ok(None);
    };
    if !chat_id.starts_with("oc_") || !thread_id.starts_with("omt_") {
        bail!("invalid Feishu topic binding");
    }
    let anchor = metadata
        .get("feishu_reply_anchor")
        .and_then(serde_json::Value::as_str);
    let anchor_thread = metadata
        .get("feishu_reply_anchor_thread_id")
        .and_then(serde_json::Value::as_str);
    if anchor_thread != Some(thread_id) || !anchor.is_some_and(|id| id.starts_with("om_")) {
        bail!("Feishu topic reply anchor is unavailable; wait for a new message in this topic");
    }
    Ok(anchor)
}

pub(crate) fn read_status(data_dir: &Path, session_id: &str, key: &str) -> Result<DeliveryReceipt> {
    let raw = std::fs::read(record_path(data_dir, session_id, key))
        .context("proactive delivery not found")?;
    let record: OutboxRecord = serde_json::from_slice(&raw)?;
    let mut receipt = record.receipt;
    if receipt.state == "sending" {
        receipt.state = "uncertain".into();
        receipt.error =
            Some("the sending runtime stopped before confirming Feishu delivery".into());
    }
    Ok(receipt)
}

pub(crate) fn print_receipt(receipt: &DeliveryReceipt, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(receipt)?);
    } else {
        println!(
            "{}: {} (key: {})",
            receipt.state, receipt.session_id, receipt.idempotency_key
        );
        if let Some(message_id) = &receipt.platform_message_id {
            println!("Feishu message: {message_id}");
        }
        if let Some(error) = &receipt.error {
            eprintln!("{error}");
        }
    }
    Ok(())
}

fn write_private_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("record path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".proactive-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    drop(file);
    crate::atomic_file::replace(&temporary, path)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub(crate) async fn start_control(
    data_dir: &Path,
) -> Result<(mpsc::Receiver<ControlCommand>, ControlGuard)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let token = format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let descriptor = ControlDescriptor {
        port: listener.local_addr()?.port(),
        token: token.clone(),
        pid: std::process::id(),
    };
    let path = descriptor_path(data_dir);
    write_private_json(&path, &descriptor)?;
    let (tx, rx) = mpsc::channel::<ControlCommand>(16);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let tx = tx.clone();
            let token = token.clone();
            tokio::spawn(async move {
                let _ = serve_connection(stream, &token, tx).await;
            });
        }
    });
    Ok((
        rx,
        ControlGuard {
            path,
            token: descriptor.token,
        },
    ))
}

async fn serve_connection(
    stream: TcpStream,
    token: &str,
    tx: mpsc::Sender<ControlCommand>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let reader = BufReader::new(reader);
    let mut line = Vec::new();
    reader
        .take(MAX_WIRE_BYTES as u64)
        .read_until(b'\n', &mut line)
        .await?;
    let response = match serde_json::from_slice::<WireRequest>(&line) {
        Ok(request) if request.token == token => {
            let (response, rx) = oneshot::channel();
            tx.send(ControlCommand {
                operation: request.operation,
                response,
            })
            .await?;
            rx.await?
        }
        _ => WireResponse {
            receipt: None,
            error: Some("invalid local control request".into()),
        },
    };
    writer.write_all(&serde_json::to_vec(&response)?).await?;
    writer.write_all(b"\n").await?;
    Ok(())
}

async fn call_control(
    data_dir: &Path,
    operation: ControlOperation,
) -> Result<Option<WireResponse>> {
    let descriptor: ControlDescriptor = match std::fs::read(descriptor_path(data_dir)) {
        Ok(raw) => serde_json::from_slice(&raw)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut stream = match tokio::time::timeout(
        Duration::from_secs(2),
        TcpStream::connect(("127.0.0.1", descriptor.port)),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        _ => return Ok(None),
    };
    let request = WireRequest {
        token: descriptor.token,
        operation,
    };
    stream.write_all(&serde_json::to_vec(&request)?).await?;
    stream.write_all(b"\n").await?;
    let mut line = Vec::new();
    BufReader::new(stream)
        .take(MAX_WIRE_BYTES as u64)
        .read_until(b'\n', &mut line)
        .await?;
    Ok(Some(serde_json::from_slice(&line)?))
}

pub(crate) async fn try_send_online(
    data_dir: &Path,
    request: DeliveryRequest,
) -> Result<Option<DeliveryReceipt>> {
    match call_control(data_dir, ControlOperation::Send(request)).await? {
        Some(WireResponse {
            receipt: Some(receipt),
            ..
        }) => Ok(Some(receipt)),
        Some(WireResponse {
            error: Some(error), ..
        }) => Err(anyhow::anyhow!(error)),
        Some(_) => bail!("local control returned no delivery receipt"),
        None => Ok(None),
    }
}

pub(crate) async fn try_status_online(
    data_dir: &Path,
    session_id: String,
    idempotency_key: String,
) -> Result<Option<DeliveryReceipt>> {
    match call_control(
        data_dir,
        ControlOperation::Status {
            session_id,
            idempotency_key,
        },
    )
    .await?
    {
        Some(WireResponse {
            receipt: Some(receipt),
            ..
        }) => Ok(Some(receipt)),
        Some(WireResponse {
            error: Some(error), ..
        }) => Err(anyhow::anyhow!(error)),
        Some(_) => bail!("local control returned no delivery receipt"),
        None => Ok(None),
    }
}

pub(crate) async fn run_control(
    mut rx: mpsc::Receiver<ControlCommand>,
    service: Rc<DeliveryService>,
) {
    while let Some(command) = rx.recv().await {
        let result = match command.operation {
            ControlOperation::Send(request) => service.send(request).await,
            ControlOperation::Status {
                session_id,
                idempotency_key,
            } => service.status(&session_id, &idempotency_key),
        };
        let response = match result {
            Ok(receipt) => WireResponse {
                receipt: Some(receipt),
                error: None,
            },
            Err(error) => WireResponse {
                receipt: None,
                error: Some(format!("{error:#}")),
            },
        };
        let _ = command.response.send(response);
    }
}

pub(crate) struct DeliveryService {
    runtime: Rc<Runtime>,
    gateways: HashMap<String, FeishuGateway>,
    data_dir: PathBuf,
}

impl DeliveryService {
    pub(crate) fn new(
        runtime: Rc<Runtime>,
        gateways: HashMap<String, FeishuGateway>,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            runtime,
            gateways,
            data_dir,
        }
    }

    fn record_path(&self, session_id: &str, key: &str) -> PathBuf {
        record_path(&self.data_dir, session_id, key)
    }

    fn load(&self, session_id: &str, key: &str) -> Result<Option<OutboxRecord>> {
        match std::fs::read(self.record_path(session_id, key)) {
            Ok(raw) => Ok(Some(serde_json::from_slice(&raw)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, record: &OutboxRecord) -> Result<()> {
        write_private_json(
            &self.record_path(&record.receipt.session_id, &record.receipt.idempotency_key),
            record,
        )
    }

    pub(crate) fn status(&self, session_id: &str, key: &str) -> Result<DeliveryReceipt> {
        self.load(session_id, key)?
            .map(|record| record.receipt)
            .context("proactive delivery not found")
    }

    pub(crate) async fn send(&self, request: DeliveryRequest) -> Result<DeliveryReceipt> {
        if request.text.trim().is_empty() || request.text.len() > MAX_TEXT_BYTES {
            bail!("message text must be nonempty and at most {MAX_TEXT_BYTES} bytes");
        }
        let rendered_card = im_feishu::client::card_content(&request.text);
        let estimated_body = serde_json::to_vec(&serde_json::json!({
            "receive_id": "oc_00000000000000000000000000000000",
            "msg_type": "interactive",
            "reply_in_thread": true,
            "content": rendered_card,
            "uuid": "00000000-0000-0000-0000-000000000000",
        }))?;
        if estimated_body.len() > 29_000 {
            bail!("rendered Feishu card request exceeds 30 KB");
        }
        let key = request
            .idempotency_key
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if key.trim().is_empty() || key.len() > 200 {
            bail!("invalid idempotency key");
        }
        let _guard = self
            .runtime
            .bot
            .acquire_thread_run_lock(&request.session_id)
            .await;
        let session = self
            .runtime
            .sessions
            .lock()
            .await
            .get(&request.session_id)
            .context("session not found in selected profile")?;
        let binding = &session.channel_binding;
        if binding.platform != "feishu" && !binding.platform.starts_with("feishu:") {
            bail!(
                "session channel {} does not support proactive delivery",
                binding.platform
            );
        }
        let gateway = self
            .gateways
            .get(&binding.platform)
            .context("Feishu connector is not configured or enabled")?;
        let mut record = if let Some(record) = self.load(&request.session_id, &key)? {
            ensure_same_delivery(
                &record,
                &request.text,
                &binding.platform,
                &binding.channel_id,
            )?;
            record
        } else {
            let record = OutboxRecord {
                receipt: DeliveryReceipt {
                    session_id: request.session_id.clone(),
                    delivery_id: uuid::Uuid::new_v4().to_string(),
                    idempotency_key: key,
                    state: "pending".into(),
                    platform_message_id: None,
                    error: None,
                },
                text: request.text,
                platform: binding.platform.clone(),
                channel_id: binding.channel_id.clone(),
                provider_uuid: uuid::Uuid::new_v4().to_string(),
                first_attempt_unix: None,
            };
            self.save(&record)?;
            record
        };
        if record.receipt.state == "complete" {
            return Ok(record.receipt);
        }
        if record.receipt.platform_message_id.is_none() {
            if unconfirmed_retry_window_expired(&record, unix_now()) {
                record.receipt.state = "uncertain".into();
                record.receipt.error = Some("Feishu delivery is unconfirmed and its one-hour UUID deduplication window has expired; verify manually".into());
                self.save(&record)?;
                return Ok(record.receipt);
            }
            let target = match topic_reply_anchor(&record.channel_id, &session.metadata) {
                Ok(target) => target.map(str::to_owned),
                Err(error) => {
                    record.receipt.state = "failed".into();
                    record.receipt.error = Some(error.to_string());
                    self.save(&record)?;
                    return Ok(record.receipt);
                }
            };
            if let Err(error) = gateway.ensure_authenticated().await {
                record.receipt.state = "failed".into();
                record.receipt.error =
                    Some(format!("Feishu authentication failed: {error:#}"));
                self.save(&record)?;
                return Ok(record.receipt);
            }
            if record.first_attempt_unix.is_none() {
                record.first_attempt_unix = Some(unix_now());
            }
            record.receipt.state = "sending".into();
            record.receipt.error = None;
            self.save(&record)?;
            let result = tokio::time::timeout(Duration::from_secs(30), async {
                if let Some(anchor) = target {
                    gateway
                        .reply_card_in_thread_with_uuid(
                            &anchor,
                            &record.text,
                            &record.provider_uuid,
                        )
                        .await
                } else {
                    gateway
                        .send_card_with_uuid(
                            &record.channel_id,
                            &record.text,
                            &record.provider_uuid,
                        )
                        .await
                }
            })
            .await;
            match result {
                Ok(Ok(message_id)) if !message_id.is_empty() => {
                    record.receipt.platform_message_id = Some(message_id);
                    record.receipt.state = "sent".into();
                    self.save(&record)?;
                }
                outcome => {
                    let uncertain = match &outcome {
                        Err(_) => true,
                        Ok(Err(error)) => error
                            .chain()
                            .any(|cause| cause.downcast_ref::<reqwest::Error>().is_some()),
                        _ => true,
                    };
                    record.receipt.state = if uncertain { "uncertain" } else { "failed" }.into();
                    record.receipt.error = Some(match outcome {
                        Err(_) => "Feishu send timed out; delivery may have succeeded".into(),
                        Ok(Err(error)) => format!("Feishu send failed: {error:#}"),
                        _ => "Feishu did not return a message id".into(),
                    });
                    self.save(&record)?;
                    return Ok(record.receipt);
                }
            }
        }
        self.runtime
            .bot
            .append_proactive_assistant_message(
                &request.session_id,
                &record.receipt.delivery_id,
                &record.text,
                &record.platform,
                record
                    .receipt
                    .platform_message_id
                    .as_deref()
                    .unwrap_or_default(),
            )
            .await?;
        record.receipt.state = "complete".into();
        record.receipt.error = None;
        self.save(&record)?;
        Ok(record.receipt)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_lock_is_exclusive_and_recoverable() {
        let dir = tempfile::tempdir().unwrap();
        let first = ProfileLock::try_acquire(dir.path()).unwrap().unwrap();
        assert!(ProfileLock::try_acquire(dir.path()).unwrap().is_none());
        drop(first);
        assert!(ProfileLock::try_acquire(dir.path()).unwrap().is_some());
    }

    #[test]
    fn status_reads_persisted_receipt_without_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let record = OutboxRecord {
            receipt: DeliveryReceipt {
                session_id: "session-1".into(),
                delivery_id: "delivery-1".into(),
                idempotency_key: "script-key".into(),
                state: "uncertain".into(),
                platform_message_id: None,
                error: Some("unconfirmed".into()),
            },
            text: "# Markdown".into(),
            platform: "feishu".into(),
            channel_id: "oc_chat".into(),
            provider_uuid: "provider-1".into(),
            first_attempt_unix: Some(1),
        };
        assert!(ensure_same_delivery(&record, "# Markdown", "feishu", "oc_chat").is_ok());
        assert!(ensure_same_delivery(&record, "different", "feishu", "oc_chat").is_err());
        assert!(ensure_same_delivery(&record, "# Markdown", "feishu", "oc_other").is_err());
        assert!(!unconfirmed_retry_window_expired(&record, 3600));
        assert!(unconfirmed_retry_window_expired(&record, 3601));
        let path = record_path(dir.path(), "session-1", "script-key");
        write_private_json(&path, &record).unwrap();
        let receipt = read_status(dir.path(), "session-1", "script-key").unwrap();
        assert_eq!(receipt.state, "uncertain");
        assert_eq!(receipt.idempotency_key, "script-key");
        assert!(read_status(dir.path(), "session-2", "script-key").is_err());
    }

    #[test]
    fn routes_direct_group_and_topic_without_fallback() {
        let mut metadata = serde_json::Map::new();
        assert_eq!(topic_reply_anchor("oc_direct", &metadata).unwrap(), None);
        assert_eq!(topic_reply_anchor("oc_group", &metadata).unwrap(), None);
        assert!(topic_reply_anchor("oc_group:thread:omt_topic", &metadata).is_err());
        metadata.insert("feishu_reply_anchor".into(), serde_json::json!("om_reply"));
        metadata.insert(
            "feishu_reply_anchor_thread_id".into(),
            serde_json::json!("omt_other"),
        );
        assert!(topic_reply_anchor("oc_group:thread:omt_topic", &metadata).is_err());
        metadata.insert(
            "feishu_reply_anchor_thread_id".into(),
            serde_json::json!("omt_topic"),
        );
        assert_eq!(
            topic_reply_anchor("oc_group:thread:omt_topic", &metadata).unwrap(),
            Some("om_reply")
        );
        assert!(topic_reply_anchor("invalid", &metadata).is_err());
    }

    #[tokio::test]
    async fn authenticated_control_round_trips_status() {
        let dir = tempfile::tempdir().unwrap();
        let (mut rx, _guard) = start_control(dir.path()).await.unwrap();
        let caller = tokio::spawn({
            let path = dir.path().to_owned();
            async move { try_status_online(&path, "s".into(), "k".into()).await }
        });
        let command = rx.recv().await.unwrap();
        assert!(matches!(command.operation, ControlOperation::Status { .. }));
        command
            .response
            .send(WireResponse {
                receipt: Some(DeliveryReceipt {
                    session_id: "s".into(),
                    delivery_id: "d".into(),
                    idempotency_key: "k".into(),
                    state: "complete".into(),
                    platform_message_id: Some("om_1".into()),
                    error: None,
                }),
                error: None,
            })
            .unwrap();
        assert_eq!(
            caller
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .platform_message_id
                .as_deref(),
            Some("om_1")
        );
        let descriptor: ControlDescriptor =
            serde_json::from_slice(&std::fs::read(descriptor_path(dir.path())).unwrap()).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", descriptor.port))
            .await
            .unwrap();
        let invalid = WireRequest {
            token: "wrong-token".into(),
            operation: ControlOperation::Status {
                session_id: "s".into(),
                idempotency_key: "k".into(),
            },
        };
        stream
            .write_all(&serde_json::to_vec(&invalid).unwrap())
            .await
            .unwrap();
        stream.write_all(b"\n").await.unwrap();
        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .await
            .unwrap();
        let response: WireResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            response.error.as_deref(),
            Some("invalid local control request")
        );
        assert!(rx.try_recv().is_err());
    }
}
