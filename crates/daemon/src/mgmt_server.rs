//! Management WebSocket server (Noise_XX authenticated encryption).
//!
//! Listens on a dedicated port (`DAEMON_MGMT_ADDR`, default `0.0.0.0:50052`)
//! fully separate from the agent gRPC port.
//!
//! # Security model
//!
//! Both daemon and admin-ui hold static X25519 keypairs.  The Noise_XX
//! handshake provides **mutual authentication** and **forward secrecy**.
//! On top of Noise, a management token gates first access:
//!
//! - Daemon generates a random 16-byte hex token on first start, printed to
//!   logs so the operator can copy it into the admin-ui "Add Daemon" dialog.
//! - On first connection from a new admin public key the client must supply
//!   this token; the key is then added to `trusted_admins`.
//! - Subsequent connections from trusted keys proceed without a token.
//!
//! # Identity persistence
//!
//! `mgmt_identity.json` in the daemon's working directory:
//! ```json
//! {
//!   "privkey": "<base64-32-bytes>",
//!   "token":   "<hex-16-bytes>",
//!   "trusted_admins": ["<base64-32-bytes-pubkey>", …]
//! }
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::{collections::HashSet, sync::Arc as StdArc};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use mgmt_api::{
    methods, AgentFileParams, AgentFileResult, AuthParams, AuthResult, ContainerOpParams,
    ContainerOpResult, DaemonStatusResult, MgmtRequest, MgmtResponse, OwnerGetResult,
    SecretDeleteParams, SecretEntry, SecretSetParams, UserBanListResult, UserBanParams,
    UserChannel, UserDeleteParams, UserInfo, UserLinkParams, UserLinkResult, UserListResult,
    UserUnlinkParams, VolumeAddParams, VolumeRemoveParams,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sysinfo::System;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio_tungstenite::{accept_async, tungstenite::Message, WebSocketStream};
use tracing::{info, warn};

use crate::docker::DockerManager;
use crate::rpc_server::RpcServer;
use crate::secret_store::SecretStore;
use crate::volume_store::VolumeStore;
use matcher::OwnerMatcher;
use user_store::UserStore;

const NOISE_PATTERN: &str = "Noise_XX_25519_AESGCM_SHA256";
const IDENTITY_FILE: &str = "mgmt_identity.json";
const MAX_NOISE_MSG: usize = 65535;

// ── Identity ──────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
struct MgmtIdentity {
    /// Base64-encoded 32-byte X25519 private key.
    privkey: String,
    /// Hex-encoded 16-byte pairing token (shown in startup logs).
    token: String,
    /// Base64-encoded 32-byte public keys of trusted admin clients.
    trusted_admins: Vec<String>,
}

impl MgmtIdentity {
    fn load_or_create() -> Result<Self> {
        let path = PathBuf::from(IDENTITY_FILE);
        if path.exists() {
            let raw = std::fs::read_to_string(&path).context("reading mgmt_identity.json")?;
            serde_json::from_str(&raw).context("parsing mgmt_identity.json")
        } else {
            let builder = snow::Builder::new(
                NOISE_PATTERN
                    .parse()
                    .map_err(|e| anyhow!("noise params: {e}"))?,
            );
            let keypair = builder
                .generate_keypair()
                .map_err(|e| anyhow!("keygen: {e:?}"))?;

            let mut token_bytes = [0u8; 16];
            use rand::RngCore as _;
            rand::thread_rng().fill_bytes(&mut token_bytes);

            let identity = Self {
                privkey: base64::engine::general_purpose::STANDARD.encode(&keypair.private),
                token: hex::encode(token_bytes),
                trusted_admins: vec![],
            };
            identity.save()?;
            Ok(identity)
        }
    }

    fn save(&self) -> Result<()> {
        let path = PathBuf::from(IDENTITY_FILE);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)
            .context("writing mgmt_identity.json.tmp")?;
        std::fs::rename(&tmp, &path).context("renaming mgmt_identity")?;
        Ok(())
    }

    fn privkey_bytes(&self) -> Result<Vec<u8>> {
        base64::engine::general_purpose::STANDARD
            .decode(&self.privkey)
            .context("decode privkey")
    }

    fn pubkey_fingerprint(&self) -> Result<String> {
        let raw = self.privkey_bytes()?;
        let arr: [u8; 32] = raw
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("privkey not 32 bytes"))?;
        let secret = x25519_dalek::StaticSecret::from(arr);
        let pubkey = x25519_dalek::PublicKey::from(&secret);
        Ok(hex::encode(Sha256::digest(pubkey.as_bytes())))
    }
}

