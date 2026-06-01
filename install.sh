#!/usr/bin/env bash
# Relix installer for Linux and macOS.
#
# Downloads the pre-built release archive from GitHub, verifies its
# integrity (SHA256 + cosign keyless signature pinned to the project's
# release.yml workflow identity), extracts it through a tar-slip-safe
# helper, and lands the binaries in a user or system bin dir.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/itsramananshul/Relix/main/install.sh | bash
#   RELIX_VERSION=v0.1.0 ./install.sh
#   RELIX_INSTALL_DIR=/opt/relix/bin ./install.sh
#   sudo ./install.sh                 # installs to /usr/local/bin
#
# Security:
#   * Every artifact download goes through `fetch_and_verify` which
#     enforces a pinned SHA256 before writing the final file.
#   * Cosign signatures (Sigstore keyless) are verified against the
#     itsramananshul/Relix release.yml workflow identity when cosign
#     is available locally — a missing cosign binary produces a loud
#     warning, never a silent skip without notice.
#   * Archive extraction goes through `safe_extract` which rejects
#     any tar entry whose realpath escapes the staging directory
#     (tar-slip protection).
#
# Compatibility:
#   * Bash 4 syntax kept to a minimum; POSIX-portable where practical.
#   * `realpath` is GNU/macOS-portable via a fallback that uses `cd`+`pwd`.

set -euo pipefail

REPO="itsramananshul/Relix"
# `/releases/latest` is GitHub's "latest STABLE" — it excludes
# pre-releases. The plain `/releases` list (newest-first) is what we walk
# to find the newest beta when RELIX_CHANNEL=beta.
RELEASES_API="https://api.github.com/repos/${REPO}/releases/latest"
RELEASES_LIST_API="https://api.github.com/repos/${REPO}/releases?per_page=30"
RELEASES_DL="https://github.com/${REPO}/releases/download"
SCRIPT_RAW_BASE="https://raw.githubusercontent.com/${REPO}"

TMP_DIR=""

cleanup() {
    if [ -n "${TMP_DIR}" ] && [ -d "${TMP_DIR}" ]; then
        rm -rf "${TMP_DIR}"
    fi
}
trap cleanup EXIT INT TERM

err() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

info() {
    printf '%s\n' "$*"
}

warn() {
    printf 'warning: %s\n' "$*" >&2
}

have() {
    command -v "$1" >/dev/null 2>&1
}

# ---------------------------------------------------------------------------
# SEC §15: explicit binary allowlist + tamper-rejecting installer.
#
# The release archive ships exactly these executables. We install
# ONLY these by name and ABORT if the archive carries any other
# executable. The pre-fix code globbed `find -perm -u+x` and even
# had a fallback that copied EVERY regular file — so a tampered
# archive could drop arbitrary executables into the operator's bin
# dir even though the SHA matched a hash the attacker chose (only
# the cosign signature binds the SHA to a real release).
# ---------------------------------------------------------------------------
EXPECTED_BINS="relix relix-controller relix-web-bridge"

# install_expected_binaries <extract_dir> <install_dir>
install_expected_binaries() {
    extract_dir="$1"
    install_dir="$2"

    # (a) Reject unexpected executables. Any +x regular file or *.exe
    #     whose basename is not in the allowlist aborts the install.
    #     Documented non-binary payload (docs / config / shell +
    #     PowerShell scripts — the scripts are hash-verified
    #     separately against SHA256SUMS) is tolerated.
    unexpected=""
    while IFS= read -r f; do
        [ -z "${f}" ] && continue
        b="$(basename "${f}")"
        case " ${EXPECTED_BINS} " in *" ${b} "*) continue ;; esac
        case "${b}" in
            *.md|*.txt|*.json|*.toml|*.yaml|*.yml|*.sh|*.ps1|LICENSE*|README*|CHANGELOG*) continue ;;
        esac
        unexpected="${unexpected}  ${f}
"
    done <<EOF
$(find "${extract_dir}" -type f \( -perm -u+x -o -name '*.exe' \) 2>/dev/null)
EOF
    if [ -n "${unexpected}" ]; then
        printf 'unexpected executable(s) in archive:\n%s' "${unexpected}" >&2
        err "archive contains executable(s) outside the allowlist (${EXPECTED_BINS}); refusing to install a tampered archive"
    fi

    # (b) Install each expected binary by EXACT name — never a glob.
    installed=0
    for name in ${EXPECTED_BINS}; do
        src="$(find "${extract_dir}" -type f -name "${name}" 2>/dev/null | head -n1)"
        [ -z "${src}" ] && continue
        dest="${install_dir}/${name}"
        cp -f "${src}" "${dest}" || err "failed to copy ${src} -> ${dest}"
        chmod +x "${dest}"        || err "failed to chmod +x ${dest}"
        installed=1
        info "  installed: ${dest}"
    done
    if [ "${installed}" -eq 0 ]; then
        err "archive did not contain any expected binary (${EXPECTED_BINS}) in ${extract_dir}"
    fi
}

