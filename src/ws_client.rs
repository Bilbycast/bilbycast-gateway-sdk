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
//! backoff from `ReconnectBackoff`. Backoff resets when an authenticated
//! session ends with a close rather than an error.
//!
//! ## Every await is bounded
//!
//! The reconnect loop is one task, so a single `.await` in it that never
//! returns ends reconnection for the life of the process: the gateway sits
//! offline in the manager until someone restarts it, with nothing logged
//! after "connecting to …". That is how bilbycast-edge#180 stranded three
//! production edges, and this client had every one of the same unbounded
//! awaits. Each step now has a bound, the same model the edge and the relay
//! use:
//!
//! - **Dial** — DNS, TCP, TLS and the HTTP upgrade share one 30 s deadline,
//!   and TCP tries the manager's addresses Happy-Eyeballs style, so one
//!   address that silently drops SYNs cannot use up the budget.
//! - **Auth** — 10 s to write the auth frame, 10 s for the reply.
//! - **Session** — a reader running beside the writes timestamps every frame
//!   and learns how often the manager pings. A manager silent for four ping
//!   intervals (never less than 90 s) is taken for dead. Every write is
//!   bounded at 300 s, which also covers a manager that never pings.
//! - **Commands** — run on a worker task, one at a time, never inline in the
//!   session loop: a long vendor command would otherwise stop the reader, and
//!   its `command_ack` would wait on the outbound channel only that parked
//!   loop drains.
//! - **Shutdown** — 2 s to write the Close frame, then up to 30 s for a
//!   command already running to finish.

use futures_util::stream::{FuturesUnordered, SplitSink, SplitStream};
use futures_util::{SinkExt, Stream, StreamExt};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinError;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::error::{Error as WsError, ProtocolError};
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Bytes, Message, Utf8Bytes};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::auth::{parse_auth_response, AuthOutcome};
use crate::config::GatewayConfig;
use crate::dispatch::{action_is_get_config, CommandHandler};
use crate::emit::{command_ack_payload, Emitter, OutboundFrame};
use crate::envelope::{auth_reconnect, auth_register, envelope, IncomingMessage};
use crate::errors::{CommandError, SdkError};
use crate::tls;

/// Size of the outbound channel that `Emitter` writes into.
const OUTBOUND_CHANNEL_SIZE: usize = 256;

/// Hard deadline on the whole dial: DNS, TCP, TLS and the HTTP upgrade.
///
/// `tokio_tungstenite::connect_async*` has no deadline of its own, and this
/// dial used to be raced only against shutdown. A peer that completes TCP
/// (and TLS) and then never returns the 101 leaves no unacknowledged data on
/// the socket, so TCP never times anything out either: the wait was for
/// ever, and with it the reconnect loop.
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a TCP attempt has to itself before the next address is tried
/// alongside it (RFC 8305's Connection Attempt Delay). See
/// [`connect_staggered`].
const CONNECT_ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// A dial slower than this is logged even when it eventually succeeds, so the
/// condition is visible before it becomes an outage.
const SLOW_DIAL_WARN: Duration = Duration::from_secs(5);

/// Deadline on writing the auth frame. A stalled peer stops draining long
/// before it closes, so this write can block just as the dial can.
const AUTH_SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard timeout for the auth reply.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// Inbound silence is never held to less than this, however fast the manager
/// pings. It is only a floor: the real limit scales with the cadence the
/// manager is actually using (see [`InboundCadence::silence_limit`]).
const SILENCE_FLOOR: Duration = Duration::from_secs(90);

/// How many ping intervals of silence end a session. Four, so one lost ping
/// or one slow burst on the path cannot do it on its own.
const SILENCE_PINGS: u32 = 4;

/// How often the reader checks for silence. Coarse on purpose: the limit is
/// measured in minutes, and a check is a clock read.
const SILENCE_CHECK: Duration = Duration::from_secs(5);

/// How many of the most recent ping gaps the cadence is learned from.
const PING_GAPS_KEPT: usize = 8;

/// No ping gap counts for more than this. It is the longest keepalive the
/// manager can be set to, so a longer gap is the manager stalling rather than
/// pinging slowly, and a stall must not teach the watchdog to wait longer.
const MAX_PING_GAP: Duration = Duration::from_secs(300);

/// Bound on every write the session makes, independent of the ping cadence.
///
/// A write that cannot complete for five minutes means the manager has
/// stopped draining the socket: a wedged per-connection handler, or a proxy
/// that kept the TCP connection and stalled upstream. Its kernel keeps
/// ACKing our zero-window probes, so TCP never gives up and nothing else
/// would end the wait. It is also the only bound before the first ping, and
/// against a manager too old to ping at all.
const WRITE_STALL_LIMIT: Duration = Duration::from_secs(300);

/// Bound on the Close frame written at shutdown. A manager that has stopped
/// reading would otherwise hold SIGTERM up until systemd's SIGKILL.
const CLOSE_SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// How long `run()` waits at shutdown for a command that has already
/// started, so that stopping the gateway does not cut a vendor command off
/// part-way through a change. Bounded like every other await here, so a
/// handler that never returns cannot hold shutdown up for ever.
const COMMAND_DRAIN_LIMIT: Duration = Duration::from_secs(30);

/// Frames the reader has handed the session loop that the loop has not yet
/// taken. Only a loop held up in a write lets this fill.
const INBOUND_CHANNEL_SIZE: usize = 256;

/// Bytes of text frames the reader may have handed the session loop that the
/// loop has not yet taken; past this the reader waits for the loop before it
/// reads on. The channel bounds only how many: 256 frames of
/// [`MAX_INBOUND_WS_BYTES`] would be 1.25 GiB.
const INBOUND_BUDGET_BYTES: usize = 4 * MAX_INBOUND_WS_BYTES;

/// Commands that may wait behind the one running, and at most
/// [`COMMAND_QUEUE_BYTES`] of them. Past either the session loop answers each
/// new command with a `gateway_busy` error ack at once, so the manager hears
/// "not run" rather than timing out on a command that might still run much
/// later.
const COMMAND_QUEUE_SIZE: usize = 64;

/// Bytes of commands, as they came off the wire, that may wait behind the one
/// running. A command is queued as its frame and parsed only when it starts,
/// because a parsed action can take many times its wire size.
const COMMAND_QUEUE_BYTES: usize = 4 * MAX_INBOUND_WS_BYTES;

/// `error_code` on the ack for a command refused because the queue is full.
const BUSY_ERROR_CODE: &str = "gateway_busy";

/// `error_code` on the ack for a command whose handler panicked.
const PANICKED_ERROR_CODE: &str = "handler_panicked";

