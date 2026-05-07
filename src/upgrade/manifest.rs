// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Manifest schema + URL whitelist (gateway-sdk variant).
//!
//! Identical schema to `bilbycast-edge/src/upgrade/manifest.rs`. The
//! only behavioural difference is that
//! [`derive_release_base_url`] takes the repo as a parameter so each
//! sidecar binary can pin its own GitHub release URL.

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};

use super::UpgradeConfig;

/// Hosts allowed to serve manifests / tarballs.
pub const ALLOWED_URL_HOSTS: &[&str] = &["github.com", "objects.githubusercontent.com"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub device_type: String,
    pub channel: String,
    pub released_at: String,
    pub sequence: u64,
    pub artefacts: Vec<ManifestArtefact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestArtefact {
    pub arch: String,
    pub variant: String,
    pub url: String,
    pub sha256: String,
}

impl Manifest {
    pub fn validate_request(
        &self,
        requested_version: &str,
        requested_channel: &str,
        expected_device_type: &str,
    ) -> Result<()> {
        if self.version != requested_version {
            bail!("manifest version mismatch: requested {requested_version}, got {}", self.version);
        }
        if self.channel != requested_channel {
            bail!("manifest channel mismatch: requested {requested_channel}, got {}", self.channel);
        }
        if self.device_type != expected_device_type {
            bail!(
                "manifest device_type mismatch: expected {expected_device_type}, got {}",
                self.device_type
            );
        }
        if self.artefacts.is_empty() {
            bail!("manifest has no artefacts");
        }
        for a in &self.artefacts {
            validate_url_host(&a.url)?;
            if a.sha256.len() != 64 || !a.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
                bail!(
                    "manifest artefact for arch={} has malformed sha256 {:?}",
                    a.arch,
                    a.sha256
                );
            }
            if a.arch.is_empty() || a.arch.len() > 64 {
                bail!("manifest artefact arch is empty or too long");
            }
            if a.variant.is_empty() || a.variant.len() > 32 {
                bail!("manifest artefact variant is empty or too long");
            }
        }
        Ok(())
    }

    pub fn pick_artefact(&self, arch: &str, variant: &str) -> Option<&ManifestArtefact> {
        self.artefacts
            .iter()
            .find(|a| a.arch == arch && a.variant == variant)
    }
}

pub fn validate_url_host(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url)
        .map_err(|e| anyhow!("URL {url:?} is not a valid URL: {e}"))?;
    if parsed.scheme() != "https" {
        bail!("URL {url:?} must use https://");
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("URL {url:?} has no host"))?;
    if !ALLOWED_URL_HOSTS.iter().any(|h| host == *h) {
        bail!(
            "URL {url:?} host {host:?} is not in the upgrade host whitelist {:?}",
            ALLOWED_URL_HOSTS
        );
    }
    if url.len() > 2048 {
        bail!("URL {url:?} exceeds 2048 chars");
    }
    Ok(())
}

/// Construct the GitHub release base URL for the given repo + version.
/// `repo` should be `<owner>/<name>` and is validated only via the
/// resulting URL host check (no allowlist of repos in the SDK — each
/// gateway pins its own via `UpgradeProfile.repo`).
pub fn derive_release_base_url(repo: &str, version: &str) -> Result<String> {
    let v = semver::Version::parse(version).map_err(|e| {
        anyhow!("requested version {version:?} is not valid semver: {e}")
    })?;
    let url = format!("https://github.com/{repo}/releases/download/v{v}");
    validate_url_host(&url)?;
    Ok(url)
}

pub fn check_version_window(
    target_v: &semver::Version,
    current_v: &semver::Version,
    cfg: &UpgradeConfig,
) -> Result<()> {
    if let Some(ref min_str) = cfg.min_version {
        let min = semver::Version::parse(min_str)
            .map_err(|e| anyhow!("min_version {min_str:?} is not valid semver: {e}"))?;
        if *target_v < min {
            bail!("requested {target_v} < min_version {min} configured on this gateway");
        }
    }
    if target_v == current_v {
        bail!("requested {target_v} matches the currently installed version");
    }
    if target_v.major != current_v.major {
        bail!(
            "requested {target_v} crosses a major version boundary from {current_v} \
             — major upgrades require a fresh install, not the auto-upgrader"
        );
    }
    if target_v.minor + u64::from(cfg.rollback_grace) < current_v.minor {
        bail!(
            "requested {target_v} is more than {} minor versions behind current {current_v} \
             (rollback_grace = {})",
            cfg.rollback_grace,
            cfg.rollback_grace
        );
    }
    Ok(())
}