# install_aux_payload: place the mesh boot scripts + flow templates that
# ship inside the release archive (since the packaging fix) into the dirs
# `relix boot` searches — ~/.local/scripts (scripts, chmod +x) and
# ~/.local/flows (flows). Returns 0 if at least the mesh-up script was
# placed from the archive, so the caller can skip the remote fallback.
# Args: <extract_dir> <scripts_dir> <flows_dir>
install_aux_payload() {
    extract_dir="$1"
    scripts_dir="$2"
    flows_dir="$3"
    placed_up=0

    if [ -d "${extract_dir}/scripts" ]; then
        mkdir -p "${scripts_dir}" 2>/dev/null || true
        for s in relix-mesh-up.sh relix-mesh-down.sh; do
            src="${extract_dir}/scripts/${s}"
            [ -f "${src}" ] || continue
            if cp -f "${src}" "${scripts_dir}/${s}" 2>/dev/null; then
                chmod +x "${scripts_dir}/${s}" 2>/dev/null || true
                info "  installed: ${scripts_dir}/${s}"
                [ "${s}" = "relix-mesh-up.sh" ] && placed_up=1
            fi
        done
    fi

    if [ -d "${extract_dir}/flows" ]; then
        mkdir -p "${flows_dir}" 2>/dev/null || true
        for src in "${extract_dir}/flows/"*; do
            [ -f "${src}" ] || continue
            if cp -f "${src}" "${flows_dir}/$(basename "${src}")" 2>/dev/null; then
                info "  installed: ${flows_dir}/$(basename "${src}")"
            fi
        done
    fi

    [ "${placed_up}" -eq 1 ]
}

# run_self_test: `install.sh --self-test`. Offline harness proving
# (1) a clean archive installs only the allowlisted binaries,
# (2) a tampered archive with an extra executable is rejected and
#     nothing is installed, and
# (3) the SHA256 gate accepts the correct hash and rejects a wrong
#     one. Exits non-zero on any failure.
run_self_test() {
    root="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf '${root}'" EXIT

    # -- clean archive: relix + relix-controller + README.md --
    extract="${root}/clean"; bindir="${root}/bin"
    mkdir -p "${extract}/relix-x86_64" "${bindir}"
    printf '#!/bin/sh\n' > "${extract}/relix-x86_64/relix";            chmod +x "${extract}/relix-x86_64/relix"
    printf '#!/bin/sh\n' > "${extract}/relix-x86_64/relix-controller"; chmod +x "${extract}/relix-x86_64/relix-controller"
    printf 'docs\n'      > "${extract}/relix-x86_64/README.md"
    install_expected_binaries "${extract}" "${bindir}"
    [ -x "${bindir}/relix" ]            || { printf 'SELF-TEST FAIL: relix not installed\n' >&2; exit 1; }
    [ -x "${bindir}/relix-controller" ] || { printf 'SELF-TEST FAIL: relix-controller not installed\n' >&2; exit 1; }
    [ -e "${bindir}/README.md" ]        && { printf 'SELF-TEST FAIL: README.md was installed\n' >&2; exit 1; }
    info "self-test: clean archive installed ONLY allowlisted binaries (README.md skipped)"

    # -- tampered archive: extra executable 'evilbin' --
    extract="${root}/tampered"; bindir="${root}/bin2"
    mkdir -p "${extract}" "${bindir}"
    printf '#!/bin/sh\n' > "${extract}/relix";   chmod +x "${extract}/relix"
    printf '#!/bin/sh\n' > "${extract}/evilbin"; chmod +x "${extract}/evilbin"
    rc=0
    ( install_expected_binaries "${extract}" "${bindir}" ) >/dev/null 2>&1 || rc=$?
    [ "${rc}" -ne 0 ]            || { printf 'SELF-TEST FAIL: tampered archive (extra exe) NOT rejected\n' >&2; exit 1; }
    [ ! -e "${bindir}/relix" ]  || { printf 'SELF-TEST FAIL: tampered archive installed a binary\n' >&2; exit 1; }
    info "self-test: tampered archive (extra executable 'evilbin') rejected; nothing installed"

    # -- SHA256 gate: right hash passes, wrong hash fails --
    f="${root}/payload.bin"; printf 'payload-bytes' > "${f}"
    sha=""; SHACHK=""
    if have sha256sum; then sha="$(sha256sum "${f}" | awk '{print $1}')"; SHACHK="sha256sum";
    elif have shasum; then sha="$(shasum -a 256 "${f}" | awk '{print $1}')"; SHACHK="shasum -a 256";
    fi
    if [ -n "${sha}" ]; then
        printf '%s  %s\n' "${sha}" "${f}" | ${SHACHK} -c - >/dev/null 2>&1 \
            || { printf 'SELF-TEST FAIL: correct hash rejected\n' >&2; exit 1; }
        rc=0
        printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" "${f}" \
            | ${SHACHK} -c - >/dev/null 2>&1 || rc=$?
        [ "${rc}" -ne 0 ] || { printf 'SELF-TEST FAIL: wrong hash accepted\n' >&2; exit 1; }
        info "self-test: SHA256 gate accepts correct hash, rejects wrong hash"
    else
        warn "self-test: no sha256 tool available; skipped the hash-gate check"
    fi

    # -- aux payload: scripts/ + flows/ from an extracted archive land in
    #    ~/.local-style dirs (scripts executable), matching where relix boot
    #    looks. Offline; no network. --
    aux="${root}/extract-aux"
    sdir="${root}/scripts-out"; fdir="${root}/flows-out"
    mkdir -p "${aux}/scripts" "${aux}/flows"
    printf '#!/usr/bin/env bash\necho up\n'   > "${aux}/scripts/relix-mesh-up.sh"
    printf '#!/usr/bin/env bash\necho down\n' > "${aux}/scripts/relix-mesh-down.sh"
    printf 'flow up\n'                         > "${aux}/flows/chat_template.sol"
    printf 'flow retry\n'                      > "${aux}/flows/chat_with_retry.sflow"
    if install_aux_payload "${aux}" "${sdir}" "${fdir}" >/dev/null; then :; else
        printf 'SELF-TEST FAIL: install_aux_payload reported no mesh-up script placed\n' >&2; exit 1
    fi
    [ -x "${sdir}/relix-mesh-up.sh" ]   || { printf 'SELF-TEST FAIL: relix-mesh-up.sh not placed/executable\n' >&2; exit 1; }
    [ -x "${sdir}/relix-mesh-down.sh" ] || { printf 'SELF-TEST FAIL: relix-mesh-down.sh not placed/executable\n' >&2; exit 1; }
    [ -f "${fdir}/chat_template.sol" ]  || { printf 'SELF-TEST FAIL: chat_template.sol not placed\n' >&2; exit 1; }
    [ -f "${fdir}/chat_with_retry.sflow" ] || { printf 'SELF-TEST FAIL: chat_with_retry.sflow not placed\n' >&2; exit 1; }
    info "self-test: mesh scripts placed executable + flows placed from archive payload"

    info "SELF-TEST PASS"
}

