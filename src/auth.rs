// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Bilbycast-EULA

//! Registration + reconnect authentication, plus optional credential-persistence
//! helpers.
//!
//! Two modes:
//! - **First connect**: `auth { registration_token, software_version, device_type, protocol_version }`
//!   → server replies `register_ack { node_id, node_secret }`. Persist the pair.
//! - **Reconnect**: `auth { node_id, node_secret, software_version, device_type, protocol_version }`
//!   → server replies `auth_ok` (on success) or `auth_error { error | message }`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::errors::SdkError;

/// Persistent credentials for a registered gateway.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedCredentials {
    pub node_id: Option<String>,
    pub node_secret: Option<String>,
    /// Kept on the struct so consumers can do one-shot rewrites via
    /// `persist_credentials`: populate `registration_token = Some(...)` once,
    /// then clear it on first successful registration.
    pub registration_token: Option<String>,
}

impl PersistedCredentials {
    pub fn has_credentials(&self) -> bool {
        self.node_id.is_some() && self.node_secret.is_some()
    }
}

/// Load credentials from a JSON file on disk. Returns an empty struct if the
/// file does not exist.
pub fn load_credentials(path: &Path) -> Result<PersistedCredentials, SdkError> {
    if !path.exists() {
        return Ok(PersistedCredentials::default());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| SdkError::CredentialsIo(format!("read {}: {e}", path.display())))?;
    let creds: PersistedCredentials = serde_json::from_str(&text)?;
    Ok(creds)
}

/// Persist credentials to a JSON file on disk with `0600` permissions.
///
/// Called by the SDK after a successful `register_ack` so the gateway can
/// reconnect. Creates the parent directory if it doesn't exist.
pub fn persist_credentials(path: &Path, creds: &PersistedCredentials) -> Result<(), SdkError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                SdkError::CredentialsIo(format!("mkdir {}: {e}", parent.display()))
            })?;
        }
    }
    let json = serde_json::to_string_pretty(creds)?;
    std::fs::write(path, json)
        .map_err(|e| SdkError::CredentialsIo(format!("write {}: {e}", path.display())))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| SdkError::CredentialsIo(format!("chmod {}: {e}", path.display())))?;
    }
    Ok(())
}

/// Convenience for consumers that want to let the SDK own credential
/// persistence.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    pub path: PathBuf,
}

impl CredentialStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<PersistedCredentials, SdkError> {
        load_credentials(&self.path)
    }

    pub fn save(&self, creds: &PersistedCredentials) -> Result<(), SdkError> {
        persist_credentials(&self.path, creds)
    }
}

/// Decoded result of an initial auth handshake (as seen by the client).
#[derive(Debug, Clone)]
pub enum AuthOutcome {
    /// First-time registration succeeded — persist these.
    Registered {
        node_id: String,
        node_secret: String,
    },
    /// Reconnect succeeded. No new credentials.
    Authenticated,
}

/// Parse a manager auth response envelope (as JSON text).
pub fn parse_auth_response(text: &str) -> Result<AuthOutcome, SdkError> {
    let v: serde_json::Value = serde_json::from_str(text)?;
    let msg_type = v
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let payload = v.get("payload").cloned().unwrap_or(serde_json::Value::Null);

    match msg_type.as_str() {
        "register_ack" => {
            let node_id = payload
                .get("node_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| SdkError::Auth("register_ack missing node_id".into()))?
                .to_string();
            let node_secret = payload
                .get("node_secret")
                .and_then(|v| v.as_str())
                .ok_or_else(|| SdkError::Auth("register_ack missing node_secret".into()))?
                .to_string();
            Ok(AuthOutcome::Registered {
                node_id,
                node_secret,
            })
        }
        "auth_ok" => Ok(AuthOutcome::Authenticated),
        "auth_error" => {
            // Manager uses either "error" or "message" depending on code path.
            let err = payload
                .get("error")
                .and_then(|v| v.as_str())
                .or_else(|| payload.get("message").and_then(|v| v.as_str()))
                .or_else(|| v.get("message").and_then(|v| v.as_str()))
                .unwrap_or("unknown auth error")
                .to_string();
            Err(SdkError::Auth(err))
        }
        other => Err(SdkError::Auth(format!(
            "unexpected auth response type: {other:?}"
        ))),
    }
}
