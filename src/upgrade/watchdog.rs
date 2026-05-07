// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Boot watchdog for staged gateway upgrades.
//!
//! Mirrors `bilbycast-edge/src/upgrade/watchdog.rs` but emits via the
//! generic `tokio::sync::mpsc::Sender<UpgradeEvent>` channel instead
//! of the edge's `EventSender`. Sidecars wire the receiver through
//! their existing `Emitter` so upgrade events ride the same WS path
//! as every other vendor event.

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use tokio::sync::mpsc::Sender;

use super::state::{self, InstallState, UpgradeStatus};
use super::{UpgradeConfig, UpgradeEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogOutcome {
    Continue,
    RolledBack { from_version: String, to_version: String },
    PendingHealth { attempt: u32 },
}

pub fn run_boot_watchdog(
    upgrade_cfg: Option<&UpgradeConfig>,
    events: &Sender<UpgradeEvent>,
) -> Result<WatchdogOutcome> {
    let Some(cfg) = upgrade_cfg else { return Ok(WatchdogOutcome::Continue); };
    if !cfg.enabled { return Ok(WatchdogOutcome::Continue); }

    let state_path = state::path(&cfg.install_root);
    if !state_path.exists() { return Ok(WatchdogOutcome::Continue); }

    let mut current = match state::load(&state_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("upgrade watchdog: state.json unreadable, continuing: {e:#}");
            return Ok(WatchdogOutcome::Continue);
        }
    };

    match current.status {
        UpgradeStatus::Stable => Ok(WatchdogOutcome::Continue),
        UpgradeStatus::StagedManual => Ok(WatchdogOutcome::Continue),
        UpgradeStatus::RolledBack => {
            let from = current
                .previous_version
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let to = current.current_version.clone();
            let _ = events.try_send(UpgradeEvent {
                severity: "critical",
                error_code: "upgrade_rolled_back",
                from_version: Some(from.clone()),
                to_version: Some(to.clone()),
                channel: None,
                message: format!(
                    "Upgrade to {from} rolled back automatically — running on {to}"
                ),
                size_bytes: None,
            });
            current.status = UpgradeStatus::Stable;
            current.boot_attempts = 0;
            let _ = state::save(&state_path, &current);
            Ok(WatchdogOutcome::RolledBack {
                from_version: from,
                to_version: to,
            })
        }
        UpgradeStatus::PendingHealth => {
            let attempt = state::bump_boot_attempts(&state_path)?;
            if attempt > cfg.max_boot_attempts {
                tracing::error!(
                    "upgrade watchdog: boot_attempts {attempt} exceeded max — rolling back"
                );
                if let Err(e) = revert_to_previous(&cfg.install_root, &mut current) {
                    tracing::error!("upgrade watchdog: revert failed: {e:#}");
                }
                current.status = UpgradeStatus::RolledBack;
                let _ = state::save(&state_path, &current);
                let _ = events.try_send(UpgradeEvent {
                    severity: "critical",
                    error_code: "upgrade_rolled_back",
                    from_version: Some(current.current_version.clone()),
                    to_version: current.previous_version.clone(),
                    channel: None,
                    message: format!(
                        "Upgrade to {} failed — reverted symlink to {:?}, exiting for respawn",
                        current.current_version, current.previous_version
                    ),
                    size_bytes: None,
                });
                std::process::exit(1);
            }
            Ok(WatchdogOutcome::PendingHealth { attempt })
        }
    }
}

pub fn record_healthy_beat(install_root: &Path) {
    let p = state::path(install_root);
    let _ = state::update(&p, |s| {
        if s.status == UpgradeStatus::PendingHealth {
            s.last_health_at = Some(Utc::now());
        }
        Ok(())
    });
}

pub fn finalize_if_stable(
    install_root: &Path,
    cfg: &UpgradeConfig,
    events: &Sender<UpgradeEvent>,
) {
    let p = state::path(install_root);
    let Ok(state) = state::load(&p) else { return; };
    if state.status != UpgradeStatus::PendingHealth { return; }
    let Some(last) = state.last_health_at else { return; };
    let healthy_for = (Utc::now() - state.staged_at).num_seconds().max(0) as u32;
    if healthy_for < cfg.boot_health_window_secs { return; }
    let lag = (Utc::now() - last).num_seconds();
    if lag > 60 { return; }
    let from = state.previous_version.clone().unwrap_or_else(|| "unknown".to_string());
    let to = state.current_version.clone();
    let _ = state::update(&p, |s| { s.status = UpgradeStatus::Stable; Ok(()) });
    let _ = events.try_send(UpgradeEvent {
        severity: "info",
        error_code: "upgrade_completed",
        from_version: Some(from),
        to_version: Some(to.clone()),
        channel: Some(state.channel.clone()),
        message: format!("Upgrade to {to} stable after {healthy_for}s of healthy beats"),
        size_bytes: None,
    });
}

pub async fn run_watchdog_periodic(
    install_root: std::path::PathBuf,
    cfg: UpgradeConfig,
    events: Sender<UpgradeEvent>,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {
                finalize_if_stable(&install_root, &cfg, &events);
            }
        }
    }
}

fn revert_to_previous(install_root: &Path, state: &mut InstallState) -> Result<()> {
    let prev_v = state
        .previous_version
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no previous_version to revert to"))?;
    let prev_target = install_root.join("versions").join(&prev_v);
    if !prev_target.is_dir() {
        anyhow::bail!("previous version directory missing: {}", prev_target.display());
    }
    let current = install_root.join("current");
    let tmp = install_root.join("current.tmp");
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&prev_target, &tmp)?;
    fs::rename(&tmp, &current)?;
    let failed = std::mem::replace(&mut state.current_version, prev_v);
    state.previous_version = Some(failed);
    Ok(())
}
