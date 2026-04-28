// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Sidecar HTTP reverse-proxy: framed multiplex over the existing manager WS.
//!
//! Lets the manager open one or more streams to a 3rd-party device's HTTP/HTTPS
//! UI through the sidecar, without a separate TCP tunnel. The sidecar uses its
//! own pre-configured `reqwest::Client` (TLS pin / self-signed posture honoured)
//! and pipes response bytes back as chunked `proxy_data { dir: "resp" }` frames.
//!
//! Wire types (envelope `type` value):
//! - manager → sidecar: `proxy_open`, `proxy_data` (`dir: "req"`), `proxy_close`
//! - sidecar → manager: `proxy_resp_head`, `proxy_data` (`dir: "resp"`), `proxy_close`
//!
//! Frame body cap: `max_chunk_bytes` (default 2 MiB) before base64. Stays well
//! under the 5 MiB WS message cap with envelope/header overhead.
//!
//! Per-node concurrent stream cap: `max_concurrent_streams` (default 8). Excess
//! `proxy_open` frames are immediately rejected with `proxy_close { reason:
//! "stream_limit" }`.
//!
//! `target_base_url` is hard-pinned: any request whose resolved URL host /
//! scheme / port differs from the configured base is refused with
//! `proxy_close { reason: "invalid_url" }`. Defends path-traversal escape.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::emit::Emitter;

/// Capability string the sidecar advertises in `HealthPayload.capabilities` so
/// the manager UI gates the "Open Device Web UI" button.
pub const PROXY_CAPABILITY: &str = "web-ui-proxy";

/// Configuration for the SDK's built-in HTTP proxy. Construct one and pass to
/// [`crate::GatewayClient::with_http_proxy`].
#[derive(Debug, Clone)]
pub struct HttpProxyConfig {
    /// Hard-pinned target. Scheme / host / port are matched exactly on every
    /// request — a `proxy_open` whose resolved URL deviates is refused.
    pub target_base_url: reqwest::Url,
    /// Pre-configured client. Reuse the gateway's existing `reqwest::Client`
    /// so TLS pinning / self-signed posture / timeout / connection pool are
    /// honoured. Do **not** enable `cookie_store(true)` — the proxy must
    /// faithfully round-trip browser cookies via `Cookie` header per request,
    /// not stash them server-side.
    pub client: reqwest::Client,
    /// Maximum concurrent in-flight streams per gateway. Excess `proxy_open`
    /// frames are immediately rejected. Default 8.
    pub max_concurrent_streams: usize,
    /// Maximum bytes per response chunk frame (before base64). Default 2 MiB.
    pub max_chunk_bytes: usize,
    /// Allowed HTTP methods. Default: GET POST PUT DELETE PATCH HEAD OPTIONS.
    pub allow_methods: Vec<String>,
}

impl HttpProxyConfig {
    /// Build a config with sensible defaults. Caller supplies the pinned
    /// target and the pre-configured `reqwest::Client`.
    pub fn new(target_base_url: reqwest::Url, client: reqwest::Client) -> Self {
        Self {
            target_base_url,
            client,
            max_concurrent_streams: 8,
            max_chunk_bytes: 2_000_000,
            allow_methods: vec![
                "GET".into(),
                "POST".into(),
                "PUT".into(),
                "DELETE".into(),
                "PATCH".into(),
                "HEAD".into(),
                "OPTIONS".into(),
            ],
        }
    }
}

/// In-flight stream registry + per-stream task spawning. Constructed by the
/// SDK when the user calls `with_http_proxy`. Public for integration testing
/// — vendor code never instantiates this directly.
#[doc(hidden)]
pub struct ProxyDispatcher {
    cfg: HttpProxyConfig,
    emitter: Emitter,
    streams: Arc<Mutex<HashMap<u64, StreamHandle>>>,
}

struct StreamHandle {
    /// Sender for inbound request-body chunks (only populated when the manager
    /// signalled `has_request_body: true`).
    req_body_tx: Option<mpsc::Sender<Bytes>>,
    cancel: CancellationToken,
}

