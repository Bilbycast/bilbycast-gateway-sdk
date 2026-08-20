# Changelog

All notable changes to `bilbycast-gateway-sdk` are recorded here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Not yet cut into a version: the crate is still `version = "0.9.0"`, which was
set before the fixes below landed. A consumer pinning `0.9.0` therefore cannot
tell from the version alone whether it has the working checks or the no-ops —
pin the git revision until the next bump.

### Security

- **Sigstore verification of the upgrade manifest was a no-op and is now
  implemented.** `verify_cert_and_rekor` loaded the trust root and discarded
  it, leaving only a signature check against the public key embedded in the
  certificate the *upgrade server* supplied — and the `ALLOWED_SIGNERS`
  identity allowlist read its issuer / repo / ref OID extensions out of that
  same unauthenticated certificate. Minting a keypair, self-signing a
  certificate carrying the allowlisted strings and signing your own manifest
  passed every check: arbitrary code execution as the gateway service user,
  which is precisely the actor Sigstore keyless is deployed to defeat. The
  edge's working implementation is now ported over — Fulcio chain validation,
  the leaf's validity window checked against the Rekor integrated time, the
  Rekor body bound to *this* manifest and *this* certificate, and the Rekor
  SET signature. Rekor inclusion is mandatory; a bundle without it is refused.
- **`pem_to_der` now accepts the encoding `cosign` actually writes.**
  `cosign sign-blob --bundle` emits the `cert` field as base64-of-PEM, and the
  raw-PEM-only reader rejected every genuine bundle before a signature was
  checked. Both encodings are accepted; a corrupt PEM block is still an error
  rather than a silent base64 retry.
- **The pinned-certificate TLS verifier never checked CertificateVerify.**
  `verify_tls12_signature` and `verify_tls13_signature` returned
  `HandshakeSignatureValid::assertion()` without looking. Certificates are
  public, so an on-path attacker could replay the real manager's leaf: chain
  validation passes, the fingerprint pin matches, and the one step that proves
  possession of the private key was never evaluated — after which the sidecar
  sends its node secret to the attacker. `PinnedCertVerifier` now holds a
  `WebPkiServerVerifier`, built once in `build_pinned_config`, and delegates
  chain validation, both signature callbacks and `supported_verify_schemes` to
  it, keeping only the fingerprint comparison as this crate's own logic.
  Delegation rather than a hand-rolled check because correct verification is
  scheme-dependent and TLS 1.3 has curve-binding semantics rustls does not
  enforce for you. `InsecureCertVerifier` is untouched — asserting everything
  is the intended behaviour of the double-keyed `accept_self_signed_cert`
  escape hatch.

## [0.9.0]

### Changed — SOURCE BREAKING

- **`GatewayConfig` gained a required field, `allow_plaintext_ws: bool`.** The
  struct has all-public fields, no `#[non_exhaustive]` and no `Default`, so
  every exhaustive struct literal must add it. Both in-repo consumers
  (`bilbycast-appear-x-api-gateway`, `bilbycast-gateway-template`) set it to
  `false`; a production sidecar carrying a node secret should do the same.

  The break is deliberate rather than accidental: the field exists so a
  sidecar has to *consent in its own configuration* to plaintext, and a
  `Default` would have let the consent be skipped silently — which is the
  defect being fixed. Migration is one line:

  ```rust
  let cfg = GatewayConfig {
      // …
      allow_plaintext_ws: false,
      // …
  };
  ```

### Security

- **Plaintext `ws://` to the manager now requires BOTH the config field
  `allow_plaintext_ws = true` AND `BILBYCAST_SDK_ALLOW_PLAINTEXT_WS=1`.**
  Previously the environment variable alone was enough, so one variable set
  anywhere — a unit file, a shell profile, a container spec — silently
  downgraded a vendor sidecar's manager link from TLS to cleartext while it
  was carrying its node secret, with nothing in the sidecar's own
  configuration recording or consenting to it. This now matches the shape of
  the sibling escape hatch (`accept_self_signed_cert` +
  `BILBYCAST_ALLOW_INSECURE=1`). Either key alone is refused, and the error
  names which half is missing.

### Added

- **CI** (`.github/workflows/ci.yml`) — check, test and `clippy -D warnings`,
  plus a `consumers` job that builds `bilbycast-gateway-template` and
  `bilbycast-appear-x-api-gateway` against the SDK checkout under test. The
  crate previously had no automation of any kind, which is how the
  source-breaking change above reached `main` with both consumers left
  uncompilable.

## [0.8.0]

### Changed
- **Dependency currency sweep.** `tokio-tungstenite` 0.29 → 0.30 (matches
  bilbycast-relay, which was already there) and `base64` 0.22 → 0.23. Both
  are drop-in at every call site in this crate.
- `sigstore` 0.13 → 0.14, which pulls `tough` 0.22 and clears
  RUSTSEC GHSA-4v58-8p28-2rq3 / GHSA-8m7c-8m39-rv4x.
- **`x509-cert` deliberately held at 0.2** even though 0.3.0 is published:
  `sigstore` 0.14 still requires `^0.2`, and bumping ours alone resolves both
  majors into the graph — a second X.509 parser in the release-signature
  verification path for no functional gain. Move it in lockstep with sigstore.

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