/// Maximum size of an incoming WebSocket message / frame from the manager.
///
/// Mirrors the manager-side cap (`MAX_WS_MSG_SIZE` in `manager-core`). The
/// manager will already reject anything larger before it hits the wire, so
/// in normal operation this limit is never approached. It exists as
/// defense-in-depth: if the manager's cap is ever bypassed or
/// misconfigured, bounding the SDK side prevents a hostile or buggy peer
/// from forcing a multi-gigabyte allocation in the sidecar.
///
/// More than one message can be held at a time, because the reader keeps
/// reading while the session loop is held up in a write and commands wait
/// behind the one running, so those queues are bounded in bytes as well as
/// in count. At most [`INBOUND_BUDGET_BYTES`] of frames wait for the loop and
/// [`COMMAND_QUEUE_BYTES`] of commands for the worker. Besides those there
/// is one frame in the reader's hands and one in tungstenite's buffer, the
/// parse of the frame the loop is routing, and the command running, parsed.
///
/// `tokio-tungstenite`'s default is 64 MiB / 16 MiB (message / frame),
/// which is well above anything legitimate WS traffic produces here.
const MAX_INBOUND_WS_BYTES: usize = 5 * 1024 * 1024;

/// The stream every dial produces: the same type `connect_async_tls_with_config`
/// returned, so nothing downstream of the dial changed.
type ManagerStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type ManagerSink = SplitSink<ManagerStream, Message>;
type ManagerSource = SplitStream<ManagerStream>;

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
    /// The turn to run commands, held by one session's command worker at a
    /// time. It is handed out in the order the workers asked for it, so a
    /// session's worker starts only once the previous one has finished, and
    /// commands never overlap across a reconnect either.
    command_turn: Arc<Semaphore>,
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
            command_turn: Arc::new(Semaphore::new(1)),
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

    /// Run the connect/reconnect loop until shutdown is requested. A command
    /// that has started by then is given up to 30 s to finish before this
    /// returns. Its ack is not sent, as no session is left to carry it, and
    /// from the moment the loop ends every [`Emitter`] send fails.
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
                Ok(ConnectionResult::AuthenticatedThenLost(e)) => {
                    // Backoff carries on rather than resetting, as it always
                    // has for a session that ended in an error.
                    warn!("Gateway SDK: session with {url} lost: {e}");
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

        // Let a command that has started finish, or the runtime's teardown
        // could cut it off part-way through a change. It may belong to an
        // earlier session (shutdown can come during the backoff or a dial),
        // so wait for the command turn rather than for this session's worker:
        // every session is over, so no worker starts another command, and the
        // one running gives the turn up when it returns. Nothing drains the
        // outbound channel any more, so close it first: the command's ack,
        // and any vendor emit, then fails at once instead of waiting for room
        // that never comes.
        self.outbound_rx.close();
        if tokio::time::timeout(COMMAND_DRAIN_LIMIT, self.command_turn.acquire()).await.is_err() {
            warn!(
                "Gateway SDK: a command was still running {}s after shutdown; abandoning it",
                COMMAND_DRAIN_LIMIT.as_secs()
            );
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

        let ws_stream = tokio::select! {
            res = dial_manager(url, ws_config, connector) => res?,
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
        tokio::select! {
            sent = tokio::time::timeout(
                AUTH_SEND_TIMEOUT,
                write.send(Message::Text(auth_frame.into())),
            ) => match sent {
                Ok(sent) => sent?,
                Err(_) => {
                    return Err(SdkError::WebSocket(format!(
                        "the manager did not take the auth frame within {}s",
                        AUTH_SEND_TIMEOUT.as_secs()
                    )));
                }
            },
            _ = self.shutdown.cancelled() => return Ok(ConnectionResult::ShutdownRequested),
        }

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

        // 3. The session.
        Ok(self.run_session(write, read).await)
    }

    /// Run one authenticated session until the manager closes it, the session
    /// dies or shutdown is requested.
    ///
    /// Three parts. The reader ([`read_manager`]) reads every frame the moment
    /// it arrives, and is where liveness is judged. The session loop
    /// ([`drive_session`]) is the only writer. The command worker
    /// ([`run_commands`]), spawned, runs the vendor's handler. The reader and
    /// the loop run in this task, raced against each other and against
    /// shutdown, so a write the manager is not draining holds up neither the
    /// reading nor a shutdown.
    async fn run_session(
        &mut self,
        mut write: ManagerSink,
        read: ManagerSource,
    ) -> ConnectionResult {
        let started = Instant::now();
        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CHANNEL_SIZE);
        let inbound_budget = Arc::new(Semaphore::new(INBOUND_BUDGET_BYTES));
        let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE_SIZE);

        // Cancelled as soon as the session ends, so the worker never starts a
        // command for a session that has already ended. The guard covers this
        // future being dropped part-way.
        let session_over = CancellationToken::new();
        let _session_over = session_over.clone().drop_guard();
        tokio::spawn(run_commands(
            self.command_turn.clone(),
            command_rx,
            session_over.clone(),
            self.handler.clone(),
            Emitter::new(self.outbound_tx.clone()),
        ));

        let shutdown = self.shutdown.clone();
        let end = tokio::select! {
            biased;

            _ = shutdown.cancelled() => ConnectionResult::ShutdownRequested,

            (silent, limit) = read_manager(read, inbound_tx, inbound_budget, started) => {
                ConnectionResult::AuthenticatedThenLost(SdkError::WebSocket(format!(
                    "nothing from the manager for {}s, past the {}s its pings allow — \
                     taking the session for dead",
                    silent.as_secs(),
                    limit.as_secs()
                )))
            }

            end = drive_session(
                &mut write,
                inbound_rx,
                &mut self.outbound_rx,
                command_tx,
                self.cfg.heartbeat_interval,
                &self.cfg.software_version,
            ) => end,
        };

        // Now, not when this returns: the Close below can take its full 2 s,
        // and a command that finished in the meantime would otherwise let the
        // worker start the next one queued, for shutdown to wait on.
        session_over.cancel();

        if let ConnectionResult::ShutdownRequested = end {
            info!("Gateway SDK: shutdown requested, closing WebSocket");
            let close = write.send(Message::Close(None));
            let _ = tokio::time::timeout(CLOSE_SEND_TIMEOUT, close).await;
        }
        end
    }
}

enum ConnectionResult {
    /// Authenticated, then the manager closed the session or the stream ended.
    AuthenticatedThenClosed,
    /// Authenticated, then the session died: silence, a stalled write, a
    /// socket error.
    AuthenticatedThenLost(SdkError),
    ShutdownRequested,
}

