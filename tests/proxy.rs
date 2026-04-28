// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integration tests for the SDK's HTTP-proxy framing.
//!
//! These tests don't spin up a real WS — they construct a `ProxyDispatcher`
//! directly, hand it `proxy_open` payloads as if the manager had sent them,
//! and assert the sequence of frames it emits via the outbound channel.
//!
//! The "device" is a wiremock HTTP server. Each test verifies one slice of
//! the framed protocol: response head shape, multi-chunk streaming, large
//! body chunking, target-URL pinning, concurrent-stream cap, etc.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use bilbycast_gateway_sdk::{Emitter, HttpProxyConfig, ProxyDispatcher};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a `ProxyDispatcher` against a wiremock server and return:
/// - the dispatcher (Arc, share with handlers)
/// - the wiremock server (keep alive for the test)
/// - the receiver end of the dispatcher's outbound channel (drain frames here)
async fn build_dispatcher() -> (
    Arc<ProxyDispatcher>,
    MockServer,
    mpsc::Receiver<bilbycast_gateway_sdk::emit::OutboundFrame>,
) {
    let mock = MockServer::start().await;
    let (tx, rx) = mpsc::channel::<bilbycast_gateway_sdk::emit::OutboundFrame>(256);
    let emitter = Emitter::new(tx);
    let target = reqwest::Url::parse(&mock.uri()).unwrap();
    let cfg = HttpProxyConfig::new(target, reqwest::Client::new());
    let dispatcher = ProxyDispatcher::new(cfg, emitter);
    (dispatcher, mock, rx)
}

/// Drain frames from the outbound channel until either:
/// - we've collected `max_frames`, OR
/// - `timeout` elapses with no frame.
async fn drain_frames(
    rx: &mut mpsc::Receiver<bilbycast_gateway_sdk::emit::OutboundFrame>,
    max_frames: usize,
    timeout: Duration,
) -> Vec<Value> {
    let mut frames = Vec::new();
    while frames.len() < max_frames {
        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Some(frame)) => {
                let v: Value = serde_json::from_str(&frame.0).unwrap();
                frames.push(v);
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    frames
}

#[tokio::test]
async fn round_trip_get_returns_resp_head_then_body_then_eof() {
    let (dispatcher, mock, mut rx) = build_dispatcher().await;
    Mock::given(method("GET"))
        .and(path("/hello"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("hi from device")
                .insert_header("Content-Type", "text/plain"),
        )
        .mount(&mock)
        .await;

    dispatcher
        .handle_proxy_open(json!({
            "stream_id": 7,
            "method": "GET",
            "path": "/hello",
            "query": "",
            "headers": [],
            "has_request_body": false,
            "timeout_ms": 5000,
        }))
        .await;

    let frames = drain_frames(&mut rx, 8, Duration::from_secs(2)).await;
    assert!(!frames.is_empty(), "no frames emitted");

    // First frame: proxy_resp_head
    assert_eq!(frames[0]["type"], "proxy_resp_head");
    assert_eq!(frames[0]["payload"]["stream_id"], 7);
    assert_eq!(frames[0]["payload"]["status"], 200);
    let head_headers = frames[0]["payload"]["headers"].as_array().unwrap();
    let has_ct = head_headers.iter().any(|h| {
        h.as_array()
            .and_then(|a| a.first())
            .and_then(|s| s.as_str())
            .map(|s| s.eq_ignore_ascii_case("content-type"))
            .unwrap_or(false)
    });
    assert!(has_ct, "content-type header missing");

    // Subsequent frames: one or more proxy_data frames, last with eof=true
    let data_frames: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "proxy_data")
        .collect();
    assert!(!data_frames.is_empty(), "no proxy_data frames");
    let last = data_frames.last().unwrap();
    assert_eq!(last["payload"]["dir"], "resp");
    assert_eq!(last["payload"]["eof"], true);

    // Concatenate body chunks and assert content.
    let mut body_bytes = Vec::new();
    for f in &data_frames {
        if let Some(b64) = f["payload"]["chunk_b64"].as_str() {
            if !b64.is_empty() {
                body_bytes.extend(B64.decode(b64).unwrap());
            }
        }
    }
    assert_eq!(body_bytes, b"hi from device");
}

#[tokio::test]
async fn large_body_is_split_into_multiple_chunks() {
    let (dispatcher, mock, mut rx) = build_dispatcher().await;
    // 5 MiB body — should chunk into ~3 frames at the 2 MiB default cap.
    let big = vec![b'X'; 5 * 1024 * 1024];
    Mock::given(method("GET"))
        .and(path("/big"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(big.clone()))
        .mount(&mock)
        .await;

    dispatcher
        .handle_proxy_open(json!({
            "stream_id": 1,
            "method": "GET",
            "path": "/big",
            "query": "",
            "headers": [],
            "has_request_body": false,
            "timeout_ms": 10000,
        }))
        .await;

    let frames = drain_frames(&mut rx, 16, Duration::from_secs(5)).await;
    let data_frames: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "proxy_data")
        .collect();

    // At least 2 chunks (5 MiB / 2 MiB cap = 3 chunks; last is partial + eof).
    assert!(
        data_frames.len() >= 2,
        "expected multiple chunks, got {}",
        data_frames.len()
    );
    // Last chunk has eof=true.
    assert_eq!(data_frames.last().unwrap()["payload"]["eof"], true);
    // All non-final chunks have eof=false.
    for f in &data_frames[..data_frames.len() - 1] {
        assert_eq!(f["payload"]["eof"], false);
    }

    // Reassemble + assert byte-exact round-trip.
    let mut got = Vec::new();
    for f in &data_frames {
        let b64 = f["payload"]["chunk_b64"].as_str().unwrap_or("");
        if !b64.is_empty() {
            got.extend(B64.decode(b64).unwrap());
        }
    }
    assert_eq!(got, big);
}

#[tokio::test]
async fn rejects_open_when_method_not_allowed() {
    let (dispatcher, _mock, mut rx) = build_dispatcher().await;
    dispatcher
        .handle_proxy_open(json!({
            "stream_id": 99,
            "method": "TRACE",  // not in default allow_methods
            "path": "/foo",
            "query": "",
            "headers": [],
            "has_request_body": false,
            "timeout_ms": 5000,
        }))
        .await;

    let frames = drain_frames(&mut rx, 4, Duration::from_secs(1)).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["type"], "proxy_close");
    assert_eq!(frames[0]["payload"]["reason"], "method_not_allowed");
}

