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

## 4a. Participating in the Flows + Topology views

If your target device has the concept of a "flow" — a routed
input → output signal path — you can have it appear on the manager's
Flows page (`/flows`) and as edges on the Topology page (`/topology`).
Three things must all be true together:

1. **The gateway emits `stats.flows[]`** in every `stats` envelope (this
   section).
2. **The manager-side driver claims `ManagedEntityKind::Flow`** in
   `managed_entity_kinds()`.
3. **The manager-side driver lists `UiSection::Flows`** in
   `ui_capabilities().shows_sections`.

`Tunnels` follows the parallel contract: `tunnels[]` on `stats` ↔
`ManagedEntityKind::Tunnel` ↔ `UiSection::Tunnels`.

Steps 2 and 3 are two lines in the manager-side `lib.rs` — see
[`bilbycast-manager/docs/adding-a-device-type.md`](https://github.com/bilbycast/bilbycast-manager/blob/main/docs/adding-a-device-type.md)
section A.2.6a. The manager's `DriverRegistry::register` panics at
startup if you wire up only one half of the pair, so half-wired drivers
fail loud rather than rendering an empty Flows tab forever.

The minimum element shape the manager UI reads off each `stats.flows[]`
entry — extra fields are ignored, so you can attach whatever else you
want:

```jsonc
{
  "flow_id": "uuid",                  // required — keys the row, dedups across snapshots
  "flow_name": "Human label",         // optional — falls back to flow_id in the UI
  "active_input_id": "uuid",          // optional — preferred over the cached config's input_id
  "input": {                          // required for topology signal-path edges
    "input_type": "srt",              // routed through the JS protocolColor() table
    "mode": "caller|listener|rendezvous",
    "remote_addr": "host:port",       // when mode = caller / rendezvous
    "local_addr":  "host:port"        // when mode = listener
  },
  "outputs": [{                       // optional but expected for any flow with egress
    "output_id": "uuid",
    "output_name": "Human label",
    "output_type": "udp|rtp|srt|rist|rtmp|hls|cmaf|webrtc|...",
    "mode": "caller|listener|rendezvous",
    "dest_addr": "host:port"          // or one of: dest_url, ingest_url, whip_url, remote_addr, local_addr
  }]
}
```

Worked snippet inside your polling loop:

```rust
let flows = serde_json::json!([{
    "flow_id":   stream.uuid,
    "flow_name": stream.label,
    "input":     {
        "input_type": "srt",
        "mode": "caller",
        "remote_addr": stream.src,
    },
    "outputs": [{
        "output_id":   stream.uuid,
        "output_type": "udp",
        "mode":        "listener",
        "local_addr":  stream.dst,
    }]
}]);

emitter.emit_stats(serde_json::json!({
    "flows":         flows,
    "uptime_secs":   uptime,
    "active_flows":  active,
    "total_flows":   total,
})).await?;
```

**`flow_id` must be stable across polls.** If you mint a fresh UUID on
every snapshot, the UI treats every poll as a new row and the Flows
page churns. Derive it from a vendor-stable identifier (a session id, a
slot index, whatever the chassis itself uses), not from the current
`Instant`.

If your target device has no native flow concept (a chassis-level
multiplexer with per-port IP I/O, a glue device, a config-only
appliance), it's fine — and recommended — to leave `stats.flows[]`
absent and skip steps 2–3 on the driver side. The node still appears on
the Topology page when the driver sets `list_in_topology = true`; it
just won't have signal-path edges drawn through it. **Appear X is in
this state by design** (see §8).

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

For tests you'll need **both** halves of the plaintext opt-in — the SDK
refuses plain `ws://` URLs otherwise:

```rust
let mut cfg = GatewayConfig::minimal(url, "my_device", "0.1.0");
cfg.allow_plaintext_ws = true;                       // config half
// and in the environment:
// BILBYCAST_SDK_ALLOW_PLAINTEXT_WS=1                // env half
```

Either alone is refused, and the error says which half is missing. Two keys
rather than one because this was environment-only, which meant a single
variable set anywhere — a unit file, a shell profile, a container spec —
silently downgraded a sidecar's manager link from TLS to cleartext while it
was carrying its node secret. The self-signed-certificate escape hatch
(`accept_self_signed_cert` + `BILBYCAST_ALLOW_INSECURE=1`) already worked
this way; this now matches it.

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
- `appear_x/polling.rs` — polling engine, emits `stats` / `health` /
  `event` envelopes.
- `appear_x/commands.rs` — command handler, translates manager commands
  to Appear X JSON-RPC calls.

The migration onto the SDK is complete: the Appear X gateway no longer
carries its own WS client or credential store. `src/main.rs` builds a
`bilbycast_gateway_sdk::GatewayClient` via `GatewayClient::connect`, and
credential persistence uses the SDK's `CredentialStore` helper. There is
no `src/ws/` tree — the binary now contains only vendor-specific code.

The Appear X gateway deliberately does **not** emit `stats.flows[]`
(see §4a) — the chassis is a multiplexer / coder mesh with IP inputs
and outputs, not a one-input-many-outputs signal pipeline that maps
naturally onto a bilbycast "flow". `AppearXDriver` therefore claims
neither `UiSection::Flows` nor `ManagedEntityKind::Flow`, and the node
appears as a Topology dot with no signal-path edges. This is a product
decision, not a missing feature.

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
| `GATEWAY_WS_PROTOCOL_VERSION` | `1` — deliberately pinned; manager is at `WS_PROTOCOL_VERSION = 4`, so they intentionally do **not** match. Mismatch is tolerated (manager only warns). |

## 9a. Remote upgrade (Sigstore-keyless, parameterised)

Every gateway sidecar can opt into manager-driven binary upgrades. The
SDK ships the same Sigstore-verified machinery the edge uses, just
parameterised by your repo + binary name + identity allowlist. See
[`bilbycast-edge/docs/upgrade.md`](../../bilbycast-edge/docs/upgrade.md)
for the operator runbook and
[`bilbycast-edge/docs/security.md`](../../bilbycast-edge/docs/security.md)
for the trust model.

> **Reference implementation**:
> [`bilbycast-appear-x-api-gateway`](../../bilbycast-appear-x-api-gateway/)
> wires every step below. Concrete files to copy from:
> - `src/upgrade_profile.rs` — `ALLOWED_SIGNERS` + `UpgradeProfile` const
> - `src/main.rs` — boot watchdog, coordinator, event-forwarder task,
>   periodic watchdog, healthy-beat recorder, capability advertisement
> - `src/appear_x/commands.rs::dispatch_upgrade_binary` — the WS arm
> - `packaging/install-appear-x-gateway.sh` + the matching systemd unit
> - `.github/workflows/nightly-release.yml` — Sigstore signing, manifest
>   builder, paranoid self-verify, release publish

### Wire-up

1. **Pin your repo + workflow path** in a `&'static [AllowedSigner]`
   constant. Never reuse another binary's allowlist:
   ```rust
   use bilbycast_gateway_sdk::upgrade::{AllowedSigner, UpgradeProfile};

   pub const MY_SIGNERS: &[AllowedSigner] = &[
       AllowedSigner {
           issuer: "https://token.actions.githubusercontent.com",
           repo: "https://github.com/Bilbycast/<your-repo>",
           ref_pattern: "refs/tags/v*",
           workflow: "https://github.com/Bilbycast/<your-repo>/.github/workflows/nightly-release.yml",
       },
   ];

   pub const PROFILE: UpgradeProfile = UpgradeProfile {
       repo: "Bilbycast/<your-repo>",
       binary_name: "<your-binary>",
       device_type: "<your-device-type>",  // matches manifest.json
       allowed_signers: MY_SIGNERS,
   };
   ```

2. **Open an event channel** and **run the boot watchdog before any
   other init**, so a crash-loop on a freshly-staged binary triggers
   the symlink revert + `exit(1)` on the (`max_boot_attempts` + 1)th
   boot:
   ```rust
   use bilbycast_gateway_sdk::upgrade::{
       run_boot_watchdog, UpgradeCoordinator, UpgradeEvent, WatchdogOutcome,
   };

   let (upgrade_event_tx, upgrade_event_rx) =
       tokio::sync::mpsc::channel::<UpgradeEvent>(64);

   match run_boot_watchdog(cfg.upgrade.as_ref(), &upgrade_event_tx) {
       Ok(WatchdogOutcome::RolledBack { from_version, to_version }) => {
           tracing::warn!("rolled back {from_version} → {to_version}");
       }
       Ok(_) => {}
       Err(e) => tracing::warn!("boot watchdog error: {e:#}"),
   }
   ```

3. **Construct the `UpgradeCoordinator`** before connecting the WS so
   it's available to your command handler from the first frame:
   ```rust
   let upgrade_coord = cfg.upgrade.as_ref().map(|cfg| {
       std::sync::Arc::new(UpgradeCoordinator::new(
           PROFILE,
           cfg.clone(),
           upgrade_event_tx.clone(),
           env!("CARGO_PKG_VERSION").to_string(),
       ))
   });
   ```

4. **Spawn an event-forwarder task** that drains the channel into your
   `Emitter`, so upgrade lifecycle events ride the same WS path as
   every other vendor event:
   ```rust
   tokio::spawn(async move {
       while let Some(ev) = upgrade_event_rx.recv().await {
           let severity = match ev.severity {
               "critical" => EventSeverity::Critical,
               "major" => EventSeverity::Major,
               _ => EventSeverity::Info,
           };
           let event = GatewayEvent::new(severity, "upgrade", ev.message)
               .with_error_code(ev.error_code);
           let _ = emitter.emit_event(event).await;
       }
   });
   ```

5. **Add an `upgrade_binary` arm** to your `CommandHandler::handle_command`
   that calls `coord.stage(version, channel, target_arch, variant)`
   then schedules `std::process::exit(0)` after a short drain so
   systemd respawns into the new binary via the `current/` symlink.
   See `bilbycast-appear-x-api-gateway::commands::dispatch_upgrade_binary`
   for a copy-paste implementation. **Important**: route the arm even
   when your real handler hasn't initialised yet (e.g. while you're
   still discovering the vendor device's capabilities). Sidecar
   self-upgrade must work when the target is unreachable, otherwise
   you can't ship a fix to a sidecar whose target is offline.

6. **Spawn the periodic watchdog** so the binary transitions
   `pending_health → stable` after the configured
   `boot_health_window_secs`:
   ```rust
   if let Some(ref up_cfg) = cfg.upgrade {
       if up_cfg.enabled {
           tokio::spawn(upgrade::watchdog::run_watchdog_periodic(
               up_cfg.install_root.clone(),
               up_cfg.clone(),
               upgrade_event_tx.clone(),
               shutdown.clone(),
           ));
       }
   }
   ```

7. **Stamp `last_health_at`** from a small ticker (or, better, from
   your manager-auth callback like
   [`bilbycast-edge/src/manager/client.rs`](../../bilbycast-edge/src/manager/client.rs)).
   The periodic watchdog uses this timestamp + a 60 s freshness
   window to decide whether to promote.

8. **Advertise the `"upgrade"` capability** on every health envelope
   so the manager UI lights up the per-node Upgrade button. Advertise
   it unconditionally — the SDK upgrade module is always compiled in,
   mirroring the edge's baseline. When the operator hasn't wired
   `[upgrade]` in the gateway TOML, `dispatch_upgrade_binary` safely
   refuses with `upgrade_disabled` and a pointer at the missing
   config; the button stays visible so operators can discover the
   feature instead of having to first edit the TOML to make it appear.

### Release pipeline (GitHub Actions)

The shared canonical-manifest builder lives at
[`bilbycast-gateway-sdk/scripts/build-manifest.sh`](../scripts/build-manifest.sh).
It takes `<binary_prefix>` + `<device_type>` arguments so every vendor
sidecar uses the same canonicalisation logic. Your release workflow
should:

1. Build per-arch tarballs containing binary + LICENSE + README +
   `packaging/` + `config/`. Naming convention:
   `<binary_prefix>-<arch>-linux.tar.gz`. Versionless asset names so
   `/releases/latest/download/<asset>` always resolves.

2. Check out the SDK with `path: bilbycast-gateway-sdk` and call
   `bilbycast-gateway-sdk/scripts/build-manifest.sh` to produce
   `manifest.json`.

3. Sign the manifest with `cosign sign-blob --bundle` (Sigstore
   keyless — no long-lived signing key). Add `id-token: write` to the
   workflow's `permissions:` block.

4. **Self-verify** the signature with `cosign verify-blob` against
   your `ALLOWED_SIGNERS` regex BEFORE publishing the release. This
   catches any mismatch between the workflow path and the allowlist
   here, not in production rollback territory.

5. Add `manifest.json`, `manifest.sig.bundle`, and the standalone
   install / uninstall / service unit files to the release alongside
   the per-arch tarballs.

The SDK uses the same Sigstore Fulcio + Rekor pipeline as the edge —
sidecars piggyback on the existing trust roots, so vendors don't
manage signing keys.

## 9b. Packaging — install bundle + systemd unit

Pair a curl-pipe-bash installer with a hardened systemd unit so
operators can stand up your sidecar in one shot. The Appear X gateway
ships the canonical pattern under
[`bilbycast-appear-x-api-gateway/packaging/`](../../bilbycast-appear-x-api-gateway/packaging/);
copy + adapt:

| File | Purpose |
|------|---------|
| `install-<name>-gateway.sh` | curl-pipe-bash installer — downloads + verifies signed manifest, lays out `/opt/bilbycast/<name>-gateway/`, writes initial config.toml, installs systemd unit, polls for service health |
| `uninstall-<name>-gateway.sh` | removes service, install root, data root; preserves config unless `--purge-config`; preserves `bilbycast-gateway` user when other gateways are still installed |
| `bilbycast-<name>-gateway.service` | hardened systemd unit (`ProtectSystem=strict`, no `CapabilityBoundingSet`, no `AmbientCapabilities`, no `/dev/dri`/`/dev/snd` device allow-list) |
| `bilbycast-<name>-gateway.sysusers` | `systemd-sysusers` config for the service account |

Conventions:

- **Install root**: `/opt/bilbycast/<name>-gateway/` so multiple gateways
  on one host don't collide. Mirrors the edge's `/opt/bilbycast/edge/`
  layout — `versions/<v>/`, `current` symlink, `state.json`,
  `config.toml`, `credentials.json`.
- **Data root**: `/var/lib/bilbycast/<name>-gateway/` for any state
  that doesn't fit in the install root.
- **Service account**: `bilbycast-gateway` (shared across vendor
  sidecars on a host, since they all do the same kind of thing — talk
  to the manager + a vendor device).
- **Systemd unit**: `ExecStart` resolves through `current/` so a
  successful staged upgrade lands the moment the old binary exits.
  Use `Restart=always` paired with `StartLimitBurst` so a crash-loop
  caps out and the boot watchdog can drive the rollback.
- **Env file**: `/etc/bilbycast/<name>-gateway.env` for `RUST_LOG` etc.
  so operators can tune log verbosity without editing the unit.

> **Tip**: keep your installer's `--manager wss://...
> --registration-token <tok> --<vendor-flags>` argument shape parallel
> to the Appear X installer. Operators standing up a fleet of mixed
> sidecars get familiar muscle memory.

## 10. What this SDK deliberately does NOT do

- **Client-side event rate limiting** (the manager's 1000/min per-node
  limiter). The Appear X gateway implements a 950/min self-gate in
  `src/event_gate.rs`; a future SDK release may lift it into the SDK as
  an opt-in helper. For now, implement it in your gateway if you expect
  alarm-storm scenarios.
- **Config-template enforcement**, managed-flow push-status tracking,
  tunnel reconciliation, etc. Those are manager-side concepts driven by
  the `DeviceDriver` implementation in `manager-core/src/drivers/`, not
  by the gateway itself.
- **TOML parsing.** Consumers own their `config.toml` schema. The SDK's
  `GatewayConfig` is serde-compatible, so you can embed it verbatim
  under a `[manager]` section.
- **Vendor HTTP client.** Bring your own `reqwest` / `hyper` / etc.