/// Dial the manager: DNS, TCP, then TLS and the HTTP upgrade, every stage
/// inside one [`DIAL_TIMEOUT`].
///
/// This is what `connect_async_tls_with_config` does, staged so that each
/// stage is bounded and TCP is never stuck on one address. tokio's
/// `TcpStream::connect(host)`, which `connect_async*` uses, tries the resolved
/// addresses strictly one after another with no per-address deadline, so a
/// first address that silently drops SYNs holds it for the kernel's SYN
/// timeout (about 127 s on Linux). Inside a 30 s budget the second address
/// would then never be tried, and a gateway that used to connect slowly over
/// a broken IPv6 path would never connect at all. Raising the deadline or
/// sorting IPv4 first only moves that cliff; [`connect_staggered`] removes it.
///
/// The last stage is `client_async_tls_with_config`, the very function
/// `connect_async_tls_with_config` hands its socket to, so TLS, SNI,
/// certificate pinning and the self-signed verifier behave exactly as before.
/// Nagle stays on, as the old `disable_nagle: false` left it.
async fn dial_manager(
    url: &str,
    ws_config: WebSocketConfig,
    connector: tokio_tungstenite::Connector,
) -> Result<ManagerStream, SdkError> {
    let started = Instant::now();
    let deadline = started + DIAL_TIMEOUT;
    let (host, port) = manager_host_port(url)?;

    let lookup = tokio::net::lookup_host((host.as_str(), port));
    let addrs: Vec<SocketAddr> = match tokio::time::timeout_at(deadline, lookup).await {
        Ok(Ok(addrs)) => addrs.collect(),
        Ok(Err(e)) => {
            return Err(SdkError::WebSocket(format!(
                "manager DNS lookup for {host} failed: {e}"
            )));
        }
        Err(_) => {
            return Err(SdkError::WebSocket(format!(
                "manager DNS lookup for {host} timed out after {}s",
                DIAL_TIMEOUT.as_secs()
            )));
        }
    };
    let count = addrs.len();

    let tcp = match tokio::time::timeout_at(
        deadline,
        connect_staggered(interleave_families(addrs), TcpStream::connect),
    )
    .await
    {
        Ok(Ok(Connected { stream, addr, abandoned })) => {
            debug!("Gateway SDK: TCP connected to {host} at {addr}");
            if !abandoned.is_empty() {
                let abandoned: Vec<String> = abandoned.iter().map(SocketAddr::to_string).collect();
                warn!(
                    "Gateway SDK: TCP connected to {host} at {addr}, after {} gave no answer — \
                     that path to the manager may be broken (often IPv6)",
                    abandoned.join(", ")
                );
            }
            stream
        }
        Ok(Err(e)) => {
            return Err(SdkError::WebSocket(format!(
                "TCP connect to {host} ({count} addresses) failed: {e}"
            )));
        }
        Err(_) => {
            return Err(SdkError::WebSocket(format!("TCP connect to {host} timed out")));
        }
    };

    let upgrade =
        tokio_tungstenite::client_async_tls_with_config(url, tcp, Some(ws_config), Some(connector));
    let (stream, _response) = match tokio::time::timeout_at(deadline, upgrade).await {
        Ok(Ok(ok)) => ok,
        Ok(Err(e)) => {
            return Err(SdkError::WebSocket(format!(
                "TLS/WebSocket upgrade with {host} failed: {e}"
            )));
        }
        Err(_) => {
            return Err(SdkError::WebSocket(format!(
                "TLS/WebSocket upgrade with {host} timed out after {}s",
                DIAL_TIMEOUT.as_secs()
            )));
        }
    };

    let took = started.elapsed();
    if took >= SLOW_DIAL_WARN {
        warn!(
            "Gateway SDK: manager dial to {host} took {:.1}s — slow handshake, watch for a stall",
            took.as_secs_f64()
        );
    }
    Ok(stream)
}

/// The host and port to resolve for a manager URL: the port defaults to 443
/// for `wss://` and 80 for `ws://`, and an IPv6 literal loses its brackets.
fn manager_host_port(url: &str) -> Result<(String, u16), SdkError> {
    let parsed = url::Url::parse(url)?;
    let host = match parsed.host() {
        Some(url::Host::Domain(domain)) => domain.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => return Err(SdkError::Config(format!("manager URL {url} has no host"))),
    };
    let port = parsed.port_or_known_default().ok_or_else(|| {
        SdkError::Config(format!("manager URL {url} has no port and no default one"))
    })?;
    Ok((host, port))
}

/// Order addresses for [`connect_staggered`]: alternate address families,
/// starting with the family of the first address the resolver returned
/// (RFC 8305 §4). A host whose IPv6 path is broken then reaches its first
/// IPv4 address one stagger in, rather than after every IPv6 address has had
/// a turn.
fn interleave_families(addrs: Vec<SocketAddr>) -> VecDeque<SocketAddr> {
    let Some(first) = addrs.first() else {
        return VecDeque::new();
    };
    let lead_is_v6 = first.is_ipv6();
    let (lead, other): (Vec<_>, Vec<_>) =
        addrs.into_iter().partition(|addr| addr.is_ipv6() == lead_is_v6);
    let mut ordered = VecDeque::with_capacity(lead.len() + other.len());
    let (mut lead, mut other) = (lead.into_iter(), other.into_iter());
    loop {
        match (lead.next(), other.next()) {
            (None, None) => return ordered,
            (a, b) => ordered.extend(a.into_iter().chain(b)),
        }
    }
}

/// What [`connect_staggered`] connected to.
struct Connected<S> {
    stream: S,
    addr: SocketAddr,
    /// Addresses started before `addr` that had neither connected nor failed
    /// when it won: the path that is quietly dropping SYNs.
    abandoned: Vec<SocketAddr>,
}

/// Connect to whichever of `addrs` answers first, Happy-Eyeballs style
/// (RFC 8305, without its DNS racing).
///
/// The first attempt starts at once. Each further one starts
/// [`CONNECT_ATTEMPT_DELAY`] after the last if nothing has connected yet, or
/// straight away when an attempt fails, and attempts already started keep
/// running. The first to connect wins and the rest are dropped, which closes
/// them. When every address has failed, the last error is returned.
///
/// There is no per-address deadline: the caller bounds the whole stage with
/// what is left of [`DIAL_TIMEOUT`]. What matters is that one silent address
/// can no longer hold the others back.
async fn connect_staggered<S, F, Fut>(
    mut waiting: VecDeque<SocketAddr>,
    connect: F,
) -> io::Result<Connected<S>>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = io::Result<S>>,
{
    let start = |seq: usize, addr: SocketAddr| {
        let attempt = connect(addr);
        async move { (seq, addr, attempt.await) }
    };
    let mut attempts = FuturesUnordered::new();
    // Started and not yet finished, by start order.
    let mut in_flight: Vec<(usize, SocketAddr)> = Vec::new();
    let mut started = 0;
    let mut last_err = None;

    loop {
        // Every pass starts the next address: the first at once, each later
        // one because the last attempt failed or its stagger ran out.
        if let Some(addr) = waiting.pop_front() {
            in_flight.push((started, addr));
            attempts.push(start(started, addr));
            started += 1;
        }
        if attempts.is_empty() {
            return Err(last_err.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "the name resolved to no addresses")
            }));
        }

        tokio::select! {
            Some((seq, addr, result)) = attempts.next() => {
                in_flight.retain(|(s, _)| *s != seq);
                match result {
                    Ok(stream) => {
                        let abandoned = in_flight
                            .into_iter()
                            .filter(|(s, _)| *s < seq)
                            .map(|(_, addr)| addr)
                            .collect();
                        return Ok(Connected { stream, addr, abandoned });
                    }
                    // A failure frees its turn now, not at the next stagger.
                    Err(e) => {
                        debug!("Gateway SDK: TCP connect to {addr} failed: {e}");
                        last_err = Some(e);
                    }
                }
            }
            () = tokio::time::sleep(CONNECT_ATTEMPT_DELAY), if !waiting.is_empty() => {}
        }
    }
}