#[tokio::test]
async fn rejects_open_when_url_is_invalid() {
    let (dispatcher, _mock, mut rx) = build_dispatcher().await;
    dispatcher
        .handle_proxy_open(json!({
            "stream_id": 42,
            "method": "GET",
            "path": "no-leading-slash",
            "query": "",
            "headers": [],
            "has_request_body": false,
            "timeout_ms": 5000,
        }))
        .await;

    let frames = drain_frames(&mut rx, 4, Duration::from_secs(1)).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["type"], "proxy_close");
    assert_eq!(frames[0]["payload"]["reason"], "invalid_url");
}

#[tokio::test]
async fn enforces_concurrent_stream_cap() {
    let (mock, mut rx) = {
        let m = MockServer::start().await;
        // Slow responder so the streams are still in-flight when the next opens.
        Mock::given(method("GET")).and(path("/slow")).respond_with(
            ResponseTemplate::new(200)
                .set_body_string("ok")
                .set_delay(Duration::from_millis(800)),
        )
        .mount(&m)
        .await;
        let (tx, rx) = mpsc::channel(256);
        let emitter = Emitter::new(tx);
        let mut cfg = HttpProxyConfig::new(reqwest::Url::parse(&m.uri()).unwrap(), reqwest::Client::new());
        cfg.max_concurrent_streams = 2;
        let dispatcher = ProxyDispatcher::new(cfg, emitter);
        // Open three streams in quick succession — the third should be
        // immediately refused with stream_limit.
        for sid in 1..=3u64 {
            dispatcher
                .handle_proxy_open(json!({
                    "stream_id": sid,
                    "method": "GET",
                    "path": "/slow",
                    "query": "",
                    "headers": [],
                    "has_request_body": false,
                    "timeout_ms": 5000,
                }))
                .await;
        }
        (m, rx)
    };
    let _ = mock; // keep alive

    // Drain enough frames to see the immediate stream_limit close plus
    // the in-flight responses.
    let frames = drain_frames(&mut rx, 16, Duration::from_secs(3)).await;
    let stream3_close = frames.iter().find(|f| {
        f["type"] == "proxy_close"
            && f["payload"]["stream_id"].as_u64() == Some(3)
            && f["payload"]["reason"] == "stream_limit"
    });
    assert!(stream3_close.is_some(), "stream 3 should be refused");
}

#[tokio::test]
async fn forwards_request_body_chunks() {
    let (dispatcher, mock, mut rx) = build_dispatcher().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(ResponseTemplate::new(201).set_body_string("created"))
        .mount(&mock)
        .await;

    dispatcher
        .handle_proxy_open(json!({
            "stream_id": 5,
            "method": "POST",
            "path": "/echo",
            "query": "",
            "headers": [["Content-Type", "application/json"]],
            "has_request_body": true,
            "timeout_ms": 5000,
        }))
        .await;

    // Push a single chunk + eof.
    let body = b"{\"key\":\"value\"}";
    dispatcher
        .handle_proxy_data_req(json!({
            "stream_id": 5,
            "dir": "req",
            "chunk_b64": B64.encode(body),
            "eof": true,
        }))
        .await;

    let frames = drain_frames(&mut rx, 8, Duration::from_secs(2)).await;
    let head = frames
        .iter()
        .find(|f| f["type"] == "proxy_resp_head")
        .expect("missing resp_head");
    assert_eq!(head["payload"]["status"], 201);
}

#[tokio::test]
async fn proxy_close_from_manager_aborts_stream() {
    let (mock, mut rx) = {
        let m = MockServer::start().await;
        Mock::given(method("GET")).and(path("/slow")).respond_with(
            ResponseTemplate::new(200)
                .set_body_string("ok")
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&m)
        .await;
        let (tx, rx) = mpsc::channel(64);
        let emitter = Emitter::new(tx);
        let cfg = HttpProxyConfig::new(reqwest::Url::parse(&m.uri()).unwrap(), reqwest::Client::new());
        let dispatcher = ProxyDispatcher::new(cfg, emitter);
        dispatcher
            .handle_proxy_open(json!({
                "stream_id": 8,
                "method": "GET",
                "path": "/slow",
                "query": "",
                "headers": [],
                "has_request_body": false,
                "timeout_ms": 10000,
            }))
            .await;
        // Immediately cancel.
        dispatcher
            .handle_proxy_close(json!({"stream_id": 8, "reason": "client_cancelled"}))
            .await;
        (m, rx)
    };
    let _ = mock;

    // Stream task may emit a proxy_close on its own when the cancel token
    // fires; or no frames at all because the registry was wiped before the
    // task could emit. Either is correct — assert we don't get a complete
    // resp_head + eof sequence.
    let frames = drain_frames(&mut rx, 16, Duration::from_millis(800)).await;
    let saw_eof = frames
        .iter()
        .any(|f| f["type"] == "proxy_data" && f["payload"]["eof"] == true);
    assert!(!saw_eof, "stream should not have emitted eof after cancel");
}