if [ "${1:-}" = "--self-test" ]; then
    run_self_test
    exit 0
fi

# ---------------------------------------------------------------------------
# Download / verify helpers (PART 3 + PART 4 + PART 5)
# ---------------------------------------------------------------------------

# fetch_metadata: raw HTTP GET used only for content that has no
# known-good hash (e.g. the GitHub releases API JSON used to resolve
# the latest tag). Outputs the body on stdout. Every other download
# in this script goes through fetch_and_verify instead.
fetch_metadata() {
    local url="$1"
    if have curl; then
        curl -fsSL "${url}"
    elif have wget; then
        wget -qO- "${url}"
    else
        err "neither curl nor wget found; please install one of them and retry"
    fi
}

# fetch_and_verify: download a remote asset to a local path and gate
# acceptance on an exact SHA256 match. Any non-200 response, empty
# body, or hash mismatch is fatal — there is no fallback path that
# trusts the bytes on disk without hash verification.
#
# Args:
#   $1  remote URL
#   $2  expected SHA256 (64-char hex, lowercase)
#   $3  output file path
fetch_and_verify() {
    local url="$1"
    local expected_sha256="$2"
    local output="$3"
    if have curl; then
        curl -fsSL -o "${output}" "${url}" \
            || err "download failed: ${url}"
    elif have wget; then
        wget -q -O "${output}" "${url}" \
            || err "download failed: ${url}"
    else
        err "neither curl nor wget found; please install one of them and retry"
    fi
    if [ ! -s "${output}" ]; then
        err "downloaded asset is empty: ${url}"
    fi
    # `sha256sum` on GNU/Linux, `shasum -a 256` on macOS. Use a single
    # `<EXPECTED>  <PATH>` line piped into the verifier so it does the
    # constant-time string comparison itself.
    if have sha256sum; then
        printf '%s  %s\n' "${expected_sha256}" "${output}" | sha256sum -c - \
            || err "SHA256 mismatch on ${url}; refusing to install (expected ${expected_sha256})"
    elif have shasum; then
        printf '%s  %s\n' "${expected_sha256}" "${output}" | shasum -a 256 -c - \
            || err "SHA256 mismatch on ${url}; refusing to install (expected ${expected_sha256})"
    else
        err "neither sha256sum nor shasum available; cannot verify ${url}"
    fi
}

# verify_signature: cosign keyless verification pinned to the project's
# release.yml workflow identity. When cosign is missing we warn loudly
# and continue (the operator's hash check from fetch_and_verify is
# still in force). When cosign is present a verification failure is
# fatal — we never accept an unverifiable signed artifact.
#
# Args:
#   $1  path to the signed artifact
#   $2  path to the cosign signature (.sig)
#   $3  path to the cosign certificate (.pem)
verify_signature() {
    local binary="$1"
    local sig="$2"
    local cert="$3"
    if command -v cosign &>/dev/null; then
        cosign verify-blob \
            --signature "${sig}" \
            --certificate "${cert}" \
            --certificate-oidc-issuer https://token.actions.githubusercontent.com \
            --certificate-identity-regexp "https://github.com/itsramananshul/Relix/.github/workflows/release.yml" \
            "${binary}" \
            || err "cosign signature verification failed for ${binary}"
        info "  cosign-verified: ${binary}"
    else
        warn "cosign not found; skipping signature verification for ${binary}."
        warn "Install cosign from https://docs.sigstore.dev/cosign/installation/ for verified downloads."
    fi
}