impl ProxyDispatcher {
    #[doc(hidden)]
    pub fn new(cfg: HttpProxyConfig, emitter: Emitter) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            emitter,
            streams: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Handle a `proxy_open` envelope from the manager.
    #[doc(hidden)]
    pub async fn handle_proxy_open(self: &Arc<Self>, payload: Value) {
        let stream_id = match payload.get("stream_id").and_then(|v| v.as_u64()) {
            Some(id) => id,
            None => {
                debug!("proxy_open without stream_id, ignoring");
                return;
            }
        };

        // Stream-cap check — done while holding the lock so two concurrent
        // opens can't race past the limit.
        {
            let streams = self.streams.lock().await;
            if streams.len() >= self.cfg.max_concurrent_streams {
                drop(streams);
                let _ = self
                    .emitter
                    .emit_proxy_close(stream_id, "stream_limit", None)
                    .await;
                return;
            }
        }

        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET")
            .to_string();
        let path = payload
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("/")
            .to_string();
        let query = payload
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let req_headers: Vec<(String, String)> = payload
            .get("headers")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|pair| {
                        let pair = pair.as_array()?;
                        let k = pair.first()?.as_str()?.to_string();
                        let v = pair.get(1)?.as_str()?.to_string();
                        Some((k, v))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let has_request_body = payload
            .get("has_request_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let timeout_ms = payload
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(30_000);

        if !self
            .cfg
            .allow_methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(&method))
        {
            let _ = self
                .emitter
                .emit_proxy_close(stream_id, "method_not_allowed", Some(&method))
                .await;
            return;
        }

        let target_url = match self.build_target_url(&path, &query) {
            Ok(u) => u,
            Err(e) => {
                let _ = self
                    .emitter
                    .emit_proxy_close(stream_id, "invalid_url", Some(&e))
                    .await;
                return;
            }
        };

        let (req_body_tx, req_body_rx) = if has_request_body {
            let (tx, rx) = mpsc::channel::<Bytes>(8);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        let cancel = CancellationToken::new();
        let handle = StreamHandle {
            req_body_tx,
            cancel: cancel.clone(),
        };
        {
            let mut streams = self.streams.lock().await;
            streams.insert(stream_id, handle);
        }

        let cfg = self.cfg.clone();
        let emitter = self.emitter.clone();
        let streams_handle = self.streams.clone();
        let timeout = Duration::from_millis(timeout_ms);

        tokio::spawn(async move {
            let result = tokio::select! {
                r = run_stream(
                    &cfg,
                    &emitter,
                    stream_id,
                    method,
                    target_url,
                    req_headers,
                    req_body_rx,
                    timeout,
                ) => r,
                _ = cancel.cancelled() => {
                    debug!(stream_id, "proxy stream cancelled");
                    Err("cancelled".to_string())
                }
            };

            // Always remove from registry first.
            {
                let mut streams = streams_handle.lock().await;
                streams.remove(&stream_id);
            }

            if let Err(reason) = result {
                let _ = emitter
                    .emit_proxy_close(stream_id, "upstream_error", Some(&reason))
                    .await;
            }
        });
    }

    /// Handle a `proxy_data` envelope (request-body chunk only).
    #[doc(hidden)]
    pub async fn handle_proxy_data_req(self: &Arc<Self>, payload: Value) {
        let stream_id = match payload.get("stream_id").and_then(|v| v.as_u64()) {
            Some(id) => id,
            None => return,
        };
        let dir = payload.get("dir").and_then(|v| v.as_str()).unwrap_or("");
        if dir != "req" {
            // Ignore frames the manager has no business sending (e.g. dir=resp).
            return;
        }
        let chunk_b64 = payload
            .get("chunk_b64")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let eof = payload
            .get("eof")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let tx = {
            let streams = self.streams.lock().await;
            streams
                .get(&stream_id)
                .and_then(|h| h.req_body_tx.clone())
        };
        let Some(tx) = tx else {
            // Stream not found or not expecting a body — drop silently.
            return;
        };

        if !chunk_b64.is_empty() {
            match B64.decode(chunk_b64.as_bytes()) {
                Ok(bytes) => {
                    let _ = tx.send(Bytes::from(bytes)).await;
                }
                Err(e) => {
                    warn!(stream_id, "invalid base64 in proxy_data req chunk: {e}");
                    let _ = self
                        .emitter
                        .emit_proxy_close(stream_id, "decode_error", Some(&e.to_string()))
                        .await;
                    self.cancel_stream(stream_id).await;
                    return;
                }
            }
        }

        if eof {
            // Drop the sender to close the request-body stream cleanly.
            let mut streams = self.streams.lock().await;
            if let Some(h) = streams.get_mut(&stream_id) {
                h.req_body_tx = None;
            }
        }
    }

    /// Handle a `proxy_close` envelope from the manager.
    #[doc(hidden)]
    pub async fn handle_proxy_close(self: &Arc<Self>, payload: Value) {
        let stream_id = match payload.get("stream_id").and_then(|v| v.as_u64()) {
            Some(id) => id,
            None => return,
        };
        self.cancel_stream(stream_id).await;
    }

    async fn cancel_stream(&self, stream_id: u64) {
        let mut streams = self.streams.lock().await;
        if let Some(h) = streams.remove(&stream_id) {
            h.cancel.cancel();
        }
    }

    /// Compose `target_base_url + path + query` and verify the resolved URL
    /// stays on the same scheme / host / port. Defends path-traversal escape.
    fn build_target_url(&self, path: &str, query: &str) -> Result<reqwest::Url, String> {
        // Path must start with '/' — reject attempts to inject scheme/host.
        if !path.starts_with('/') {
            return Err("path must start with '/'".into());
        }
        let mut composed = format!(
            "{}://{}",
            self.cfg.target_base_url.scheme(),
            self.cfg
                .target_base_url
                .host_str()
                .ok_or_else(|| "target_base_url missing host".to_string())?,
        );
        if let Some(port) = self.cfg.target_base_url.port() {
            composed.push(':');
            composed.push_str(&port.to_string());
        }
        composed.push_str(path);
        if !query.is_empty() {
            // Strip leading '?' if the manager included it.
            let q = query.strip_prefix('?').unwrap_or(query);
            if !q.is_empty() {
                composed.push('?');
                composed.push_str(q);
            }
        }

        let url = reqwest::Url::parse(&composed).map_err(|e| e.to_string())?;

        if url.scheme() != self.cfg.target_base_url.scheme()
            || url.host_str() != self.cfg.target_base_url.host_str()
            || url.port_or_known_default() != self.cfg.target_base_url.port_or_known_default()
        {
            return Err("resolved URL outside target_base_url".into());
        }
        Ok(url)
    }
}

/// Per-stream worker: build the reqwest, await the response head, stream the
/// body back as chunked `proxy_data` frames. Returns `Err(reason)` on any
/// upstream failure so the dispatcher can emit `proxy_close { upstream_error }`.
#[allow(clippy::too_many_arguments)]
async fn run_stream(
    cfg: &HttpProxyConfig,
    emitter: &Emitter,
    stream_id: u64,
    method: String,
    url: reqwest::Url,
    req_headers: Vec<(String, String)>,
    req_body_rx: Option<mpsc::Receiver<Bytes>>,
    timeout: Duration,
) -> Result<(), String> {
    let method_parsed = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|e| format!("invalid method: {e}"))?;

    let mut req = cfg.client.request(method_parsed, url.clone()).timeout(timeout);

    // Forward headers, but filter out hop-by-hop, host, accept-encoding, referer.
    // Set Accept-Encoding: identity so the device returns uncompressed bytes
    // (avoids gzip/base64 double-encoding ambiguity downstream). Set Referer
    // to the device's own URL so Referer-strict device UIs accept the request.
    for (k, v) in &req_headers {
        let lower = k.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "host"
                | "accept-encoding"
                | "referer"
                | "connection"
                | "proxy-connection"
                | "keep-alive"
                | "transfer-encoding"
                | "upgrade"
                | "te"
                | "trailer"
                | "content-length"
        ) {
            continue;
        }
        req = req.header(k, v);
    }
    req = req.header("Accept-Encoding", "identity");
    req = req.header("Referer", url.as_str());

