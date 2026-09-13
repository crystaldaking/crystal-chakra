#!/bin/sh
# Chakra installer (issue #203): verified download, user-owned install dir,
# idempotent PATH setup, failure preservation.
#
# Usage:
#   install.sh [--version vX.Y.Z] [--dir PATH] [--no-path-modify]
#              [--base-url URL]
#
# Defaults: latest stable GitHub release, ~/.local/bin.
# Test hooks (not part of the supported interface): CHAKRA_OS / CHAKRA_ARCH
# override platform detection; --base-url accepts file:// fixtures.

set -eu

REPO="crystaldaking/crystal-chakra"
API_BASE="https://api.github.com"
BASE_URL="https://github.com/${REPO}/releases"
VERSION=""
VERSION_EXPLICIT=0
INSTALL_DIR="${HOME}/.local/bin"
MODIFY_PATH=1

log() { printf '%s\n' "$*"; }
fail() { printf 'chakra installer: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; VERSION_EXPLICIT=1; shift 2 ;;
        --dir) INSTALL_DIR="$2"; shift 2 ;;
        --no-path-modify) MODIFY_PATH=0; shift ;;
        --base-url) BASE_URL="$2"; API_BASE="$2"; shift 2 ;;
        *) fail "unknown argument: $1" ;;
    esac
done

# --- platform detection (unsupported combinations fail before changes) ---
os="${CHAKRA_OS:-$(uname -s)}"
arch="${CHAKRA_ARCH:-$(uname -m)}"
case "${os}/${arch}" in
    Linux/x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
    Darwin/arm64|Darwin/aarch64) TARGET="aarch64-apple-darwin" ;;
    Darwin/x86_64) TARGET="x86_64-apple-darwin" ;;
    *) fail "unsupported platform ${os}/${arch}; see README for source build" ;;
esac

command -v curl >/dev/null 2>&1 || fail "curl is required"

fetch() { # fetch URL DEST
    curl -fsSL --max-time 60 --retry 2 -o "$2" "$1" \
        || fail "download failed: $1 (offline? interrupted? previous installation untouched)"
}

# --- resolve version (pinned to one release tag for every download) ---
if [ -z "${VERSION}" ]; then
    latest_json="${TMPDIR:-/tmp}/chakra-latest.$$.json"
    fetch "${API_BASE}/repos/${REPO}/releases/latest" "${latest_json}"
    VERSION=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "${latest_json}" | head -n 1)
    rm -f "${latest_json}"
    [ -n "${VERSION}" ] || fail "could not resolve the latest stable release tag"
fi
case "${VERSION}" in
    v[0-9]*.[0-9]*.[0-9]*) ;;
    *) fail "malformed release tag: ${VERSION}" ;;
esac

# --- downgrade guard: re-installs upgrade; downgrades need --version ---
version_key() { # numeric compare key: v0.4.10 -> 000000004010; empty when unparseable
    case "${1#v}" in
        *[!0-9.]*) printf '' ;;
        *) printf '%03d%03d%03d' $(printf '%s' "${1#v}" | tr '.' ' ') ;;
    esac
}
if [ -x "${INSTALL_DIR}/chakra" ]; then
    installed="$("${INSTALL_DIR}/chakra" --version 2>/dev/null | awk '{print $2}')"
    installed_key="$(version_key "${installed}")"
    if [ -n "${installed}" ] && [ -n "${installed_key}" ] \
        && [ "${installed_key}" -gt "$(version_key "${VERSION}")" ] \
        && [ "${VERSION_EXPLICIT}" != "1" ]; then
        fail "installed chakra ${installed} is newer than ${VERSION}; pass --version explicitly to downgrade"
    fi
    if [ -n "${installed}" ] && [ -z "${installed_key}" ]; then
        log "note: installed version '${installed}' is not a strict vX.Y.Z release; skipping the downgrade guard"
    fi
fi

ARCHIVE="chakra-${VERSION}-${TARGET}.tar.gz"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

