// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Bilbycast-EULA

//! WebSocket envelope format + protocol-version constant.
//!
//! The manager-facing protocol uses a JSON envelope:
//!
//! ```json
//! { "type": "stats", "timestamp": "2026-04-23T...Z", "payload": { ... } }
//! ```
//!
//! Authentication as the first frame uses `"type": "auth"` with a payload
//! containing either `registration_token` (new nodes) or `node_id`+`node_secret`
//! (reconnecting nodes). See `bilbycast-manager/crates/manager-core/src/models/ws_protocol.rs`
//! — the wire types match those `serde` shapes.

use chrono::Utc;
use serde_json::{json, Value};

/// WebSocket protocol version understood by this SDK. Must match the manager's
/// `WS_PROTOCOL_VERSION` constant. Manager logs a warning on mismatch but does
/// not reject — unknown message types and fields are handled gracefully on both
/// sides.
pub const GATEWAY_WS_PROTOCOL_VERSION: u32 = 1;

/// Build a JSON envelope string ready for the wire.
pub fn envelope(msg_type: &str, payload: Value) -> String {
    let env = json!({
        "type": msg_type,
        "timestamp": Utc::now().to_rfc3339(),
        "payload": payload,
    });
    // Serialisation of a JSON Value of known shape never fails.
    serde_json::to_string(&env).unwrap_or_default()
}

/// Build the first-frame `auth` envelope for a new registration.
pub fn auth_register(
    registration_token: &str,
    software_version: &str,
    device_type: &str,
) -> String {
    envelope(
        "auth",
        json!({
            "registration_token": registration_token,
            "software_version": software_version,
            "device_type": device_type,
            "protocol_version": GATEWAY_WS_PROTOCOL_VERSION,
        }),
    )
}

/// Build the first-frame `auth` envelope for a reconnecting node.
pub fn auth_reconnect(
    node_id: &str,
    node_secret: &str,
    software_version: &str,
    device_type: &str,
) -> String {
    envelope(
        "auth",
        json!({
            "node_id": node_id,
            "node_secret": node_secret,
            "software_version": software_version,
            "device_type": device_type,
            "protocol_version": GATEWAY_WS_PROTOCOL_VERSION,
        }),
    )
}

/// Decoded manager → gateway message shape (after resilient parsing).
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub msg_type: String,
    pub payload: Value,
}

impl IncomingMessage {
    /// Parse an inbound JSON envelope into `(msg_type, payload)`. Returns
    /// `None` on malformed JSON — callers should log and drop, never panic.
    pub fn parse(text: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(text).ok()?;
        let msg_type = v.get("type").and_then(|t| t.as_str())?.to_string();
        let payload = v.get("payload").cloned().unwrap_or(Value::Null);
        Some(Self { msg_type, payload })
    }
}
