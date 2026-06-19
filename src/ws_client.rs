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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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

/// Lock-free, cloneable view of the manager-link connection state.
///
/// This is the SDK's device-side link indicator: a sidecar built on the SDK
/// can clone a [`ConnectionState`] handle (via [`GatewayClient::connection_state`])
/// before `run()` and read it from any task — an HTTP `/health` endpoint, a
/// local UI, a status LED driver — to surface "manager link up/down" locally,
/// without touching the manager↔node WebSocket protocol.
///
/// Updates are written by the SDK's connect/reconnect loop:
/// `connected` flips to `true` on a successful auth handshake and back to
/// `false` the moment the session ends or a connect attempt fails.
#[derive(Debug, Clone)]
pub struct ConnectionState {
    connected: Arc<AtomicBool>,
    /// Unix epoch seconds of the most recent successful auth, or `0` if the
    /// link has never come up since process start.
    last_connect_epoch: Arc<AtomicU64>,
}

impl ConnectionState {
    fn new() -> Self {
        Self {
            connected: Arc::new(AtomicBool::new(false)),
            last_connect_epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    /// `true` while the manager WebSocket link is up (authenticated and not yet
    /// closed). Lock-free; safe to poll on a hot path.
    pub fn is_connected(&self) -> bool {
        // Acquire pairs with the Release stores in `set_connected` /
        // `set_disconnected`: a reader that observes `connected == true` is
        // guaranteed to also see the epoch written before the flag.
        self.connected.load(Ordering::Acquire)
    }

    /// Unix epoch seconds of the most recent successful auth handshake, or
    /// `None` if the link has never come up since process start.
    pub fn last_connect_epoch(&self) -> Option<u64> {
        match self.last_connect_epoch.load(Ordering::Acquire) {
            0 => None,
            secs => Some(secs),
        }
    }

    /// Mark the link up and record the connect time. Called on successful auth.
    fn set_connected(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Store the epoch BEFORE the connected flag, both with Release ordering,
        // so a reader observing `connected == true` via an Acquire load is
        // guaranteed to also see this epoch (no torn cross-atomic view).
        self.last_connect_epoch.store(now, Ordering::Release);
        self.connected.store(true, Ordering::Release);
    }

    /// Mark the link down. Called on session end / connect failure.
    fn set_disconnected(&self) {
        self.connected.store(false, Ordering::Release);
    }
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self::new()
    }
}

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
    /// Lock-free manager-link state, cloneable to any task for local surfacing.
    connection_state: ConnectionState,
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
            connection_state: ConnectionState::new(),
        })
    }

    /// Cloneable, lock-free handle to the manager-link connection state.
    ///
    /// Call this before `run()` and hand the clone to any task (HTTP health
    /// endpoint, local UI, status indicator) that needs to surface whether the
    /// manager link is up. The handle keeps reflecting live state across
    /// reconnects for the lifetime of the process.
    pub fn connection_state(&self) -> ConnectionState {
        self.connection_state.clone()
    }

    /// Convenience: `true` while the manager WebSocket link is up. Equivalent to
    /// `self.connection_state().is_connected()`.
    pub fn is_connected(&self) -> bool {
        self.connection_state.is_connected()
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

            let result = self.one_connection(&url).await;

            // Whatever the outcome, the link is now down. Flip the public
            // connection flag so any task surfacing local link state reacts
            // immediately. (`one_connection` sets it true on successful auth.)
            self.connection_state.set_disconnected();

            match result {
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
            // Primary device-side outage signal for headless sidecars (no local
            // UI): one rate-limited WARN per reconnect attempt while the manager
            // link is down. An operator tailing the log sees the outage and the
            // retry cadence without it spamming once-per-second.
            warn!(
                "Gateway SDK: manager link unavailable, reconnecting (attempt {attempt}, next in {}s)",
                delay.as_secs()
            );
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

        // Auth succeeded — the manager link is up. Publish it on the lock-free
        // connection flag so any task surfacing local link state sees it.
        self.connection_state.set_connected();

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_state_starts_disconnected() {
        let s = ConnectionState::new();
        assert!(!s.is_connected());
        assert_eq!(s.last_connect_epoch(), None);
    }

    #[test]
    fn connection_state_tracks_up_then_down() {
        let s = ConnectionState::new();
        s.set_connected();
        assert!(s.is_connected());
        // Connect epoch is recorded (non-zero in any realistic test run).
        assert!(s.last_connect_epoch().is_some());

        let epoch_after_up = s.last_connect_epoch();
        s.set_disconnected();
        assert!(!s.is_connected());
        // Going down preserves the last-connect timestamp.
        assert_eq!(s.last_connect_epoch(), epoch_after_up);
    }

    #[test]
    fn connection_state_clone_shares_underlying_atomics() {
        let s = ConnectionState::new();
        let cloned = s.clone();
        s.set_connected();
        // The clone observes the same state — it's a handle, not a snapshot.
        assert!(cloned.is_connected());
        s.set_disconnected();
        assert!(!cloned.is_connected());
    }
}
