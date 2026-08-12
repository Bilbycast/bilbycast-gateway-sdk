// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Tarball extraction + atomic symlink swap (gateway-sdk variant).
//!
//! Mirrors `bilbycast-edge/src/upgrade/apply.rs` with one
//! parameterisation change: [`stage_new_version`] takes the binary
//! filename (`profile.binary_name`) so the same flow serves every
//! sidecar, not just the edge.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use super::manifest::Manifest;

pub fn stage_new_version(
    install_root: &Path,
    new_version: &str,
    binary_name: &str,
    tarball_bytes: &[u8],
    manifest: &Manifest,
) -> Result<PathBuf> {
    if new_version != manifest.version {
        bail!(
            "internal error: stage_new_version called with new_version={new_version} but manifest.version={}",
            manifest.version
        );
    }
    fs::create_dir_all(install_root.join("versions"))
        .with_context(|| format!("creating {}/versions", install_root.display()))?;

    let final_dir = install_root.join("versions").join(new_version);
    let partial_dir = install_root
        .join("versions")
        .join(format!("{new_version}.partial"));
    if partial_dir.exists() {
        fs::remove_dir_all(&partial_dir)?;
    }
    if final_dir.exists() {
        let current_link = install_root.join("current");
        if current_link.read_link().ok().as_deref() != Some(final_dir.as_path()) {
            fs::remove_dir_all(&final_dir)?;
        } else {
            return Ok(final_dir);
        }
    }
    fs::create_dir(&partial_dir)?;
    extract_tarball(tarball_bytes, &partial_dir)?;

    let binary_src = locate_binary(&partial_dir, binary_name)
        .ok_or_else(|| anyhow!("binary {binary_name:?} not found in extracted tarball"))?;
    let mut perms = fs::metadata(&binary_src)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&binary_src, perms)?;
    if binary_src.parent() != Some(&partial_dir) {
        let dest = partial_dir.join(binary_name);
        if dest != binary_src {
            fs::rename(&binary_src, &dest)?;
        }
    }
    if let Ok(d) = fs::File::open(&partial_dir) {
        let _ = d.sync_all();
    }
    fs::rename(&partial_dir, &final_dir)?;
    swap_current_symlink(install_root, &final_dir)?;
    let _ = gc_versions(install_root, new_version, 3);
    Ok(final_dir)
}

fn extract_tarball(tarball_bytes: &[u8], dest: &Path) -> Result<()> {
    use flate2::read::GzDecoder;
    use std::io::Cursor;
    let cursor = Cursor::new(tarball_bytes);
    let gz = GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(gz);
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);
    let entries = archive.entries()?;
    for entry in entries {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path.is_absolute() {
            bail!("tar entry has absolute path {:?} — refusing", path);
        }
        if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            bail!("tar entry has '..' segment {:?} — refusing", path);
        }
        let target = dest.join(&path);
        if !target.starts_with(dest) {
            bail!("tar entry would escape extraction root: {:?}", path);
        }
        entry.unpack(&target)?;
    }
    Ok(())
}

fn locate_binary(root: &Path, name: &str) -> Option<PathBuf> {
    let direct = root.join(name);
    if direct.is_file() { return Some(direct); }
    for entry in fs::read_dir(root).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let nested = p.join(name);
            if nested.is_file() { return Some(nested); }
        }
    }
    None
}

fn swap_current_symlink(install_root: &Path, new_target: &Path) -> Result<()> {
    let current = install_root.join("current");
    let previous = install_root.join("previous");
    let tmp = install_root.join("current.tmp");
    if let Ok(existing_target) = current.read_link() {
        let _ = fs::remove_file(&previous);
        std::os::unix::fs::symlink(&existing_target, &previous)?;
    }
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(new_target, &tmp)?;
    fs::rename(&tmp, &current)?;
    if let Ok(d) = fs::File::open(install_root) {
        let _ = d.sync_all();
    }
    Ok(())
}

pub fn gc_versions(install_root: &Path, current_version: &str, keep: usize) -> Result<()> {
    let versions_dir = install_root.join("versions");
    let entries: Vec<(PathBuf, std::time::SystemTime)> = fs::read_dir(&versions_dir)?
        .filter_map(|r| r.ok())
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_string_lossy().into_owned();
            if name == current_version { return None; }
            if name.ends_with(".partial") {
                let _ = fs::remove_dir_all(&path);
                return None;
            }
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((path, mtime))
        })
        .collect();
    let mut sorted = entries;
    sorted.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
    for (path, _) in sorted.into_iter().skip(keep) {
        let _ = fs::remove_dir_all(&path);
    }
    Ok(())
}

pub fn manual_apply_pending(install_root: &Path) -> Result<()> {
    let state_path = super::state::path(install_root);
    let mut state = super::state::load(&state_path)?;
    if state.status != super::state::UpgradeStatus::StagedManual {
        bail!("no manual-staged upgrade pending (state.status = {:?})", state.status);
    }
    let target = install_root.join("versions").join(&state.current_version);
    if !target.is_dir() {
        bail!("staged version directory missing: {}", target.display());
    }
    swap_current_symlink(install_root, &target)?;
    state.status = super::state::UpgradeStatus::PendingHealth;
    state.boot_attempts = 0;
    super::state::save(&state_path, &state)?;
    Ok(())
}
