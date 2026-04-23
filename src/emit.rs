// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Bilbycast-EULA

//! Ergonomic helpers for the common gateway → manager message types.
//!
//! [`Emitter`] wraps the outbound channel that feeds the WS write task.
//! Clone it freely — all clones share the same channel.

use bytes::Bytes;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::errors::{CommandError, SdkError};
use crate::events::GatewayEvent;

/// Outbound WS message queued for the write task.
///
/// This is a pre-encoded envelope string (the write task just wraps it in
/// `Message::Text` and flushes), which keeps the hot path free of repeated
/// JSON serialization per consumer.
#[derive(Debug, Clone)]
pub struct OutboundFrame(pub String);

/// Ergonomic producer for all gateway → manager messages.
#[derive(Debug, Clone)]
pub struct Emitter {
    tx: mpsc::Sender<OutboundFrame>,
}

impl Emitter {
    pub(crate) fn new(tx: mpsc::Sender<OutboundFrame>) -> Self {
        Self { tx }
    }

    async fn send_envelope(&self, msg_type: &str, payload: Value) -> Result<(), SdkError> {
        let frame = OutboundFrame(crate::envelope::envelope(msg_type, payload));
        self.tx
            .send(frame)
            .await
            .map_err(|e| SdkError::Channel(e.to_string()))
    }

    /// Emit a stats snapshot. The payload shape is up to the driver on the
    /// manager side — see `DeviceDriver::extract_metrics()`. Inputs/outputs/
    /// alarms etc. live here.
    pub async fn emit_stats(&self, stats: Value) -> Result<(), SdkError> {
        self.send_envelope("stats", stats).await
    }

    /// Emit a health message. The manager tracks node liveness from these;
    /// send one every `heartbeat_interval` (the SDK loop already does this
    /// for you — use this only for ad-hoc updates that shouldn't wait for
    /// the next tick).
    pub async fn emit_health(&self, health: Value) -> Result<(), SdkError> {
        self.send_envelope("health", health).await
    }

    /// Emit an event. See [`GatewayEvent`] for the standard catalog.
    pub async fn emit_event(&self, event: GatewayEvent) -> Result<(), SdkError> {
        self.send_envelope("event", event.to_payload()).await
    }

    /// Emit a raw event payload. Prefer [`Self::emit_event`] for type safety.
    pub async fn emit_event_raw(&self, payload: Value) -> Result<(), SdkError> {
        self.send_envelope("event", payload).await
    }

    /// Emit a JPEG thumbnail for one flow. Matches the edge's
    /// per-flow thumbnail protocol; the manager stores it in
    /// `NodeHub.thumbnail_cache` keyed on `"node_id:flow_id"`.
    pub async fn emit_thumbnail(&self, flow_id: &str, jpeg: Bytes) -> Result<(), SdkError> {
        // Base64-encode the JPEG — the manager-side protocol expects a JSON payload.
        let encoded = base64_encode(&jpeg);
        self.send_envelope(
            "thumbnail",
            json!({
                "flow_id": flow_id,
                "image_base64": encoded,
            }),
        )
        .await
    }

    /// Emit a `config_response`. Called in response to a `get_config`
    /// command. The manager stores `payload.config` (or the envelope payload
    /// directly) as the node's `cached_config`.
    pub async fn emit_config_response(&self, config: Value) -> Result<(), SdkError> {
        self.send_envelope("config_response", config).await
    }

    /// Emit a `command_ack`. Usually called by [`crate::dispatch`] after the
    /// consumer's [`crate::CommandHandler`] returns; exposed here for exotic
    /// flows (e.g., late replies).
    pub async fn emit_command_ack(
        &self,
        command_id: &str,
        result: Result<Value, CommandError>,
    ) -> Result<(), SdkError> {
        let mut payload = serde_json::Map::new();
        payload.insert("command_id".into(), Value::String(command_id.into()));
        match result {
            Ok(data) => {
                payload.insert("success".into(), Value::Bool(true));
                if !matches!(data, Value::Null) {
                    payload.insert("data".into(), data);
                }
            }
            Err(err) => {
                payload.insert("success".into(), Value::Bool(false));
                payload.insert("error".into(), Value::String(err.message.clone()));
                payload.insert("error_code".into(), Value::String(err.code.clone()));
                if let Some(details) = err.details {
                    payload.insert("details".into(), details);
                }
            }
        }
        self.send_envelope("command_ack", Value::Object(payload)).await
    }

    /// Emit a `pong` in response to a manager `ping` envelope. The SDK read
    /// task already handles this transparently; exposed for completeness.
    pub async fn emit_pong(&self) -> Result<(), SdkError> {
        self.send_envelope("pong", Value::Null).await
    }
}

// ── Minimal standalone base64 encoder (no extra crate) ──

const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | (data[i + 2] as u32);
        out.push(B64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = data.len() - i;
    if rem == 1 {
        let n = (data[i] as u32) << 16;
        out.push(B64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
        out.push(B64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
