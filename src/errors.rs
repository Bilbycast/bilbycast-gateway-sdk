// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! SDK error types.

use thiserror::Error;

/// Top-level SDK error.
#[derive(Debug, Error)]
pub enum SdkError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("TLS configuration error: {0}")]
    Tls(String),

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("authentication timed out after 10 seconds")]
    AuthTimeout,

    #[error("credentials I/O error: {0}")]
    CredentialsIo(String),

    #[error("channel send/recv error: {0}")]
    Channel(String),

    #[error("serde_json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("shutdown requested")]
    Shutdown,

    #[error("{0}")]
    Other(String),
}

impl From<tokio_tungstenite::tungstenite::Error> for SdkError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        SdkError::WebSocket(e.to_string())
    }
}

impl From<url::ParseError> for SdkError {
    fn from(e: url::ParseError) -> Self {
        SdkError::Config(format!("invalid URL: {e}"))
    }
}

/// Error returned by a `CommandHandler` to signal a command failure.
///
/// Maps onto the manager's `command_ack` envelope: the `code` rides on
/// `command_ack.error_code` and `message` on `command_ack.error`.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct CommandError {
    /// Stable machine-readable error category. Conventional values:
    /// `"validation_error"`, `"unsupported_codec"`, `"timeout"`,
    /// `"unknown_action"`, `"port_conflict"`, `"bind_failed"`, etc.
    pub code: String,
    /// Human-readable message.
    pub message: String,
    /// Optional structured details, e.g. `{ "field": "outputs.0.bitrate" }`.
    pub details: Option<serde_json::Value>,
}

impl CommandError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn unknown_action(action: impl Into<String>) -> Self {
        let a: String = action.into();
        Self::new("unknown_action", format!("Unknown action: {a}"))
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new("validation_error", message)
    }
}
