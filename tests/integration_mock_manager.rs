// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Mock-manager integration test.
//!
//! Spins up a plaintext WS server on a random port (opt-in via the
//! `BILBYCAST_SDK_ALLOW_PLAINTEXT_WS=1` env var plus the
//! `allow_plaintext_ws` config field, both of which the SDK requires),
//! connects the SDK to it, and exercises:
//! - Register with a token → `register_ack` + node_id/node_secret
//! - Server sends a `command` → handler dispatched → `command_ack` matches
//! - SDK emits stats / event / health → server receives expected envelopes
//! - Server closes the socket → SDK reconnects with persisted credentials
//! - `get_config` command → SDK replies with `config_response` + ack
//! - `ping` → `pong`
//!
//! And the manager-link liveness cases (see the section at the end):
//! - a peer that never answers the upgrade → the dial gives up and retries
//! - a manager that pings, then goes silent → the session is dropped and
//!   redialled
//! - a command longer than the silence limit → the session stays up, keeps
//!   answering pings, and the ack arrives
//! - a manager that stops reading → dropped at the write bound, not before;
//!   shutdown stays prompt
//! - more commands than the queue holds → answered `gateway_busy`
//! - a Ping and a Close read together → a clean close, which resets the
//!   reconnect backoff
//! - a command running when its session ends → it finishes, its ack rides
//!   the next session, and that session's commands wait for it
//! - reconnects behind a command that never returns → no pile-up of workers,
//!   and the commands of the sessions that ended never run
//! - shutdown while a command runs → `run()` waits for it, up to 30 s, and
//!   starts nothing queued behind it

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bilbycast_gateway_sdk::{
    CommandError, CommandHandler, GatewayClient, GatewayConfig, GatewayEvent,
    GATEWAY_WS_PROTOCOL_VERSION,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

struct RecordingHandler {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    config_snapshot: Value,
}

#[async_trait]
impl CommandHandler for RecordingHandler {
    async fn handle_command(
        &self,
        command_id: String,
        action: Value,
    ) -> Result<Value, CommandError> {
        self.calls
            .lock()
            .unwrap()
            .push((command_id, action.clone()));

        match action.get("type").and_then(|t| t.as_str()) {
            Some("echo") => Ok(action.get("params").cloned().unwrap_or(Value::Null)),
            Some("boom") => Err(CommandError::new("boom_code", "intentional failure")
                .with_details(json!({ "field": "whatever" }))),
            Some(other) => Err(CommandError::unknown_action(other)),
            None => Err(CommandError::validation("missing type")),
        }
    }

    async fn on_config_request(&self) -> Value {
        self.config_snapshot.clone()
    }
}

/// What the mock manager observed during one test run.
#[derive(Debug, Default)]
struct Observed {
    auths: Vec<Value>,
    stats: Vec<Value>,
    events: Vec<Value>,
    healths: Vec<Value>,
    command_acks: Vec<Value>,
    config_responses: Vec<Value>,
    thumbnails: Vec<Value>,
    pongs: u32,
}

/// Drive one WS connection end-to-end on the server side. `script` decides
/// what to send once auth is complete and when to hang up.
async fn run_mock_server_conn(
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    observed: Arc<AsyncMutex<Observed>>,
    script: ServerScript,
    issued_node_id: Arc<AsyncMutex<Option<String>>>,
) {
    let (mut write, mut read) = ws.split();

    // 1. Wait for auth frame.
    let auth_msg = match read.next().await {
        Some(Ok(Message::Text(t))) => t.to_string(),
        other => panic!("expected auth text frame, got {other:?}"),
    };
    let auth_val: Value = serde_json::from_str(&auth_msg).unwrap();
    observed.lock().await.auths.push(auth_val.clone());
    assert_eq!(auth_val["type"], "auth");

    let payload = &auth_val["payload"];
    assert_eq!(
        payload["protocol_version"].as_u64().unwrap(),
        GATEWAY_WS_PROTOCOL_VERSION as u64
    );

    // 2. Reply with register_ack or auth_ok depending on which path was taken.
    let ack = if payload.get("registration_token").is_some() {
        let node_id = "node-abc".to_string();
        let node_secret = "secret-xyz".to_string();
        *issued_node_id.lock().await = Some(node_id.clone());
        json!({
            "type": "register_ack",
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "payload": {
                "node_id": node_id,
                "node_secret": node_secret,
            },
        })
    } else {
        assert!(payload.get("node_id").is_some());
        assert!(payload.get("node_secret").is_some());
        json!({
            "type": "auth_ok",
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "payload": {},
        })
    };
    write
        .send(Message::Text(serde_json::to_string(&ack).unwrap().into()))
        .await
        .unwrap();

    // 3. Run the server script in parallel with an inbound reader task.
    let script_cmds = script.commands.clone();
    let close_after = script.close_after;
    let writer_observed = observed.clone();

    // Writer half: send scripted commands, then optionally close.
    let writer_task = tokio::spawn(async move {
        for (delay, env) in script_cmds {
            tokio::time::sleep(delay).await;
            write
                .send(Message::Text(serde_json::to_string(&env).unwrap().into()))
                .await
                .ok();
        }
        if close_after {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = write.send(Message::Close(None)).await;
        } else {
            // Keep alive indefinitely
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        }
        let _ = writer_observed; // shut up unused warn
    });

    // Reader half: record everything.
    while let Some(Ok(Message::Text(text))) = read.next().await {
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let t = v
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let payload = v.get("payload").cloned().unwrap_or(Value::Null);
        let mut obs = observed.lock().await;
        match t.as_str() {
            "stats" => obs.stats.push(payload),
            "event" => obs.events.push(payload),
            "health" => obs.healths.push(payload),
            "command_ack" => obs.command_acks.push(payload),
            "config_response" => obs.config_responses.push(payload),
            "thumbnail" => obs.thumbnails.push(payload),
            "pong" => obs.pongs += 1,
            _ => {}
        }
    }

    writer_task.abort();
}

#[derive(Clone)]
struct ServerScript {
    /// Commands to send after auth, with a delay before each.
    commands: Vec<(Duration, Value)>,
    /// If true, close the socket after sending all commands.
    close_after: bool,
}

/// Spawn a one-shot mock manager on a random port. Returns the ws:// URL and
/// a handle bundling the observation state.
async fn spawn_mock_manager(
    scripts: Vec<ServerScript>,
) -> (
    String,
    Arc<AsyncMutex<Observed>>,
    Arc<AsyncMutex<Option<String>>>,
    mpsc::Sender<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let url = format!("ws://127.0.0.1:{port}/ws/node");

    let observed = Arc::new(AsyncMutex::new(Observed::default()));
    let issued_node_id = Arc::new(AsyncMutex::new(None));
    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);

    let observed_clone = observed.clone();
    let issued_clone = issued_node_id.clone();
    tokio::spawn(async move {
        let mut script_iter = scripts.into_iter();
        loop {
            tokio::select! {
                _ = stop_rx.recv() => break,
                r = listener.accept() => {
                    let (stream, _) = match r {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let script = match script_iter.next() {
                        Some(s) => s,
                        None => ServerScript { commands: vec![], close_after: false },
                    };
                    let ws = match tokio_tungstenite::accept_async(stream).await {
                        Ok(ws) => ws,
                        Err(_) => continue,
                    };
                    let obs = observed_clone.clone();
                    let nid = issued_clone.clone();
                    tokio::spawn(async move {
                        run_mock_server_conn(ws, obs, script, nid).await;
                    });
                }
            }
        }
    });

    (url, observed, issued_node_id, stop_tx)
}

/// Set the env half of the plaintext-`ws://` opt-in. The config half
/// (`GatewayConfig::allow_plaintext_ws`) is set per-test by
/// [`test_config`] — the SDK requires BOTH, so that no single environment
/// variable can downgrade a real sidecar's manager link to cleartext.
fn enable_plaintext_ws() {
    std::env::set_var("BILBYCAST_SDK_ALLOW_PLAINTEXT_WS", "1");
}

/// A `GatewayConfig` pointed at the mock manager, carrying the config half
/// of the plaintext opt-in.
fn test_config(url: impl Into<String>) -> GatewayConfig {
    let mut cfg = GatewayConfig::minimal(url, "test_device", "0.0.1-test");
    cfg.allow_plaintext_ws = true;
    cfg
}

#[tokio::test]
async fn register_then_auth_reconnect_then_commands_and_close() {
    enable_plaintext_ws();

    // Script 1: send echo command, ping command, get_config command,
    //           then close so the SDK reconnects.
    let script_1 = ServerScript {
        commands: vec![
            (
                Duration::from_millis(50),
                json!({
                    "type": "command",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "payload": {
                        "command_id": "cmd-1",
                        "action": { "type": "echo", "params": { "ok": true } },
                    },
                }),
            ),
            (
                Duration::from_millis(30),
                json!({
                    "type": "ping",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "payload": null,
                }),
            ),
            (
                Duration::from_millis(30),
                json!({
                    "type": "command",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "payload": {
                        "command_id": "cmd-2",
                        "action": { "type": "get_config" },
                    },
                }),
            ),
            (
                Duration::from_millis(30),
                json!({
                    "type": "command",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "payload": {
                        "command_id": "cmd-3",
                        "action": { "type": "boom" },
                    },
                }),
            ),
        ],
        close_after: true,
    };

    // Script 2: accept the reconnect, send no commands, stay open.
    let script_2 = ServerScript {
        commands: vec![],
        close_after: false,
    };

    let (url, observed, _issued, _stop) =
        spawn_mock_manager(vec![script_1, script_2]).await;

    // Build + start the gateway client.
    let mut cfg = test_config(url.clone());
    cfg.registration_token = Some("test-token-1234".into());
    // Very fast reconnect so the second script fires within the test timeout.
    cfg.reconnect_backoff = bilbycast_gateway_sdk::ReconnectBackoff {
        steps_secs: vec![1, 1, 1, 1, 1],
    };

    let calls = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    let handler = Arc::new(RecordingHandler {
        calls: calls.clone(),
        config_snapshot: json!({ "node_type": "test", "ok": true }),
    });

    let client = GatewayClient::connect(cfg, handler).await.unwrap();
    let emitter = client.emitter();
    let shutdown = client.shutdown_token();

    // Spawn client run loop.
    let run_task = tokio::spawn(client.run());

    // Emit some outbound traffic while the connection is up.
    tokio::time::sleep(Duration::from_millis(150)).await;
    emitter.emit_stats(json!({ "active_flows": 1 })).await.unwrap();
    emitter
        .emit_event(GatewayEvent::info("vendor_api", "hello from test"))
        .await
        .unwrap();
    emitter
        .emit_thumbnail("flow-1", bytes::Bytes::from_static(&[0xFF, 0xD8, 0xFF, 0xD9]))
        .await
        .unwrap();

    // Let the script drain, the close happen, and the SDK reconnect.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Tear down.
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(3), run_task).await;

    let obs = observed.lock().await;

    // Register + reconnect both happened.
    assert_eq!(obs.auths.len(), 2, "expected register + reconnect");
    assert!(obs.auths[0]["payload"]
        .get("registration_token")
        .is_some());
    assert_eq!(
        obs.auths[1]["payload"]["node_id"].as_str(),
        Some("node-abc")
    );
    assert_eq!(
        obs.auths[1]["payload"]["node_secret"].as_str(),
        Some("secret-xyz")
    );

    // Commands dispatched.
    let handler_calls = calls.lock().unwrap();
    assert!(
        handler_calls.iter().any(|(id, _)| id == "cmd-1"),
        "echo command should have been dispatched to handler"
    );
    assert!(
        handler_calls.iter().any(|(id, _)| id == "cmd-3"),
        "boom command should have been dispatched to handler"
    );
    // get_config is NOT expected to reach handle_command — it goes through
    // on_config_request instead.
    assert!(
        !handler_calls.iter().any(|(id, _)| id == "cmd-2"),
        "get_config should not hit handle_command"
    );

    // Command acks: success for cmd-1, success for cmd-2 (config_response companion),
    // failure with error_code for cmd-3.
    let ack_1 = obs
        .command_acks
        .iter()
        .find(|a| a["command_id"] == "cmd-1")
        .expect("cmd-1 ack");
    assert_eq!(ack_1["success"], true);
    assert_eq!(ack_1["data"]["ok"], true);

    let ack_2 = obs
        .command_acks
        .iter()
        .find(|a| a["command_id"] == "cmd-2")
        .expect("cmd-2 ack");
    assert_eq!(ack_2["success"], true);

    let ack_3 = obs
        .command_acks
        .iter()
        .find(|a| a["command_id"] == "cmd-3")
        .expect("cmd-3 ack");
    assert_eq!(ack_3["success"], false);
    assert_eq!(ack_3["error_code"], "boom_code");
    assert_eq!(ack_3["details"]["field"], "whatever");

    // config_response arrived with the snapshot.
    assert_eq!(obs.config_responses.len(), 1);
    assert_eq!(obs.config_responses[0]["ok"], true);

    // Ping → pong.
    assert!(obs.pongs >= 1);

    // Stats + events + thumbnail arrived.
    assert!(obs.stats.iter().any(|s| s["active_flows"] == 1));
    assert!(obs.events.iter().any(|e| e["category"] == "vendor_api"));
    assert!(
        obs.thumbnails
            .iter()
            .any(|t| t["flow_id"] == "flow-1"
                && t["image_base64"].as_str().is_some()),
        "thumbnail envelope should have flow_id + image_base64"
    );

    // Health heartbeat or explicit — at least one health.
    // (The 15s heartbeat interval default is too slow for this test; the
    // auth/register/ack already exercises the envelope shape, and we don't
    // actively emit a health, so we don't assert heartbeat count here.)
    let _ = obs.healths.len();
}