# --- verified download: checksum before any install step ---
fetch "${BASE_URL}/download/${VERSION}/${ARCHIVE}" "${WORK}/${ARCHIVE}"
fetch "${BASE_URL}/download/${VERSION}/SHA256SUMS" "${WORK}/SHA256SUMS"
(
    cd "${WORK}"
    grep " ${ARCHIVE}\$" SHA256SUMS >"SHA256SUMS.${ARCHIVE}" \
        || fail "no checksum entry for ${ARCHIVE}"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum --check "SHA256SUMS.${ARCHIVE}" >/dev/null
    else
        shasum -a 256 --check "SHA256SUMS.${ARCHIVE}" >/dev/null
    fi
) || fail "checksum verification failed for ${ARCHIVE}; nothing was installed"

tar -xzf "${WORK}/${ARCHIVE}" -C "${WORK}" \
    || fail "archive extraction failed; nothing was installed"
[ -f "${WORK}/chakra-${VERSION}-${TARGET}/chakra" ] \
    || fail "archive does not contain the chakra binary; nothing was installed"

# --- atomic binary swap: previous binary stays usable until this point ---
mkdir -p "${INSTALL_DIR}"
tmp_bin="${INSTALL_DIR}/.chakra.new.$$"
cp "${WORK}/chakra-${VERSION}-${TARGET}/chakra" "${tmp_bin}"
chmod 0755 "${tmp_bin}"
mv -f "${tmp_bin}" "${INSTALL_DIR}/chakra"

installed_version="$("${INSTALL_DIR}/chakra" --version | awk '{print $2}')"
[ "${installed_version}" = "${VERSION#v}" ] \
    || fail "installed binary reports ${installed_version}, expected ${VERSION#v}"

# --- idempotent PATH setup through a managed shell block ---
BLOCK_BEGIN="# >>> chakra path >>>"
BLOCK_END="# <<< chakra path <<<"
path_note=""
configure_path() { # RC_FILE
    rc="$1"
    if grep -qF "${BLOCK_BEGIN}" "${rc}" 2>/dev/null; then
        path_note="PATH block already present in ${rc} (left unchanged)"
        return 0
    fi
    if printf '%s' ":${PATH}:" | grep -q ":${INSTALL_DIR}:"; then
        path_note="${INSTALL_DIR} is already on PATH in this shell"
        return 0
    fi
    [ -f "${rc}" ] || : >"${rc}"
    {
        printf '%s\n' "${BLOCK_BEGIN}"
        printf 'export PATH="%s:$PATH"\n' "${INSTALL_DIR}"
        printf '%s\n' "${BLOCK_END}"
    } >>"${rc}"
    path_note="added ${INSTALL_DIR} to PATH in ${rc}; open a new terminal or run: . ${rc}"
}

if [ "${MODIFY_PATH}" = "1" ]; then
    shell_name="$(basename "${SHELL:-}")"
    case "${shell_name}" in
        zsh) configure_path "${HOME}/.zshrc" ;;
        bash)
            if [ -f "${HOME}/.bashrc" ]; then
                configure_path "${HOME}/.bashrc"
            else
                configure_path "${HOME}/.bash_profile"
            fi
            ;;
        *)
            path_note="unrecognized shell '${shell_name:-unknown}'; add this to your shell startup file manually: export PATH=\"${INSTALL_DIR}:\$PATH\""
            ;;
    esac
else
    path_note="PATH setup skipped (--no-path-modify)"
fi

# --- conflict warning and summary ---
resolved="$(command -v chakra 2>/dev/null || true)"
if [ -n "${resolved}" ] && [ "${resolved}" != "${INSTALL_DIR}/chakra" ]; then
    log "warning: 'chakra' currently resolves to ${resolved}; earlier PATH entries shadow ${INSTALL_DIR}"
fi
log "installed chakra ${installed_version} to ${INSTALL_DIR}/chakra"
log "${path_note}"
log "next: open a new terminal (PATH changes never affect the parent shell), then run 'chakra init --agent <client>' to set up your agent (see README)"