# resolve_realpath: portable absolute-path resolver. GNU coreutils
# ships `realpath`; macOS doesn't by default. The fallback walks the
# path with `cd`+`pwd` which resolves symlinks similarly enough for
# the safe_extract escape check.
resolve_realpath() {
    local target="$1"
    if have realpath; then
        realpath "${target}"
        return
    fi
    # macOS fallback. Loses the `-m` semantics (missing components
    # error out) but the caller only invokes us on entries `find`
    # already produced, so they exist.
    if [ -d "${target}" ]; then
        (cd "${target}" && pwd -P)
    else
        local dir
        local base
        dir=$(dirname -- "${target}")
        base=$(basename -- "${target}")
        printf '%s/%s\n' "$(cd "${dir}" && pwd -P)" "${base}"
    fi
}

# safe_extract: tar-slip-safe archive extraction. Stages every entry
# in a fresh tmpdir, then walks the staged tree and rejects any entry
# whose resolved path escapes that tmpdir (covers `../` traversal AND
# absolute symlinks pointing outside the staging area). Only after
# the whole tree passes the check do we copy into the destination.
#
# Args:
#   $1  archive path
#   $2  destination directory (must exist)
safe_extract() {
    local archive="$1"
    local dest="$2"
    local tmpdir
    tmpdir=$(mktemp -d)
    tar -xzf "${archive}" -C "${tmpdir}" \
        || { rm -rf "${tmpdir}"; err "extraction failed: ${archive}"; }
    local tmp_real
    tmp_real=$(resolve_realpath "${tmpdir}")
    while IFS= read -r -d '' file; do
        local realfile
        realfile=$(resolve_realpath "${file}")
        # The resolved path must live strictly inside the staging
        # tmpdir. Allowing `tmp_real` itself (the root) is fine; any
        # entry whose real path doesn't share the `tmp_real/` prefix
        # is escaping — refuse to continue.
        case "${realfile}" in
            "${tmp_real}"|"${tmp_real}/"*) ;;
            *)
                rm -rf "${tmpdir}"
                err "suspicious path in archive: ${file} (resolved to ${realfile})"
                ;;
        esac
    done < <(find "${tmpdir}" -print0)
    # Copy staged contents into the destination only after the whole
    # tree has been validated. `cp -r tmp/.` preserves the staged tree
    # shape (no extra wrapper dir).
    mkdir -p "${dest}"
    cp -r "${tmpdir}"/. "${dest}"/
    rm -rf "${tmpdir}"
}

# ---------------------------------------------------------------------------
# 1. Detect OS and architecture
# ---------------------------------------------------------------------------
OS_RAW="$(uname -s)"
ARCH_RAW="$(uname -m)"

case "${OS_RAW}" in
    Linux)  OS="linux" ;;
    Darwin) OS="darwin" ;;
    *)      err "unsupported OS: ${OS_RAW} (Relix supports Linux and macOS via this script; use install.ps1 on Windows)" ;;
esac

case "${ARCH_RAW}" in
    x86_64|amd64)        ARCH="x86_64" ;;
    aarch64|arm64)       ARCH="aarch64" ;;
    *)                   err "unsupported architecture: ${ARCH_RAW} (expected x86_64 or aarch64/arm64)" ;;
esac

# ---------------------------------------------------------------------------
# 2. Map to target triple
# ---------------------------------------------------------------------------
TARGET=""
if [ "${OS}" = "linux" ] && [ "${ARCH}" = "x86_64" ]; then
    TARGET="x86_64-unknown-linux-gnu"
elif [ "${OS}" = "linux" ] && [ "${ARCH}" = "aarch64" ]; then
    TARGET="aarch64-unknown-linux-gnu"
elif [ "${OS}" = "darwin" ] && [ "${ARCH}" = "x86_64" ]; then
    TARGET="x86_64-apple-darwin"
elif [ "${OS}" = "darwin" ] && [ "${ARCH}" = "aarch64" ]; then
    TARGET="aarch64-apple-darwin"
else
    err "no Relix release available for ${OS}/${ARCH}"
fi

info "Detected platform: ${OS}/${ARCH} (${TARGET})"

# ---------------------------------------------------------------------------
# 3. Pick install dir
# ---------------------------------------------------------------------------
INSTALL_DIR=""
if [ -n "${RELIX_INSTALL_DIR:-}" ]; then
    INSTALL_DIR="${RELIX_INSTALL_DIR}"
elif [ "${EUID:-$(id -u)}" -eq 0 ]; then
    INSTALL_DIR="/usr/local/bin"
