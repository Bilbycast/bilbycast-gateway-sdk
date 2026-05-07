// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Remote binary upgrade machinery for gateway sidecars.
//!
//! Mirrors `bilbycast-edge/src/upgrade/` but parameterised by
//! [`UpgradeProfile`] so a single SDK build serves every vendor
//! gateway. Each sidecar pins its own GitHub repo, binary name,
//! and `ALLOWED_SIGNERS` identity allowlist:
//!
//! ```no_run
//! use bilbycast_gateway_sdk::upgrade::{
//!     AllowedSigner, UpgradeConfig, UpgradeCoordinator, UpgradeProfile,
//! };
//!
//! const APPEAR_X_SIGNERS: &[AllowedSigner] = &[
//!     AllowedSigner {
//!         issuer: "https://token.actions.githubusercontent.com",
//!         repo: "https://github.com/Bilbycast/bilbycast-appear-x-api-gateway",
//!         ref_pattern: "refs/tags/v*",
//!         workflow: "https://github.com/Bilbycast/bilbycast-appear-x-api-gateway/.github/workflows/nightly-release.yml",
//!     },
//! ];
//!
//! # async fn run() -> anyhow::Result<()> {
//! let profile = UpgradeProfile {
//!     repo: "Bilbycast/bilbycast-appear-x-api-gateway",
//!     binary_name: "bilbycast-appear-x-api-gateway",
//!     device_type: "appear-x-gateway",
//!     allowed_signers: APPEAR_X_SIGNERS,
//! };
//! let cfg = UpgradeConfig::default(); // or load from gateway TOML
//! let (events_tx, _events_rx) = tokio::sync::mpsc::channel(64);
//! let coord = UpgradeCoordinator::new(profile, cfg, events_tx, env!("CARGO_PKG_VERSION").into());
//! let _ = coord; // pass through to the SDK's CommandHandler dispatch loop
//! # Ok(()) }
//! ```
//!
//! Vendors call [`UpgradeCoordinator::stage`] from their
//! `CommandHandler::handle_command` implementation when they receive
//! a `command { type: "upgrade_binary", ... }` envelope. On success
//! the gateway must exit (`std::process::exit(0)`) so systemd
//! respawns into the new binary via the `current/` symlink. The
//! [`run_boot_watchdog`] entry point should be called from `main`
//! before any other init so a crash-loop on the new binary
//! auto-rolls-back.

#![allow(dead_code)]

pub mod apply;
pub mod download;
pub mod manifest;
pub mod state;
pub mod trust;
pub mod verify;
pub mod watchdog;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

pub use manifest::{Manifest, ManifestArtefact};
pub use state::{InstallState, UpgradeStatus};
pub use trust::AllowedSigner;
pub use watchdog::{run_boot_watchdog, WatchdogOutcome};

/// Identity card for one downstream binary. Each gateway provides its
/// own `&'static UpgradeProfile` constant — there is no global
/// fallback.
#[derive(Debug, Clone, Copy)]
pub struct UpgradeProfile {
    /// `Bilbycast/<repo>` GitHub repo. Drives the deterministic URL
    /// the edge / gateway constructs at staging time. The host part
    /// is enforced separately via the URL whitelist.
    pub repo: &'static str,
    /// Filename of the binary inside the release tarball
    /// (e.g. `bilbycast-appear-x-api-gateway`).
    pub binary_name: &'static str,
    /// `device_type` string the manifest must declare (matches the
    /// driver name in `bilbycast-manager`).
    pub device_type: &'static str,
    /// Identity allowlist for Sigstore cert verification. Every entry
    /// pins (issuer, repo, ref_pattern, workflow). One mismatch and
    /// the staging is rejected with `upgrade_identity_not_allowed`.
    pub allowed_signers: &'static [AllowedSigner],
}

/// Operator-controlled upgrade policy. Mirrors
/// `bilbycast-edge/src/config/models.rs::UpgradeConfig`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_allowed_channels")]
    pub allowed_channels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_version: Option<String>,
    #[serde(default = "default_rollback_grace")]
    pub rollback_grace: u32,
    #[serde(default = "default_install_root")]
    pub install_root: PathBuf,
    #[serde(default = "default_boot_health_window_secs")]
    pub boot_health_window_secs: u32,
    #[serde(default = "default_max_boot_attempts")]
    pub max_boot_attempts: u32,
    #[serde(default)]
    pub manual_only: bool,
}