// ── MgmtContext ───────────────────────────────────────────────────────────────

/// Shared daemon state made accessible to management handlers.
#[derive(Clone)]
pub struct MgmtContext {
    pub docker: Option<DockerManager>,
    pub owner: OwnerMatcher,
    pub user_store: StdArc<UserStore>,
    pub secret_store: Arc<RwLock<SecretStore>>,
    pub volume_store: Arc<RwLock<VolumeStore>>,
    pub rpc: RpcServer,
    pub data_dir: PathBuf,
    pub start_time: Instant,
    /// Reused `sysinfo::System` for process metrics (CPU diff needs prior sample).
    pub sys: Arc<Mutex<System>>,
}

// ── MgmtServer ────────────────────────────────────────────────────────────────

pub struct MgmtServer {
    identity: Arc<RwLock<MgmtIdentity>>,
    fingerprint: String,
    ctx: MgmtContext,
}

impl MgmtServer {
    /// Load (or generate) the daemon identity, return a server ready to serve.
    pub fn new(ctx: MgmtContext) -> Result<Self> {
        let identity = MgmtIdentity::load_or_create()?;
        let fingerprint = identity.pubkey_fingerprint()?;
        Ok(Self {
            identity: Arc::new(RwLock::new(identity)),
            fingerprint,
            ctx,
        })
    }

    /// SHA-256 fingerprint of the daemon's Noise static public key (hex).
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Management pairing token (hex) — shown in startup log.
    pub async fn token(&self) -> String {
        self.identity.read().await.token.clone()
    }

