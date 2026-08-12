// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

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

    /// Permit plaintext `ws://` manager URLs. **Integration tests only.**
    ///
    /// Like [`Self::accept_self_signed_cert`], this needs *two* keys: this
    /// field **and** `BILBYCAST_SDK_ALLOW_PLAINTEXT_WS=1` in the
    /// environment. Either alone is refused.
    ///
    /// The two-key shape is the point. This used to be an environment
    /// variable on its own, which meant a single variable — set anywhere
    /// in a unit file, a shell profile, a container spec — silently
    /// downgraded a vendor sidecar's manager link from TLS to cleartext,
    /// carrying its node secret. Every other credential-bearing escape
    /// hatch in this codebase requires the config to consent as well, so
    /// that turning it on is a deliberate, reviewable, greppable edit to a
    /// file someone owns.
    #[serde(default)]
    pub allow_plaintext_ws: bool,

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
            allow_plaintext_ws: false,
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
            // Tests can opt into ws://, but only with BOTH keys: the
            // config field and the environment variable. See
            // `GatewayConfig::allow_plaintext_ws`.
            if !url.starts_with("wss://") && !(self.allow_plaintext_ws && plaintext_ws_env_set()) {
                let hint = if self.allow_plaintext_ws {
                    "`allow_plaintext_ws` is set in the config, but \
                     BILBYCAST_SDK_ALLOW_PLAINTEXT_WS=1 is not set in the environment — \
                     both are required."
                } else if plaintext_ws_env_set() {
                    "BILBYCAST_SDK_ALLOW_PLAINTEXT_WS=1 is set, but `allow_plaintext_ws` is \
                     not set in the config — both are required."
                } else {
                    "Integration tests may set `allow_plaintext_ws: true` in the config AND \
                     BILBYCAST_SDK_ALLOW_PLAINTEXT_WS=1 in the environment."
                };
                return Err(SdkError::Config(format!(
                    "manager_urls[{i}] = {url:?} must use wss:// (TLS). \
                     Plaintext ws:// connections are not allowed. {hint}"
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
        // Heartbeat interval bounds — defends the manager against a
        // misconfigured gateway spamming health envelopes (which would
        // crowd out other nodes on the same instance).
        let hb_secs = self.heartbeat_interval.as_secs();
        if hb_secs < 5 || hb_secs > 300 {
            return Err(SdkError::Config(format!(
                "heartbeat_interval must be 5..=300 seconds (got {hb_secs}s). \
                 Shorter intervals risk overwhelming the manager's per-node \
                 event budget; longer intervals make node health detection \
                 too coarse for operator dashboards."
            )));
        }
        Ok(())
    }

    /// True if the client is ready to authenticate as a previously-registered node.
    pub fn has_credentials(&self) -> bool {
        self.node_id.is_some() && self.node_secret.is_some()
    }
}

/// Half of the plaintext-`ws://` opt-in. The other half is the
/// `allow_plaintext_ws` config field; both are required. See
/// [`GatewayConfig::allow_plaintext_ws`] for why.
fn plaintext_ws_env_set() -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn base_cfg() -> GatewayConfig {
        GatewayConfig::minimal("wss://manager.example.com", "test", "0.0.1")
            .with_registration_token("rt-123")
    }

    impl GatewayConfig {
        // Helper for tests to set the registration token in one call.
        fn with_registration_token(mut self, token: &str) -> Self {
            self.registration_token = Some(token.into());
            self
        }
    }

    #[test]
    fn heartbeat_default_is_in_bounds() {
        let cfg = base_cfg();
        assert!(cfg.validate().is_ok(), "default 15 s heartbeat must validate");
    }

    #[test]
    fn heartbeat_too_small_rejected() {
        let mut cfg = base_cfg();
        cfg.heartbeat_interval = Duration::from_secs(0);
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("5..=300"), "got: {err}");
        cfg.heartbeat_interval = Duration::from_secs(4);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn heartbeat_too_large_rejected() {
        let mut cfg = base_cfg();
        cfg.heartbeat_interval = Duration::from_secs(301);
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("5..=300"), "got: {err}");
        cfg.heartbeat_interval = Duration::from_secs(3600);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn heartbeat_at_boundaries_accepted() {
        let mut cfg = base_cfg();
        cfg.heartbeat_interval = Duration::from_secs(5);
        assert!(cfg.validate().is_ok());
        cfg.heartbeat_interval = Duration::from_secs(300);
        assert!(cfg.validate().is_ok());
    }

    // ── plaintext ws:// needs BOTH keys ─────────────────────────────────
    //
    // These do not touch the environment. `plaintext_ws_env_set()` reads
    // process state, and mutating it from a test races every other test in
    // the binary, so the config half is what is pinned here: with
    // `allow_plaintext_ws` false, a `ws://` URL must be refused no matter
    // what the environment says — which is the half that closes the
    // one-variable bypass.

    #[test]
    fn plaintext_ws_is_refused_without_the_config_key() {
        let cfg = GatewayConfig::minimal("ws://manager.test/ws/node", "t", "0.0.1");
        assert!(!cfg.allow_plaintext_ws, "the config key must default to off");
        let err = cfg.validate().expect_err("ws:// must be refused");
        let msg = err.to_string();
        assert!(msg.contains("must use wss://"), "{msg}");
    }

    #[test]
    fn wss_is_accepted_regardless_of_the_plaintext_keys() {
        let mut cfg = GatewayConfig::minimal("wss://manager.test/ws/node", "t", "0.0.1");
        // Unrelated to the scheme check, but `validate` runs every rule.
        cfg.registration_token = Some("token".into());
        cfg.validate().expect("wss:// is always fine");
    }

    #[test]
    fn the_error_names_which_half_is_missing() {
        // An operator who has set one of the two needs to be told which,
        // otherwise the message reads as "this is simply not allowed".
        let mut cfg = GatewayConfig::minimal("ws://manager.test/ws/node", "t", "0.0.1");
        cfg.allow_plaintext_ws = true;
        if !plaintext_ws_env_set() {
            let msg = cfg.validate().expect_err("env half absent").to_string();
            assert!(
                msg.contains("BILBYCAST_SDK_ALLOW_PLAINTEXT_WS=1 is not set"),
                "{msg}"
            );
        }
    }
}