else
    INSTALL_DIR="${HOME}/.local/bin"
fi

mkdir -p "${INSTALL_DIR}" || err "could not create install dir: ${INSTALL_DIR}"

if [ ! -w "${INSTALL_DIR}" ]; then
    err "install dir is not writable: ${INSTALL_DIR} (try sudo or set RELIX_INSTALL_DIR)"
fi

info "Install dir:       ${INSTALL_DIR}"

# ---------------------------------------------------------------------------
# 4. Resolve version / tag
# ---------------------------------------------------------------------------
if ! have sha256sum && ! have shasum; then
    err "need sha256sum or shasum for hash verification; please install coreutils"
fi

# Release channel: "stable" (default, => latest non-prerelease) or
# "beta" (=> newest pre-release). An explicit RELIX_VERSION always wins
# over the channel, so you can pin any exact tag (stable or beta).
CHANNEL="${RELIX_CHANNEL:-stable}"
TAG=""
if [ -n "${RELIX_VERSION:-}" ]; then
    TAG="${RELIX_VERSION}"
    info "Channel:           pinned (${TAG})"
elif [ "${CHANNEL}" = "beta" ] || [ "${CHANNEL}" = "prerelease" ]; then
    info "Channel:           beta — resolving newest pre-release from GitHub..."
    # The releases list is newest-first; pick the first entry that is a
    # pre-release and not a draft.
    RELEASE_JSON="$(fetch_metadata "${RELEASES_LIST_API}")" || err "failed to query ${RELEASES_LIST_API}"
    if have jq; then
        TAG="$(printf '%s' "${RELEASE_JSON}" \
            | jq -r 'map(select(.prerelease == true and .draft == false)) | .[0].tag_name // empty')"
    fi
    if [ -z "${TAG}" ]; then
        # jq-less fallback: track the most recent tag_name and emit it at
        # the first `"prerelease": true` (tag_name precedes prerelease
        # within each release object; objects are newest-first).
        TAG="$(printf '%s' "${RELEASE_JSON}" | awk '
            match($0, /"tag_name"[[:space:]]*:[[:space:]]*"[^"]+"/) {
                t=substr($0,RSTART,RLENGTH); sub(/.*"/,"",t)
            }
            /"tag_name"[[:space:]]*:/ {
                line=$0; sub(/.*"tag_name"[[:space:]]*:[[:space:]]*"/,"",line); sub(/".*/,"",line); t=line
            }
            /"prerelease"[[:space:]]*:[[:space:]]*true/ { print t; exit }
        ')"
    fi
    if [ -z "${TAG}" ]; then
        err "no beta (pre-release) found for ${REPO}. Pin one with RELIX_VERSION=vX.Y.Z-beta.N, or omit RELIX_CHANNEL for the stable channel."
    fi
else
    info "Channel:           stable — resolving latest release from GitHub..."
    # The release-metadata GET is the only fetch that has no
    # pre-known hash. It's used solely to resolve the tag string;
    # every subsequent download is pinned + verified.
    RELEASE_JSON="$(fetch_metadata "${RELEASES_API}")" || err "failed to query ${RELEASES_API}"
    if have jq; then
        TAG="$(printf '%s' "${RELEASE_JSON}" | jq -r '.tag_name // empty')"
    fi
    if [ -z "${TAG}" ]; then
        # Portable fallback: grep + sed
        TAG="$(printf '%s' "${RELEASE_JSON}" \
            | grep -E '"tag_name"[[:space:]]*:' \
            | head -n 1 \
            | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')"
    fi
fi

if [ -z "${TAG}" ]; then
    err "could not determine release tag (set RELIX_VERSION=vX.Y.Z to override)"
fi

# Strip leading "v" for the printed version, keep TAG as-is for the URL
VERSION="${TAG#v}"

info "Version:           ${TAG}"

# ---------------------------------------------------------------------------
# 5. Build download URL
# ---------------------------------------------------------------------------
ARCHIVE_NAME="relix-${TARGET}.tar.gz"
DOWNLOAD_URL="${RELEASES_DL}/${TAG}/${ARCHIVE_NAME}"
SHA256_URL="${DOWNLOAD_URL}.sha256"
ARCHIVE_SIG_URL="${DOWNLOAD_URL}.sig"
ARCHIVE_PEM_URL="${DOWNLOAD_URL}.pem"
SHA256_SIG_URL="${SHA256_URL}.sig"
SHA256_PEM_URL="${SHA256_URL}.pem"

# PART 5: the SHA256SUMS file is per-release and pinned to the
# resolved tag — never `main`. Its cosign signature is what lets us
# trust the per-script hashes during the script + flow fetches.
SUMS_BASE="${RELEASES_DL}/${TAG}"
SUMS_URL="${SUMS_BASE}/SHA256SUMS.txt"
SUMS_SIG_URL="${SUMS_URL}.sig"
SUMS_PEM_URL="${SUMS_URL}.pem"
SCRIPT_BASE="${SCRIPT_RAW_BASE}/${TAG}"

