# Changelog

All notable changes to `bilbycast-gateway-sdk` are recorded here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Shared canonical-manifest builder** at `scripts/build-manifest.sh`.
  Generalises the edge's per-binary script with `<binary_prefix>` +
  `<device_type>` arguments, so every vendor sidecar uses the same
  canonicalisation logic for the Sigstore-signed `manifest.json`.
- `writing-a-gateway.md` §9a (Remote upgrade) rewritten as a complete
  step-by-step wiring guide pointing at `bilbycast-appear-x-api-gateway`
  as the canonical reference implementation.
- `writing-a-gateway.md` §9b (Packaging — install bundle + systemd unit)
  with the canonical `/opt/bilbycast/<name>-gateway/` layout and
  service-account conventions.

## [0.7.1]

### Added
- Shared release-pipeline + docs + sidecar reference wiring that make the
  `upgrade::*` machinery usable end-to-end (the shared canonical-manifest
  builder and the `writing-a-gateway.md` §9a/§9b guides previously tracked
  under Unreleased land here).

## [0.6.0]

### Added
- **`upgrade::*` machinery** — the manager-driven, Sigstore-keyless remote
  binary upgrade stack mirroring bilbycast-edge: manifest fetch + bundle
  verification against a per-binary `ALLOWED_SIGNERS` allowlist, SHA-256
  pinning, boot watchdog. Parameterised by `UpgradeProfile { repo,
  binary_name, device_type, allowed_signers }` so each vendor sidecar
  accepts only its own release workflow's signatures.

## [0.2.0] – [0.5.0]

Intermediate releases between the initial 0.1.0 and the upgrade machinery
in 0.6.0. These versions were not captured here at the time; consult the
git history for per-release detail.

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
