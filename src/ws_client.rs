// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Main WebSocket client: WSS connect, reconnect, heartbeat, dispatch.
//!
//! Lifecycle:
//! - `GatewayClient::connect(cfg, handler)` validates config, builds the
//!   TLS config, and returns a constructed-but-not-yet-running client.
//! - `client.run()` enters the connect/reconnect loop until the shutdown
//!   token fires.
//!
//! The connect loop uses multi-URL failover: on WS close or auth failure,
//! we rotate to the next entry in `manager_urls` and apply exponential
//! backoff from `ReconnectBackoff`. Backoff resets on a successful auth.

use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::auth::{parse_auth_response, AuthOutcome};
use crate::config::GatewayConfig;
use crate::dispatch::{action_is_get_config, CommandHandler};
use crate::emit::{Emitter, OutboundFrame};
use crate::envelope::{auth_reconnect, auth_register, envelope, IncomingMessage};
use crate::errors::{CommandError, SdkError};
use crate::tls;

/// Size of the outbound channel that `Emitter` writes into.
const OUTBOUND_CHANNEL_SIZE: usize = 256;

/// Hard timeout for the auth handshake.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum size of an incoming WebSocket message / frame from the manager.
///
/// Mirrors the manager-side cap (`MAX_WS_MSG_SIZE` in `manager-core`). The
/// manager will already reject anything larger before it hits the wire, so
/// in normal operation this limit is never approached. It exists as
/// defense-in-depth: if the manager's cap is ever bypassed or
/// misconfigured, bounding the SDK side prevents a hostile or buggy peer
/// from forcing a multi-gigabyte allocation in the sidecar.
///
/// `tokio-tungstenite`'s default is 64 MiB / 16 MiB (message / frame),
/// which is well above anything legitimate WS traffic produces here.
const MAX_INBOUND_WS_BYTES: usize = 5 * 1024 * 1024;

/// State shared between the read task and the connect loop across reconnects.
#[derive(Debug, Clone, Default)]
struct LiveCredentials {
    node_id: Option<String>,
    node_secret: Option<String>,
    registration_token: Option<String>,
}

/// Callback fired on first-time registration with `(node_id, node_secret)`.
pub type RegisterCallback = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// The running gateway client.
pub struct GatewayClient {
    cfg: GatewayConfig,
    handler: Arc<dyn CommandHandler>,
    outbound_rx: mpsc::Receiver<OutboundFrame>,
    outbound_tx: mpsc::Sender<OutboundFrame>,
    shutdown: CancellationToken,
    /// Populated after first successful registration. Consumers interested in
    /// persisting credentials should call [`Self::credentials_handle`] before
    /// `run()` starts, then save the observed values after every `register_ack`.
    credentials: Arc<Mutex<LiveCredentials>>,
    on_register: Option<RegisterCallback>,
}

impl GatewayClient {
    /// Build but do not yet run a client.
    pub async fn connect(
        cfg: GatewayConfig,
        handler: Arc<dyn CommandHandler>,
    ) -> Result<Self, SdkError> {
        cfg.validate()?;

        let (outbound_tx, outbound_rx) = mpsc::channel::<OutboundFrame>(OUTBOUND_CHANNEL_SIZE);
        let credentials = Arc::new(Mutex::new(LiveCredentials {
            node_id: cfg.node_id.clone(),
            node_secret: cfg.node_secret.clone(),
            registration_token: cfg.registration_token.clone(),
        }));

        Ok(Self {
            cfg,
            handler,
            outbound_rx,
            outbound_tx,
            shutdown: CancellationToken::new(),
            credentials,
            on_register: None,
        })
    }

    /// Get an emitter for sending stats / events / health / thumbnails. Can be
    /// cloned freely — every clone shares the outbound channel.
    pub fn emitter(&self) -> Emitter {
        Emitter::new(self.outbound_tx.clone())
    }

    /// Cancellation token propagated to read + write + heartbeat tasks.
    /// Cancel to trigger a graceful shutdown.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Register a callback invoked once with `(node_id, node_secret)` on every
    /// successful first-time registration. Typical use: write the pair to disk.
    /// Reconnects do not trigger this callback.
    pub fn on_register<F>(&mut self, callback: F)
    where
        F: Fn(&str, &str) + Send + Sync + 'static,
    {
        self.on_register = Some(Arc::new(callback));
    }

    /// Read-only snapshot of the live node_id / node_secret. Populated after
    /// registration; `None` values mean "still waiting".
    pub fn current_credentials(&self) -> (Option<String>, Option<String>) {
        let guard = self.credentials.lock().expect("credentials mutex poisoned");
        (guard.node_id.clone(), guard.node_secret.clone())
    }