#[tokio::test]
async fn unknown_manager_message_type_is_ignored() {
    enable_plaintext_ws();

    let script = ServerScript {
        commands: vec![
            (
                Duration::from_millis(50),
                json!({
                    "type": "some_future_feature_v99",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "payload": { "anything": true },
                }),
            ),
            (
                Duration::from_millis(30),
                json!({
                    "type": "command",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "payload": {
                        "command_id": "after-unknown",
                        "action": { "type": "echo", "params": { "ok": true } },
                    },
                }),
            ),
        ],
        close_after: false,
    };

    let (url, observed, _issued, _stop) = spawn_mock_manager(vec![script]).await;

    let mut cfg = test_config(url);
    cfg.registration_token = Some("test-token".into());

    let handler = Arc::new(RecordingHandler {
        calls: Arc::new(Mutex::new(vec![])),
        config_snapshot: Value::Null,
    });

    let client = GatewayClient::connect(cfg, handler).await.unwrap();
    let shutdown = client.shutdown_token();
    let run_task = tokio::spawn(client.run());

    tokio::time::sleep(Duration::from_millis(500)).await;
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(3), run_task).await;

    let obs = observed.lock().await;
    assert!(
        obs.command_acks
            .iter()
            .any(|a| a["command_id"] == "after-unknown" && a["success"] == true),
        "SDK should have survived the unknown message type and processed the follow-up command"
    );
}

