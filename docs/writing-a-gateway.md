# Writing a Gateway

A step-by-step guide to integrating a 3rd-party broadcast device into the
bilbycast ecosystem via a gateway sidecar.

> **Manager-side plugin first.** For the `DeviceDriver` that pairs with
> this gateway, see
> [`bilbycast-manager/docs/adding-a-device-type.md`](../../bilbycast-manager/docs/adding-a-device-type.md).
> That doc's Section B walks the full 3rd-party path end-to-end (gateway
> + driver). This doc covers the gateway half in depth.

## 1. What is a gateway?

Bilbycast manages three first-party device types directly:

- **bilbycast-edge** — media transport nodes
- **bilbycast-relay** — QUIC relay servers
- **bilbycast-appear-x-api-gateway** — reference 3rd-party sidecar

For any device we don't control natively, the integration model is a
**sidecar gateway**: a small Rust binary, deployed 1:1 with the vendor unit,
that speaks the manager's WebSocket protocol on one side and the vendor's
native API on the other.

### Cardinality: one sidecar per chassis

The SDK is designed around **one gateway process per vendor chassis**, even
when a single chassis hosts multiple cards / boards / slots. The Appear X
gateway is the reference: one sidecar polls every populated slot inside one
chassis at the chassis HTTPS endpoint defined by `[appear_x] address`.

Don't fan a single sidecar out across multiple chassis HTTPS endpoints. The
boundary that matters for monitoring and failure isolation is the chassis,
not the cards inside it. Reasons:

- **Failure isolation.** A flaky uplink to chassis A doesn't blind monitoring
  of chassis B if they have separate sidecars.
- **1 node = 1 chassis** in the manager. Every dashboard widget, detail
  page, audit row, and `cached_health` blob assumes one node = one
  vendor unit. Multi-target sidecars would need to register multiple
  "virtual" nodes to keep the manager's mental model intact.
- **`gateway_target` is single-valued.** The reachability sub-status the
  manager renders (third "Target down" dashboard state, Gateway Module
  detail header) carries one `target_address` and one `reachable` bool.
  Multi-target would require fanning that out into a per-target array,
  with cascading complications in the dashboard, event pipeline, and
  audit trail.
- **Polling state is naturally per-chassis.** Auth cookies, JSON-RPC
  sessions, alarm dedup, NMOS subscriptions — all of it is keyed on the
  chassis endpoint. Sharing across chassis would force the gateway to
  carry a per-target dispatcher with no real benefit.

If a future use case demands multi-chassis aggregation, that's a v2
protocol shape (`gateway_targets: Vec<GatewayTargetHealth>`) that ripples
into every consumer of `gateway_target`. Out of scope today.

```
bilbycast-manager  ←──── WSS (bilbycast gateway protocol) ────┐
                                                              │
                                          ┌───────────────────┴─────────────┐
                                          │   your-gateway (this SDK)        │
                                          ├─────────────────────────────────┤
                                          │  polling loop    command handler │
                                          ├────────┬──────────────┬─────────┤
                                          │        ▼              ▼         │
                                          └── vendor API (HTTP / JSON-RPC / SNMP / …)
                                                              │
                                                              ▼
                                                    [vendor device]
```

The gateway runs three concurrent loops:

1. **WS client** — handled entirely by this SDK's `GatewayClient`.
2. **Polling engine** — yours. Periodically reads the vendor device and
   emits `stats` / `health` / `event` envelopes via the `Emitter`.
3. **Command handler** — yours. Implements `CommandHandler` so the SDK
   can dispatch manager-originated commands to your vendor translation layer.

## 2. Minimum viable gateway — four files

See the companion crate `bilbycast-gateway-template/` for a runnable
skeleton. The minimum is:

- `Cargo.toml` — depends on `bilbycast-gateway-sdk` and `tokio`.
- `src/main.rs` — loads config, instantiates `GatewayClient`, spawns the
  polling task, awaits `client.run()`.
