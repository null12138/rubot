#!/usr/bin/env bash
set -euo pipefail

REPO="null12138/rubot"
ACTION="${RUBOT_INSTALL_ACTION:-install}"
INSTALL_DIR="${RUBOT_INSTALL_DIR:-/usr/local/bin}"
MODE="${RUBOT_INSTALL_MODE:-auto}"

msg()  { printf "\033[1;34m==>\033[0m %s\n" "$*"; }
warn() { printf "\033[1;33mwarning:\033[0m %s\n" "$*" >&2; }
die()  { printf "\033[1;31merror:\033[0m %s\n" "$*" >&2; exit 1; }

usage() {
    cat <<EOF
Usage:
  install.sh [install|update|uninstall] [--install-dir DIR] [--source|--release]

Environment:
  RUBOT_INSTALL_ACTION    install | update | uninstall
  RUBOT_INSTALL_DIR       target directory for rubot binary
  RUBOT_INSTALL_MODE      auto | source | release

Examples:
  ./install.sh
  ./install.sh update
  ./install.sh uninstall
  ./install.sh install --source
  curl -fsSL https://raw.githubusercontent.com/null12138/rubot/main/install.sh | bash
  curl -fsSL https://raw.githubusercontent.com/null12138/rubot/main/install.sh | bash -s -- update
  curl -fsSL https://raw.githubusercontent.com/null12138/rubot/main/install.sh | bash -s -- uninstall
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        install|update|uninstall)
            ACTION="$1"
            shift
            ;;
        --install-dir)
            [[ $# -ge 2 ]] || die "--install-dir requires a value"
            INSTALL_DIR="$2"
            shift 2
            ;;
        --source)
            MODE="source"
            shift
            ;;
        --release)
            MODE="release"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "Unknown argument: $1"
            ;;
    esac
done

case "${ACTION}" in
    install|update|uninstall) ;;
    *) die "Invalid action: ${ACTION}" ;;
esac

case "${MODE}" in
    auto|source|release) ;;
    *) die "Invalid mode: ${MODE}" ;;
esac

if command -v curl >/dev/null 2>&1; then
    download_file() {
        curl -fsSL \
            -H "User-Agent: rubot-installer" \
            "$1" \
            -o "$2"
    }
elif command -v wget >/dev/null 2>&1; then
    download_file() {
        wget -qO "$2" \
            --header="User-Agent: rubot-installer" \
            "$1"
    }
else
    die "Need curl or wget"
fi

run_with_privilege() {
    if "$@" 2>/dev/null; then
        return 0
    fi
    if command -v sudo >/dev/null 2>&1; then
        sudo "$@"
        return 0
    fi
    die "Permission denied and sudo is not available"
}

target_path() {
    printf "%s/rubot" "${INSTALL_DIR%/}"
}

resolve_mode() {
    case "${MODE}" in
        source|release)
            printf "%s" "${MODE}"
            ;;
        auto)
            if [[ -f "Cargo.toml" ]] && grep -q '^name = "rubot"' Cargo.toml && command -v cargo >/dev/null 2>&1; then
                printf "source"
            else
                printf "release"
            fi
            ;;
    esac
}

remove_target() {
    local target
    target="$(target_path)"
    if [[ ! -e "${target}" ]]; then
        warn "No installed rubot found at ${target}"
        return 0
    fi
    if [[ -w "${target}" ]]; then
        rm -f "${target}"
    else
        run_with_privilege rm -f "${target}"
    fi
    msg "Removed ${target}"
}

cleanup_parent_dir_if_empty() {
    local dir
    dir="${INSTALL_DIR%/}"
    [[ -d "${dir}" ]] || return 0
    if [[ -z "$(find "${dir}" -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null)" ]]; then
        if [[ -w "${dir}" ]]; then
            rmdir "${dir}" 2>/dev/null || true
        elif command -v sudo >/dev/null 2>&1; then
            sudo rmdir "${dir}" 2>/dev/null || true
        fi
    fi
}

if [[ "${ACTION}" == "uninstall" ]]; then
    remove_target
    cleanup_parent_dir_if_empty
    exit 0
fi

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "${OS}-${ARCH}" in
    linux-x86_64)              ASSET="rubot-linux-amd64.tar.gz" ;;
    linux-aarch64|linux-arm64) ASSET="rubot-linux-arm64.tar.gz" ;;
    linux-armv7l|linux-armhf)  ASSET="rubot-linux-armhf.tar.gz" ;;
    darwin-x86_64)             ASSET="rubot-macos-amd64.tar.gz" ;;
    darwin-arm64)              ASSET="rubot-macos-arm64.tar.gz" ;;
    *) die "Unsupported platform: ${OS}-${ARCH}" ;;
esac

TMPDIR="$(mktemp -d)"
trap 'rm -rf "${TMPDIR}"' EXIT
RESOLVED_MODE="$(resolve_mode)"
BINARY=""

if [[ "${RESOLVED_MODE}" == "source" ]]; then
    msg "Installing from local source checkout"
    command -v cargo >/dev/null 2>&1 || die "cargo is required for --source mode"
    [[ -f "Cargo.toml" ]] || die "Cargo.toml not found; run from the repo root or use --release"
    msg "Building rubot..."
    cargo build --release --locked
    BINARY="$(pwd)/target/release/rubot"
    [[ -f "${BINARY}" ]] || die "Built binary not found at ${BINARY}"
else
    msg "Detected: ${OS}-${ARCH} → ${ASSET}"
    download_url="https://github.com/${REPO}/releases/latest/download/${ASSET}"

    msg "Downloading ${ASSET}..."
    if ! download_file "${download_url}" "${TMPDIR}/${ASSET}"; then
        die "Failed to download ${ASSET}. If no release is published yet, run this script from the repo root with --source."
    fi

    msg "Extracting..."
    tar xzf "${TMPDIR}/${ASSET}" -C "${TMPDIR}"
    BINARY="${TMPDIR}/rubot"
    [[ -f "${BINARY}" ]] || die "Binary not found in archive"
    chmod +x "${BINARY}"
fi

msg "$( [[ "${ACTION}" == "update" ]] && printf "Updating" || printf "Installing" ) to $(target_path)"
if [[ -d "${INSTALL_DIR}" ]]; then
    :
elif [[ -w "$(dirname "${INSTALL_DIR}")" ]]; then
    mkdir -p "${INSTALL_DIR}"
else
    run_with_privilege mkdir -p "${INSTALL_DIR}"
fi

if [[ -w "${INSTALL_DIR}" ]] || [[ ! -e "$(target_path)" && -w "$(dirname "${INSTALL_DIR}")" ]]; then
    install -m 755 "${BINARY}" "$(target_path)"
else
    run_with_privilege install -m 755 "${BINARY}" "$(target_path)"
fi

if [[ -x "$(target_path)" ]]; then
    version_line="$("$(target_path)" --version 2>/dev/null || true)"
    [[ -n "${version_line}" ]] && msg "Installed ${version_line}"
fi

if ! command -v rubot >/dev/null 2>&1; then
    warn "rubot is not currently on PATH in this shell. You may need to add ${INSTALL_DIR} to PATH or restart the terminal."
fi

for cmd in bash python3; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        warn "${cmd} not found — code_exec may need it"
    fi
done

echo ""
msg "Done."
echo "  Run: rubot --version"
echo "  Start: rubot"
echo "  Configure inside rubot with:"
echo "    /config set api_base_url <url>"
echo "    /config set api_key <key>"
echo "    /config set model <model>"