// ── Manager-link liveness ───────────────────────────────────────────────
//
// What stranded three edges offline in bilbycast-edge#180, this client had
// too: a dial with no deadline, nothing watching a session that had gone
// quiet, writes with no bound, and commands run inline in the session loop.
//
// The deadlines involved run from 30 s to five minutes, so each test moves
// onto tokio's paused clock once its first connection is up: from then on the
// clock jumps straight to the next timer whenever every task is waiting, and
// a five-minute stall takes milliseconds. The loopback I/O stays real, so a
// jump can land while a packet is in flight. The first connection is made on
// the real clock for that reason, and after it `wait_until`'s 50 ms polling
// keeps every jump that short, so no deadline expires early. (The dial test
// has no poller, but asserts nothing finer than "it dialled again".)

/// The mock manager's keepalive, as `ws_keepalive_interval_secs` would set it.
const PING_EVERY: Duration = Duration::from_secs(5);

/// What a liveness mock's connection does once the SDK has authenticated.
#[derive(Clone)]
enum Behaviour {
    /// Ping `count` times, a second apart, then never send another frame —
    /// while still reading everything the gateway sends.
    PingThenSilent { count: usize },
    /// Send these envelopes, then ping every [`PING_EVERY`].
    CommandsThenPing { commands: Vec<Value> },
    /// Never read again; ping every [`PING_EVERY`].
    PingButNeverRead,
    /// Send these envelopes in a single write, then as
    /// [`Behaviour::PingButNeverRead`].
    CommandsThenStopReading { commands: Vec<Value> },
    /// Read everything, send nothing.
    Quiet,
    /// Send these frames in a single write, so the gateway reads them
    /// together, then read until the gateway hangs up.
    Burst { frames: Vec<Message> },
    /// Send these envelopes, close the session `after` that, and read until
    /// the gateway hangs up.
    CommandsThenClose { commands: Vec<Value>, after: Duration },
}