    if let Some(rx) = req_body_rx {
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx)
            .map(|b: Bytes| Ok::<Bytes, std::io::Error>(b));
        req = req.body(reqwest::Body::wrap_stream(stream));
    }

    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let resp_headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            // Strip Content-Encoding because we forced identity outbound.
            // Strip Transfer-Encoding / Connection — hop-by-hop.
            let name = k.as_str().to_ascii_lowercase();
            if matches!(
                name.as_str(),
                "content-encoding"
                    | "transfer-encoding"
                    | "connection"
                    | "keep-alive"
                    | "proxy-connection"
                    | "upgrade"
                    | "te"
                    | "trailer"
            ) {
                return None;
            }
            Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))
        })
        .collect();

    emitter
        .emit_proxy_resp_head(stream_id, status, &resp_headers)
        .await
        .map_err(|e| e.to_string())?;

    // Stream the body, re-chunking to <= max_chunk_bytes per frame.
    let mut body_stream = resp.bytes_stream();
    let mut pending = bytes::BytesMut::new();
    while let Some(chunk) = body_stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        pending.extend_from_slice(&chunk);
        while pending.len() >= cfg.max_chunk_bytes {
            let frame = pending.split_to(cfg.max_chunk_bytes).freeze();
            emitter
                .emit_proxy_data_resp(stream_id, &frame, false)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    // Final chunk with eof=true (may be empty).
    let final_frame = pending.freeze();
    emitter
        .emit_proxy_data_resp(stream_id, &final_frame, true)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cfg(base: &str) -> HttpProxyConfig {
        HttpProxyConfig::new(
            reqwest::Url::parse(base).unwrap(),
            reqwest::Client::new(),
        )
    }

    #[test]
    fn build_target_url_happy_path() {
        let cfg = make_cfg("https://10.0.0.5/");
        let dispatcher = ProxyDispatcher::new(cfg, Emitter::new(tokio::sync::mpsc::channel(1).0));
        let url = dispatcher
            .build_target_url("/mmi/index.html", "?tab=alarms")
            .unwrap();
        assert_eq!(url.as_str(), "https://10.0.0.5/mmi/index.html?tab=alarms");
    }

    #[test]
    fn build_target_url_with_port() {
        let cfg = make_cfg("https://10.0.0.5:8443/");
        let dispatcher = ProxyDispatcher::new(cfg, Emitter::new(tokio::sync::mpsc::channel(1).0));
        let url = dispatcher.build_target_url("/foo", "").unwrap();
        assert_eq!(url.as_str(), "https://10.0.0.5:8443/foo");
    }

    #[test]
    fn build_target_url_rejects_path_without_slash() {
        let cfg = make_cfg("https://10.0.0.5/");
        let dispatcher = ProxyDispatcher::new(cfg, Emitter::new(tokio::sync::mpsc::channel(1).0));
        assert!(dispatcher.build_target_url("foo", "").is_err());
    }

    #[test]
    fn build_target_url_rejects_scheme_in_path() {
        let cfg = make_cfg("https://10.0.0.5/");
        let dispatcher = ProxyDispatcher::new(cfg, Emitter::new(tokio::sync::mpsc::channel(1).0));
        // Without leading slash → rejected. Defends against attempts to
        // inject `http://other/x` as the "path" parameter.
        assert!(dispatcher.build_target_url("http://evil.com/x", "").is_err());
        assert!(dispatcher.build_target_url("https://evil.com/x", "").is_err());
    }

    #[test]
    fn build_target_url_keeps_path_with_double_slash_on_target_host() {
        // A path that *starts* with `//` is still a valid path; the URL parser
        // keeps the configured host. The HTTP request goes to the pinned host
        // — the device's own server gets to decide what `//evil/x` means in
        // its filesystem, but we never connect anywhere else.
        let cfg = make_cfg("https://10.0.0.5/");
        let dispatcher = ProxyDispatcher::new(cfg, Emitter::new(tokio::sync::mpsc::channel(1).0));
        let url = dispatcher.build_target_url("//evil.example.com/x", "").unwrap();
        assert_eq!(url.host_str(), Some("10.0.0.5"));
    }

    #[test]
    fn build_target_url_strips_leading_question_mark_from_query() {
        let cfg = make_cfg("https://10.0.0.5/");
        let dispatcher = ProxyDispatcher::new(cfg, Emitter::new(tokio::sync::mpsc::channel(1).0));
        let url = dispatcher.build_target_url("/foo", "x=1&y=2").unwrap();
        assert_eq!(url.as_str(), "https://10.0.0.5/foo?x=1&y=2");
    }
}
