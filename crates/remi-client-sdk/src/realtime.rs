use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval, sleep};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use url::Url;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
const JOIN_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct RealtimeConfig {
    pub supabase_url: String,
    pub supabase_anon_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum RemiRealtimeEvent {
    ThingsDocChanged {
        document_uuid: String,
        data_type: String,
        source_device_id: Option<String>,
    },
    TriggerFired {
        trigger_id: String,
        trigger_name: String,
        source_device_id: Option<String>,
    },
    ChatReply {
        session_id: String,
        message_id: String,
    },
    SyncRequest,
}

#[derive(Clone, Debug)]
struct RealtimeSession {
    user_id: String,
    jwt: String,
}

#[derive(Default)]
struct RealtimeState {
    config: Option<RealtimeConfig>,
    session: Option<RealtimeSession>,
    run_token: Option<CancellationToken>,
    run_task: Option<JoinHandle<()>>,
}

#[derive(Debug, Deserialize)]
struct PhoenixEnvelope {
    topic: String,
    event: String,
    payload: Value,
    #[serde(default, rename = "ref")]
    ref_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BroadcastPayload {
    event: String,
    payload: Value,
}

#[derive(Debug, Serialize)]
struct PhoenixPush<'a> {
    topic: &'a str,
    event: &'a str,
    payload: Value,
    #[serde(rename = "ref")]
    ref_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    join_ref: Option<String>,
}

pub struct SupabaseRealtimeManager {
    inner: Mutex<RealtimeState>,
    event_tx: broadcast::Sender<RemiRealtimeEvent>,
}

impl Default for SupabaseRealtimeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SupabaseRealtimeManager {
    pub fn new() -> Self {
        let (event_tx, _rx) = broadcast::channel(2048);
        Self {
            inner: Mutex::new(RealtimeState::default()),
            event_tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RemiRealtimeEvent> {
        self.event_tx.subscribe()
    }

    pub async fn configure(&self, config: Option<RealtimeConfig>) -> Result<(), String> {
        let normalized = config.map(normalize_config).transpose()?;
        let mut state = self.inner.lock().await;
        state.config = normalized;
        restart_worker_if_possible(&mut state, self.event_tx.clone());
        Ok(())
    }

    pub async fn start_user_stream(&self, user_id: String, jwt: String) {
        let mut state = self.inner.lock().await;
        state.session = Some(RealtimeSession { user_id, jwt });
        restart_worker_if_possible(&mut state, self.event_tx.clone());
    }

    pub async fn stop(&self) {
        let mut state = self.inner.lock().await;
        state.session = None;
        stop_worker(&mut state);
    }
}

fn normalize_config(config: RealtimeConfig) -> Result<RealtimeConfig, String> {
    let supabase_url = config.supabase_url.trim().to_string();
    let supabase_anon_key = config.supabase_anon_key.trim().to_string();
    if supabase_url.is_empty() || supabase_anon_key.is_empty() {
        return Err("Supabase Realtime config is incomplete".to_string());
    }
    Ok(RealtimeConfig {
        supabase_url,
        supabase_anon_key,
    })
}

fn stop_worker(state: &mut RealtimeState) {
    if let Some(token) = state.run_token.take() {
        token.cancel();
    }
    if let Some(task) = state.run_task.take() {
        task.abort();
    }
}

fn restart_worker_if_possible(
    state: &mut RealtimeState,
    event_tx: broadcast::Sender<RemiRealtimeEvent>,
) {
    stop_worker(state);

    let Some(config) = state.config.clone() else {
        tracing::info!("Supabase Realtime config not set; skipping Realtime worker start");
        return;
    };
    let Some(session) = state.session.clone() else {
        tracing::debug!("Supabase Realtime session not available yet; waiting for auth");
        return;
    };

    let token = CancellationToken::new();
    let worker_token = token.clone();
    let task = tokio::spawn(async move {
        run_realtime_loop(config, session, event_tx, worker_token).await;
    });

    state.run_token = Some(token);
    state.run_task = Some(task);
}

async fn run_realtime_loop(
    config: RealtimeConfig,
    session: RealtimeSession,
    event_tx: broadcast::Sender<RemiRealtimeEvent>,
    cancel: CancellationToken,
) {
    let ws_url = match build_realtime_ws_url(&config) {
        Ok(url) => url,
        Err(error) => {
            tracing::error!(%error, "Failed to build Supabase Realtime websocket URL");
            return;
        }
    };

    let mut backoff = RECONNECT_BACKOFF_INITIAL;
    loop {
        if cancel.is_cancelled() {
            return;
        }

        match run_single_connection(
            ws_url.as_str(),
            session.clone(),
            event_tx.clone(),
            cancel.clone(),
        )
        .await
        {
            Ok(()) => return,
            Err(error) => {
                if cancel.is_cancelled() {
                    return;
                }
                tracing::warn!(%error, user_id = %session.user_id, "Supabase Realtime connection dropped; scheduling reconnect");
            }
        }

        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = sleep(backoff) => {}
        }

        backoff = std::cmp::min(backoff.saturating_mul(2), RECONNECT_BACKOFF_MAX);
    }
}

async fn run_single_connection(
    ws_url: &str,
    session: RealtimeSession,
    event_tx: broadcast::Sender<RemiRealtimeEvent>,
    cancel: CancellationToken,
) -> Result<(), String> {
    let (stream, _) = connect_async(ws_url)
        .await
        .map_err(|error| format!("websocket connect failed: {error}"))?;
    let (mut writer, mut reader) = stream.split();

    let topic = format!("realtime:user:{}:events", session.user_id);
    let join_ref = "1".to_string();
    let mut next_ref = 2_u64;
    let mut current_token = crate::auth::auth_get_access_token()
        .await
        .unwrap_or_else(|| session.jwt.clone());

    send_frame(
        &mut writer,
        &PhoenixPush {
            topic: &topic,
            event: "phx_join",
            payload: json!({
                "config": {
                    "broadcast": {
                        "ack": false,
                        "self": false,
                    },
                    "presence": {
                        "enabled": false,
                    },
                    "postgres_changes": [],
                    "private": true,
                },
                "access_token": current_token,
            }),
            ref_id: Some(join_ref.clone()),
            join_ref: Some(join_ref.clone()),
        },
    )
    .await?;

    let join_deadline = sleep(JOIN_TIMEOUT);
    tokio::pin!(join_deadline);

    let mut heartbeat = interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    heartbeat.tick().await;
    let mut joined = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = send_frame(
                    &mut writer,
                    &PhoenixPush {
                        topic: &topic,
                        event: "phx_leave",
                        payload: json!({}),
                        ref_id: Some(next_ref_string(&mut next_ref)),
                        join_ref: Some(join_ref.clone()),
                    },
                ).await;
                let _ = writer.close().await;
                return Ok(());
            }
            _ = &mut join_deadline, if !joined => {
                return Err("timed out waiting for Supabase Realtime channel join".to_string());
            }
            _ = heartbeat.tick() => {
                if let Some(fresh_token) = crate::auth::auth_get_access_token().await {
                    if fresh_token != current_token {
                        current_token = fresh_token.clone();
                        send_frame(
                            &mut writer,
                            &PhoenixPush {
                                topic: &topic,
                                event: "access_token",
                                payload: json!({ "access_token": fresh_token }),
                                ref_id: Some(next_ref_string(&mut next_ref)),
                                join_ref: Some(join_ref.clone()),
                            },
                        )
                        .await?;
                    }
                }

                send_frame(
                    &mut writer,
                    &PhoenixPush {
                        topic: "phoenix",
                        event: "heartbeat",
                        payload: json!({}),
                        ref_id: Some(next_ref_string(&mut next_ref)),
                        join_ref: None,
                    },
                )
                .await?;
            }
            message = reader.next() => {
                let message = match message {
                    Some(Ok(message)) => message,
                    Some(Err(error)) => return Err(format!("websocket read failed: {error}")),
                    None => return Err("Supabase Realtime websocket closed".to_string()),
                };

                match message {
                    Message::Text(text) => {
                        let envelope: PhoenixEnvelope = serde_json::from_str(text.as_ref())
                            .map_err(|error| format!("invalid realtime frame: {error}"))?;
                        if envelope.topic == topic || matches!(envelope.event.as_str(), "broadcast" | "phx_reply" | "phx_error" | "phx_close") {
                            tracing::info!(
                                user_id = %session.user_id,
                                envelope_topic = %envelope.topic,
                                envelope_event = %envelope.event,
                                "Supabase Realtime frame received"
                            );
                        }
                        if envelope.topic == topic && envelope.event == "phx_reply" && envelope.ref_id.as_deref() == Some(join_ref.as_str()) {
                            let status = envelope.payload.get("status").and_then(Value::as_str).unwrap_or_default();
                            if status != "ok" {
                                return Err(format!("Supabase Realtime join rejected: {}", envelope.payload));
                            }
                            joined = true;
                            tracing::info!(user_id = %session.user_id, topic = %topic, "Supabase Realtime channel subscribed");
                            continue;
                        }

                        if envelope.topic == topic && matches!(envelope.event.as_str(), "phx_error" | "phx_close") {
                            return Err(format!("channel closed with event {}", envelope.event));
                        }

                        if envelope.topic == topic && envelope.event == "broadcast" {
                            let broadcast: BroadcastPayload = match serde_json::from_value(envelope.payload) {
                                Ok(payload) => payload,
                                Err(error) => {
                                    tracing::warn!(%error, "Discarding invalid broadcast payload");
                                    continue;
                                }
                            };
                            if broadcast.event == "remi_event" {
                                match serde_json::from_value::<RemiRealtimeEvent>(broadcast.payload) {
                                    Ok(event) => {
                                        tracing::info!(
                                            user_id = %session.user_id,
                                            event = %describe_realtime_event(&event),
                                            "Supabase Realtime remi_event received"
                                        );
                                        let _ = event_tx.send(event);
                                    }
                                    Err(error) => {
                                        tracing::warn!(%error, "Discarding invalid remi_event payload");
                                    }
                                }
                            } else {
                                tracing::info!(
                                    user_id = %session.user_id,
                                    broadcast_event = %broadcast.event,
                                    "Ignoring non-remi_event realtime broadcast"
                                );
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        writer
                            .send(Message::Pong(payload))
                            .await
                            .map_err(|error| format!("websocket pong failed: {error}"))?;
                    }
                    Message::Close(_) => return Err("Supabase Realtime websocket closed".to_string()),
                    _ => {}
                }
            }
        }
    }
}

fn build_realtime_ws_url(config: &RealtimeConfig) -> Result<Url, String> {
    let mut url = Url::parse(config.supabase_url.trim())
        .map_err(|error| format!("invalid Supabase URL: {error}"))?;
    match url.scheme() {
        "https" => url
            .set_scheme("wss")
            .map_err(|_| "failed to convert Supabase URL scheme to wss".to_string())?,
        "http" => url
            .set_scheme("ws")
            .map_err(|_| "failed to convert Supabase URL scheme to ws".to_string())?,
        "wss" | "ws" => {}
        other => {
            return Err(format!("unsupported Supabase URL scheme: {other}"));
        }
    }
    url.set_path("/realtime/v1/websocket");
    url.query_pairs_mut()
        .clear()
        .append_pair("apikey", config.supabase_anon_key.trim())
        .append_pair("vsn", "1.0.0");
    Ok(url)
}

async fn send_frame<S>(writer: &mut S, frame: &PhoenixPush<'_>) -> Result<(), String>
where
    S: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let json = serde_json::to_string(frame)
        .map_err(|error| format!("failed to encode websocket frame: {error}"))?;
    writer
        .send(Message::Text(json.into()))
        .await
        .map_err(|error| format!("websocket write failed: {error}"))
}

fn next_ref_string(next_ref: &mut u64) -> String {
    let current = *next_ref;
    *next_ref += 1;
    current.to_string()
}

fn describe_realtime_event(event: &RemiRealtimeEvent) -> &'static str {
    match event {
        RemiRealtimeEvent::ThingsDocChanged { .. } => "things_doc_changed",
        RemiRealtimeEvent::TriggerFired { .. } => "trigger_fired",
        RemiRealtimeEvent::ChatReply { .. } => "chat_reply",
        RemiRealtimeEvent::SyncRequest => "sync_request",
    }
}

#[cfg(test)]
mod tests {
    use super::{RealtimeConfig, build_realtime_ws_url, normalize_config};

    #[test]
    fn normalize_config_rejects_empty_values() {
        let result = normalize_config(RealtimeConfig {
            supabase_url: "  ".to_string(),
            supabase_anon_key: "token".to_string(),
        });

        assert!(result.is_err());
    }

    #[test]
    fn build_ws_url_uses_supabase_websocket_endpoint() {
        let url = build_realtime_ws_url(&RealtimeConfig {
            supabase_url: "https://example.supabase.co".to_string(),
            supabase_anon_key: "anon-key".to_string(),
        })
        .expect("websocket url should be built");

        assert_eq!(
            url.as_str(),
            "wss://example.supabase.co/realtime/v1/websocket?apikey=anon-key&vsn=1.0.0"
        );
    }
}