/// When things happened on a liveness mock, across all its connections.
#[derive(Clone, Default)]
struct LinkLog {
    auths: Arc<Mutex<Vec<Instant>>>,
    pings: Arc<Mutex<Vec<Instant>>>,
    pongs: Arc<Mutex<Vec<Instant>>>,
    acks: Arc<Mutex<Vec<(Instant, Value)>>>,
}

impl LinkLog {
    fn auths(&self) -> Vec<Instant> {
        self.auths.lock().unwrap().clone()
    }

    fn acks(&self) -> Vec<(Instant, Value)> {
        self.acks.lock().unwrap().clone()
    }

    fn ack(&self, command_id: &str) -> Option<(Instant, Value)> {
        self.acks().into_iter().find(|(_, ack)| ack["command_id"] == command_id)
    }

    fn pongs_between(&self, from: Instant, to: Instant) -> usize {
        self.pongs.lock().unwrap().iter().filter(|t| (from..=to).contains(*t)).count()
    }
}

/// Spawn a mock manager that serves its N-th connection with the N-th
/// behaviour, and [`Behaviour::Quiet`] once they run out.
async fn spawn_link_mock(behaviours: Vec<Behaviour>) -> (String, LinkLog) {
    // A small receive buffer, which accepted sockets inherit, so a connection
    // that stops reading backs the gateway's writes up after a few megabytes
    // rather than after however much the kernel would autotune to.
    let socket = TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(64 * 1024).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(16).unwrap();
    let url = format!("ws://{}/ws/node", listener.local_addr().unwrap());

    let log = LinkLog::default();
    let conn_log = log.clone();
    tokio::spawn(async move {
        let mut behaviours = behaviours.into_iter();
        while let Ok((stream, _)) = listener.accept().await {
            // Nagle off, or a ping sent while an earlier one is unacknowledged
            // waits for the gateway's delayed ACK — which, when the gateway's
            // own writes are stalled and it has nothing to piggyback on, runs
            // on the kernel's real-time timer while the paused clock races
            // past the silence limit.
            stream.set_nodelay(true).unwrap();
            let behaviour = behaviours.next().unwrap_or(Behaviour::Quiet);
            let log = conn_log.clone();
            tokio::spawn(async move {
                if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                    serve_link(ws, behaviour, log).await;
                }
            });
        }
    });
    (url, log)
}

async fn serve_link(ws: WebSocketStream<TcpStream>, behaviour: Behaviour, log: LinkLog) {
    let (mut write, mut read) = ws.split();

    let Some(Ok(Message::Text(auth))) = read.next().await else {
        return;
    };
    let auth: Value = serde_json::from_str(&auth).unwrap();
    let reply = if auth["payload"].get("registration_token").is_some() {
        json!({
            "type": "register_ack",
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "payload": { "node_id": "node-1", "node_secret": "secret-1" },
        })
    } else {
        json!({ "type": "auth_ok", "timestamp": chrono::Utc::now().to_rfc3339(), "payload": {} })
    };
    if write.send(Message::Text(reply.to_string().into())).await.is_err() {
        return;
    }
    log.auths.lock().unwrap().push(Instant::now());

    let reads = !matches!(
        behaviour,
        Behaviour::PingButNeverRead | Behaviour::CommandsThenStopReading { .. }
    );
    let reader = async {
        if !reads {
            return std::future::pending().await;
        }
        while let Some(Ok(message)) = read.next().await {
            match message {
                Message::Pong(_) => log.pongs.lock().unwrap().push(Instant::now()),
                Message::Text(text) => {
                    let v: Value = serde_json::from_str(&text).unwrap_or_default();
                    if v["type"] == "command_ack" {
                        log.acks.lock().unwrap().push((Instant::now(), v["payload"].clone()));
                    }
                }
                _ => {}
            }
        }
    };
    let writer = async {
        let (pings, every) = match behaviour {
            Behaviour::PingThenSilent { count } => (count, Duration::from_secs(1)),
            Behaviour::CommandsThenPing { commands } => {
                for command in commands {
                    if write.send(Message::Text(command.to_string().into())).await.is_err() {
                        return;
                    }
                }
                (usize::MAX, PING_EVERY)
            }
            Behaviour::CommandsThenStopReading { commands } => {
                for command in commands {
                    if write.feed(Message::Text(command.to_string().into())).await.is_err() {
                        return;
                    }
                }
                if write.flush().await.is_err() {
                    return;
                }
                (usize::MAX, PING_EVERY)
            }
            Behaviour::PingButNeverRead => (usize::MAX, PING_EVERY),
            Behaviour::Quiet => (0, PING_EVERY),
            Behaviour::CommandsThenClose { commands, after } => {
                for command in commands {
                    if write.send(Message::Text(command.to_string().into())).await.is_err() {
                        return;
                    }
                }
                tokio::time::sleep(after).await;
                let _ = write.send(Message::Close(None)).await;
                (0, PING_EVERY)
            }
            Behaviour::Burst { frames } => {
                for frame in frames {
                    if write.feed(frame).await.is_err() {
                        return;
                    }
                }
                let _ = write.flush().await;
                (0, PING_EVERY)
            }
        };
        for _ in 0..pings {
            tokio::time::sleep(every).await;
            if write.send(Message::Ping(Vec::new().into())).await.is_err() {
                return;
            }
            log.pings.lock().unwrap().push(Instant::now());
        }
        std::future::pending().await
    };
    // Until the gateway hangs up: the reader sees the end of the stream, or,
    // on a connection that no longer reads, the writer's next ping fails.
    tokio::select! {
        () = reader => {}
        () = writer => {}
    }
}

