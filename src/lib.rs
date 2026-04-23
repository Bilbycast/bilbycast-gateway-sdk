// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Bilbycast-EULA

//! # bilbycast-gateway-sdk
//!
//! SDK for building 3rd-party device gateway sidecars that integrate with
//! `bilbycast-manager`.
//!
//! A "gateway" is a small sidecar binary that:
//! - Connects outbound to the manager over WSS using the same auth / envelope
//!   protocol as edge / relay nodes.
//! - Polls a vendor device's native API (JSON-RPC, REST, SNMP, …) and maps
//!   responses to `stats` / `health` / `event` envelopes.
//! - Translates `command` envelopes from the manager into vendor API calls
//!   and replies with `command_ack`.
//!
//! This crate formalises the manager-facing plumbing — WSS connect, TLS with
//! optional pinning, auth (register + reconnect), heartbeats, reconnect with
//! exponential backoff, graceful shutdown, and standard emitter helpers — so
//! vendors only implement [`CommandHandler`] and their polling loop.
//!
//! ## Minimum viable gateway
//!
//! ```no_run
//! use std::sync::Arc;
//! use bilbycast_gateway_sdk::{
//!     CommandError, CommandHandler, GatewayClient, GatewayConfig,
//! };
//! use async_trait::async_trait;
//! use serde_json::Value;
//!
//! struct MyHandler;
//!
//! #[async_trait]
//! impl CommandHandler for MyHandler {
//!     async fn handle_command(
//!         &self,
//!         _command_id: String,
//!         action: Value,
//!     ) -> Result<Value, CommandError> {
//!         match action.get("type").and_then(|t| t.as_str()) {
//!             Some("ping") => Ok(serde_json::json!({ "pong": true })),
//!             Some(other) => Err(CommandError::unknown_action(other)),
//!             None => Err(CommandError::validation("missing action.type")),
//!         }
//!     }
//!
//!     async fn on_config_request(&self) -> Value {
//!         serde_json::json!({ "status": "hello" })
//!     }
//! }
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let cfg = GatewayConfig::minimal(
//!     "wss://manager.example.com:8443/ws/node",
//!     "my_device",
//!     env!("CARGO_PKG_VERSION"),
//! );
//! let client = GatewayClient::connect(cfg, Arc::new(MyHandler)).await?;
//! let emitter = client.emitter();
//! // spawn your polling task with `emitter`…
//! client.run().await?;
//! # Ok(()) }
//! ```

pub mod auth;
pub mod config;
pub mod dispatch;
pub mod emit;
pub mod envelope;
pub mod errors;
pub mod events;
pub mod tls;
pub mod ws_client;

// ── Public re-exports (flat facade) ──

pub use async_trait::async_trait;
pub use bytes;
pub use tokio_util::sync::CancellationToken;

pub use auth::{
    load_credentials, parse_auth_response, persist_credentials, AuthOutcome, CredentialStore,
    PersistedCredentials,
};
pub use config::{GatewayConfig, ReconnectBackoff};
pub use dispatch::CommandHandler;
pub use emit::{Emitter, OutboundFrame};
pub use envelope::{envelope, IncomingMessage, GATEWAY_WS_PROTOCOL_VERSION};
pub use errors::{CommandError, SdkError};
pub use events::{categories, EventSeverity, GatewayEvent};
pub use ws_client::GatewayClient;