/// What the session has heard from the manager, and how often it pings.
///
/// The manager pings every node on its `ws_keepalive_interval_secs` setting,
/// which an operator can set anywhere from 1 to 300 s (default 15 s) and which
/// stays fixed for the life of one connection. So the silence that means
/// "dead" is learned from the pings themselves rather than assumed.
///
/// The watchdog is **unarmed until the first ping**: a manager from before the
/// keepalive never pings, and treating its quiet as death would drop a healthy
/// session every few minutes. The first gap is measured from the start of the
/// session, an over-estimate of the interval and so the safe direction. The
/// limit comes from the **median** of the last [`PING_GAPS_KEPT`] gaps, not
/// the widest: a running maximum only ever grows, and one 85 s stall on the
/// manager would hold the limit at 340 s for the rest of the session.
#[derive(Debug)]
struct InboundCadence {
    started: Instant,
    last_frame: Instant,
    last_ping: Option<Instant>,
    gaps: VecDeque<Duration>,
}

impl InboundCadence {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            last_frame: now,
            last_ping: None,
            gaps: VecDeque::with_capacity(PING_GAPS_KEPT),
        }
    }

    /// Any frame at all from the manager proves the session is alive.
    fn on_frame(&mut self, now: Instant) {
        self.last_frame = now;
    }

    /// A ping also teaches the cadence.
    fn on_ping(&mut self, now: Instant) {
        self.on_frame(now);
        let since = self.last_ping.unwrap_or(self.started);
        let gap = now.saturating_duration_since(since).min(MAX_PING_GAP);
        if self.gaps.len() == PING_GAPS_KEPT {
            self.gaps.pop_front();
        }
        self.gaps.push_back(gap);
        self.last_ping = Some(now);
    }

    /// The silence that ends the session, or `None` while no ping has come:
    /// the watchdog is unarmed until then.
    fn silence_limit(&self) -> Option<Duration> {
        let mut gaps: Vec<Duration> = self.gaps.iter().copied().collect();
        gaps.sort_unstable();
        // The upper median: with an even count it takes the longer of the two
        // middle gaps, which is the safe direction.
        let median = *gaps.get(gaps.len() / 2)?;
        Some(SILENCE_FLOOR.max(median.saturating_mul(SILENCE_PINGS)))
    }

    /// How long the manager has been silent, with the limit, if that is past
    /// the limit.
    fn dead_for(&self, now: Instant) -> Option<(Duration, Duration)> {
        let limit = self.silence_limit()?;
        let silent = now.saturating_duration_since(self.last_frame);
        (silent > limit).then_some((silent, limit))
    }
}

/// What the reader hands the session loop, in the order it read it.
enum Inbound {
    /// A text frame, with its share of [`INBOUND_BUDGET_BYTES`], given back
    /// once the loop has routed it.
    Text(Utf8Bytes, OwnedSemaphorePermit),
    /// A WS Ping to answer with a Pong.
    Ping(Bytes),
    Closed(Option<CloseFrame>),
    Failed(WsError),
    Ended,
}

/// Read the manager for the whole session, beside the session loop's writes.
///
/// This is where liveness is judged, and it is a future of its own rather
/// than a branch of the session loop's `select!` on purpose. The loop awaits
/// its writes inside its branches; one held up by slow writes (a burst of
/// stats or thumbnails on a starved uplink) would leave the manager's pings
/// unread in the kernel buffer, and a watchdog stamped from that branch would
/// take a live but busy manager for a dead one. Here every frame is stamped as
/// it is read, whatever the writes are doing.
///
/// Text, a Close, an error or the end of the stream go to the session loop in
/// the order they were read, and a Ping goes over as a request for a Pong.
/// The order has the loop route a command before it ends the session on what
/// was read after it, but routing is not running: the worker starts nothing
/// for a session that has ended, so a command read in the same burst as the
/// end runs only if the worker took it before the loop reached the end, a
/// race on a multi-thread runtime (see [`run_commands`]). A text frame waits
/// for its share of `budget` first, so a loop held up in a write holds up the
/// reading once that many bytes are waiting for it, rather than once the
/// channel's count is. Returns only when the manager has been silent past its
/// limit, as `(silent, limit)`. Every other end of the session is the loop's
/// to report, so once it has been handed one, this parks.
async fn read_manager<S>(
    mut read: S,
    inbound: mpsc::Sender<Inbound>,
    budget: Arc<Semaphore>,
    started: Instant,
) -> (Duration, Duration)
where
    S: Stream<Item = Result<Message, WsError>> + Unpin,
{
    let mut cadence = InboundCadence::new(started);
    let mut check = tokio::time::interval(SILENCE_CHECK);
    check.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Frames first, so that after a wait on a full channel below,
            // whatever queued up in the socket is read before silence is
            // judged.
            biased;

            frame = read.next() => {
                let now = Instant::now();
                let handover = match frame {
                    Some(Ok(Message::Ping(data))) => {
                        cadence.on_ping(now);
                        // tungstenite has already queued its own Pong for this
                        // ping, so a full channel costs nothing here.
                        let _ = inbound.try_send(Inbound::Ping(data));
                        continue;
                    }
                    Some(Ok(Message::Text(text))) => {
                        cadence.on_frame(now);
                        // A frame is at most `MAX_INBOUND_WS_BYTES`, a quarter
                        // of the budget, so this comes back once the loop has
                        // routed what is ahead of it.
                        let cost = u32::try_from(text.len()).unwrap_or(u32::MAX);
                        let Ok(held) = budget.clone().acquire_many_owned(cost).await else {
                            return std::future::pending().await;
                        };
                        Inbound::Text(text, held)
                    }
                    Some(Ok(Message::Close(frame))) => {
                        cadence.on_frame(now);
                        Inbound::Closed(frame)
                    }
                    Some(Ok(Message::Binary(_))) => {
                        cadence.on_frame(now);
                        debug!("Gateway SDK: ignoring binary message from manager");
                        continue;
                    }
                    Some(Ok(Message::Pong(_) | Message::Frame(_))) => {
                        cadence.on_frame(now);
                        continue;
                    }
                    Some(Err(e)) => Inbound::Failed(e),
                    None => Inbound::Ended,
                };
                let more = matches!(handover, Inbound::Text(..));
                if inbound.send(handover).await.is_err() || !more {
                    return std::future::pending().await;
                }
                // Waiting for the loop, for budget or for room in the channel,
                // is this side's backpressure, not the manager going quiet,
                // and nothing was read meanwhile. Time the silence from here:
                // with the next frame only half arrived, the check would
                // otherwise judge it from a stamp taken before the wait.
                cadence.on_frame(Instant::now());
            }

            _ = check.tick() => {
                if let Some(dead) = cadence.dead_for(Instant::now()) {
                    return dead;
                }
            }
        }
    }
}