impl Default for UpgradeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_channels: default_allowed_channels(),
            min_version: None,
            rollback_grace: default_rollback_grace(),
            install_root: default_install_root(),
            boot_health_window_secs: default_boot_health_window_secs(),
            max_boot_attempts: default_max_boot_attempts(),
            manual_only: false,
        }
    }
}

fn default_allowed_channels() -> Vec<String> {
    vec!["stable".to_string()]
}
fn default_rollback_grace() -> u32 { 1 }
fn default_install_root() -> PathBuf {
    PathBuf::from("/opt/bilbycast/gateway")
}
fn default_boot_health_window_secs() -> u32 { 120 }
fn default_max_boot_attempts() -> u32 { 3 }

pub mod error_codes {
    pub const UPGRADE_DISABLED: &str = "upgrade_disabled";
    pub const UPGRADE_CHANNEL_NOT_ALLOWED: &str = "upgrade_channel_not_allowed";
    pub const UPGRADE_VERSION_TOO_OLD: &str = "upgrade_version_too_old";
    pub const UPGRADE_VERSION_INVALID: &str = "upgrade_version_invalid";
    pub const UPGRADE_SEQUENCE_TOO_OLD: &str = "upgrade_sequence_too_old";
    pub const UPGRADE_SIGNATURE_INVALID: &str = "upgrade_signature_invalid";
    pub const UPGRADE_IDENTITY_NOT_ALLOWED: &str = "upgrade_identity_not_allowed";
    pub const UPGRADE_REKOR_INVALID: &str = "upgrade_rekor_invalid";
    pub const UPGRADE_IN_PROGRESS: &str = "upgrade_in_progress";
    pub const UPGRADE_URL_INVALID: &str = "upgrade_url_invalid";
    pub const UPGRADE_CHECKSUM_MISMATCH: &str = "upgrade_checksum_mismatch";
    pub const UPGRADE_EXTRACT_FAILED: &str = "upgrade_extract_failed";
    pub const UPGRADE_DISK_FULL: &str = "upgrade_disk_full";
    pub const UPGRADE_NETWORK_ERROR: &str = "upgrade_network_error";
    pub const UPGRADE_MANIFEST_INVALID: &str = "upgrade_manifest_invalid";
    pub const UPGRADE_VARIANT_MISMATCH: &str = "upgrade_variant_mismatch";
    pub const UPGRADE_ARCH_MISMATCH: &str = "upgrade_arch_mismatch";
}

#[derive(Debug, Clone)]
pub struct UpgradeError {
    pub message: String,
    pub code: &'static str,
}

impl UpgradeError {
    pub fn new(code: &'static str, msg: impl Into<String>) -> Self {
        Self { message: msg.into(), code }
    }
}

impl std::fmt::Display for UpgradeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for UpgradeError {}

#[derive(Debug, Clone)]
pub struct StagedUpgrade {
    pub from_version: String,
    pub to_version: String,
    pub channel: String,
    pub variant: String,
    pub arch: String,
    pub install_dir: PathBuf,
}

/// Lifecycle event emitted to the SDK's `Emitter` so the gateway's
/// existing manager-event pipeline carries the upgrade events without
/// any extra plumbing.
#[derive(Debug, Clone, Serialize)]
pub struct UpgradeEvent {
    pub severity: &'static str,
    pub error_code: &'static str,
    pub from_version: Option<String>,
    pub to_version: Option<String>,
    pub channel: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

/// Process-wide coordinator. Cheap to clone; the single-flight guard
/// is `Arc<Mutex<bool>>`.
#[derive(Clone)]
pub struct UpgradeCoordinator {
    profile: UpgradeProfile,
    config: Arc<tokio::sync::RwLock<UpgradeConfig>>,
    events: tokio::sync::mpsc::Sender<UpgradeEvent>,
    current_version: String,
    in_flight: Arc<Mutex<bool>>,
}

impl UpgradeCoordinator {
    pub fn new(
        profile: UpgradeProfile,
        config: UpgradeConfig,
        events: tokio::sync::mpsc::Sender<UpgradeEvent>,
        current_version: String,
    ) -> Self {
        Self {
            profile,
            config: Arc::new(tokio::sync::RwLock::new(config)),
            events,
            current_version,
            in_flight: Arc::new(Mutex::new(false)),
        }
    }

