#!/usr/bin/env bash
# Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Build the canonical `manifest.json` for a vendor-sidecar release.
#
# This is the **generic** version of the manifest builder used by every
# gateway sidecar. The bilbycast-edge release workflow has its own
# (older, edge-specific) copy at `bilbycast-edge/scripts/build-manifest.sh`;
# new vendor sidecars should reference *this* file.
#
# Usage:
#   build-manifest.sh <version> <channel> <sequence> <artifact_dir> <device_type> <binary_prefix> [release_repo]
#
# Where `<artifact_dir>` contains the just-built tarballs alongside their
# `.sha256` siblings (one per arch/variant). The script discovers each
# pair, plucks the SHA-256 from the sidecar file, and emits a single
# canonical JSON document on stdout. The JSON is canonicalised via
# `jq -cS` so the byte sequence we sign is reproducible — every release
# workflow run that builds the same set of artefacts produces the same
# manifest bytes.
#
# `<binary_prefix>` is the tarball name prefix (e.g.
# `bilbycast-appear-x-api-gateway`) — used to parse `<arch>` and `<variant>`
# out of `${binary_prefix}-<arch>[-<variant>].tar.gz`.
#
# `<device_type>` must match `UpgradeProfile::device_type` on the gateway
# side, otherwise the gateway will reject the manifest with
# `upgrade_manifest_invalid`.
#
# `[release_repo]` optionally overrides the GitHub repo path used in the
# artefact `url` field. Defaults to the `RELEASE_REPO` env var, then to
# `Bilbycast/<binary_prefix>` as a last resort.
#
# The manifest schema is documented in
# `bilbycast-gateway-sdk/src/upgrade/manifest.rs`.

set -euo pipefail

VERSION="${1:?usage: build-manifest.sh <version> <channel> <sequence> <artifact_dir> <device_type> <binary_prefix> [release_repo]}"
CHANNEL="${2:?missing channel}"
SEQUENCE="${3:?missing sequence}"
ARTIFACT_DIR="${4:?missing artifact directory}"
DEVICE_TYPE="${5:?missing device_type}"
BINARY_PREFIX="${6:?missing binary_prefix}"
RELEASE_REPO="${7:-${RELEASE_REPO:-Bilbycast/${BINARY_PREFIX}}}"

if [[ ! -d "${ARTIFACT_DIR}" ]]; then
    echo "build-manifest.sh: artifact directory ${ARTIFACT_DIR} does not exist" >&2
    exit 1
fi

# Released-at: ISO-8601 UTC, second precision. The release workflow's
# clock is the source of truth — operators can later confirm signing
# time via the Rekor entry's integratedTime.
RELEASED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# Walk artifact_dir for `*.tar.gz.sha256` sidecars. Each pair represents
# one (arch, variant) tuple. Naming convention:
#   ${BINARY_PREFIX}-<arch>-<linux|darwin>[-<variant>].tar.gz
#   ${BINARY_PREFIX}-<arch>-<linux|darwin>[-<variant>].tar.gz.sha256
artefacts_json="["
first=1
shopt -s nullglob
for sha_file in "${ARTIFACT_DIR}"/*.tar.gz.sha256; do
    tarball_name="$(basename "${sha_file%.sha256}")"
    tarball_path="${ARTIFACT_DIR}/${tarball_name}"
    if [[ ! -f "${tarball_path}" ]]; then
        echo "build-manifest.sh: skipping ${sha_file} — no matching tarball" >&2
        continue
    fi

    # First field of the sha256sum sidecar is the digest.
    sha256="$(awk '{ print $1 }' < "${sha_file}")"
    if [[ -z "${sha256}" || ${#sha256} -ne 64 ]]; then
        echo "build-manifest.sh: invalid sha256 in ${sha_file}" >&2
        exit 1
    fi

    # Parse arch + variant out of the tarball name.
    bare="${tarball_name#${BINARY_PREFIX}-}"
    bare="${bare%.tar.gz}"
    case "${bare}" in
        *-linux-full)
            arch="${bare%-full}"
            variant="full"
            ;;
        *-linux)
            arch="${bare}"
            variant="default"
            ;;
        *-darwin-full|*-darwin)
            arch="${bare%-full}"
            variant="${bare##*-}"
            [[ "${variant}" != "full" ]] && variant="default"
            ;;
        *)
            echo "build-manifest.sh: cannot parse arch/variant from ${tarball_name}" >&2
            exit 1
            ;;
    esac

    url="https://github.com/${RELEASE_REPO}/releases/download/v${VERSION}/${tarball_name}"

    if [[ ${first} -eq 0 ]]; then
        artefacts_json+=","
    fi
    first=0
    artefacts_json+=$(jq -nc \
        --arg arch "${arch}" \
        --arg variant "${variant}" \
        --arg url "${url}" \
        --arg sha256 "${sha256}" \
        '{arch: $arch, variant: $variant, url: $url, sha256: $sha256}')
done
artefacts_json+="]"

if [[ "${artefacts_json}" == "[]" ]]; then
    echo "build-manifest.sh: no tarballs found under ${ARTIFACT_DIR}" >&2
    exit 1
fi

# Canonicalise via `jq -cS` so the byte sequence we sign is
# deterministic across runs and reorderable inputs.
jq -cS \
    --arg version "${VERSION}" \
    --arg device_type "${DEVICE_TYPE}" \
    --arg channel "${CHANNEL}" \
    --arg released_at "${RELEASED_AT}" \
    --argjson sequence "${SEQUENCE}" \
    --argjson artefacts "${artefacts_json}" \
    -n '{version: $version, device_type: $device_type, channel: $channel, released_at: $released_at, sequence: $sequence, artefacts: $artefacts}'
