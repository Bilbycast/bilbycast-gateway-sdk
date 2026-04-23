# Changelog

All notable changes to `bilbycast-gateway-sdk` are recorded here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-04-23

### Added
- Initial public release.
- `GatewayClient`: WSS connect, auth (register OR reconnect via `node_id` + `node_secret`), heartbeat, exponential reconnect backoff (1/2/5/10/30s), graceful shutdown via `CancellationToken`.
- `Emitter`: stats, events (typed `GatewayEvent` with severity taxonomy), health, thumbnails, `config_response`, `command_ack` with `error_code`.
- `CommandHandler` trait for vendor command dispatch; string-keyed with catch-all arms for backwards-compatible protocol evolution.
- Standard envelope shape `{type, timestamp, payload}` matching `bilbycast-manager` WS protocol v1.
- Multi-URL failover (`manager.urls[]`, up to 16).
- Cert pinning via SHA-256 fingerprint; `accept_self_signed_cert` gated by `BILBYCAST_ALLOW_INSECURE=1`.
- `CredentialStore` helper (0600 JSON) + `on_register` callback for credential persistence.
- Mock-manager integration test harness.
- `bilbycast-gateway-template` sibling crate: runnable starter for new vendor gateways.

### Known gaps (tracked for 0.2.x)
- No built-in event rate-limiter (gateways self-gate — Appear X ships its own 950/min `EventGate`).
- No on-reconnect callback (callers poll `current_credentials()` today).
- No `GatewayConfig::from_persisted()` helper.
