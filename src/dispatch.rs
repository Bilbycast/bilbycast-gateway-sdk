// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Bilbycast-EULA

//! Incoming command dispatcher.
//!
//! The SDK owns the read loop and funnels every `command` envelope into
//! the user-supplied [`CommandHandler`] trait. The return value is packed
//! into a `command_ack` automatically — the handler never has to touch
//! the wire format.
//!
//! Separate hook `on_config_request` handles the `get_config` command
//! specifically, so the gateway can return its consolidated state
//! snapshot (which the manager stores as `cached_config`) without having
//! to synthesise a fake "success" result.

use async_trait::async_trait;
use serde_json::Value;

use crate::errors::CommandError;

/// User-supplied dispatcher for manager → gateway commands.
///
/// Implementations should:
/// - Map `action["type"]` to a vendor-specific operation.
/// - Return `Ok(Value::Null)` for fire-and-forget commands, or `Ok(data)`
///   to include a result payload on `command_ack.data`.
/// - Return `Err(CommandError)` on failure; the SDK lifts `code` → `error_code`
///   and `message` → `error`, preserving the edge's unified error-code model.
#[async_trait]
pub trait CommandHandler: Send + Sync + 'static {
    /// Handle one command from the manager.
    async fn handle_command(
        &self,
        command_id: String,
        action: Value,
    ) -> Result<Value, CommandError>;

    /// Return the current snapshot of the gateway's config, to be sent as a
    /// `config_response` envelope. Called whenever the manager issues a
    /// `get_config` command. Default: returns `null`.
    async fn on_config_request(&self) -> Value {
        Value::Null
    }
}

/// Convenience helper: does this action look like `get_config`?
pub fn action_is_get_config(action: &Value) -> bool {
    matches!(
        action.get("type").and_then(|t| t.as_str()),
        Some("get_config")
    )
}