/// The liveness tests' handler, which records when each command starts and
/// finishes. A `slow` command sleeps for `action.secs` and a `hang` command
/// waits for `release`; everything else succeeds at once.
#[derive(Default)]
struct LinkHandler {
    started: Mutex<Vec<(String, Instant)>>,
    finished: Mutex<Vec<(String, Instant)>>,
    release: tokio::sync::Notify,
}

impl LinkHandler {
    /// The commands started so far, in order.
    fn started(&self) -> Vec<String> {
        self.started.lock().unwrap().iter().map(|(id, _)| id.clone()).collect()
    }

    fn started_at(&self, command_id: &str) -> Option<Instant> {
        let started = self.started.lock().unwrap();
        started.iter().find(|(id, _)| id == command_id).map(|(_, at)| *at)
    }

    fn finished_at(&self, command_id: &str) -> Option<Instant> {
        let finished = self.finished.lock().unwrap();
        finished.iter().find(|(id, _)| id == command_id).map(|(_, at)| *at)
    }
}

#[async_trait]
impl CommandHandler for LinkHandler {
    async fn handle_command(
        &self,
        command_id: String,
        action: Value,
    ) -> Result<Value, CommandError> {
        self.started.lock().unwrap().push((command_id.clone(), Instant::now()));
        match action["type"].as_str() {
            Some("slow") => {
                let secs = action["secs"].as_u64().unwrap_or(0);
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
            Some("hang") => self.release.notified().await,
            _ => {}
        }
        self.finished.lock().unwrap().push((command_id, Instant::now()));
        Ok(Value::Null)
    }
}

fn command(id: &str, action: Value) -> Value {
    json!({
        "type": "command",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "payload": { "command_id": id, "action": action },
    })
}

type RunTask = tokio::task::JoinHandle<Result<(), bilbycast_gateway_sdk::SdkError>>;

/// Start an SDK client with node credentials against `url`, reconnecting
/// after 1 s whatever the attempt.
async fn start_link_client(
    url: String,
    handler: Arc<LinkHandler>,
) -> (RunTask, bilbycast_gateway_sdk::Emitter, bilbycast_gateway_sdk::CancellationToken) {
    start_link_client_backing_off(url, handler, vec![1]).await
}

/// [`start_link_client`] with these reconnect backoff steps.
async fn start_link_client_backing_off(
    url: String,
    handler: Arc<LinkHandler>,
    steps_secs: Vec<u64>,
) -> (RunTask, bilbycast_gateway_sdk::Emitter, bilbycast_gateway_sdk::CancellationToken) {
    enable_plaintext_ws();
    let mut cfg = test_config(url);
    cfg.node_id = Some("node-1".into());
    cfg.node_secret = Some("secret-1".into());
    cfg.reconnect_backoff = bilbycast_gateway_sdk::ReconnectBackoff { steps_secs };
    let client = GatewayClient::connect(cfg, handler).await.unwrap();
    let emitter = client.emitter();
    let shutdown = client.shutdown_token();
    (tokio::spawn(client.run()), emitter, shutdown)
}

/// Poll `cond` until it holds or `limit` passes, on whichever clock is
/// running. Returns whether it held.
async fn wait_until(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cond()
}

/// Up to the first authenticated session on the real clock, then pause it.
async fn first_session_then_pause_the_clock(log: &LinkLog) {
    assert!(
        wait_until(Duration::from_secs(10), || log.auths().len() == 1).await,
        "the first session should authenticate"
    );
    tokio::time::pause();
}

/// Stop the client and check `run()` returns.
async fn stop(run_task: RunTask, shutdown: bilbycast_gateway_sdk::CancellationToken) {
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), run_task)
        .await
        .expect("run() should return on shutdown")
        .unwrap()
        .unwrap();
}

/// The stall behind bilbycast-edge#180: the peer accepts the TCP connection,
/// reads the upgrade request and never answers. The dial was raced only
/// against shutdown, so `run()` sat there for ever and never dialled again.
#[tokio::test]
async fn a_dial_to_a_peer_that_never_answers_is_abandoned_and_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws/node", listener.local_addr().unwrap());
    let (accepted_tx, mut accepted) = mpsc::unbounded_channel::<Instant>();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let _ = accepted_tx.send(Instant::now());
            // Read the upgrade request, and whatever follows, and never write
            // a byte back.
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while matches!(socket.read(&mut buf).await, Ok(n) if n > 0) {}
            });
        }
    });

    let (run_task, _emitter, shutdown) =
        start_link_client(url, Arc::new(LinkHandler::default())).await;

    tokio::time::timeout(Duration::from_secs(10), accepted.recv())
        .await
        .expect("the SDK should dial")
        .unwrap();
    tokio::time::pause();
    // The 30 s dial deadline, then a 1 s backoff.
    tokio::time::timeout(Duration::from_secs(120), accepted.recv())
        .await
        .expect("the SDK must give up on a peer that never answers and dial again, not hang")
        .unwrap();

    stop(run_task, shutdown).await;
}