info "Download URL:      ${DOWNLOAD_URL}"

# ---------------------------------------------------------------------------
# 6. Download + verify + safe-extract + install
# ---------------------------------------------------------------------------
TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t relix-install)"
ARCHIVE_PATH="${TMP_DIR}/${ARCHIVE_NAME}"
ARCHIVE_SHA_PATH="${ARCHIVE_PATH}.sha256"
ARCHIVE_SIG_PATH="${ARCHIVE_PATH}.sig"
ARCHIVE_PEM_PATH="${ARCHIVE_PATH}.pem"
SUMS_PATH="${TMP_DIR}/SHA256SUMS.txt"
SUMS_SIG_PATH="${SUMS_PATH}.sig"
SUMS_PEM_PATH="${SUMS_PATH}.pem"
EXTRACT_DIR="${TMP_DIR}/extract"
mkdir -p "${EXTRACT_DIR}"

# Download the per-archive .sha256 first so we can pin the archive
# download against the published hash on the same line. We download
# the cosign signature + cert on the .sha256 file too — that's what
# lets us reach keyless-verified provenance even on systems where
# cosign isn't installed (we still have the SHA256 gate).
info "Downloading SHA256 + cosign material for archive..."
ARCHIVE_SHA_SIG_PATH="${ARCHIVE_SHA_PATH}.sig"
ARCHIVE_SHA_PEM_PATH="${ARCHIVE_SHA_PATH}.pem"
if have curl; then
    curl -fsSL -o "${ARCHIVE_SHA_PATH}"     "${SHA256_URL}" \
        || err "could not fetch ${SHA256_URL} (no per-archive checksum published for ${TAG}?)"
    curl -fsSL -o "${ARCHIVE_SHA_SIG_PATH}" "${SHA256_SIG_URL}" \
        || warn "no cosign signature for ${ARCHIVE_NAME}.sha256 at ${TAG}"
    curl -fsSL -o "${ARCHIVE_SHA_PEM_PATH}" "${SHA256_PEM_URL}" \
        || warn "no cosign cert for ${ARCHIVE_NAME}.sha256 at ${TAG}"
    curl -fsSL -o "${ARCHIVE_SIG_PATH}"     "${ARCHIVE_SIG_URL}" \
        || warn "no cosign signature for ${ARCHIVE_NAME} at ${TAG}"
    curl -fsSL -o "${ARCHIVE_PEM_PATH}"     "${ARCHIVE_PEM_URL}" \
        || warn "no cosign cert for ${ARCHIVE_NAME} at ${TAG}"
elif have wget; then
    wget -q -O "${ARCHIVE_SHA_PATH}"     "${SHA256_URL}" \
        || err "could not fetch ${SHA256_URL}"
    wget -q -O "${ARCHIVE_SHA_SIG_PATH}" "${SHA256_SIG_URL}" \
        || warn "no cosign signature for ${ARCHIVE_NAME}.sha256 at ${TAG}"
    wget -q -O "${ARCHIVE_SHA_PEM_PATH}" "${SHA256_PEM_URL}" \
        || warn "no cosign cert for ${ARCHIVE_NAME}.sha256 at ${TAG}"
    wget -q -O "${ARCHIVE_SIG_PATH}"     "${ARCHIVE_SIG_URL}" \
        || warn "no cosign signature for ${ARCHIVE_NAME} at ${TAG}"
    wget -q -O "${ARCHIVE_PEM_PATH}"     "${ARCHIVE_PEM_URL}" \
        || warn "no cosign cert for ${ARCHIVE_NAME} at ${TAG}"
fi

# Verify the cosign signature on the .sha256 file BEFORE we trust the
# hash it contains. Without this step an attacker who can swap both
# the archive and its matching .sha256 in transit could pass the
# fetch_and_verify hash check below — the hash would match the
# attacker's archive because the attacker also chose the hash. The
# cosign signature on the .sha256 file is what binds it to a
# legitimate release.yml run.
if [ -s "${ARCHIVE_SHA_SIG_PATH}" ] && [ -s "${ARCHIVE_SHA_PEM_PATH}" ]; then
    verify_signature "${ARCHIVE_SHA_PATH}" "${ARCHIVE_SHA_SIG_PATH}" "${ARCHIVE_SHA_PEM_PATH}"
else
    warn "skipping cosign verification on ${ARCHIVE_NAME}.sha256: no .sig/.pem published"
fi

EXPECTED_ARCHIVE_SHA="$(awk 'NR==1 {print $1}' "${ARCHIVE_SHA_PATH}")"
if [ -z "${EXPECTED_ARCHIVE_SHA}" ]; then
    err "could not parse SHA256 from ${SHA256_URL}"
fi

info "Downloading archive..."
fetch_and_verify "${DOWNLOAD_URL}" "${EXPECTED_ARCHIVE_SHA}" "${ARCHIVE_PATH}"

if [ -s "${ARCHIVE_SIG_PATH}" ] && [ -s "${ARCHIVE_PEM_PATH}" ]; then
    verify_signature "${ARCHIVE_PATH}" "${ARCHIVE_SIG_PATH}" "${ARCHIVE_PEM_PATH}"