- `src/vendor.rs` — the vendor translation layer (polling + command mapping).
- `config.toml` — standard `[manager]` section plus your `[vendor]` section.

That's it. The SDK handles the WebSocket protocol, TLS, auth, reconnect,
heartbeats, and graceful shutdown.

## 3. The CommandHandler trait

```rust
#[async_trait]
pub trait CommandHandler: Send + Sync + 'static {
    async fn handle_command(
        &self,
        command_id: String,
        action: Value,
    ) -> Result<Value, CommandError>;

    async fn on_config_request(&self) -> Value { Value::Null }
}
```

### Dispatching on `action.type`

Commands from the manager land as `{ "type": "<action_name>", ...params }`.
Typical implementation:

```rust
async fn handle_command(
    &self,
    _command_id: String,
    action: Value,
) -> Result<Value, CommandError> {
    match action.get("type").and_then(|t| t.as_str()) {
        Some("get_inputs") => {
            let inputs = self.vendor.get_inputs().await
                .map_err(|e| CommandError::new("vendor_api_error", e.to_string()))?;
            Ok(inputs)
        }
        Some("set_input") => {
            let slot = action.get("slot").and_then(|v| v.as_u64())
                .ok_or_else(|| CommandError::validation("slot required"))?;
            self.vendor.set_input(slot as u8, &action).await
                .map_err(|e| CommandError::new("vendor_api_error", e.to_string()))?;
            Ok(Value::Null)
        }
        Some(other) => Err(CommandError::unknown_action(other)),
        None => Err(CommandError::validation("missing action.type")),
    }
}
```

`CommandError::code` rides on the `command_ack.error_code` field so the
manager UI can highlight the offending form field — use the same taxonomy
as the edge's unified error codes (`port_conflict`, `bind_failed`,
`validation_error`, `unsupported_codec`, `unknown_action`, etc.).

### `get_config`

The manager issues `{ "type": "get_config" }` to refresh its cached copy
of the node's state. The SDK intercepts this:

1. Calls `CommandHandler::on_config_request()` to get your snapshot.
2. Emits a `config_response` envelope with that snapshot.
3. Emits a successful `command_ack` for the original `get_config` command_id.

Your `on_config_request()` should assemble whatever the manager UI should
see as "the current configuration of this node" — typically a roll-up of
your polling engine's latest snapshots.

## 4. The polling engine

Your polling engine is independent of the SDK. Spawn it as a tokio task,
pass it an `Emitter`, and let it send whatever your vendor API yields:

```rust
let emitter = client.emitter();
let shutdown = client.shutdown_token();
let vendor = VendorClient::new(&cfg.vendor)?;

tokio::spawn(async move {
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tick.tick() => {}
        }
        match vendor.snapshot().await {
            Ok(snapshot) => {
                let _ = emitter.emit_stats(snapshot).await;
            }
            Err(e) => {
                let _ = emitter.emit_event(
                    GatewayEvent::major(
                        categories::VENDOR_API,
                        format!("Vendor API error: {e}"),
                    ),
                ).await;
            }
        }
    }
});

client.run().await?;
```

Events should use the standard taxonomy in
`bilbycast_gateway_sdk::events::categories`.

### Sidecars must not exit on target unreachability

A sidecar's target device — encoder, gateway chassis, mixer, whatever
— will be powered down, rebooted, taken offline for maintenance, or
moved between subnets over its lifetime. **The sidecar process must
ride through every one of those events without exiting.**

Concretely:

- **Polling loop**: the snippet above already does this — vendor API
  errors emit a `vendor_api_error` (or vendor-specific) event and the
  loop continues to the next tick. Don't `return Err(...)` out of the
  task on an HTTP timeout or refused TCP — that just kills polling
  and leaves the gateway silently degraded.