/// A manager that pings and then goes quiet while still taking what the
/// gateway writes: a wedged handler behind a live TCP connection. The
/// heartbeat writes kept succeeding and nothing noticed the quiet, so the
/// session stayed up for ever with nobody at the other end. Now it is taken
/// for dead after four ping intervals, held to the 90 s floor, and redialled.
#[tokio::test]
async fn a_manager_that_goes_silent_is_dropped_and_redialled() {
    let (url, log) = spawn_link_mock(vec![Behaviour::PingThenSilent { count: 2 }]).await;
    let (run_task, _emitter, shutdown) =
        start_link_client(url, Arc::new(LinkHandler::default())).await;
    first_session_then_pause_the_clock(&log).await;

    assert!(
        wait_until(Duration::from_secs(300), || log.auths().len() >= 2).await,
        "a silent manager must be dropped and redialled"
    );
    let last_ping = *log.pings.lock().unwrap().last().expect("the manager pinged");
    let redialled = log.auths()[1];
    let quiet_for = redialled - last_ping;
    assert!(
        quiet_for >= Duration::from_secs(90),
        "dropped {quiet_for:?} after the last ping, inside the 90 s floor"
    );
    assert!(
        quiet_for <= Duration::from_secs(120),
        "dropped {quiet_for:?} after the last ping; the floor is 90 s and checks run every 5 s"
    );

    stop(run_task, shutdown).await;
}

/// A command that runs longer than the silence limit — an `upgrade_binary`
/// fetching a tarball, say — while the manager keeps pinging and the vendor
/// keeps emitting.
///
/// The handler used to run inline in the session loop. For the whole command
/// the manager's pings sat unread, so to the manager the gateway had gone
/// quiet; and once the vendor's frames filled the outbound channel the
/// command's own ack waited on a channel only that parked loop drains, so the
/// session deadlocked. Now the reader answers the pings while the command
/// runs, the session stays up, and the ack arrives.
#[tokio::test]
async fn a_long_command_neither_silences_nor_wedges_the_session() {
    let slow = command("slow-1", json!({ "type": "slow", "secs": 150 }));
    let (url, log) =
        spawn_link_mock(vec![Behaviour::CommandsThenPing { commands: vec![slow] }]).await;
    let handler = Arc::new(LinkHandler::default());
    let (run_task, emitter, shutdown) = start_link_client(url, handler.clone()).await;
    first_session_then_pause_the_clock(&log).await;

    // Appear X keeps emitting whether or not anyone is draining the channel.
    let vendor = tokio::spawn(async move {
        while emitter.emit_stats(json!({ "tick": true })).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });

    assert!(
        wait_until(Duration::from_secs(400), || log.ack("slow-1").is_some()).await,
        "the long command's ack must arrive"
    );
    let started = handler.started_at("slow-1").expect("the command ran");
    let finished = handler.finished_at("slow-1").expect("the command finished");
    let answered = log.pongs_between(started, finished);
    assert!(
        answered >= 20,
        "only {answered} pongs while a 150 s command ran under a {PING_EVERY:?} keepalive: \
         the gateway stopped reading"
    );
    assert_eq!(log.auths().len(), 1, "the session must stay up through the command");

    vendor.abort();
    stop(run_task, shutdown).await;
}

/// Emit 32 MiB, several times what the socket buffers hold, so that against a
/// manager that has stopped reading a write stalls.
///
/// A fixed amount rather than a stream: the paused clock moves only when
/// every task is waiting, and a flood that never ends would keep the next
/// session busy draining it for ever, with time standing still.
fn flood(emitter: bilbycast_gateway_sdk::Emitter) -> tokio::task::JoinHandle<()> {
    let blob = "x".repeat(64 * 1024);
    tokio::spawn(async move {
        for _ in 0..512 {
            if emitter.emit_stats(json!({ "blob": blob })).await.is_err() {
                return;
            }
        }
    })
}

/// A manager that stops reading but keeps pinging — a proxy holding the
/// connection with its upstream stalled. The gateway's writes back up, and
/// they back up inside the session loop, so a watchdog stamped from that loop
/// would see silence at 90 s and flap a session whose manager is plainly
/// there. The reader still hears the pings, so the session stays up until
/// the 300 s write bound decides the manager has stopped draining, and is
/// redialled then. Without the bound the write waited for ever.
#[tokio::test]
async fn a_manager_that_stops_reading_is_dropped_at_the_write_bound_not_before() {
    let (url, log) = spawn_link_mock(vec![Behaviour::PingButNeverRead]).await;
    let (run_task, emitter, shutdown) =
        start_link_client(url, Arc::new(LinkHandler::default())).await;
    let vendor = flood(emitter);
    first_session_then_pause_the_clock(&log).await;
    let session_up = log.auths()[0];

    assert!(
        wait_until(Duration::from_secs(500), || log.auths().len() >= 2).await,
        "a session whose writes have stalled must end at the write bound"
    );
    let lasted = log.auths()[1] - session_up;
    assert!(
        lasted >= Duration::from_secs(250),
        "the session ended after {lasted:?}, though the manager was still pinging: \
         only the 300 s write bound should end it"
    );
    assert!(lasted <= Duration::from_secs(400), "the session lasted {lasted:?}");

    vendor.abort();
    stop(run_task, shutdown).await;
}

/// Shutdown while a write is stuck. Shutdown used to be one branch of the
/// session loop's `select!`, which a write that never completes stops the
/// loop from ever reaching: SIGTERM then waited for systemd's SIGKILL.
#[tokio::test]
async fn shutdown_is_prompt_while_the_manager_is_not_reading() {
    let (url, log) = spawn_link_mock(vec![Behaviour::PingButNeverRead]).await;
    let (run_task, emitter, shutdown) =
        start_link_client(url, Arc::new(LinkHandler::default())).await;
    let vendor = flood(emitter);
    first_session_then_pause_the_clock(&log).await;
    tokio::time::sleep(Duration::from_secs(30)).await;

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), run_task)
        .await
        .expect("run() must return within the 2 s Close bound, not wait on the stuck write")
        .unwrap()
        .unwrap();
    vendor.abort();
}