/// The session loop: the only writer of frames on the socket, tungstenite's
/// own replies to pings aside. It sends what the vendor emitted, the
/// heartbeat, pongs and the replies it makes itself, and hands commands to the
/// worker.
///
/// Every write is bounded by [`WRITE_STALL_LIMIT`], and nothing else in here
/// waits on anything unbounded (commands go to the worker with `try_send`), so
/// the loop always comes back round to drain the outbound channel that the
/// worker's acks and the vendor's frames ride on.
async fn drive_session(
    write: &mut ManagerSink,
    mut inbound: mpsc::Receiver<Inbound>,
    outbound: &mut mpsc::Receiver<OutboundFrame>,
    commands: mpsc::Sender<QueuedCommand>,
    heartbeat_interval: Duration,
    software_version: &str,
) -> ConnectionResult {
    let mut heartbeat = tokio::time::interval(heartbeat_interval);
    heartbeat.tick().await; // skip the immediate tick
    let command_budget = Arc::new(Semaphore::new(COMMAND_QUEUE_BYTES));

    loop {
        let sent = tokio::select! {
            biased;

            // Inbound from manager
            frame = inbound.recv() => match frame {
                Some(Inbound::Text(text, _held)) => {
                    route_inbound(&text, &commands, &command_budget, write).await
                }
                Some(Inbound::Ping(data)) => {
                    // tungstenite answers pings itself, but being explicit is
                    // safer.
                    send_bounded(write, Message::Pong(data), "pong").await
                }
                Some(Inbound::Closed(frame)) => {
                    info!("Gateway SDK: manager closed connection: {frame:?}");
                    return ConnectionResult::AuthenticatedThenClosed;
                }
                Some(Inbound::Failed(e)) => {
                    error!("Gateway SDK: WebSocket error: {e}");
                    return ConnectionResult::AuthenticatedThenLost(SdkError::from(e));
                }
                Some(Inbound::Ended) | None => {
                    info!("Gateway SDK: WebSocket stream ended");
                    return ConnectionResult::AuthenticatedThenClosed;
                }
            },

            // Outbound frames from Emitter clones
            Some(frame) = outbound.recv() => {
                send_bounded(write, Message::Text(frame.0.into()), "outbound").await
            }

            // Periodic health heartbeat
            _ = heartbeat.tick() => {
                let health = json!({
                    "status": "ok",
                    "version": software_version,
                });
                let frame = envelope("health", health);
                send_bounded(write, Message::Text(frame.into()), "heartbeat").await
            }
        };
        if let Err(e) = sent {
            return ConnectionResult::AuthenticatedThenLost(e);
        }
    }
}

/// Write one frame, giving up after [`WRITE_STALL_LIMIT`].
///
/// A frame refused because the manager has closed the connection does not
/// fail the session. The reader runs beside the writes, so it can read the
/// manager's Close while a frame is still on its way here (the Pong for a
/// ping read just before the Close, say), and tungstenite refuses every write
/// from then on. The reader has handed that Close to the session loop behind
/// whatever it read first, so the loop ends the session on it as a clean
/// close. Failing here instead would log a clean close as a lost session and
/// leave the reconnect backoff climbing.
async fn send_bounded(
    write: &mut ManagerSink,
    message: Message,
    what: &str,
) -> Result<(), SdkError> {
    match tokio::time::timeout(WRITE_STALL_LIMIT, write.send(message)).await {
        Ok(Err(WsError::Protocol(ProtocolError::SendAfterClosing))) => {
            debug!(
                "Gateway SDK: not writing a {what} frame: the manager has closed the connection"
            );
            Ok(())
        }
        Ok(sent) => sent.map_err(SdkError::from),
        Err(_) => Err(SdkError::WebSocket(format!(
            "the manager stopped reading: a {what} frame could not be written in {}s",
            WRITE_STALL_LIMIT.as_secs()
        ))),
    }
}

