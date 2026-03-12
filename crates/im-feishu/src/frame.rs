//! Protobuf frame types for the Feishu WebSocket long-connection protocol.
//!
//! Wire schema (proto2, using gogo/protobuf on the Go side):
//!
//! ```proto
//! message Header { required string key = 1; required string value = 2; }
//! message Frame {
//!   required uint64 SeqID           = 1;
//!   required uint64 LogID           = 2;
//!   required int32  service         = 3;
//!   required int32  method          = 4;
//!   repeated Header headers         = 5;
//!   optional string payload_encoding = 6;
//!   optional string payload_type    = 7;
//!   optional bytes  payload         = 8;
//!   optional string LogIDNew        = 9;
//! }
//! ```
//!
//! `method` values:
//!  * `0` — Control frame (ping / pong / handshake)
//!  * `1` — Data frame  (event / card)

use prost::Message;

/// Protobuf `Header` — a key/value pair in a [`PbFrame`].
#[derive(Clone, PartialEq, Message)]
pub struct PbHeader {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

/// Protobuf `Frame` — the top-level container for all Feishu WS messages.
#[derive(Clone, PartialEq, Message)]
pub struct PbFrame {
    #[prost(uint64, tag = "1")]
    pub seq_id: u64,
    #[prost(uint64, tag = "2")]
    pub log_id: u64,
    #[prost(int32, tag = "3")]
    pub service: i32,
    #[prost(int32, tag = "4")]
    pub method: i32,
    #[prost(message, repeated, tag = "5")]
    pub headers: Vec<PbHeader>,
    #[prost(string, optional, tag = "6")]
    pub payload_encoding: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub payload_type: Option<String>,
    #[prost(bytes = "vec", optional, tag = "8")]
    pub payload: Option<Vec<u8>>,
    #[prost(string, optional, tag = "9")]
    pub log_id_new: Option<String>,
}

impl PbFrame {
    /// Look up a header value by key (case-sensitive).
    pub fn get_header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| h.value.as_str())
    }

    /// Encode the response frame for a data event ACK.
    ///
    /// Mirrors what the Go SDK does: copy `seq_id`/`log_id`/`service`/`method`
    /// and the original headers back, then set `payload` to the JSON response.
    pub fn encode_ack(&self) -> Vec<u8> {
        let ack = PbFrame {
            seq_id: self.seq_id,
            log_id: self.log_id,
            service: self.service,
            method: self.method,
            headers: self.headers.clone(),
            payload: Some(b"{\"code\":200}".to_vec()),
            ..Default::default()
        };
        let mut buf = Vec::new();
        ack.encode(&mut buf).expect("PbFrame encode");
        buf
    }
}