/// More commands than the queue holds while one runs: every command past the
/// queue is refused at once with `gateway_busy`, so the manager hears "not
/// run" rather than timing out on it, and the queued ones still run, in
/// order, once the long one finishes. None goes unanswered.
#[tokio::test]
async fn a_full_command_queue_is_answered_busy() {
    let mut commands = vec![command("slow-1", json!({ "type": "slow", "secs": 100 }))];
    commands.extend((0..100).map(|n| command(&format!("echo-{n}"), json!({ "type": "echo" }))));
    let (url, log) = spawn_link_mock(vec![Behaviour::CommandsThenPing { commands }]).await;
    let (run_task, _emitter, shutdown) =
        start_link_client(url, Arc::new(LinkHandler::default())).await;
    first_session_then_pause_the_clock(&log).await;

    assert!(
        wait_until(Duration::from_secs(400), || log.acks().len() >= 101).await,
        "every command must be answered, got {} acks",
        log.acks().len()
    );
    let acks = log.acks();
    assert_eq!(acks.len(), 101, "one ack per command");
    let (slow_done, slow) = log.ack("slow-1").unwrap();
    assert_eq!(slow["success"], true);

    let busy: Vec<_> = acks.iter().filter(|(_, a)| a["error_code"] == "gateway_busy").collect();
    assert!(!busy.is_empty(), "the commands past the queue must be refused as busy");
    assert!(
        busy.iter().all(|(at, _)| *at < slow_done),
        "a busy refusal must come at once, not after the running command"
    );

    let ran: Vec<u32> = acks
        .iter()
        .filter(|(_, a)| a["success"] == true && a["command_id"] != "slow-1")
        .map(|(_, a)| a["command_id"].as_str().unwrap()["echo-".len()..].parse().unwrap())
        .collect();
    assert_eq!(ran.len() + busy.len(), 100);
    assert!(ran.windows(2).all(|w| w[0] < w[1]), "queued commands ran out of order: {ran:?}");

    stop(run_task, shutdown).await;
}

/// A Ping and a Close read together, as they are when they arrive back to
/// back or while the session loop is held up in a write. The reader reads
/// both before the loop gets to its Pong for the ping, and tungstenite
/// refuses every write once it has read a Close. That refusal ended the
/// session as lost rather than closed, which does not reset the reconnect
/// backoff. Two such closes in a row: reset each time, the third session
/// follows the second within a second; counted as losses, the second wait is
/// the backoff's 60 s step.
#[tokio::test]
async fn a_close_read_behind_a_ping_is_a_clean_close() {
    let ping_then_close = Behaviour::Burst {
        frames: vec![Message::Ping(Vec::new().into()), Message::Close(None)],
    };
    let (url, log) = spawn_link_mock(vec![ping_then_close.clone(), ping_then_close]).await;
    let (run_task, _emitter, shutdown) =
        start_link_client_backing_off(url, Arc::new(LinkHandler::default()), vec![1, 60]).await;
    first_session_then_pause_the_clock(&log).await;

    assert!(
        wait_until(Duration::from_secs(30), || log.auths().len() >= 3).await,
        "a session the manager closed must reset the backoff, not count as lost"
    );

    stop(run_task, shutdown).await;
}

/// A command still running when its session ends. It finishes, and its ack
/// goes out on the next session; that session's command waits for it rather
/// than running beside it, as a vendor handler written for one command at a
/// time expects.
#[tokio::test]
async fn commands_never_overlap_across_a_reconnect() {
    let first = Behaviour::CommandsThenClose {
        commands: vec![command("slow-1", json!({ "type": "slow", "secs": 20 }))],
        after: Duration::from_secs(1),
    };
    let second = Behaviour::CommandsThenPing {
        commands: vec![command("echo-2", json!({ "type": "echo" }))],
    };
    let (url, log) = spawn_link_mock(vec![first, second]).await;
    let handler = Arc::new(LinkHandler::default());
    let (run_task, _emitter, shutdown) = start_link_client(url, handler.clone()).await;
    first_session_then_pause_the_clock(&log).await;

    assert!(
        wait_until(Duration::from_secs(60), || {
            log.ack("slow-1").is_some() && log.ack("echo-2").is_some()
        })
        .await,
        "both commands must be answered"
    );
    let reconnected = log.auths()[1];
    let (acked, ack) = log.ack("slow-1").unwrap();
    assert_eq!(ack["success"], true);
    assert!(acked > reconnected, "the ack must go out on the next session");
    let slow_done = handler.finished_at("slow-1").unwrap();
    let echo_started = handler.started_at("echo-2").unwrap();
    assert!(
        echo_started >= slow_done,
        "the next session's command started {:?} before the running one finished",
        slow_done - echo_started
    );

    stop(run_task, shutdown).await;
}

/// A handler that never returns while the link keeps reconnecting. Each
/// session's worker used to wait on the previous session's worker for as
/// long as the hang lasted, holding its session's queue, so every reconnect
/// parked one more. A worker whose session ends before its turn now returns,
/// so the number of live workers (each holds a handle to the handler) stays
/// put however often the link flaps. The commands of the sessions that ended
/// never run, before or after the hang ends.
#[tokio::test]
async fn reconnects_behind_a_hung_command_do_not_pile_up_workers() {
    const FLAPS: usize = 10;
    let mut behaviours = vec![Behaviour::CommandsThenClose {
        commands: vec![command("hang-1", json!({ "type": "hang" }))],
        after: Duration::from_secs(1),
    }];
    behaviours.extend((2..=FLAPS).map(|n| Behaviour::CommandsThenClose {
        commands: vec![command(&format!("echo-{n}"), json!({ "type": "echo" }))],
        after: Duration::from_secs(1),
    }));
    // The session after the last flap is the mock's default, Quiet, and
    // stays up.
    let (url, log) = spawn_link_mock(behaviours).await;
    let handler = Arc::new(LinkHandler::default());
    let (run_task, _emitter, shutdown) = start_link_client(url, handler.clone()).await;
    first_session_then_pause_the_clock(&log).await;

    assert!(
        wait_until(Duration::from_secs(120), || log.auths().len() > FLAPS).await,
        "the link must keep reconnecting behind the hung command"
    );
    // This test's, the client's, the hung command's worker and its task, and
    // the live session's worker waiting for the turn.
    assert!(
        wait_until(Duration::from_secs(5), || Arc::strong_count(&handler) <= 5).await,
        "{} handles to the handler after {FLAPS} reconnects: the workers of ended sessions are \
         still parked",
        Arc::strong_count(&handler)
    );

    handler.release.notify_one();
    assert!(
        wait_until(Duration::from_secs(10), || log.ack("hang-1").is_some()).await,
        "the hung command's ack must arrive once it returns"
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        handler.started(),
        ["hang-1"],
        "a command queued in a session that ended must never run"
    );

    stop(run_task, shutdown).await;
}

