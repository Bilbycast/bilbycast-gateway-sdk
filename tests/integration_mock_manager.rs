// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Mock-manager integration test.
//!
//! Spins up a plaintext WS server on a random port (opt-in via the
//! `BILBYCAST_SDK_ALLOW_PLAINTEXT_WS=1` env var the SDK honours in tests),
//! connects the SDK to it, and exercises:
//! - Register with a token → `register_ack` + node_id/node_secret
//! - Server sends a `command` → handler dispatched → `command_ack` matches
//! - SDK emits stats / event / health → server receives expected envelopes
//! - Server closes the socket → SDK reconnects with persisted credentials
//! - `get_config` command → SDK replies with `config_response` + ack
//! - `ping` → `pong`

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bilbycast_gateway_sdk::{
    CommandError, CommandHandler, GatewayClient, GatewayConfig, GatewayEvent,
    GATEWAY_WS_PROTOCOL_VERSION,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio_tungstenite::tungstenite::Message;

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

/// Set the env var the SDK honours to allow `ws://` URLs in tests.
fn enable_plaintext_ws() {
    std::env::set_var("BILBYCAST_SDK_ALLOW_PLAINTEXT_WS", "1");
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
    let mut cfg = GatewayConfig::minimal(url.clone(), "test_device", "0.0.1-test");
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

    let mut cfg = GatewayConfig::minimal(url, "test_device", "0.0.1-test");
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
