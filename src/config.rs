// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Bilbycast-EULA

//! Standard gateway config shape + reconnect backoff.
//!
//! Vendor sidecars typically wrap this in a larger `AppConfig` alongside
//! their vendor-specific section, e.g.:
//!
//! ```toml
//! [manager]
//! urls = ["wss://manager.example.com:8443/ws/node"]
//! registration_token = "..."
//!
//! [vendor]
//! address = "192.168.1.100"
//! ```
//!
//! `GatewayConfig` corresponds to the `[manager]` section.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::errors::SdkError;

/// Configuration for a gateway client. Consumers typically build this from
/// the `[manager]` section of their sidecar's TOML config.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayConfig {
    /// Ordered list of manager WebSocket URLs. Must be `wss://`. 1–16 entries.
    /// The client rotates on WS close; a single-instance deployment uses a
    /// one-element list.
    pub manager_urls: Vec<String>,

    /// Device type string. Must match a driver registered in the manager
    /// (e.g., `"appear_x"`, or whatever `DeviceDriver::device_type()` returns).
    pub device_type: String,

    /// Reported software version of the gateway itself. Surfaced on the
    /// manager UI.
    pub software_version: String,

    /// Persisted node ID (populated after first successful registration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,

    /// Persisted node secret (envelope-encrypted at rest on the consumer side
    /// if desired; the SDK stores it verbatim via [`crate::auth::persist_credentials`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_secret: Option<String>,

    /// One-time registration token supplied by the manager's "Add Node"
    /// flow. Consumed on first connect; consumers should clear it after
    /// registration succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_token: Option<String>,

    /// Accept self-signed TLS certs on the manager connection. This flag
    /// is ignored (with a hard error) unless `BILBYCAST_ALLOW_INSECURE=1`
    /// is set in the environment.
    #[serde(default)]
    pub accept_self_signed_cert: bool,

    /// Optional SHA-256 certificate fingerprint for certificate pinning.
    /// Colon-separated lowercase hex, e.g. `"ab:cd:ef:..."`. Takes precedence
    /// over `accept_self_signed_cert`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_fingerprint: Option<String>,

    /// How often to send the health heartbeat. Default: 15 seconds.
    #[serde(default = "default_heartbeat", with = "serde_duration_secs")]
    pub heartbeat_interval: Duration,

    /// Reconnect backoff policy between connection attempts.
    #[serde(default)]
    pub reconnect_backoff: ReconnectBackoff,
}

fn default_heartbeat() -> Duration {
    Duration::from_secs(15)
}

impl GatewayConfig {
    /// Build a minimal config given a single URL, device type, and software
    /// version. Useful for tests and tiny consumers.
    pub fn minimal(
        manager_url: impl Into<String>,
        device_type: impl Into<String>,
        software_version: impl Into<String>,
    ) -> Self {
        Self {
            manager_urls: vec![manager_url.into()],
            device_type: device_type.into(),
            software_version: software_version.into(),
            node_id: None,
            node_secret: None,
            registration_token: None,
            accept_self_signed_cert: false,
            cert_fingerprint: None,
            heartbeat_interval: default_heartbeat(),
            reconnect_backoff: ReconnectBackoff::default(),
        }
    }

    /// Validate static invariants of the config. Called by
    /// [`crate::GatewayClient::connect`].
    pub fn validate(&self) -> Result<(), SdkError> {
        if self.manager_urls.is_empty() {
            return Err(SdkError::Config(
                "manager_urls must contain at least one entry".into(),
            ));
        }
        if self.manager_urls.len() > 16 {
            return Err(SdkError::Config(format!(
                "manager_urls may contain at most 16 entries (got {})",
                self.manager_urls.len()
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for (i, url) in self.manager_urls.iter().enumerate() {
            if url.len() > 2048 {
                return Err(SdkError::Config(format!(
                    "manager_urls[{i}] must be at most 2048 characters"
                )));
            }
            // Tests can opt into ws:// via the `allow_plaintext_ws` option.
            if !url.starts_with("wss://") && !allow_plaintext_ws() {
                return Err(SdkError::Config(format!(
                    "manager_urls[{i}] = {url:?} must use wss:// (TLS). \
                     Plaintext ws:// connections are not allowed. \
                     (Integration tests may set BILBYCAST_SDK_ALLOW_PLAINTEXT_WS=1.)"
                )));
            }
            let _ = url::Url::parse(url)
                .map_err(|e| SdkError::Config(format!("manager_urls[{i}]: {e}")))?;
            if !seen.insert(url.as_str()) {
                return Err(SdkError::Config(format!(
                    "manager_urls[{i}] = {url:?} is a duplicate"
                )));
            }
        }
        if self.device_type.is_empty() || self.device_type.len() > 64 {
            return Err(SdkError::Config(
                "device_type must be 1..=64 chars".into(),
            ));
        }
        if self.node_id.is_some() != self.node_secret.is_some() {
            return Err(SdkError::Config(
                "node_id and node_secret must be provided together (or both omitted)".into(),
            ));
        }
        if self.node_id.is_none() && self.registration_token.is_none() {
            return Err(SdkError::Config(
                "either (node_id + node_secret) or registration_token must be set".into(),
            ));
        }
        Ok(())
    }

    /// True if the client is ready to authenticate as a previously-registered node.
    pub fn has_credentials(&self) -> bool {
        self.node_id.is_some() && self.node_secret.is_some()
    }
}

/// Honored only in integration tests and only when explicitly enabled.
fn allow_plaintext_ws() -> bool {
    std::env::var("BILBYCAST_SDK_ALLOW_PLAINTEXT_WS")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Reconnect backoff policy. Exponential 1 s → 2 s → 5 s → 10 s → 30 s by default,
/// reset on a successful auth.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReconnectBackoff {
    pub steps_secs: Vec<u64>,
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        Self {
            steps_secs: vec![1, 2, 5, 10, 30],
        }
    }
}

impl ReconnectBackoff {
    /// Return the Duration to wait before the N-th retry (1-indexed).
    /// Saturates at the last step.
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        if self.steps_secs.is_empty() {
            return Duration::from_secs(5);
        }
        let idx = ((attempt.max(1) as usize) - 1).min(self.steps_secs.len() - 1);
        Duration::from_secs(self.steps_secs[idx])
    }
}

// Serde helper: serialize Duration as whole seconds.
mod serde_duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(Duration::from_secs(secs))
    }
}