/// Shutdown while a command runs. The command used to run inside the session
/// loop, so shutdown waited for it; once it moved to a worker, `run()`
/// returned within the 2 s Close bound, and a sidecar's `main` returning
/// right after it tore the runtime down with the command part-way through.
/// Now `run()` returns once the command has finished, and promptly then,
/// even against a manager that has stopped reading while the vendor's frames
/// fill the outbound channel, where the command's ack has nowhere to go.
#[tokio::test]
async fn shutdown_lets_a_started_command_finish() {
    let slow = command("slow-1", json!({ "type": "slow", "secs": 20 }));
    let (url, log) =
        spawn_link_mock(vec![Behaviour::CommandsThenStopReading { commands: vec![slow] }]).await;
    let handler = Arc::new(LinkHandler::default());
    let (run_task, emitter, shutdown) = start_link_client(url, handler.clone()).await;
    let vendor = flood(emitter);
    first_session_then_pause_the_clock(&log).await;
    assert!(
        wait_until(Duration::from_secs(10), || handler.started_at("slow-1").is_some()).await,
        "the command should start"
    );
    tokio::time::sleep(Duration::from_secs(5)).await;

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(60), run_task)
        .await
        .expect("run() should return once the command has finished")
        .unwrap()
        .unwrap();
    let returned = Instant::now();
    let finished = handler
        .finished_at("slow-1")
        .expect("run() returned with the command still running, for the runtime to cut off");
    assert!(
        returned - finished < Duration::from_secs(1),
        "run() returned {:?} after the command finished: its ack, waiting for room in a channel \
         nothing drains, held shutdown up",
        returned - finished
    );
    vendor.abort();
}

/// Shutdown while a command that never returns runs: `run()` waits for it
/// for 30 s and no longer.
#[tokio::test]
async fn shutdown_gives_up_on_a_command_that_never_finishes() {
    let hang = command("hang-1", json!({ "type": "hang" }));
    let (url, log) =
        spawn_link_mock(vec![Behaviour::CommandsThenPing { commands: vec![hang] }]).await;
    let handler = Arc::new(LinkHandler::default());
    let (run_task, _emitter, shutdown) = start_link_client(url, handler.clone()).await;
    first_session_then_pause_the_clock(&log).await;
    assert!(
        wait_until(Duration::from_secs(10), || handler.started_at("hang-1").is_some()).await,
        "the command should start"
    );

    shutdown.cancel();
    let cancelled = Instant::now();
    tokio::time::timeout(Duration::from_secs(120), run_task)
        .await
        .expect("run() must give up on a command that never returns")
        .unwrap()
        .unwrap();
    let waited = cancelled.elapsed();
    assert!(
        (Duration::from_secs(30)..=Duration::from_secs(32)).contains(&waited),
        "run() returned {waited:?} after shutdown; it should wait 30 s for the running command"
    );
}

/// Shutdown just before a running command finishes, with another queued
/// behind it and a manager that has stopped reading, so writing the Close
/// takes its whole 2 s. The session was marked over only once that write had
/// given up, so the running command finished inside it and the worker
/// started the queued one: a command shutdown then waited on, though no
/// queued command may start once its session has ended.
#[tokio::test]
async fn no_queued_command_starts_while_the_close_is_written() {
    // In one write, so the gateway routes both before it takes the stats
    // frame below and its write sticks.
    let commands = vec![
        command("slow-1", json!({ "type": "slow", "secs": 60 })),
        command("echo-2", json!({ "type": "echo" })),
    ];
    let (url, log) =
        spawn_link_mock(vec![Behaviour::CommandsThenStopReading { commands }]).await;
    let handler = Arc::new(LinkHandler::default());
    let (run_task, emitter, shutdown) = start_link_client(url, handler.clone()).await;
    first_session_then_pause_the_clock(&log).await;
    assert!(
        wait_until(Duration::from_secs(10), || handler.started_at("slow-1").is_some()).await,
        "the command should start"
    );
    let slow_started = handler.started_at("slow-1").unwrap();
    // One frame larger than the socket buffers hold, so the session's write
    // sticks without filling the outbound channel: the running command's ack
    // must have room, or it alone would keep the worker from the next one.
    emitter.emit_stats(json!({ "blob": "x".repeat(32 << 20) })).await.unwrap();

    tokio::time::sleep_until(slow_started + Duration::from_millis(59_500)).await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(60), run_task)
        .await
        .expect("run() should return")
        .unwrap()
        .unwrap();
    assert!(handler.finished_at("slow-1").is_some(), "the running command should finish");
    assert_eq!(
        handler.started(),
        ["slow-1"],
        "a command queued behind it started after shutdown was requested"
    );
}