    /// Accept management connections forever.
    pub async fn serve(self, addr: SocketAddr) -> Result<()> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind mgmt addr {addr}"))?;
        info!(
            addr = %addr,
            fingerprint = %self.fingerprint,
            "mgmt server listening",
        );
        let this = Arc::new(self);
        loop {
            let (stream, peer) = listener.accept().await?;
            let server = Arc::clone(&this);
            tokio::spawn(async move {
                if let Err(e) = server.handle_conn(stream, peer).await {
                    warn!(%peer, "mgmt connection error: {e:#}");
                }
            });
        }
    }

    // ── Per-connection handler ─────────────────────────────────────────────────

    async fn handle_conn(&self, stream: TcpStream, peer: SocketAddr) -> Result<()> {
        let mut ws = accept_async(stream).await.context("WS upgrade")?;
        let privkey = self.identity.read().await.privkey_bytes()?;

        // ── Noise_XX handshake (responder side) ───────────────────────────────
        let params = NOISE_PATTERN
            .parse()
            .map_err(|e| anyhow!("noise params: {e}"))?;
        let mut handshake = snow::Builder::new(params)
            .local_private_key(&privkey)
            .build_responder()
            .map_err(|e| anyhow!("build responder: {e:?}"))?;

        // Step 1 — read initiator ephemeral
        let msg1 = recv_binary(&mut ws).await?;
        let mut tmp = vec![0u8; MAX_NOISE_MSG];
        handshake
            .read_message(&msg1, &mut tmp)
            .map_err(|e| anyhow!("noise msg1: {e:?}"))?;

        // Step 2 — send responder ephemeral + static
        let mut out = vec![0u8; MAX_NOISE_MSG];
        let n = handshake
            .write_message(&[], &mut out)
            .map_err(|e| anyhow!("noise msg2: {e:?}"))?;
        ws.send(Message::Binary(out[..n].to_vec())).await?;

        // Step 3 — read initiator static
        let msg3 = recv_binary(&mut ws).await?;
        let mut tmp = vec![0u8; MAX_NOISE_MSG];
        handshake
            .read_message(&msg3, &mut tmp)
            .map_err(|e| anyhow!("noise msg3: {e:?}"))?;

        // Extract the admin client's static public key for trust check / pairing
        let remote_pubkey = handshake
            .get_remote_static()
            .ok_or_else(|| anyhow!("no remote static key"))?
            .to_vec();
        let remote_pub_b64 = base64::engine::general_purpose::STANDARD.encode(&remote_pubkey);

        let mut transport = handshake
            .into_transport_mode()
            .map_err(|e| anyhow!("into transport: {e:?}"))?;

        info!(%peer, "noise handshake complete");

        // ── Auth step — first message must always be "auth" ───────────────────
        let req: MgmtRequest = recv_json(&mut ws, &mut transport).await?;
        if req.method != methods::AUTH {
            send_json(
                &mut ws,
                &mut transport,
                &MgmtResponse::err(&req.id, 401, "first message must be auth"),
            )
            .await?;
            return Ok(());
        }

        let auth_params: AuthParams =
            serde_json::from_value(req.params.clone()).unwrap_or_default();

        let already_trusted = self
            .identity
            .read()
            .await
            .trusted_admins
            .contains(&remote_pub_b64);

        if !already_trusted {
            let expected_token = self.identity.read().await.token.clone();
            if auth_params.token != expected_token {
                send_json(
                    &mut ws,
                    &mut transport,
                    &MgmtResponse::err(&req.id, 403, "invalid management token"),
                )
                .await?;
                return Ok(());
            }
            // Pair this new admin key
            {
                let mut id = self.identity.write().await;
                if !id.trusted_admins.contains(&remote_pub_b64) {
                    id.trusted_admins.push(remote_pub_b64.clone());
                    id.save()?;
                    info!(%peer, "new admin client paired");
                }
            }
            send_json(
                &mut ws,
                &mut transport,
                &MgmtResponse::ok(
                    &req.id,
                    serde_json::to_value(AuthResult {
                        ok: true,
                        paired: true,
                        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                    })?,
                ),
            )
            .await?;
        } else {
            send_json(
                &mut ws,
                &mut transport,
                &MgmtResponse::ok(
                    &req.id,
                    serde_json::to_value(AuthResult {
                        ok: true,
                        paired: false,
                        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                    })?,
                ),
            )
            .await?;
        }

        // ── Request/response loop ─────────────────────────────────────────────
        loop {
            let req = match recv_json::<MgmtRequest>(&mut ws, &mut transport).await {
                Ok(r) => r,
                Err(e) => {
                    warn!(%peer, "recv: {e:#}");
                    break;
                }
            };
            let resp = self.dispatch(&req).await;
            if let Err(e) = send_json(&mut ws, &mut transport, &resp).await {
                warn!(%peer, "send: {e:#}");
                break;
            }
        }
        Ok(())
    }

    // ── Dispatcher ────────────────────────────────────────────────────────────

    async fn dispatch(&self, req: &MgmtRequest) -> MgmtResponse {
        let id = &req.id;
        match req.method.as_str() {
            methods::DAEMON_STATUS => self.handle_status(id).await,
            methods::CONTAINER_OP => self.handle_container_op(id, &req.params).await,
            methods::OWNER_GET => self.handle_owner_get(id).await,
            methods::OWNER_RESET => self.handle_owner_reset(id).await,
            methods::SECRET_LIST => self.handle_secret_list(id).await,
            methods::SECRET_SET => self.handle_secret_set(id, &req.params).await,
            methods::SECRET_DELETE => self.handle_secret_delete(id, &req.params).await,
            methods::AGENT_FILE_READ => self.handle_agent_file_read(id, &req.params).await,
            methods::AGENT_FILE_WRITE => self.handle_agent_file_write(id, &req.params).await,
            methods::VOLUME_LIST => self.handle_volume_list(id).await,
            methods::VOLUME_ADD => self.handle_volume_add(id, &req.params).await,
            methods::VOLUME_REMOVE => self.handle_volume_remove(id, &req.params).await,
            methods::USER_LIST => self.handle_user_list(id).await,
            methods::USER_LINK => self.handle_user_link(id, &req.params).await,
            methods::USER_UNLINK => self.handle_user_unlink(id, &req.params).await,
            methods::USER_DELETE => self.handle_user_delete(id, &req.params).await,
            methods::USER_BAN => self.handle_user_ban(id, &req.params).await,
            methods::USER_UNBAN => self.handle_user_unban(id, &req.params).await,
            methods::USER_BAN_LIST => self.handle_user_ban_list(id).await,
            _ => MgmtResponse::err(id, 404, format!("unknown method: {}", req.method)),
        }
    }

    // ── Handlers ──────────────────────────────────────────────────────────────

    async fn handle_status(&self, id: &str) -> MgmtResponse {
        let agent_connected = self.ctx.rpc.is_agent_connected().await;
        let container_running = if let Some(docker) = &self.ctx.docker {
            Some(docker.is_running().await)
        } else {
            None
        };

        // ── Process metrics (blocking I/O — run off the async thread) ────────────
        let sys_arc = Arc::clone(&self.ctx.sys);
        let (daemon_mem_kb, daemon_cpu_pct, agent_mem_kb, agent_cpu_pct) =
            tokio::task::spawn_blocking(move || {
                let mut sys = sys_arc.lock().unwrap();
                let daemon_pid = sysinfo::Pid::from_u32(std::process::id());
                sys.refresh_processes();
                let (dmem, dcpu) = sys
                    .process(daemon_pid)
                    .map(|p| (p.memory() / 1024, p.cpu_usage()))
                    .unwrap_or((0, 0.0));
                let (amem, acpu) = sys
                    .processes()
                    .values()
                    .find(|p| p.name() == "remi-cat-agent")
                    .map(|p| (Some(p.memory() / 1024), Some(p.cpu_usage())))
                    .unwrap_or((None, None));
                (dmem, dcpu, amem, acpu)
            })
            .await
            .unwrap_or((0, 0.0, None, None));

        match serde_json::to_value(DaemonStatusResult {
            agent_connected,
            owner_id: self.ctx.owner.owner_id(),
            uptime_secs: self.ctx.start_time.elapsed().as_secs(),
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            container_running,
            daemon_mem_kb,
            daemon_cpu_pct,
            agent_mem_kb,
            agent_cpu_pct,
        }) {
            Ok(v) => MgmtResponse::ok(id, v),
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    async fn handle_container_op(&self, id: &str, params: &serde_json::Value) -> MgmtResponse {
        let p: ContainerOpParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => return MgmtResponse::err(id, 400, e.to_string()),
        };
        let Some(docker) = &self.ctx.docker else {
            return MgmtResponse::err(id, 503, "docker not available (local mode)");
        };
        let result: Result<String> = match p.op.as_str() {
            "restart" => docker
                .restart()
                .await
                .map(|_| format!("container restarted")),
            "stop" => docker.stop().await.map(|_| format!("container stopped")),
            "start" => docker.start().await.map(|_| format!("container started")),
            "pull" => docker.pull_image().await.map(|lines| {
                lines
                    .last()
                    .cloned()
                    .unwrap_or_else(|| "pull complete".into())
            }),
            _ => Err(anyhow!("unknown op: {}", p.op)),
        };
        match result {
            Ok(msg) => match serde_json::to_value(ContainerOpResult {
                ok: true,
                message: msg,
            }) {
                Ok(v) => MgmtResponse::ok(id, v),
                Err(e) => MgmtResponse::err(id, 500, e.to_string()),
            },
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    async fn handle_owner_get(&self, id: &str) -> MgmtResponse {
        match serde_json::to_value(OwnerGetResult {
            owner_id: self.ctx.owner.owner_id(),
        }) {
            Ok(v) => MgmtResponse::ok(id, v),
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    async fn handle_owner_reset(&self, id: &str) -> MgmtResponse {
        self.ctx.owner.reset();
        MgmtResponse::ok(id, serde_json::json!({"ok": true}))
    }

    async fn handle_secret_list(&self, id: &str) -> MgmtResponse {
        let store = self.ctx.secret_store.read().await;
        let entries: Vec<SecretEntry> = store
            .keys()
            .into_iter()
            .map(|k| SecretEntry { key: k.to_string() })
            .collect();
        match serde_json::to_value(entries) {
            Ok(v) => MgmtResponse::ok(id, v),
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    async fn handle_secret_set(&self, id: &str, params: &serde_json::Value) -> MgmtResponse {
        let p: SecretSetParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => return MgmtResponse::err(id, 400, e.to_string()),
        };
        let mut store = self.ctx.secret_store.write().await;
        match store.set(p.key, p.value) {
            Ok(()) => MgmtResponse::ok(id, serde_json::json!({"ok": true})),
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    async fn handle_secret_delete(&self, id: &str, params: &serde_json::Value) -> MgmtResponse {
        let p: SecretDeleteParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => return MgmtResponse::err(id, 400, e.to_string()),
        };
        let mut store = self.ctx.secret_store.write().await;
        match store.delete(&p.key) {
            Ok(()) => MgmtResponse::ok(id, serde_json::json!({"ok": true})),
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    async fn handle_agent_file_read(&self, id: &str, params: &serde_json::Value) -> MgmtResponse {
        let p: AgentFileParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => return MgmtResponse::err(id, 400, e.to_string()),
        };
        if !is_safe_filename(&p.filename) {
            return MgmtResponse::err(id, 400, "invalid filename");
        }
        let path = self.ctx.data_dir.join(&p.filename);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return MgmtResponse::err(id, 500, e.to_string()),
        };
        match serde_json::to_value(AgentFileResult { content }) {
            Ok(v) => MgmtResponse::ok(id, v),
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    async fn handle_agent_file_write(&self, id: &str, params: &serde_json::Value) -> MgmtResponse {
        let p: AgentFileParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => return MgmtResponse::err(id, 400, e.to_string()),
        };
        if !is_safe_filename(&p.filename) {
            return MgmtResponse::err(id, 400, "invalid filename");
        }
        let path = self.ctx.data_dir.join(&p.filename);
        if let Some(dir) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(dir).await {
                return MgmtResponse::err(id, 500, e.to_string());
            }
        }
        match tokio::fs::write(&path, p.content.as_bytes()).await {
            Ok(()) => MgmtResponse::ok(id, serde_json::json!({"ok": true})),
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    // ── User management handlers ──────────────────────────────────────────────

    async fn handle_user_list(&self, id: &str) -> MgmtResponse {
        let records = self.ctx.user_store.list();
        let banned: HashSet<String> = self.ctx.owner.list_banned().into_iter().collect();
        let users: Vec<UserInfo> = records
            .into_iter()
            .map(|r| UserInfo {
                banned: banned.contains(&r.uuid),
                uuid: r.uuid,
                username: r.username,
                channels: r
                    .channels
                    .into_iter()
                    .map(|c| UserChannel {
                        channel: c.channel,
                        user_id: c.user_id,
                    })
                    .collect(),
            })
            .collect();
        match serde_json::to_value(UserListResult { users }) {
            Ok(v) => MgmtResponse::ok(id, v),
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    async fn handle_user_link(&self, id: &str, params: &serde_json::Value) -> MgmtResponse {
        let p: UserLinkParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => return MgmtResponse::err(id, 400, e.to_string()),
        };
        let old_a = self.ctx.user_store.resolve(&p.channel_a, &p.user_id_a);
        let old_b = self.ctx.user_store.resolve(&p.channel_b, &p.user_id_b);
        match self
            .ctx
            .user_store
            .link(&p.channel_a, &p.user_id_a, &p.channel_b, &p.user_id_b)
        {
            Ok(uuid) => {
                let old_ids: Vec<String> = old_a.into_iter().chain(old_b).collect();
                if let Err(e) = self.ctx.owner.migrate_blacklist(&old_ids, &uuid) {
                    return MgmtResponse::err(id, 500, e.to_string());
                }
                match serde_json::to_value(UserLinkResult { uuid }) {
                    Ok(v) => MgmtResponse::ok(id, v),
                    Err(e) => MgmtResponse::err(id, 500, e.to_string()),
                }
            }
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    async fn handle_user_unlink(&self, id: &str, params: &serde_json::Value) -> MgmtResponse {
        let p: UserUnlinkParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => return MgmtResponse::err(id, 400, e.to_string()),
        };
        match self.ctx.user_store.unlink(&p.channel, &p.user_id) {
            Ok(removed) => MgmtResponse::ok(id, serde_json::json!({"ok": removed})),
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    async fn handle_user_delete(&self, id: &str, params: &serde_json::Value) -> MgmtResponse {
        let p: UserDeleteParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => return MgmtResponse::err(id, 400, e.to_string()),
        };
        match self.ctx.user_store.delete_by_uuid(&p.uuid) {
            Ok(deleted) => MgmtResponse::ok(id, serde_json::json!({"ok": deleted})),
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    async fn handle_user_ban(&self, id: &str, params: &serde_json::Value) -> MgmtResponse {
        let p: UserBanParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => return MgmtResponse::err(id, 400, e.to_string()),
        };
        match self.ctx.owner.ban(&p.uuid) {
            Ok(matcher::BanResult::Added | matcher::BanResult::AlreadyBanned) => {
                MgmtResponse::ok(id, serde_json::json!({"ok": true}))
            }
            Ok(matcher::BanResult::ProtectedOwner) => {
                MgmtResponse::err(id, 400, "cannot ban owner or protected user")
            }
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    async fn handle_user_unban(&self, id: &str, params: &serde_json::Value) -> MgmtResponse {
        let p: UserBanParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => return MgmtResponse::err(id, 400, e.to_string()),
        };
        match self.ctx.owner.unban(&p.uuid) {
            Ok(_) => MgmtResponse::ok(id, serde_json::json!({"ok": true})),
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    async fn handle_user_ban_list(&self, id: &str) -> MgmtResponse {
        match serde_json::to_value(UserBanListResult {
            users: self.ctx.owner.list_banned(),
        }) {
            Ok(v) => MgmtResponse::ok(id, v),
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    async fn handle_volume_list(&self, id: &str) -> MgmtResponse {
        let store = self.ctx.volume_store.read().await;
        let mounts = store.mounts().to_vec();
        match serde_json::to_value(mounts) {
            Ok(v) => MgmtResponse::ok(id, v),
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }

    async fn handle_volume_add(&self, id: &str, params: &serde_json::Value) -> MgmtResponse {
        let p: VolumeAddParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => return MgmtResponse::err(id, 400, e.to_string()),
        };
        let mount = mgmt_api::VolumeMount {
            host_path: p.host_path,
            container_path: p.container_path,
            read_only: p.read_only,
        };
        let mut store = self.ctx.volume_store.write().await;
        match store.add(mount) {
            Ok(()) => MgmtResponse::ok(id, serde_json::json!({"ok": true})),
            Err(e) => MgmtResponse::err(id, 400, e.to_string()),
        }
    }

    async fn handle_volume_remove(&self, id: &str, params: &serde_json::Value) -> MgmtResponse {
        let p: VolumeRemoveParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => return MgmtResponse::err(id, 400, e.to_string()),
        };
        let mut store = self.ctx.volume_store.write().await;
        match store.remove(&p.container_path) {
            Ok(()) => MgmtResponse::ok(id, serde_json::json!({"ok": true})),
            Err(e) => MgmtResponse::err(id, 500, e.to_string()),
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Allow only safe filenames (no path traversal, no hidden files).
fn is_safe_filename(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

async fn recv_binary(ws: &mut WebSocketStream<TcpStream>) -> Result<Vec<u8>> {
    loop {
        match ws
            .next()
            .await
            .ok_or_else(|| anyhow!("WS stream ended"))??
        {
            Message::Binary(b) => return Ok(b.to_vec()),
            Message::Close(_) => bail!("connection closed"),
            _ => {}
        }
    }
}

async fn recv_json<T: serde::de::DeserializeOwned>(
    ws: &mut WebSocketStream<TcpStream>,
    transport: &mut snow::TransportState,
) -> Result<T> {
    let ctxt = recv_binary(ws).await?;
    let mut ptxt = vec![0u8; ctxt.len()];
    let n = transport
        .read_message(&ctxt, &mut ptxt)
        .map_err(|e| anyhow!("decrypt: {e:?}"))?;
    serde_json::from_slice(&ptxt[..n]).context("deserialise mgmt request")
}

async fn send_json<T: serde::Serialize>(
    ws: &mut WebSocketStream<TcpStream>,
    transport: &mut snow::TransportState,
    value: &T,
) -> Result<()> {
    let ptxt = serde_json::to_vec(value)?;
    // Noise transport message overhead is at most 16 bytes (AEAD tag).
    let mut ctxt = vec![0u8; ptxt.len() + 16];
    let n = transport
        .write_message(&ptxt, &mut ctxt)
        .map_err(|e| anyhow!("encrypt: {e:?}"))?;
    ws.send(Message::Binary(ctxt[..n].to_vec().into()))
        .await
        .context("WS send")
}