    /// Run the connect/reconnect loop until shutdown is requested.
    pub async fn run(mut self) -> Result<(), SdkError> {
        let mut url_cursor: usize = 0;
        let mut attempt: u32 = 0;

        loop {
            if self.shutdown.is_cancelled() {
                info!("Gateway SDK: shutdown requested before connect");
                break;
            }

            let url = self.cfg.manager_urls[url_cursor % self.cfg.manager_urls.len()].clone();
            info!("Gateway SDK: connecting to {url} (attempt {attempt})");

            match self.one_connection(&url).await {
                Ok(ConnectionResult::AuthenticatedThenClosed) => {
                    // Successful session — reset backoff.
                    attempt = 0;
                    info!("Gateway SDK: connection to {url} closed cleanly");
                }
                Ok(ConnectionResult::ShutdownRequested) => break,
                Err(e) => {
                    warn!("Gateway SDK: connection to {url} failed: {e}");
                }
            }

            if self.shutdown.is_cancelled() {
                break;
            }

            url_cursor = url_cursor.wrapping_add(1);
            attempt = attempt.saturating_add(1);
            let delay = self.cfg.reconnect_backoff.delay_for_attempt(attempt);
            info!(
                "Gateway SDK: reconnecting in {} seconds (next URL index {}/{})",
                delay.as_secs(),
                url_cursor % self.cfg.manager_urls.len(),
                self.cfg.manager_urls.len()
            );
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = self.shutdown.cancelled() => break,
            }
        }

        info!("Gateway SDK: shutdown complete");
        Ok(())
    }

    /// One connect → auth → message-loop cycle.
    async fn one_connection(&mut self, url: &str) -> Result<ConnectionResult, SdkError> {
        // Build a fresh TLS config every time — callers may edit their config
        // file (fingerprint rotation etc.) between reconnects. Cheap to rebuild.
        let tls_config = tls::build_tls_config(
            self.cfg.accept_self_signed_cert,
            self.cfg.cert_fingerprint.as_deref(),
        )?;
        let connector = tokio_tungstenite::Connector::Rustls(Arc::new(tls_config));

        // Bound inbound WS message + frame size to match the manager-side cap.
        // See `MAX_INBOUND_WS_BYTES` for rationale.
        let ws_config = WebSocketConfig::default()
            .max_message_size(Some(MAX_INBOUND_WS_BYTES))
            .max_frame_size(Some(MAX_INBOUND_WS_BYTES));

        let (ws_stream, _resp) = tokio::select! {
            res = tokio_tungstenite::connect_async_tls_with_config(
                url,
                Some(ws_config),
                false,
                Some(connector),
            ) => res?,
            _ = self.shutdown.cancelled() => return Ok(ConnectionResult::ShutdownRequested),
        };

        info!("Gateway SDK: WebSocket connected, authenticating…");
        let (mut write, mut read) = ws_stream.split();

        // 1. Send auth frame.
        let (node_id_copy, node_secret_copy, registration_token_copy) = {
            let c = self.credentials.lock().expect("credentials mutex poisoned");
            (
                c.node_id.clone(),
                c.node_secret.clone(),
                c.registration_token.clone(),
            )
        };
        let auth_frame = if let (Some(nid), Some(nsec)) = (&node_id_copy, &node_secret_copy) {
            auth_reconnect(nid, nsec, &self.cfg.software_version, &self.cfg.device_type)
        } else if let Some(token) = &registration_token_copy {
            auth_register(token, &self.cfg.software_version, &self.cfg.device_type)
        } else {
            return Err(SdkError::Config(
                "no credentials or registration token configured".into(),
            ));
        };
        write.send(Message::Text(auth_frame.into())).await?;

        // 2. Wait for auth response with a 10s timeout.
        let auth_text = tokio::select! {
            r = tokio::time::timeout(AUTH_TIMEOUT, read.next()) => match r {
                Ok(Some(Ok(Message::Text(t)))) => t.to_string(),
                Ok(Some(Ok(Message::Binary(_)))) => {
                    return Err(SdkError::Auth("binary auth response".into()));
                }
                Ok(Some(Ok(Message::Close(_)))) => {
                    return Err(SdkError::Auth("connection closed during auth".into()));
                }
                Ok(Some(Ok(_))) => {
                    return Err(SdkError::Auth("non-text auth frame".into()));
                }
                Ok(Some(Err(e))) => return Err(SdkError::from(e)),
                Ok(None) => {
                    return Err(SdkError::Auth("connection closed before auth response".into()));
                }
                Err(_) => return Err(SdkError::AuthTimeout),
            },
            _ = self.shutdown.cancelled() => return Ok(ConnectionResult::ShutdownRequested),
        };

        match parse_auth_response(&auth_text)? {
            AuthOutcome::Registered { node_id, node_secret } => {
                info!("Gateway SDK: registered as node_id={node_id}");
                {
                    let mut c = self.credentials.lock().expect("credentials mutex poisoned");
                    c.node_id = Some(node_id.clone());
                    c.node_secret = Some(node_secret.clone());
                    c.registration_token = None;
                }
                if let Some(cb) = &self.on_register {
                    cb(&node_id, &node_secret);
                }
            }
            AuthOutcome::Authenticated => {
                info!("Gateway SDK: authenticated with manager");
            }
        }

        // 3. Main message loop.
        let mut heartbeat = tokio::time::interval(self.cfg.heartbeat_interval);
        heartbeat.tick().await; // skip the immediate tick

        let emitter = Emitter::new(self.outbound_tx.clone());

        loop {
            tokio::select! {
                biased;

                _ = self.shutdown.cancelled() => {
                    info!("Gateway SDK: shutdown requested, closing WebSocket");
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(ConnectionResult::ShutdownRequested);
                }

                // Inbound from manager
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Err(e) = handle_inbound(
                                &text,
                                self.handler.clone(),
                                &emitter,
                            ).await {
                                debug!("Gateway SDK: inbound handler error: {e}");
                            }
                        }
                        Some(Ok(Message::Binary(_))) => {
                            debug!("Gateway SDK: ignoring binary message from manager");
                        }
                        Some(Ok(Message::Ping(data))) => {
                            // tokio-tungstenite handles automatic pongs via its
                            // default config, but being explicit is safer.
                            let _ = write.send(Message::Pong(data)).await;
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(frame))) => {
                            info!("Gateway SDK: manager closed connection: {frame:?}");
                            return Ok(ConnectionResult::AuthenticatedThenClosed);
                        }
                        Some(Ok(Message::Frame(_))) => {}
                        Some(Err(e)) => {
                            error!("Gateway SDK: WebSocket error: {e}");
                            return Err(SdkError::from(e));
                        }
                        None => {
                            info!("Gateway SDK: WebSocket stream ended");
                            return Ok(ConnectionResult::AuthenticatedThenClosed);
                        }
                    }
                }

                // Outbound frames from Emitter clones
                Some(frame) = self.outbound_rx.recv() => {
                    if let Err(e) = write.send(Message::Text(frame.0.into())).await {
                        error!("Gateway SDK: failed to send outbound frame: {e}");
                        return Err(SdkError::from(e));
                    }
                }

                // Periodic health heartbeat
                _ = heartbeat.tick() => {
                    let health = serde_json::json!({
                        "status": "ok",
                        "version": self.cfg.software_version,
                    });
                    let frame = envelope("health", health);
                    if let Err(e) = write.send(Message::Text(frame.into())).await {
                        error!("Gateway SDK: failed to send heartbeat: {e}");
                        return Err(SdkError::from(e));
                    }
                }
            }
        }
    }
}

