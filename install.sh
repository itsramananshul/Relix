#!/usr/bin/env bash
# Relix installer for Linux and macOS.
# Downloads the latest pre-built release from GitHub and installs the
# `relix` binary (and any sibling binaries) into a user or system bin dir.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/itsramananshul/Relix/main/install.sh | bash
#   RELIX_VERSION=v0.1.0 ./install.sh
#   RELIX_INSTALL_DIR=/opt/relix/bin ./install.sh
#   sudo ./install.sh                 # installs to /usr/local/bin

set -euo pipefail

REPO="itsramananshul/Relix"
RELEASES_API="https://api.github.com/repos/${REPO}/releases/latest"
RELEASES_DL="https://github.com/${REPO}/releases/download"

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

have() {
    command -v "$1" >/dev/null 2>&1
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
# Pick a downloader
DOWNLOADER=""
if have curl; then
    DOWNLOADER="curl"
elif have wget; then
    DOWNLOADER="wget"
else
    err "neither curl nor wget found; please install one of them and retry"
fi

fetch_to_stdout() {
    url="$1"
    if [ "${DOWNLOADER}" = "curl" ]; then
        curl -fsSL "${url}"
    else
        wget -qO- "${url}"
    fi
}

fetch_to_file() {
    url="$1"
    out="$2"
    if [ "${DOWNLOADER}" = "curl" ]; then
        curl -fsSL -o "${out}" "${url}"
    else
        wget -q -O "${out}" "${url}"
    fi
}

TAG=""
if [ -n "${RELIX_VERSION:-}" ]; then
    TAG="${RELIX_VERSION}"
else
    info "Resolving latest release tag from GitHub..."
    RELEASE_JSON="$(fetch_to_stdout "${RELEASES_API}")" || err "failed to query ${RELEASES_API}"
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

info "Download URL:      ${DOWNLOAD_URL}"

# ---------------------------------------------------------------------------
# 6. Download + extract + install
# ---------------------------------------------------------------------------
TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t relix-install)"
ARCHIVE_PATH="${TMP_DIR}/${ARCHIVE_NAME}"
EXTRACT_DIR="${TMP_DIR}/extract"
mkdir -p "${EXTRACT_DIR}"

info "Downloading archive..."
fetch_to_file "${DOWNLOAD_URL}" "${ARCHIVE_PATH}" \
    || err "download failed: ${DOWNLOAD_URL}"

if [ ! -s "${ARCHIVE_PATH}" ]; then
    err "downloaded archive is empty: ${ARCHIVE_PATH}"
fi

info "Extracting archive..."
tar -xzf "${ARCHIVE_PATH}" -C "${EXTRACT_DIR}" \
    || err "extraction failed: ${ARCHIVE_PATH}"

# Collect every regular executable file from the extract dir (handles either
# a flat archive or one with a top-level subdir).
INSTALLED_ANY=0
INSTALLED_BINS=""
# shellcheck disable=SC2044
while IFS= read -r bin; do
    [ -z "${bin}" ] && continue
    base="$(basename "${bin}")"
    # Skip non-binary metadata files
    case "${base}" in
        *.md|*.txt|*.json|*.toml|LICENSE*|README*|CHANGELOG*) continue ;;
    esac
    dest="${INSTALL_DIR}/${base}"
    cp -f "${bin}" "${dest}" || err "failed to copy ${bin} -> ${dest}"
    chmod +x "${dest}"      || err "failed to chmod +x ${dest}"
    INSTALLED_ANY=1
    if [ -z "${INSTALLED_BINS}" ]; then
        INSTALLED_BINS="${base}"
    else
        INSTALLED_BINS="${INSTALLED_BINS} ${base}"
    fi
    info "  installed: ${dest}"
done <<EOF
$(find "${EXTRACT_DIR}" -type f \( -perm -u+x -o -name 'relix' -o -name 'relix-*' \) 2>/dev/null)
EOF

if [ "${INSTALLED_ANY}" -eq 0 ]; then
    # Fallback: try installing every regular file (some archives strip the +x bit)
    while IFS= read -r bin; do
        [ -z "${bin}" ] && continue
        base="$(basename "${bin}")"
        case "${base}" in
            *.md|*.txt|*.json|*.toml|LICENSE*|README*|CHANGELOG*) continue ;;
        esac
        dest="${INSTALL_DIR}/${base}"
        cp -f "${bin}" "${dest}" || err "failed to copy ${bin} -> ${dest}"
        chmod +x "${dest}"      || err "failed to chmod +x ${dest}"
        INSTALLED_ANY=1
        if [ -z "${INSTALLED_BINS}" ]; then
            INSTALLED_BINS="${base}"
        else
            INSTALLED_BINS="${INSTALLED_BINS} ${base}"
        fi
        info "  installed: ${dest}"
    done <<EOF2
$(find "${EXTRACT_DIR}" -type f 2>/dev/null)
EOF2
fi

if [ "${INSTALLED_ANY}" -eq 0 ]; then
    err "archive did not contain any binaries (looked in ${EXTRACT_DIR})"
fi

if [ ! -x "${INSTALL_DIR}/relix" ]; then
    err "expected 'relix' binary not found at ${INSTALL_DIR}/relix after install"
fi

# ---------------------------------------------------------------------------
# 6b. Mesh scripts
#
# `relix boot` spawns the mesh through scripts/relix-mesh-up.sh; users
# who installed via `curl | bash` don't have a repo checkout. Drop the
# two scripts in ~/.local/scripts/ — the relix-cli locate_script helper
# falls back to this path after the repo and binary-dir lookups.
# ---------------------------------------------------------------------------
SCRIPTS_DIR="${HOME}/.local/scripts"
mkdir -p "${SCRIPTS_DIR}" || info "warning: could not create ${SCRIPTS_DIR}"

MESH_BASE_URL="https://raw.githubusercontent.com/${REPO}/main/scripts"
for script in relix-mesh-up.sh relix-mesh-down.sh; do
    target="${SCRIPTS_DIR}/${script}"
    if fetch_to_file "${MESH_BASE_URL}/${script}" "${target}"; then
        chmod +x "${target}" 2>/dev/null || true
        info "  installed: ${target}"
    else
        info "warning: could not fetch ${script} (relix boot will require a repo checkout)"
    fi
done

# ---------------------------------------------------------------------------
# 6c. Flow templates
#
# The bridge reads `flows/chat_template.sol` (and friends) at start to
# wire its OpenAI-compat / tool-routing flow VMs. The mesh script
# resolves the `flows/` directory next to itself first; drop the
# templates in ~/.local/flows/ so that probe hits on a clean binary
# install.
# ---------------------------------------------------------------------------
FLOWS_DIR="${HOME}/.local/flows"
mkdir -p "${FLOWS_DIR}" || info "warning: could not create ${FLOWS_DIR}"

FLOWS_BASE_URL="https://raw.githubusercontent.com/${REPO}/main/flows"
for flow in chat_template.sol chat.sol chat_with_tool.sol chat_with_retry.sflow; do
    target="${FLOWS_DIR}/${flow}"
    if fetch_to_file "${FLOWS_BASE_URL}/${flow}" "${target}"; then
        info "  installed: ${target}"
    else
        info "warning: could not fetch ${flow} (relix boot will need a repo checkout for flows)"
    fi
done

# ---------------------------------------------------------------------------
# 7. PATH wiring
# ---------------------------------------------------------------------------
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
