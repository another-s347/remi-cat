//! Noise_XX WebSocket client — authenticated encrypted transport.
//!
//! Each `NoiseClient::call()` opens a new WebSocket connection to a daemon,
//! performs the 3-message Noise_XX handshake, sends one `auth` request, then
//! one payload request, and returns the response together with the daemon's
//! Noise public-key fingerprint.
//!
//! # Admin identity
//!
//! The admin-ui's own keypair is stored at
//! `~/.config/remi-admin/identity.key` (raw 32 private-key bytes).
//! The file is created with a fresh random key on first use.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use mgmt_api::{methods, AuthParams, MgmtRequest, MgmtResponse};
use sha2::{Digest, Sha256};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing::debug;

const NOISE_PATTERN: &str = "Noise_XX_25519_AESGCM_SHA256";
const MAX_NOISE_MSG: usize = 65535;

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

// ── NoiseClient ───────────────────────────────────────────────────────────────

/// Holds the admin-ui's static X25519 private key.
///
/// Cheap to clone — only the private-key bytes are stored.
#[derive(Clone)]
pub struct NoiseClient {
    privkey: Vec<u8>,
}

impl NoiseClient {
    /// Load or create the admin identity at `<config_dir>/identity.key`.
    pub fn load_or_create(config_dir: &Path) -> Result<Self> {
        let key_path = config_dir.join("identity.key");
        let privkey = if key_path.exists() {
            std::fs::read(&key_path).context("reading identity.key")?
        } else {
            let builder = snow::Builder::new(
                NOISE_PATTERN
                    .parse()
                    .map_err(|e| anyhow!("noise params: {e}"))?,
            );
            let keypair = builder
                .generate_keypair()
                .map_err(|e| anyhow!("keygen: {e:?}"))?;
            std::fs::create_dir_all(config_dir).context("creating config dir")?;
            std::fs::write(&key_path, &keypair.private).context("writing identity.key")?;
            keypair.private
        };
        Ok(Self { privkey })
    }

    /// Returns the SHA-256 fingerprint of the admin's own public key.
    pub fn own_fingerprint(&self) -> Result<String> {
        let arr: [u8; 32] = self
            .privkey
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("privkey not 32 bytes"))?;
        let secret = x25519_dalek::StaticSecret::from(arr);
        let pubkey = x25519_dalek::PublicKey::from(&secret);
        Ok(hex::encode(Sha256::digest(pubkey.as_bytes())))
    }

    /// Connect to `addr`, perform Noise_XX handshake, authenticate, and
    /// send `req`.  Returns `(response, daemon_fingerprint)`.
    ///
    /// If `expected_fingerprint` is `Some(fp)` and the daemon presents a
    /// different public key, the connection is refused (fingerprint mismatch).
    /// On a first-time connection pass `None`.
    pub async fn call(
        &self,
        addr: &str,
        expected_fingerprint: Option<&str>,
        token: &str,
        req: MgmtRequest,
    ) -> Result<(MgmtResponse, String)> {
        let url = normalize_ws_url(addr);
        debug!(%url, "connecting to daemon");

        let (mut ws, _) = connect_async(&url)
            .await
            .with_context(|| format!("connect to {url}"))?;

        // ── Noise_XX handshake (initiator side) ───────────────────────────
        let params = NOISE_PATTERN
            .parse()
            .map_err(|e| anyhow!("noise params: {e}"))?;
        let mut handshake = snow::Builder::new(params)
            .local_private_key(&self.privkey)
            .build_initiator()
            .map_err(|e| anyhow!("build initiator: {e:?}"))?;

        // Step 1 — send ephemeral
        let mut out = vec![0u8; MAX_NOISE_MSG];
        let n = handshake
            .write_message(&[], &mut out)
            .map_err(|e| anyhow!("noise msg1: {e:?}"))?;
        ws.send(Message::Binary(out[..n].to_vec())).await?;

        // Step 2 — read responder ephemeral + static
        let msg2 = recv_binary(&mut ws).await?;
        let mut tmp = vec![0u8; MAX_NOISE_MSG];
        handshake
            .read_message(&msg2, &mut tmp)
            .map_err(|e| anyhow!("noise msg2: {e:?}"))?;

        // Step 3 — send initiator static
        let n = handshake
            .write_message(&[], &mut out)
            .map_err(|e| anyhow!("noise msg3: {e:?}"))?;
        ws.send(Message::Binary(out[..n].to_vec())).await?;

        // Extract daemon's static public key
        let daemon_pubkey = handshake
            .get_remote_static()
            .ok_or_else(|| anyhow!("no remote static key after handshake"))?
            .to_vec();
        let fingerprint = hex::encode(Sha256::digest(&daemon_pubkey));

        // TOFU fingerprint verification
        if let Some(expected) = expected_fingerprint {
            if fingerprint != expected {
                bail!(
                    "daemon fingerprint mismatch!\n  expected: {expected}\n  got:      {fingerprint}\n\
                     This may indicate a MITM attack or the daemon key was rotated."
                );
            }
        }

        let mut transport = handshake
            .into_transport_mode()
            .map_err(|e| anyhow!("into transport: {e:?}"))?;

        // ── Auth ──────────────────────────────────────────────────────────
        let auth_req = MgmtRequest {
            id: "auth".into(),
            method: methods::AUTH.into(),
            params: serde_json::to_value(AuthParams {
                token: token.to_string(),
            })?,
        };
        send_json(&mut ws, &mut transport, &auth_req).await?;
        let auth_resp: MgmtResponse = recv_json(&mut ws, &mut transport).await?;
        if !auth_resp.is_ok() {
            let msg = auth_resp
                .error
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("auth failed");
            bail!("auth rejected: {msg}");
        }

        // ── Payload request ───────────────────────────────────────────────
        send_json(&mut ws, &mut transport, &req).await?;
        let resp: MgmtResponse = recv_json(&mut ws, &mut transport).await?;

        // Close cleanly
        let _ = ws.close(None).await;

        Ok((resp, fingerprint))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn normalize_ws_url(addr: &str) -> String {
    if addr.starts_with("ws://") || addr.starts_with("wss://") {
        addr.to_string()
    } else {
        format!("ws://{addr}")
    }
}

async fn recv_binary(ws: &mut WsStream) -> Result<Vec<u8>> {
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
    ws: &mut WsStream,
    transport: &mut snow::TransportState,
) -> Result<T> {
    let ctxt = recv_binary(ws).await?;
    let mut ptxt = vec![0u8; ctxt.len()];
    let n = transport
        .read_message(&ctxt, &mut ptxt)
        .map_err(|e| anyhow!("decrypt: {e:?}"))?;
    serde_json::from_slice(&ptxt[..n]).context("deserialise response")
}

async fn send_json<T: serde::Serialize>(
    ws: &mut WsStream,
    transport: &mut snow::TransportState,
    value: &T,
) -> Result<()> {
    let ptxt = serde_json::to_vec(value)?;
    let mut ctxt = vec![0u8; ptxt.len() + 16];
    let n = transport
        .write_message(&ptxt, &mut ctxt)
        .map_err(|e| anyhow!("encrypt: {e:?}"))?;
    ws.send(Message::Binary(ctxt[..n].to_vec()))
        .await
        .context("WS send")
}