enum ConnectionResult {
    AuthenticatedThenClosed,
    ShutdownRequested,
}

/// Handle one inbound envelope from the manager. String-based dispatch with a
/// catch-all so new manager-side message types don't break old gateways.
async fn handle_inbound(
    text: &str,
    handler: Arc<dyn CommandHandler>,
    emitter: &Emitter,
) -> Result<(), SdkError> {
    let Some(incoming) = IncomingMessage::parse(text) else {
        debug!("Gateway SDK: malformed JSON envelope, dropping");
        return Ok(());
    };

    match incoming.msg_type.as_str() {
        "command" => {
            let command_id = incoming
                .payload
                .get("command_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let action = incoming
                .payload
                .get("action")
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            // Special-case `get_config`: invoke the config hook and emit
            // a `config_response` envelope. Ack with success.
            //
            // Emission order: ack FIRST, then config_response. The manager's
            // `command_ack` handler unconditionally invalidates the cached
            // config as a "next request fetches fresh data" defence; sending
            // the ack before the config_response means that invalidation hits
            // an empty cache (no-op), and the subsequent config_response then
            // populates `cached_config` cleanly. Reversing the order would
            // leave the cache cleared immediately after populating it, so
            // `request_config`'s poll loop would never observe the fresh
            // snapshot and would return `None` → HTTP 404 on
            // `/api/v1/nodes/{id}/config`.
            if action_is_get_config(&action) {
                let cfg = handler.on_config_request().await;
                let _ = emitter
                    .emit_command_ack(&command_id, Ok(serde_json::Value::Null))
                    .await;
                return emitter.emit_config_response(cfg).await;
            }

            // General-purpose dispatch.
            let result = handler.handle_command(command_id.clone(), action).await;
            emitter.emit_command_ack(&command_id, result).await
        }
        "ping" => emitter.emit_pong().await,
        "pong" => Ok(()),
        other => {
            // Unknown type — log and ignore; catch-all keeps new manager
            // features non-breaking for old gateways.
            debug!(
                "Gateway SDK: ignoring unknown manager message type: {other:?}"
            );
            Ok(())
        }
    }
}

// For completeness: surface a no-op `_ = CommandError` import to avoid an
// unused-warning if a future downstream consumer removes every error path.
#[allow(dead_code)]
fn _assert_command_error_exported(e: CommandError) -> CommandError {
    e
}