- **Startup capability discovery (if your vendor needs it)**: if your
  gateway runs vendor-specific discovery before steady-state polling
  (Appear X does this to learn which JSON-RPC interfaces a given
  firmware exposes), wrap that call in a retry loop with the same
  cadence the SDK uses for WS reconnect. Reuse `ReconnectBackoff`
  rather than rolling your own:

  ```rust
  use bilbycast_gateway_sdk::ReconnectBackoff;

  let backoff = ReconnectBackoff::default();
  let mut attempt: u32 = 0;
  let caps = loop {
      match vendor::discover(&client).await {
          Ok(c) => break c,
          Err(e) => {
              attempt = attempt.saturating_add(1);
              let delay = backoff.delay_for_attempt(attempt);
              warn!(
                  "Capability discovery failed (attempt {attempt}): {e:#}. \
                   Retrying in {} s",
                  delay.as_secs()
              );
              tokio::select! {
                  _ = tokio::time::sleep(delay) => {}
                  _ = tokio::signal::ctrl_c() => return Ok(()),
              }
          }
      }
  };
  ```

  Reference: `bilbycast-appear-x-api-gateway/src/main.rs` — the
  capability-discovery retry block in `main()`.

- **Reachability state**: once steady-state polling is running, drive
  `Emitter::emit_health_with_target` from your reachability tracker so
  the manager dashboard shows the third "Target down" amber state
  during outages. See the `GatewayTargetHealth` row in §9 for the
  exact field set, and `bilbycast-appear-x-api-gateway/src/appear_x/reachability.rs`
  for a worked implementation with a configurable failure threshold
  and dwell-gated `target_unreachable` / `target_recovered` events.

The only acceptable reasons for a sidecar to exit are: ctrl-c / SIGTERM
(graceful shutdown via `client.shutdown_token()`), genuinely
unrecoverable config errors (malformed `config.toml`, missing
required fields), or a panic / process-level fault. **"My target
device isn't responding right now"** is not on that list.

## 5. Config and credentials

A typical gateway's `config.toml`:

```toml
[manager]
urls = [
    "wss://manager.example.com:8443/ws/node",
]
registration_token = "paste-from-manager-add-node-flow"
credentials_file = "credentials.json"
accept_self_signed_cert = false

[vendor]
address = "192.168.1.100"
username = "admin"
password = "secret"
```

On first run, you load the `registration_token` into
`GatewayConfig.registration_token`. When the manager responds with
`register_ack`, the SDK stores the new `(node_id, node_secret)` in-process
and invokes the `on_register` callback (if registered) so you can persist
them to disk. On reconnect, load from disk, populate
`GatewayConfig.node_id` / `GatewayConfig.node_secret`, and leave
`registration_token = None`.

The SDK provides `PersistedCredentials` + `CredentialStore` helpers:

```rust
let store = CredentialStore::new("credentials.json");
let creds = store.load()?;
if let (Some(nid), Some(nsec)) = (creds.node_id, creds.node_secret) {
    cfg.node_id = Some(nid);
    cfg.node_secret = Some(nsec);
} else {
    cfg.registration_token = creds.registration_token.clone()
        .or(cfg.registration_token);
}

let mut client = GatewayClient::connect(cfg, handler).await?;
let store_for_cb = store.clone();
client.on_register(move |node_id, node_secret| {
    let creds = PersistedCredentials {
        node_id: Some(node_id.to_string()),
        node_secret: Some(node_secret.to_string()),
        registration_token: None,
    };
    let _ = store_for_cb.save(&creds);
});
```

## 6. Testing with a mock manager

`bilbycast-gateway-sdk/tests/integration_mock_manager.rs` shows how to
spin up a plaintext WS server locally, connect the SDK to it, and assert
on both directions of the wire. Copy it as the starting point for your
gateway's integration tests.

For tests you'll need to set `BILBYCAST_SDK_ALLOW_PLAINTEXT_WS=1` — the
SDK refuses plain `ws://` URLs in production.

## 7. Deployment — systemd example

