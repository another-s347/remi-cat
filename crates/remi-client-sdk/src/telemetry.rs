use std::{sync::Arc, time::Duration};

use chrono::{DateTime, TimeZone, Utc};
use once_cell::sync::OnceCell;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;
use tonic::Request;
use tonic::transport::Channel;

use crate::transport::{SharedTransport, configure_shared_transport};

pub mod proto {
    pub mod public_api {
        pub mod v1 {
            tonic::include_proto!("public_api.v1");
        }
    }

    pub use public_api::v1::public_service_client::PublicServiceClient;
    pub use public_api::v1::{MonitoringEvent, ReportAck, TelemetryReport};
}

use crate::telemetry::proto::{MonitoringEvent, PublicServiceClient, ReportAck, TelemetryReport};

/// Internal telemetry report payload structure for JSON deserialization
#[derive(Debug, Deserialize)]
struct ReportPayload {
    #[serde(rename = "deviceId")]
    device_id: String,
    #[serde(rename = "generatedAt")]
    generated_at: String,
    #[serde(rename = "eventCount")]
    event_count: u64,
    events: Vec<EventPayload>,
    manual: bool,
    trigger: String,
}

/// Internal event payload structure for JSON deserialization
#[derive(Debug, Deserialize)]
struct EventPayload {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(deserialize_with = "deserialize_event_timestamp")]
    timestamp: DateTime<Utc>,
    #[serde(default = "empty_metadata")]
    metadata: Value,
}

fn empty_metadata() -> Value {
    Value::Object(Map::new())
}

fn deserialize_event_timestamp<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct TimestampVisitor;

    impl<'de> serde::de::Visitor<'de> for TimestampVisitor {
        type Value = DateTime<Utc>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a unix timestamp in seconds or RFC3339 string")
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Utc.timestamp_opt(value, 0)
                .single()
                .ok_or_else(|| E::custom(format!("Invalid unix timestamp: {value}")))
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            let secs = i64::try_from(value)
                .map_err(|_| E::custom(format!("Unix timestamp out of range: {value}")))?;
            self.visit_i64(secs)
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            self.visit_i64(value.round() as i64)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if let Ok(int_val) = value.parse::<i64>() {
                return self.visit_i64(int_val);
            }
            DateTime::parse_from_rfc3339(value)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|err| E::custom(format!("Invalid timestamp '{value}': {err}")))
        }
    }

    deserializer.deserialize_any(TimestampVisitor)
}

fn map_report_payload(payload: ReportPayload) -> Result<TelemetryReport, String> {
    let events = payload
        .events
        .into_iter()
        .map(|event| {
            let metadata_json = serde_json::to_string(&event.metadata)
                .map_err(|err| format!("Failed to serialize metadata: {err}"))?;
            Ok(MonitoringEvent {
                event_type: event.event_type,
                timestamp: event.timestamp.to_rfc3339(),
                metadata_json,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    Ok(TelemetryReport {
        device_id: payload.device_id,
        generated_at: payload.generated_at,
        event_count: payload.event_count,
        events,
        manual: payload.manual,
        trigger: payload.trigger,
    })
}

fn ack_to_json(ack: ReportAck) -> String {
    json!({
        "status": ack.status,
        "receivedAt": ack.received_at,
    })
    .to_string()
}

struct ClientState {
    transport: Arc<SharedTransport>,
    client: Mutex<Option<PublicServiceClient<Channel>>>,
    request_timeout: Duration,
}

impl ClientState {
    async fn send_report(&self, report: TelemetryReport) -> Result<ReportAck, String> {
        let mut client_guard = self.client.lock().await;

        if client_guard.is_none() {
            let channel = self.transport.get_channel().await?;
            client_guard.replace(PublicServiceClient::new(channel));
        }

        let client = client_guard
            .as_mut()
            .expect("client must exist after initialization");

        let bearer_token = crate::auth::auth_get_bearer_token()
            .await
            .ok_or_else(|| "Authentication bearer token is not configured".to_string())?;

        let mut request = Request::new(report);
        crate::auth::auth_insert_bearer_header(&mut request, &bearer_token)?;

        let response = timeout(self.request_timeout, client.send_report(request))
            .await
            .map_err(|_| "Telemetry report request timed out".to_string())
            .and_then(|result| {
                result.map_err(|err| format!("Failed to send telemetry report: {err}"))
            });

        match response {
            Ok(ack) => Ok(ack.into_inner()),
            Err(err) => {
                client_guard.take();
                Err(err)
            }
        }
    }

    async fn drop_client(&self) {
        let mut guard = self.client.lock().await;
        guard.take();
    }
}

static TELEMETRY_CLIENT_STATE: OnceCell<Arc<RwLock<Option<Arc<ClientState>>>>> = OnceCell::new();

fn telemetry_state() -> &'static Arc<RwLock<Option<Arc<ClientState>>>> {
    TELEMETRY_CLIENT_STATE.get_or_init(|| Arc::new(RwLock::new(None)))
}

pub async fn configure_telemetry_client(config_json: String) -> Result<(), String> {
    let transport = configure_shared_transport(&config_json).await?;
    let request_timeout = transport.request_timeout();

    let state = Arc::new(ClientState {
        transport,
        client: Mutex::new(None),
        request_timeout,
    });

    let previous = {
        let mut guard = telemetry_state().write().await;
        guard.replace(state.clone())
    };

    if let Some(old) = previous {
        old.drop_client().await;
    }

    Ok(())
}

pub async fn send_telemetry_report(payload_json: String) -> Result<String, String> {
    let maybe_state = {
        let guard = telemetry_state().read().await;
        guard.clone()
    };

    let state = maybe_state.ok_or_else(|| "Telemetry client is not configured".to_string())?;

    let payload: ReportPayload = serde_json::from_str(&payload_json)
        .map_err(|err| format!("Invalid telemetry payload: {err}"))?;
    let report = map_report_payload(payload)?;

    match state.send_report(report).await {
        Ok(ack) => Ok(ack_to_json(ack)),
        Err(err) => Err(err),
    }
}