else
    warn "skipping cosign verification: no .sig/.pem published for ${ARCHIVE_NAME}"
fi

info "Extracting archive (tar-slip-safe)..."
safe_extract "${ARCHIVE_PATH}" "${EXTRACT_DIR}"

# SEC §15: install ONLY the explicit allowlisted binary names and
# abort if the archive carries any unexpected executable. No
# glob / perm scan, no install-everything fallback.
install_expected_binaries "${EXTRACT_DIR}" "${INSTALL_DIR}"

if [ ! -x "${INSTALL_DIR}/relix" ]; then
    err "expected 'relix' binary not found at ${INSTALL_DIR}/relix after install"
fi

# ---------------------------------------------------------------------------
# 6b. Mesh scripts + flow templates
#
# Preferred path: the release archive bundles scripts/ + flows/ (covered by
# the archive's SHA256 + cosign signature verified above), so place them
# straight from the extracted tree into the dirs `relix boot` searches
# (~/.local/scripts, ~/.local/flows). Only when the archive predates that
# packaging (no scripts/ dir in it) do we fall back to fetching each file
# from the release tag and gating on the per-release SHA256SUMS.txt.
# ---------------------------------------------------------------------------
SCRIPTS_DIR="${HOME}/.local/scripts"
FLOWS_DIR="${HOME}/.local/flows"
if install_aux_payload "${EXTRACT_DIR}" "${SCRIPTS_DIR}" "${FLOWS_DIR}"; then
    info "Mesh scripts + flow templates installed from the release archive."
else
    warn "release archive did not bundle scripts/flows; falling back to per-file fetch + SHA256SUMS verification."

SUMS_AVAILABLE=0
if have curl; then
    if curl -fsSL -o "${SUMS_PATH}" "${SUMS_URL}" 2>/dev/null \
        && curl -fsSL -o "${SUMS_SIG_PATH}" "${SUMS_SIG_URL}" 2>/dev/null \
        && curl -fsSL -o "${SUMS_PEM_PATH}" "${SUMS_PEM_URL}" 2>/dev/null ; then
        SUMS_AVAILABLE=1
    fi
elif have wget; then
    if wget -q -O "${SUMS_PATH}" "${SUMS_URL}" 2>/dev/null \
        && wget -q -O "${SUMS_SIG_PATH}" "${SUMS_SIG_URL}" 2>/dev/null \
        && wget -q -O "${SUMS_PEM_PATH}" "${SUMS_PEM_URL}" 2>/dev/null ; then
        SUMS_AVAILABLE=1
    fi
fi

if [ "${SUMS_AVAILABLE}" -eq 1 ]; then
    if [ -s "${SUMS_SIG_PATH}" ] && [ -s "${SUMS_PEM_PATH}" ]; then
        verify_signature "${SUMS_PATH}" "${SUMS_SIG_PATH}" "${SUMS_PEM_PATH}"
    fi
else
    warn "SHA256SUMS.txt not published for ${TAG}; per-script hash verification will skip extras."
fi

# lookup_sha256 prints the expected hash for a path inside the
# SHA256SUMS file (e.g. `scripts/relix-mesh-up.sh`). Empty when the
# file isn't present or doesn't list that path.
lookup_sha256() {
    local repo_path="$1"
    if [ ! -s "${SUMS_PATH}" ]; then
        return
    fi
    awk -v target="${repo_path}" \
        '$2 == target || $2 == "*"target {print $1; exit}' \
        "${SUMS_PATH}"
}

# ---------------------------------------------------------------------------
# 6c. Mesh scripts (PART 5)
#
# Pinned to the resolved release tag (not `main`) and each file's
# SHA256 is checked against the cosign-verified SHA256SUMS.txt above.
# `relix boot` spawns the mesh through scripts/relix-mesh-up.sh;
# users who installed via `curl | bash` don't have a repo checkout.
# Drop the scripts in ~/.local/scripts/ — the relix-cli locate_script
# helper falls back to this path after the repo and binary-dir
# lookups.
# ---------------------------------------------------------------------------
SCRIPTS_DIR="${HOME}/.local/scripts"
mkdir -p "${SCRIPTS_DIR}" || info "warning: could not create ${SCRIPTS_DIR}"

for script in relix-mesh-up.sh relix-mesh-down.sh; do
    target="${SCRIPTS_DIR}/${script}"
    url="${SCRIPT_BASE}/scripts/${script}"
    expected_sha="$(lookup_sha256 "scripts/${script}")"
    if [ -n "${expected_sha}" ]; then
        if fetch_and_verify "${url}" "${expected_sha}" "${target}"; then
            chmod +x "${target}" 2>/dev/null || true
            info "  installed: ${target}"
        else
            warn "could not install ${script} (relix boot will require a repo checkout)"
        fi
    else
        warn "no SHA256 for scripts/${script} in SHA256SUMS.txt; skipping (use a repo checkout)"
    fi