/// Route one text frame from the manager: a command goes to the worker, and
/// the rest is answered or dropped here. String-based dispatch with a
/// catch-all so new manager-side message types don't break old gateways.
async fn route_inbound(
    text: &str,
    commands: &mpsc::Sender<QueuedCommand>,
    command_budget: &Arc<Semaphore>,
    write: &mut ManagerSink,
) -> Result<(), SdkError> {
    let Some(IncomingMessage { msg_type, payload }) = IncomingMessage::parse(text) else {
        debug!("Gateway SDK: malformed JSON envelope, dropping");
        return Ok(());
    };

    match msg_type.as_str() {
        "command" => {
            let id = payload
                .get("command_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // The command queues as its frame; the parse is not kept.
            drop(payload);
            let cost = u32::try_from(text.len()).unwrap_or(u32::MAX);
            let Ok(held) = command_budget.clone().try_acquire_many_owned(cost) else {
                return refuse_busy(write, &id).await;
            };
            match commands.try_send(QueuedCommand { id, frame: text.into(), held }) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(command)) => refuse_busy(write, &command.id).await,
                // The worker outlives the session, so this means it is gone
                // for good; a new session starts a new one.
                Err(TrySendError::Closed(_)) => {
                    Err(SdkError::Channel("the command worker has stopped".into()))
                }
            }
        }
        "ping" => {
            let pong = envelope("pong", Value::Null);
            send_bounded(write, Message::Text(pong.into()), "pong").await
        }
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

/// Answer a command there is no room to queue with a `gateway_busy` error
/// ack, written here rather than sent through the outbound channel, which
/// only the session loop drains.
async fn refuse_busy(write: &mut ManagerSink, id: &str) -> Result<(), SdkError> {
    warn!(
        "Gateway SDK: the command queue is full ({COMMAND_QUEUE_SIZE} commands or {} MiB), \
         refusing command {id:?} as busy",
        COMMAND_QUEUE_BYTES >> 20
    );
    let busy = CommandError::new(
        BUSY_ERROR_CODE,
        "The gateway is still working through earlier commands and did not run \
         this one. Retry once they have finished.",
    );
    let ack = envelope("command_ack", command_ack_payload(id, Err(busy)));
    send_bounded(write, Message::Text(ack.into()), "command_ack").await
}

/// A command from the manager, waiting for the worker.
struct QueuedCommand {
    id: String,
    /// The envelope as it came off the wire, parsed again when the command
    /// starts.
    frame: Box<str>,
    /// Its share of [`COMMAND_QUEUE_BYTES`], given back when it starts.
    held: OwnedSemaphorePermit,
}

/// Run a session's commands, one at a time, off the session loop.
///
/// Vendor handlers are written for one command at a time, and get that across
/// sessions too: a worker starts only once it holds the command turn, which
/// the previous session's worker gives up when it has finished. So a command
/// still running when its session ended (an `upgrade_binary` fetching a
/// tarball, say) completes before the next session's first command starts. A
/// command that has started always runs to completion and its ack goes out on
/// whichever session is up by then. At shutdown, `run()` waits up to
/// [`COMMAND_DRAIN_LIMIT`] for it instead, and its ack is not sent.
///
/// Commands still queued when the session ends are not started, and the
/// queue is let go of then, even if a command is still running: kept until
/// the handler returned, it would sit beside the next session's queue, and
/// behind a handler that never returns it would be kept for good. The manager
/// hears nothing back for them on that session and treats them as failed;
/// running them anyway would change the device behind the operator's back.
///
/// The worker starts nothing once `session_over` is cancelled, which
/// `run_session` does as soon as it has seen the end, and that is all that is
/// guaranteed for a command read in the same burst as the manager's Close or
/// the end of the stream. The loop hands such a command over before it reads
/// the end, so a worker that is free can take it in between, and then it has
/// started like any other. On a multi-thread runtime the worker runs beside
/// the loop, and either can happen. A current-thread runtime runs the worker
/// only when the session's task yields, which between two frames of one read
/// it does not unless something in between waits (a write, room in a queue)
/// or tokio's cooperative budget runs out, so there the command is normally
/// dropped unrun. The old inline loop always ran it. Running it every time
/// would take draining the queue at a clean end, which would also run every
/// command waiting behind a long one, long after the manager gave up on them;
/// and the manager's own way of ending a session like that is deleting the
/// node, after which nobody reads the ack.
async fn run_commands(
    turn: Arc<Semaphore>,
    mut queue: mpsc::Receiver<QueuedCommand>,
    session_over: CancellationToken,
    handler: Arc<dyn CommandHandler>,
    emitter: Emitter,
) {
    // Wait for the turn, but no longer than the session lasts. A worker whose
    // session ends first has started nothing, so it has nothing to keep in
    // order: it gives up its place in the line and returns, and its queue
    // goes with it. Waiting on regardless, every reconnect behind a handler
    // that never returns would park one more worker holding one more queue.
    // Without the turn the session is over (the semaphore is never closed),
    // and the loop below breaks before it takes a command.
    let _turn = tokio::select! {
        biased;
        _ = session_over.cancelled() => None,
        held = turn.acquire_owned() => held.ok(),
    };

    loop {
        let command = tokio::select! {
            biased;
            _ = session_over.cancelled() => break,
            command = queue.recv() => match command {
                Some(command) => command,
                None => break,
            },
        };

        // Its own task, so a handler that panics costs that one command
        // rather than the worker, and the manager still hears back about it.
        let id = command.id.clone();
        let mut task = tokio::spawn(run_command(command, handler.clone(), emitter.clone()));
        let joined = tokio::select! {
            biased;
            joined = &mut task => joined,
            // The session ended while the command runs. Nothing queued
            // behind it will start, so let the queue go now rather than
            // when, if ever, the handler returns: a hung handler would
            // otherwise hold its session's queue for good, beside the next
            // session's. The turn is still held while the command is seen
            // out, so the next session's first command still waits for it.
            _ = session_over.cancelled() => {
                report_dropped(&queue);
                drop(queue);
                finish_command(task.await, &id, &emitter).await;
                return;
            }
        };
        if !finish_command(joined, &id, &emitter).await {
            return;
        }
    }

    report_dropped(&queue);
}

/// See a command's task out. A handler that panicked gets a
/// `handler_panicked` ack, so the manager still hears back about the
/// command. `false` if the task was cancelled: the runtime is shutting down.
async fn finish_command(joined: Result<(), JoinError>, id: &str, emitter: &Emitter) -> bool {
    match joined {
        Ok(()) => true,
        Err(e) if e.is_panic() => {
            error!("Gateway SDK: the handler panicked on command {id:?}: {e}");
            let panicked = CommandError::new(
                PANICKED_ERROR_CODE,
                "The gateway's handler panicked on this command; see the gateway's log.",
            );
            let _ = emitter.emit_command_ack(id, Err(panicked)).await;
            true
        }
        Err(_) => false,
    }
}

/// Log the commands an ended session leaves in its queue, none of which
/// will start.
fn report_dropped(queue: &mpsc::Receiver<QueuedCommand>) {
    let dropped = queue.len();
    if dropped > 0 {
        warn!(
            "Gateway SDK: the session ended with {dropped} command(s) not yet started; \
             they were dropped without an ack"
        );
    }
}

/// Run one command through the vendor's handler and queue its ack.
async fn run_command(command: QueuedCommand, handler: Arc<dyn CommandHandler>, emitter: Emitter) {
    // Started, so no longer waiting: its bytes leave the queue's budget, and
    // only its action is kept, parsed, while it runs.
    let QueuedCommand { id, frame, held } = command;
    drop(held);
    // `route_inbound` parsed this frame once already, so it parses again; a
    // payload that is not an object carries no action.
    let action = match IncomingMessage::parse(&frame).map(|message| message.payload) {
        Some(Value::Object(mut payload)) => payload.remove("action").unwrap_or(Value::Null),
        _ => Value::Null,
    };
    drop(frame);

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
    let queued = if action_is_get_config(&action) {
        let cfg = handler.on_config_request().await;
        let _ = emitter.emit_command_ack(&id, Ok(Value::Null)).await;
        emitter.emit_config_response(cfg).await
    } else {
        // General-purpose dispatch.
        let result = handler.handle_command(id.clone(), action).await;
        emitter.emit_command_ack(&id, result).await
    };
    if let Err(e) = queued {
        debug!("Gateway SDK: inbound handler error: {e}");
    }
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

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    // ── InboundCadence ──────────────────────────────────────────────────

    /// A manager that has not pinged has told us nothing about its cadence,
    /// and one that predates the keepalive never will. Either way the
    /// watchdog must not fire, however long the quiet.
    #[test]
    fn the_watchdog_is_unarmed_until_the_first_ping() {
        let t0 = Instant::now();
        let c = InboundCadence::new(t0);
        assert_eq!(c.silence_limit(), None);
        assert_eq!(c.dead_for(t0 + secs(3600)), None);
    }

    /// The first ping arms it, its gap measured from the start of the
    /// session: an over-estimate, so the limit can only start out too long.
    #[test]
    fn the_first_ping_arms_the_watchdog() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        c.on_ping(t0 + secs(60));
        assert_eq!(c.silence_limit(), Some(secs(240)));
        assert_eq!(c.dead_for(t0 + secs(60) + secs(239)), None);
        assert!(c.dead_for(t0 + secs(60) + secs(241)).is_some());
    }

    /// The default 15 s keepalive: four intervals is 60 s, under the floor,
    /// so the floor rules.
    #[test]
    fn a_fast_cadence_is_held_to_the_floor() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        c.on_ping(t0 + secs(15));
        c.on_ping(t0 + secs(30));
        assert_eq!(c.silence_limit(), Some(SILENCE_FLOOR));
        assert_eq!(c.dead_for(t0 + secs(30) + secs(89)), None);
        assert!(c.dead_for(t0 + secs(30) + secs(91)).is_some());
    }

    /// Pings back to back are a gap of nothing; the limit still never drops
    /// below the floor.
    #[test]
    fn back_to_back_pings_do_not_shrink_the_limit_below_the_floor() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        for _ in 0..PING_GAPS_KEPT {
            c.on_ping(t0);
        }
        assert_eq!(c.silence_limit(), Some(SILENCE_FLOOR));
    }

    /// One 85 s stall on the manager between 15 s pings. A running maximum
    /// would hold the limit at 340 s for the rest of the session; the median
    /// does not move.
    #[test]
    fn the_median_ignores_one_long_gap() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        for at in [15, 30, 115, 130, 145] {
            c.on_ping(t0 + secs(at));
        }
        assert_eq!(c.silence_limit(), Some(SILENCE_FLOOR));
    }

    /// The keepalive can be set as high as 300 s. A fixed limit would drop
    /// every healthy session on such a manager; a learned one does not.
    #[test]
    fn a_slow_cadence_scales_the_limit() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        c.on_ping(t0 + secs(300));
        c.on_ping(t0 + secs(600));
        assert_eq!(c.silence_limit(), Some(secs(1200)));
        assert_eq!(c.dead_for(t0 + secs(600) + secs(1199)), None);
    }

    /// A gap longer than any keepalive the manager allows is a stall, and
    /// counts as 300 s: a stalling manager must not teach the watchdog to
    /// wait 4 × 1000 s.
    #[test]
    fn a_gap_counts_for_at_most_the_longest_keepalive() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        for at in [1000, 2000, 3000] {
            c.on_ping(t0 + secs(at));
        }
        assert_eq!(c.silence_limit(), Some(secs(1200)));
    }

    /// Only the most recent gaps count, so the session-start over-estimate
    /// ages out rather than standing for the whole session.
    #[test]
    fn only_the_most_recent_gaps_count() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        c.on_ping(t0 + secs(300));
        let mut at = 300;
        for _ in 0..PING_GAPS_KEPT {
            at += 15;
            c.on_ping(t0 + secs(at));
        }
        assert_eq!(c.silence_limit(), Some(SILENCE_FLOOR));
    }

    /// Any frame keeps the session alive, not only pings: a manager busy
    /// sending commands is plainly there.
    #[test]
    fn any_frame_resets_the_silence() {
        let t0 = Instant::now();
        let mut c = InboundCadence::new(t0);
        c.on_ping(t0 + secs(15));
        c.on_frame(t0 + secs(80));
        assert_eq!(c.dead_for(t0 + secs(80) + secs(90)), None);
        assert!(c.dead_for(t0 + secs(80) + secs(91)).is_some());
    }

    // ── Reader ──────────────────────────────────────────────────────────

    fn text(len: usize) -> Result<Message, WsError> {
        Ok(Message::Text("x".repeat(len).into()))
    }

    /// A session loop slow to take frames. The reader hands frames over
    /// until their bytes reach its budget, then waits for the loop, rather
    /// than reading on until the channel's count is used up.
    #[tokio::test(start_paused = true)]
    async fn the_reader_waits_once_its_byte_budget_is_handed_over() {
        let frames = futures_util::stream::iter([text(1000), text(1000)])
            .chain(futures_util::stream::pending());
        let (inbound_tx, mut inbound) = mpsc::channel(INBOUND_CHANNEL_SIZE);
        let budget = Arc::new(Semaphore::new(1500));
        let reader = tokio::spawn(read_manager(frames, inbound_tx, budget, Instant::now()));

        let Some(Inbound::Text(_, first)) = inbound.recv().await else {
            panic!("the first frame should be handed over");
        };
        tokio::time::sleep(secs(60)).await;
        assert!(inbound.try_recv().is_err(), "a frame went over past the reader's budget");

        drop(first);
        let Some(Inbound::Text(..)) = inbound.recv().await else {
            panic!("the next frame should follow once the loop has taken the first");
        };
        reader.abort();
    }

    /// While the reader waits for the loop it reads nothing, so it cannot
    /// tell a quiet manager from a busy loop, and the wait must not count as
    /// silence. Here the next frame has not arrived when the wait ends, so
    /// the silence check runs at once.
    #[tokio::test(start_paused = true)]
    async fn waiting_for_the_loop_is_not_silence() {
        let (frames_tx, mut frames_rx) = mpsc::unbounded_channel();
        let frames = futures_util::stream::poll_fn(move |cx| frames_rx.poll_recv(cx));
        let (inbound_tx, mut inbound) = mpsc::channel(INBOUND_CHANNEL_SIZE);
        let budget = Arc::new(Semaphore::new(1000));
        let reader = tokio::spawn(read_manager(frames, inbound_tx, budget, Instant::now()));

        // The ping arms the watchdog, at its 90 s floor.
        for frame in [Ok(Message::Ping(Bytes::new())), text(1000), text(1000)] {
            frames_tx.send(frame).unwrap();
        }
        let Some(Inbound::Ping(_)) = inbound.recv().await else {
            panic!("the ping should be handed over");
        };
        let Some(Inbound::Text(_, first)) = inbound.recv().await else {
            panic!("the first frame should be handed over");
        };

        // The loop takes 200 s to get to the first frame, and the reader
        // holds the second meanwhile.
        tokio::time::sleep(secs(200)).await;
        drop(first);
        let Some(Inbound::Text(..)) = inbound.recv().await else {
            panic!("the second frame should follow");
        };
        tokio::time::sleep(secs(10)).await;
        assert!(
            !reader.is_finished(),
            "200 s spent waiting on the loop were taken for 200 s of silence from the manager"
        );
        reader.abort();
    }

    // ── Command worker ──────────────────────────────────────────────────

    /// A handler whose commands each wait for `release` once they have
    /// started, then succeed, or panic if `panics` is set.
    #[derive(Default)]
    struct HeldHandler {
        started: Mutex<Vec<String>>,
        running: tokio::sync::Notify,
        release: tokio::sync::Notify,
        panics: bool,
    }

    #[async_trait::async_trait]
    impl CommandHandler for HeldHandler {
        async fn handle_command(
            &self,
            command_id: String,
            _action: Value,
        ) -> Result<Value, CommandError> {
            self.started.lock().unwrap().push(command_id.clone());
            self.running.notify_one();
            self.release.notified().await;
            if self.panics {
                panic!("the handler panics on {command_id}, as the test asked");
            }
            Ok(Value::Null)
        }
    }

    /// A command as the session loop queues it, holding its share of
    /// `budget`.
    fn queued(id: &str, budget: &Arc<Semaphore>) -> QueuedCommand {
        let frame = envelope("command", json!({ "command_id": id, "action": {} }));
        let cost = u32::try_from(frame.len()).unwrap();
        let held = budget.clone().try_acquire_many_owned(cost).unwrap();
        QueuedCommand { id: id.into(), frame: frame.into(), held }
    }

    /// A worker holding the turn, with `first` running and `second` queued
    /// behind it.
    struct Running {
        worker: tokio::task::JoinHandle<()>,
        turn: Arc<Semaphore>,
        budget: Arc<Semaphore>,
        queue: mpsc::Sender<QueuedCommand>,
        session_over: CancellationToken,
        /// Where the worker's acks go.
        outbound: mpsc::Receiver<OutboundFrame>,
    }

    async fn a_command_running(handler: Arc<HeldHandler>) -> Running {
        let turn = Arc::new(Semaphore::new(1));
        let budget = Arc::new(Semaphore::new(COMMAND_QUEUE_BYTES));
        let (queue, queue_rx) = mpsc::channel(COMMAND_QUEUE_SIZE);
        let (outbound_tx, outbound) = mpsc::channel(OUTBOUND_CHANNEL_SIZE);
        let session_over = CancellationToken::new();
        let worker = tokio::spawn(run_commands(
            turn.clone(),
            queue_rx,
            session_over.clone(),
            handler.clone(),
            Emitter::new(outbound_tx),
        ));
        queue.try_send(queued("first", &budget)).unwrap();
        queue.try_send(queued("second", &budget)).unwrap();
        tokio::time::timeout(secs(1), handler.running.notified())
            .await
            .expect("the first command should start");
        Running { worker, turn, budget, queue, session_over, outbound }
    }

    fn ack_of(frame: &OutboundFrame) -> Value {
        let envelope: Value = serde_json::from_str(&frame.0).unwrap();
        assert_eq!(envelope["type"], "command_ack");
        envelope["payload"].clone()
    }

    /// The session ends while its command runs. The worker lets the queue go
    /// at once, rather than when the handler returns, which for a hung one is
    /// never; until then a second session's queue would sit beside it. It
    /// keeps the turn until the command has finished, so the next session's
    /// first command still waits for this one, and the command's ack still
    /// goes out.
    #[tokio::test(start_paused = true)]
    async fn a_session_that_ends_mid_command_lets_its_queue_go() {
        let handler = Arc::new(HeldHandler::default());
        let mut running = a_command_running(handler.clone()).await;

        running.session_over.cancel();
        assert!(
            tokio::time::timeout(secs(1), running.queue.closed()).await.is_ok(),
            "the queue of a session that has ended is held while its command runs"
        );
        assert_eq!(
            running.budget.available_permits(),
            COMMAND_QUEUE_BYTES,
            "the command queued behind the running one is still resident"
        );
        assert_eq!(
            running.turn.available_permits(),
            0,
            "the turn went before the command finished"
        );

        handler.release.notify_one();
        tokio::time::timeout(secs(1), running.worker)
            .await
            .expect("the worker should return once its command has")
            .unwrap();
        assert_eq!(running.turn.available_permits(), 1);
        let ack = ack_of(&running.outbound.try_recv().expect("the running command is acked"));
        assert_eq!(ack["command_id"], "first");
        assert_eq!(ack["success"], true);
        assert_eq!(*handler.started.lock().unwrap(), ["first"]);
    }

    /// A handler that panics once its session has ended, after the worker
    /// has let the queue go: the manager still gets the `handler_panicked`
    /// ack, on the next session.
    #[tokio::test(start_paused = true)]
    async fn a_panic_after_the_session_ended_is_still_acked() {
        let handler = Arc::new(HeldHandler { panics: true, ..HeldHandler::default() });
        let mut running = a_command_running(handler.clone()).await;

        running.session_over.cancel();
        tokio::time::timeout(secs(1), running.queue.closed())
            .await
            .expect("the worker should let the queue go");
        handler.release.notify_one();
        tokio::time::timeout(secs(1), running.worker)
            .await
            .expect("the worker should return once its command has")
            .unwrap();
        let ack = ack_of(&running.outbound.try_recv().expect("the panicked command is acked"));
        assert_eq!(ack["command_id"], "first");
        assert_eq!(ack["error_code"], PANICKED_ERROR_CODE);
        assert_eq!(*handler.started.lock().unwrap(), ["first"]);
    }

    // ── Dial ────────────────────────────────────────────────────────────

    #[test]
    fn the_dial_target_comes_from_the_url() {
        assert_eq!(
            manager_host_port("wss://manager.example.com/ws/node").unwrap(),
            ("manager.example.com".to_string(), 443)
        );
        assert_eq!(
            manager_host_port("ws://127.0.0.1/ws/node").unwrap(),
            ("127.0.0.1".to_string(), 80)
        );
        // Brackets off, or the resolver takes the literal for a host name.
        assert_eq!(
            manager_host_port("wss://[2001:db8::1]:8443/ws/node").unwrap(),
            ("2001:db8::1".to_string(), 8443)
        );
    }

    #[test]
    fn address_families_alternate_starting_with_the_first() {
        let (v6a, v6b) = (addr("[2001:db8::1]:443"), addr("[2001:db8::2]:443"));
        let (v4a, v4b) = (addr("192.0.2.1:443"), addr("192.0.2.2:443"));
        assert_eq!(interleave_families(vec![v6a, v6b, v4a, v4b]), [v6a, v4a, v6b, v4b]);
        assert_eq!(interleave_families(vec![v4a, v6a, v6b]), [v4a, v6a, v6b]);
        assert_eq!(interleave_families(vec![v4a, v4b]), [v4a, v4b]);
        assert!(interleave_families(vec![]).is_empty());
    }

    /// The shape of a broken IPv6 path: the first address silently drops
    /// SYNs and the second is a live listener. Tried one after another, as
    /// `TcpStream::connect(host)` tries them, the dial sat on the first for
    /// the kernel's SYN timeout (about 127 s) and inside the 30 s deadline
    /// never reached the second.
    #[tokio::test]
    async fn a_silent_first_address_does_not_hold_up_the_second() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = listener.local_addr().unwrap();
        let black_hole = addr("192.0.2.1:443");
        let started = Instant::now();

        let connected = connect_staggered(VecDeque::from([black_hole, live]), |addr| async move {
            if addr == black_hole {
                std::future::pending::<()>().await;
            }
            TcpStream::connect(addr).await
        })
        .await
        .expect("the live address connects");

        assert_eq!(connected.addr, live);
        assert_eq!(connected.abandoned, [black_hole], "the silent address is reported");
        assert!(started.elapsed() < secs(2), "took {:?}", started.elapsed());
    }

    /// An address that answers inside the stagger is not raced at all.
    #[tokio::test(start_paused = true)]
    async fn a_quick_first_address_is_not_raced() {
        let (quick, other) = (addr("192.0.2.1:443"), addr("192.0.2.2:443"));
        let tried = std::sync::Mutex::new(Vec::new());

        let connected = connect_staggered(VecDeque::from([quick, other]), |addr| {
            tried.lock().unwrap().push(addr);
            async move {
                tokio::time::sleep(CONNECT_ATTEMPT_DELAY / 2).await;
                Ok::<_, io::Error>(addr)
            }
        })
        .await
        .unwrap();

        assert_eq!(connected.addr, quick);
        assert_eq!(*tried.lock().unwrap(), [quick]);
    }

    /// A refused address hands its turn on at once rather than at the end of
    /// its stagger, and was not abandoned — it answered.
    #[tokio::test(start_paused = true)]
    async fn a_failed_attempt_starts_the_next_at_once() {
        let (refused, live) = (addr("192.0.2.1:443"), addr("192.0.2.2:443"));
        let started = Instant::now();

        let connected = connect_staggered(VecDeque::from([refused, live]), |addr| async move {
            if addr == refused {
                return Err(io::Error::from(io::ErrorKind::ConnectionRefused));
            }
            Ok(addr)
        })
        .await
        .unwrap();

        assert_eq!(connected.addr, live);
        assert!(connected.abandoned.is_empty());
        assert_eq!(started.elapsed(), Duration::ZERO, "the next attempt waited out the stagger");
    }

    #[tokio::test(start_paused = true)]
    async fn every_address_failing_reports_the_last_failure() {
        let (a, b) = (addr("192.0.2.1:443"), addr("192.0.2.2:443"));
        let failed = connect_staggered(VecDeque::from([a, b]), |addr| async move {
            let refused = format!("refused by {addr}");
            Err::<(), _>(io::Error::new(io::ErrorKind::ConnectionRefused, refused))
        })
        .await;
        match failed {
            Err(e) => assert_eq!(e.to_string(), format!("refused by {b}")),
            Ok(_) => panic!("nothing connected, yet the connect succeeded"),
        }
    }
}
