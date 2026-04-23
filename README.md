# bilbycast-gateway-sdk

SDK for building 3rd-party device gateway sidecars that integrate with
[bilbycast-manager](../bilbycast-manager).

A **gateway** is a small sidecar binary paired 1:1 with a vendor device
(e.g. an Appear X chassis, a compatible encoder, etc.). The sidecar:

1. Connects outbound to the manager over WSS using the same auth and envelope
   protocol as bilbycast-edge / bilbycast-relay nodes.
2. Polls the vendor device's native API (JSON-RPC, REST, SNMP, …) and reports
   `stats`, `health`, and `event` envelopes to the manager.
3. Translates `command` envelopes from the manager into vendor API calls and
   replies with `command_ack`.

This crate formalises the manager-facing plumbing so vendors only implement
their polling loop and their [`CommandHandler`](src/dispatch.rs).

## What you get

- `GatewayClient` — opinionated WSS client with:
  - Multi-URL failover across `manager_urls`
  - Automatic reconnect with exponential backoff (1 → 2 → 5 → 10 → 30 s)
  - Graceful shutdown via `CancellationToken`
  - Heartbeat every 15 s (configurable)
  - String-based dispatch with a catch-all — new manager message types
    don't break old gateways
- `Emitter` — async helpers for `emit_stats`, `emit_event`, `emit_health`,
  `emit_thumbnail`, `emit_config_response`, `emit_command_ack`. Clone freely.
- `CommandHandler` trait — your async fn receives `(command_id, action)` and
  returns `Result<Value, CommandError>`; the SDK packs it into a
  `command_ack` automatically, preserving `error_code`.
- `GatewayEvent` builder with the standard severity + category taxonomy
  (mirrors the edge's `events-and-alarms.md`).
- TLS:
  - Standard mode (system CA roots via `webpki-roots`)
  - Self-signed mode (requires `BILBYCAST_ALLOW_INSECURE=1` env var)
  - SHA-256 fingerprint pinning
- Credential persistence helpers (`PersistedCredentials`, `CredentialStore`).
- `GATEWAY_WS_PROTOCOL_VERSION` constant — currently `1`, matches the manager.

## Minimum viable gateway

```rust
use std::sync::Arc;
use async_trait::async_trait;
use serde_json::Value;
use bilbycast_gateway_sdk::{
    CommandError, CommandHandler, GatewayClient, GatewayConfig,
};

struct MyHandler;

#[async_trait]
impl CommandHandler for MyHandler {
    async fn handle_command(
        &self,
        _command_id: String,
        action: Value,
    ) -> Result<Value, CommandError> {
        match action.get("type").and_then(|t| t.as_str()) {
            Some("ping") => Ok(serde_json::json!({ "pong": true })),
            Some(other) => Err(CommandError::unknown_action(other)),
            None => Err(CommandError::validation("missing action.type")),
        }
    }

    async fn on_config_request(&self) -> Value {
        serde_json::json!({ "status": "hello" })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut cfg = GatewayConfig::minimal(
        "wss://manager.example.com:8443/ws/node",
        "my_device",
        env!("CARGO_PKG_VERSION"),
    );
    cfg.registration_token = Some("token-from-manager".into());

    let client = GatewayClient::connect(cfg, Arc::new(MyHandler)).await?;
    let emitter = client.emitter();

    // spawn your polling task with `emitter` here…

    client.run().await?;
    Ok(())
}
```

See [`docs/writing-a-gateway.md`](docs/writing-a-gateway.md) for the full walkthrough,
including how to integrate with `bilbycast-gateway-template` and how the Appear X
gateway uses this SDK as its WS client.

## Testing against a mock manager

The SDK ships a full mock-manager integration test in
[`tests/integration_mock_manager.rs`](tests/integration_mock_manager.rs). Copy it
as the starting point for your gateway's integration suite. The test exercises:

- register with a token → `register_ack` → persisted credentials
- reconnect with credentials → `auth_ok`
- manager-sent `command` → handler dispatch → `command_ack`
- SDK-emitted `stats` / `event` / `health` / `thumbnail` → server receives
- `get_config` command → SDK emits `config_response` + success ack
- `ping` → `pong`
- server-close → SDK reconnects automatically
- unknown manager message types are ignored (forward-compat)

## Running against a local manager

```bash
cargo build
# point your gateway's config.toml at wss://127.0.0.1:8443/ws/node
BILBYCAST_ALLOW_INSECURE=1 ./my-gateway --config config.toml
```

## Dependencies

Pure Rust, no OpenSSL. Pinned to match the Appear X reference gateway's
versions:

| crate | version |
|---|---|
| tokio | 1.52.1 |
| tokio-tungstenite | 0.29.0 (rustls-tls-webpki-roots) |
| rustls | 0.23.39 |
| webpki-roots | 1.0.7 |
| serde / serde_json | 1.0 |
| chrono | 0.4.44 |
| async-trait | 0.1.89 |
| tokio-util | 0.7.18 |
| futures-util | 0.3.32 |
| thiserror | 2.0 |
| url | 2.5.4 |
| bytes | 1.9 |
| sha2 | 0.10.8 |

## Licensing

Dual-licensed, same as bilbycast-edge:

- **AGPL-3.0-or-later** — see [`LICENSE`](LICENSE). Free for review,
  private deployment, and any use where you're comfortable with
  AGPL's source-disclosure obligations.
- **Commercial licence** — see [`LICENSE.commercial`](LICENSE.commercial).
  For OEMs, hardware integrators, and SaaS providers who need to
  embed this SDK without AGPL's copyleft. Contact
  `contact@bilbycast.com`.

Copyright (c) 2026 Softside Tech Pty Ltd.