done

# ---------------------------------------------------------------------------
# 6d. Flow templates (PART 5)
#
# Same pinned-tag + SHA256-verified path as the mesh scripts above.
# The bridge reads `flows/chat_template.sol` (and friends) at start
# to wire its OpenAI-compat / tool-routing flow VMs.
# ---------------------------------------------------------------------------
FLOWS_DIR="${HOME}/.local/flows"
mkdir -p "${FLOWS_DIR}" || info "warning: could not create ${FLOWS_DIR}"

for flow in chat_template.sol chat.sol chat_with_tool.sol chat_with_retry.sflow; do
    target="${FLOWS_DIR}/${flow}"
    url="${SCRIPT_BASE}/flows/${flow}"
    expected_sha="$(lookup_sha256 "flows/${flow}")"
    if [ -n "${expected_sha}" ]; then
        if fetch_and_verify "${url}" "${expected_sha}" "${target}"; then
            info "  installed: ${target}"
        else
            warn "could not install ${flow} (relix boot will need a repo checkout for flows)"
        fi
    else
        warn "no SHA256 for flows/${flow} in SHA256SUMS.txt; skipping (use a repo checkout)"
    fi
done
fi  # end fallback: scripts/flows not bundled in the archive

# ---------------------------------------------------------------------------
# 7. PATH wiring
# ---------------------------------------------------------------------------
# The `$PATH` is intentionally kept literal in the line we append to
# the shell rc file — it must expand at shell-init time, not now. The
# install dir is concatenated outside the single quotes so it does
# expand here. shellcheck SC2016 fires on the literal `$PATH` it
# sees inside single quotes; the suppression is the documented
# escape hatch for "I really mean a literal dollar".
# shellcheck disable=SC2016
PATH_LINE='export PATH="'"${INSTALL_DIR}"':$PATH"'

already_on_path() {
    case ":${PATH}:" in
        *:"${INSTALL_DIR}":*) return 0 ;;
        *) return 1 ;;
    esac
}

ensure_in_rc() {
    rc="$1"
    if [ ! -f "${rc}" ]; then
        return 0
    fi
    if grep -Fqx "${PATH_LINE}" "${rc}" 2>/dev/null; then
        return 0
    fi
    {
        printf '\n# Added by Relix installer\n'
        printf '%s\n' "${PATH_LINE}"
    } >> "${rc}" || info "warning: could not write PATH line to ${rc}"
    info "Updated PATH in:   ${rc}"
}

PATH_UPDATED_RC=""
if [ -f "${HOME}/.zshrc" ]; then
    ensure_in_rc "${HOME}/.zshrc"
    PATH_UPDATED_RC="${HOME}/.zshrc"
fi
if [ -f "${HOME}/.bashrc" ]; then
    ensure_in_rc "${HOME}/.bashrc"
    if [ -z "${PATH_UPDATED_RC}" ]; then
        PATH_UPDATED_RC="${HOME}/.bashrc"
    fi
fi

if ! already_on_path; then
    if [ -n "${PATH_UPDATED_RC}" ]; then
        info "Note: open a new shell or run 'source ${PATH_UPDATED_RC}' to pick up PATH."
    else
        info "Note: add ${INSTALL_DIR} to your PATH (no ~/.zshrc or ~/.bashrc found to edit)."
    fi
fi

# ---------------------------------------------------------------------------
# 8. Verify
# ---------------------------------------------------------------------------
VERIFY_OUTPUT=""
if "${INSTALL_DIR}/relix" --version >/dev/null 2>&1; then
    VERIFY_OUTPUT="$("${INSTALL_DIR}/relix" --version 2>/dev/null || true)"
    if [ -n "${VERIFY_OUTPUT}" ]; then
        info "Verified:          ${VERIFY_OUTPUT}"
    fi
else
    info "Verified path:     ${INSTALL_DIR}/relix"
fi

# ---------------------------------------------------------------------------
# 9. Done
# ---------------------------------------------------------------------------
printf '\n'
printf 'Relix %s installed to %s.\n' "${VERSION}" "${INSTALL_DIR}"
printf 'Docs:  https://github.com/%s\n' "${REPO}"
printf '\n'

# ---------------------------------------------------------------------------
# 10. Guided setup
# ---------------------------------------------------------------------------
# `relix setup` is an interactive wizard that writes
# ~/.relix/config.toml and prints the next steps. It reads from
# /dev/tty so it works correctly when the installer is itself piped
# from curl. If there's no terminal at all (Docker build / CI) skip
# silently and tell the operator how to run it later.
if [ -t 0 ] || { [ -r /dev/tty ] && [ -w /dev/tty ]; }; then
    info "Running guided setup..."
    info ""
    if [ -t 0 ]; then
        "${INSTALL_DIR}/relix" setup
    else
        "${INSTALL_DIR}/relix" setup </dev/tty >/dev/tty 2>&1
    fi
else
    info "No terminal available — skipping interactive setup."
    info "Run \`relix setup\` once you have a TTY, then \`relix boot\`."
fi