    pub fn profile(&self) -> &UpgradeProfile { &self.profile }
    pub async fn enabled(&self) -> bool { self.config.read().await.enabled }
    pub async fn set_config(&self, cfg: UpgradeConfig) {
        let mut g = self.config.write().await;
        *g = cfg;
    }

    pub async fn stage(
        &self,
        version: &str,
        channel: &str,
        target_arch: Option<&str>,
        variant: Option<&str>,
    ) -> Result<StagedUpgrade, UpgradeError> {
        let mut g = match self.in_flight.try_lock() {
            Ok(g) => g,
            Err(_) => {
                return Err(UpgradeError::new(
                    error_codes::UPGRADE_IN_PROGRESS,
                    "another upgrade is already staging",
                ));
            }
        };
        *g = true;

        let cfg = self.config.read().await.clone();
        if !cfg.enabled {
            return Err(UpgradeError::new(
                error_codes::UPGRADE_DISABLED,
                "upgrades.enabled = false on this gateway",
            ));
        }
        if !cfg.allowed_channels.iter().any(|c| c == channel) {
            return Err(UpgradeError::new(
                error_codes::UPGRADE_CHANNEL_NOT_ALLOWED,
                format!("channel {channel:?} not in allowed_channels {:?}", cfg.allowed_channels),
            ));
        }
        let target_v = semver::Version::parse(version).map_err(|e| {
            UpgradeError::new(
                error_codes::UPGRADE_VERSION_INVALID,
                format!("requested version {version:?} is not valid semver: {e}"),
            )
        })?;
        let current_v = semver::Version::parse(&self.current_version).map_err(|e| {
            UpgradeError::new(
                error_codes::UPGRADE_VERSION_INVALID,
                format!("current version {:?} is not valid semver: {e}", self.current_version),
            )
        })?;
        manifest::check_version_window(&target_v, &current_v, &cfg)
            .map_err(|e| UpgradeError::new(error_codes::UPGRADE_VERSION_TOO_OLD, e.to_string()))?;

        let arch = target_arch
            .map(|s| s.to_string())
            .unwrap_or_else(detect_arch);
        let variant = variant.unwrap_or("default").to_string();

        let release_base = manifest::derive_release_base_url(self.profile.repo, version)
            .map_err(|e| UpgradeError::new(error_codes::UPGRADE_URL_INVALID, e.to_string()))?;
        let manifest_url = format!("{release_base}/manifest.json");
        let bundle_url = format!("{release_base}/manifest.sig.bundle");

        let _ = self.events.send(UpgradeEvent {
            severity: "info",
            error_code: "upgrade_started",
            from_version: Some(self.current_version.clone()),
            to_version: Some(version.to_string()),
            channel: Some(channel.to_string()),
            message: format!("Staging upgrade to {version} ({channel}/{arch}/{variant})"),
            size_bytes: None,
        }).await;

        let manifest_bytes = download::fetch_text(&manifest_url, MANIFEST_FETCH_TIMEOUT)
            .await
            .map_err(|e| UpgradeError::new(error_codes::UPGRADE_NETWORK_ERROR, format!("manifest fetch failed: {e}")))?;
        let bundle_bytes = download::fetch_text(&bundle_url, MANIFEST_FETCH_TIMEOUT)
            .await
            .map_err(|e| UpgradeError::new(error_codes::UPGRADE_NETWORK_ERROR, format!("bundle fetch failed: {e}")))?;

        verify::verify_manifest_bundle(
            manifest_bytes.as_bytes(),
            bundle_bytes.as_bytes(),
            self.profile.allowed_signers,
        )
        .await
        .map_err(map_verify_error)?;

        let manifest: Manifest = serde_json::from_slice(manifest_bytes.as_bytes()).map_err(|e| {
            UpgradeError::new(
                error_codes::UPGRADE_MANIFEST_INVALID,
                format!("manifest is not valid JSON: {e}"),
            )
        })?;
        manifest
            .validate_request(version, channel, self.profile.device_type)
            .map_err(|e| UpgradeError::new(error_codes::UPGRADE_MANIFEST_INVALID, e.to_string()))?;

        let state_path = state::path(&cfg.install_root);
        if let Some(s) = state::try_load(&cfg.install_root) {
            if manifest.sequence <= s.last_sequence {
                return Err(UpgradeError::new(
                    error_codes::UPGRADE_SEQUENCE_TOO_OLD,
                    format!("manifest sequence {} ≤ last installed sequence {}", manifest.sequence, s.last_sequence),
                ));
            }
        }

        let artefact = manifest
            .pick_artefact(&arch, &variant)
            .ok_or_else(|| UpgradeError::new(
                error_codes::UPGRADE_ARCH_MISMATCH,
                format!("manifest has no artefact for arch={arch} variant={variant}"),
            ))?;

        let tarball_bytes = download::fetch_with_sha256(&artefact.url, &artefact.sha256, TARBALL_FETCH_TIMEOUT)
            .await
            .map_err(|e| {
                let s = e.to_string();
                let code = if s.contains("checksum") { error_codes::UPGRADE_CHECKSUM_MISMATCH } else { error_codes::UPGRADE_NETWORK_ERROR };
                UpgradeError::new(code, s)
            })?;

        let _ = self.events.send(UpgradeEvent {
            severity: "info",
            error_code: "upgrade_downloaded",
            from_version: Some(self.current_version.clone()),
            to_version: Some(version.to_string()),
            channel: Some(channel.to_string()),
            message: format!("Downloaded upgrade tarball ({} bytes)", tarball_bytes.len()),
            size_bytes: Some(tarball_bytes.len() as u64),
        }).await;

        if cfg.manual_only {
            return Err(UpgradeError::new(
                "upgrade_staged_manual",
                "staged successfully; manual_only is set and the symlink swap is deferred until SIGUSR1",
            ));
        }

        let install_dir = apply::stage_new_version(
            &cfg.install_root,
            version,
            self.profile.binary_name,
            &tarball_bytes,
            &manifest,
        )
        .map_err(|e| UpgradeError::new(error_codes::UPGRADE_EXTRACT_FAILED, e.to_string()))?;

        let prev_state = state::try_load(&cfg.install_root);
        let prev_version = prev_state
            .as_ref()
            .map(|s| s.current_version.clone())
            .unwrap_or_else(|| self.current_version.clone());
        let new_state = InstallState {
            current_version: version.to_string(),
            previous_version: Some(prev_version),
            channel: channel.to_string(),
            variant: variant.clone(),
            arch: arch.clone(),
            status: UpgradeStatus::PendingHealth,
            boot_attempts: 0,
            staged_at: chrono::Utc::now(),
            last_health_at: None,
            last_sequence: manifest.sequence,
        };
        state::save(&state_path, &new_state).map_err(|e| {
            UpgradeError::new(
                error_codes::UPGRADE_EXTRACT_FAILED,
                format!("state.json write failed: {e}"),
            )
        })?;

        let _ = self.events.send(UpgradeEvent {
            severity: "info",
            error_code: "upgrade_staged",
            from_version: Some(self.current_version.clone()),
            to_version: Some(version.to_string()),
            channel: Some(channel.to_string()),
            message: format!("Upgrade staged to {version} — exiting for systemd respawn"),
            size_bytes: None,
        }).await;

        Ok(StagedUpgrade {
            from_version: self.current_version.clone(),
            to_version: version.to_string(),
            channel: channel.to_string(),
            variant,
            arch,
            install_dir,
        })
    }
}

const MANIFEST_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const TARBALL_FETCH_TIMEOUT: Duration = Duration::from_secs(600);

pub fn detect_arch() -> String {
    if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-linux".to_string()
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        "aarch64-linux".to_string()
    } else {
        format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
    }
}

fn map_verify_error(e: anyhow::Error) -> UpgradeError {
    let s = e.to_string();
    let code = if s.contains("identity") {
        error_codes::UPGRADE_IDENTITY_NOT_ALLOWED
    } else if s.contains("rekor") {
        error_codes::UPGRADE_REKOR_INVALID
    } else {
        error_codes::UPGRADE_SIGNATURE_INVALID
    };
    UpgradeError::new(code, s)
}
