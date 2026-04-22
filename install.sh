#!/usr/bin/env bash
set -euo pipefail

REPO="rubot"
GITHUB_API="https://api.github.com/repos/opener/${REPO}/releases/latest"
INSTALL_DIR="/usr/local/bin"

msg()  { printf "\033[1;34m==>\033[0m %s\n" "$*"; }
warn() { printf "\033[1;33mwarning:\033[0m %s\n" "$*" >&2; }
die()  { printf "\033[1;31merror:\033[0m %s\n" "$*" >&2; exit 1; }

# --- detect OS / arch ---
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "${OS}-${ARCH}" in
    linux-x86_64)         ASSET="rubot-linux-amd64.tar.gz" ;;
    linux-aarch64|linux-arm64) ASSET="rubot-linux-arm64.tar.gz" ;;
    linux-armv7l|linux-armhf)  ASSET="rubot-linux-armhf.tar.gz" ;;
    darwin-x86_64)        ASSET="rubot-macos-amd64.tar.gz" ;;
    darwin-arm64)         ASSET="rubot-macos-arm64.tar.gz" ;;
    *) die "Unsupported platform: ${OS}-${ARCH}" ;;
esac

msg "Detected: ${OS}-${ARCH} → ${ASSET}"

# --- find latest release ---
if command -v curl >/dev/null 2>&1; then
    DL="curl -fsSL"
elif command -v wget >/dev/null 2>&1; then
    DL="wget -qO-"
else
    die "Need curl or wget"
fi

msg "Fetching latest release info..."
DOWNLOAD_URL=$($DL "${GITHUB_API}" | grep -o "\"browser_download_url\": \"[^\"]*${ASSET}\"" | head -1 | sed 's/.*: "//;s/"//')

if [ -z "${DOWNLOAD_URL}" ]; then
    die "Could not find ${ASSET} in latest release"
fi

# --- download & extract ---
TMPDIR="$(mktemp -d)"
trap 'rm -rf "${TMPDIR}"' EXIT

msg "Downloading ${ASSET}..."
$DL "${DOWNLOAD_URL}" -o "${TMPDIR}/${ASSET}"

msg "Extracting..."
tar xzf "${TMPDIR}/${ASSET}" -C "${TMPDIR}"
BINARY="${TMPDIR}/rubot"
[ -f "${BINARY}" ] || die "Binary not found in archive"

chmod +x "${BINARY}"

# --- install ---
if [ -w "${INSTALL_DIR}" ]; then
    mv "${BINARY}" "${INSTALL_DIR}/rubot"
    msg "Installed to ${INSTALL_DIR}/rubot"
else
    msg "Installing to ${INSTALL_DIR}/rubot (needs sudo)..."
    sudo mv "${BINARY}" "${INSTALL_DIR}/rubot"
    msg "Installed to ${INSTALL_DIR}/rubot"
fi

# --- verify ---
if command -v rubot >/dev/null 2>&1; then
    msg "rubot $(rubot --help 2>&1 | head -1 || echo "installed")"
else
    warn "rubot not on PATH — add ${INSTALL_DIR} to your PATH"
fi

# --- check prerequisites ---
for cmd in bash python3; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        warn "${cmd} not found — code_exec tool requires it"
    fi
done

echo ""
msg "Setup complete. Next steps:"
echo "  1. Create .env:  cp .env.example .env && \$EDITOR .env"
echo "  2. Run rubot:    rubot"