```ini
[Unit]
Description=bilbycast gateway for Acme Encoder
After=network-online.target

[Service]
Type=simple
User=bilbycast
Group=bilbycast
WorkingDirectory=/var/lib/acme-gateway
ExecStart=/usr/local/bin/acme-gateway --config /etc/acme-gateway/config.toml
Restart=always
RestartSec=5
# BILBYCAST_ALLOW_INSECURE=1 only if you actually run with a self-signed cert
# Environment=BILBYCAST_ALLOW_INSECURE=1

# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/acme-gateway

[Install]
WantedBy=multi-user.target
```

## 8. Appear X gateway as the canonical example

`bilbycast-appear-x-api-gateway` is the reference consumer of this SDK.
See its `CLAUDE.md` and `src/` for a full real-world implementation:

- `config.rs` — TOML shape (`[manager]` / `[vendor]` / `[polling]`).
- `credentials.rs` — 0600-mode JSON credential persistence (pre-SDK; the
  SDK's `CredentialStore` helper is a drop-in replacement).
- `ws/client.rs` — WS client (pre-SDK; Phase 6 of the refactor swaps it
  for `bilbycast_gateway_sdk::GatewayClient`).
- `appear_x/polling.rs` — polling engine, emits `stats` / `health` /
  `event` envelopes.
- `appear_x/commands.rs` — command handler, translates manager commands
  to Appear X JSON-RPC calls.

When Phase 6 completes, all of `ws/` will be removed and the Appear X
binary will contain only vendor-specific code.

## 9. Frequently useful SDK bits

| Symbol | Purpose |
|---|---|
| `GatewayClient::connect` | Build the client from a validated config. |
| `GatewayClient::run` | Enter the connect/reconnect loop (blocks until shutdown). |
| `GatewayClient::emitter()` | Get an emitter for stats / events / health. |
| `GatewayClient::shutdown_token()` | Cancel to trigger graceful shutdown. |
| `GatewayClient::on_register(cb)` | Callback fired on first-time registration. |
| `Emitter::emit_stats` / `emit_event` / `emit_health` | The hot-path outputs. |
| `Emitter::emit_health_with_target` | Health heartbeat plus typed `gateway_target` sub-status (target reachability, gateway host / egress IP). Drives the manager's third "Target down" amber dashboard state and the per-driver Gateway Module header. |
| `GatewayTargetHealth { reachable, target_address, gateway_host, gateway_egress_ip, last_successful_poll_unix, last_error_code, consecutive_failures }` | Sub-status for the above. `last_error_code` is a fixed enum (`http_timeout` \| `tcp_refused` \| `tls_handshake` \| `auth_rejected` \| `rpc_protocol_error` \| `other`) — never the verbose vendor error string. |
| `Emitter::emit_thumbnail` | Per-flow JPEG thumbnail (base64-encoded). |
| `GatewayEvent::critical("port_conflict", "…").with_error_code("…")` | Event builder. |
| `CommandError::unknown_action("my_action")` | Standard `error_code = unknown_action`. |
| `CommandError::validation("slot required")` | Standard `error_code = validation_error`. |
| `GATEWAY_WS_PROTOCOL_VERSION` | `1` — matches manager's `WS_PROTOCOL_VERSION`. |

## 10. What this SDK deliberately does NOT do

- **Client-side event rate limiting** (the manager's 1000/min per-node
  limiter). The Appear X gateway implements a 950/min self-gate in
  `ws/event_gate.rs`; once Phase 6 extracts it from the Appear X tree,
  a future SDK release will expose it as an opt-in helper. For now,
  implement it in your gateway if you expect alarm-storm scenarios.
- **Config-template enforcement**, managed-flow push-status tracking,
  tunnel reconciliation, etc. Those are manager-side concepts driven by
  the `DeviceDriver` implementation in `manager-core/src/drivers/`, not
  by the gateway itself.
- **TOML parsing.** Consumers own their `config.toml` schema. The SDK's
  `GatewayConfig` is serde-compatible, so you can embed it verbatim
  under a `[manager]` section.
- **Vendor HTTP client.** Bring your own `reqwest` / `hyper` / etc.
