#!/usr/bin/env bash
#
# install.sh — Install granite-cli from prebuilt binary, cargo install, or build from source.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/IBM-granite-community/granite-cli/main/install.sh | bash
#
# Environment variables:
#   GRANITE_CLI_VERSION       — Release tag to install (default: latest release)
#   GRANITE_CLI_INSTALL_DIR   — Preferred installation directory
#   VERBOSE                   — Set to "1" for verbose output
#   CI                        — Set to "1" for non-interactive mode (auto-update, no prompts)
#   NONINTERACTIVE            — Alias for CI

set -euo pipefail

# ── colour helpers ────────────────────────────────────────────────────────────
readonly RED='\033[0;31m'
readonly GREEN='\033[0;32m'
readonly YELLOW='\033[1;33m'
readonly BLUE='\033[0;34m'
readonly NC='\033[0m'

info()    { printf "${BLUE}ℹ %s${NC}\n" "$*"; }
ok()      { printf "${GREEN}✓ %s${NC}\n" "$*"; }
warn()    { printf "${YELLOW}⚠ %s${NC}\n" "$*" >&2; }
error()   { printf "${RED}✗ %s${NC}\n" "$*" >&2; }

is_ci() {
    # Non-interactive mode: skip all prompts, auto-update if newer
    [[ -n "$CI" || -n "$NONINTERACTIVE" ]] || return 1
    return 0
}

get_latest_release_version() {
    local tags_url latest_tag
    tags_url="https://api.github.com/repos/${OWNER}/${REPO}/releases/latest"

    if command -v curl &>/dev/null; then
        latest_tag="$(curl -fsSL --max-time 15 -H "Accept: application/vnd.github.v3+json" "$tags_url" 2>/dev/null \
                      | awk -F'"' '/tag_name/ {print $4}')"
    elif command -v wget &>/dev/null; then
        latest_tag="$(wget -qO- --timeout=15 "$tags_url" 2>/dev/null \
                      | awk -F'"' '/tag_name/ {print $4}')"
    fi

    if [[ -z "$latest_tag" ]]; then
        error "Could not fetch latest release from GitHub API."
        error "Specify GRANITE_CLI_VERSION=<tag> and try again."
        exit 1
    fi

    echo "${latest_tag}"
}

# ── configuration ─────────────────────────────────────────────────────────────
readonly OWNER="ibm-granite-community"
readonly REPO="granite-cli"
readonly BIN_NAME="granite-cli"

VERSION="${GRANITE_CLI_VERSION:-$(get_latest_release_version)}"
VERSION="v$(echo ${VERSION} | sed 's,^v,,')"
PREFERRED_INSTALL_DIR="${GRANITE_CLI_INSTALL_DIR:-}"
VERBOSE="${VERBOSE:-}"
CI="${CI:-}"
NONINTERACTIVE="${NONINTERACTIVE:-}"

# ── termux detection ─────────────────────────────────────────────────────────
is_termux() {
    # Termux sets $PREFIX to /data/data/com.termux/files/usr and may provide
    # the termux-version command
    [[ -n "${PREFIX:-}" && "${PREFIX}" == /data/data/com.termux/files/usr ]] || \
        command -v termux-version &>/dev/null
}

# ── WSL detection ─────────────────────────────────────────────────────────────
# Reliably detects Windows Subsystem for Linux (WSL 1 & 2).
# Method recommended by Ben Hillis (WSL developer at Microsoft).
is_wsl() {
    # Check /proc/version for "Microsoft" or "WSL" strings
    if [[ -r /proc/version ]] && grep -qEi "(Microsoft|WSL)" /proc/version 2>/dev/null; then
        return 0
    fi
    # Also check /proc/sys/kernel/osrelease as a secondary probe
    if [[ -r /proc/sys/kernel/osrelease ]] && grep -qEi "(Microsoft|WSL)" /proc/sys/kernel/osrelease 2>/dev/null; then
        return 0
    fi
    return 1
}

# ── platform detection ────────────────────────────────────────────────────────
detect_os() {
    case "$(uname -s | tr '[:upper:]' '[:lower:]')" in
        linux*)
            # WSL runs as Linux but needs Windows binaries
            if is_wsl; then
                echo "pc-windows-msvc"
            else
                echo "unknown"
            fi
            ;;
        darwin*)  echo "apple-darwin" ;;
        cygwin*|msys*|mingw*) echo "pc-windows-msvc" ;;
        *)        echo "$(uname -s | tr '[:upper:]' '[:lower:]')" ;;
    esac
}

detect_arch() {
    local arch
    arch="$(uname -m)"
    case "$arch" in
        x86_64)   echo "x86_64" ;;
        aarch64|arm64) echo "aarch64" ;;
        armv7l|armv7) echo "armv7" ;;
        arm*)     echo "armv7" ;;
        i686|i386) echo "i686" ;;
        *)        echo "$arch" ;;
    esac
}

OS="$(detect_os)"
ARCH="$(detect_arch)"

info "Detected platform: ${OS}/${ARCH}"

if is_termux; then
    info "Running in Termux environment"
fi

# ── binary name helpers ───────────────────────────────────────────────────────
get_binary_name() {
    case "$OS" in
        *windows*) echo "${BIN_NAME}.exe" ;;
        *)         echo "${BIN_NAME}" ;;
    esac
}

get_archive_name() {
    local bin_name
    bin_name="$(get_binary_name)"
    case "$OS" in
        *windows*) echo "${bin_name}.zip" ;;
        *)         echo "${bin_name}.tar.gz" ;;
    esac
}

# ── release URL helpers ───────────────────────────────────────────────────────
get_release_url() {
    local url
    if [[ -n "$VERSION" ]]; then
        url="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/"
    else
        url="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/"
    fi
    echo "$url"
}

get_asset_url() {
    local url
    url="$(get_release_url)"
    # Release assets are named: granite-cli-{os}-{arch}
    # (e.g., granite-cli-linux-x86_64, granite-cli-macos-aarch64)
    local os_name
    case "$OS" in
        *linux*)  os_name="linux" ;;
        *darwin*) os_name="macos" ;;
        *windows*) os_name="windows" ;;
        unknown)
            # Fallback for unrecognised OS strings; re-check uname.
            case "$(uname -s | tr '[:upper:]' '[:lower:]')" in
                darwin*) os_name="macos" ;;
                cygwin*|msys*|mingw*) os_name="windows" ;;
                *) os_name="$OS" ;;
            esac
            ;;
        *) os_name="$OS" ;;
    esac
    local asset_name="granite-cli-${os_name}-${ARCH}"
    # Add .exe extension for Windows assets
    [[ "$OS" == *windows* ]] && asset_name="${asset_name}.exe"
    echo "${url}${asset_name}"
}

# ── installation directory selection ─────────────────────────────────────────
user_bin_dir() {
    # WSL runs Linux (detectable via /proc/version) so use Linux paths.
    # Native Windows uses ~/AppData/Local/bin. WSL users expect ~/.local/bin.
    if is_wsl; then
        echo "${HOME}/.local/bin"
        return
    fi
    case "$OS" in
        *windows*) echo "${HOME}/AppData/Local/bin" ;;
        *)
            if is_termux; then
                echo "${HOME}/bin"
            else
                echo "${HOME}/.local/bin"
            fi
            ;;
    esac
}

system_bin_dir() {
    # WSL runs Linux (detectable via /proc/version) so use Linux paths.
    # Native Windows uses C:/Program Files (requires admin). WSL users are in
    # a Linux environment and expect /usr/local/bin or ~/.local/bin.
    if is_wsl; then
        echo "/usr/local/bin"
        return
    fi
    case "$OS" in
        *windows*) echo "C:/Program Files/${BIN_NAME}" ;;
        *)
            if is_termux; then
                echo "${PREFIX}/bin"
            else
                echo "/usr/local/bin"
            fi
            ;;
    esac
}

# Check if a directory is on PATH
on_path() {
    local dir="$1"
    local p
    for p in ${PATH//:/ }; do
        if [[ "$p" = "$dir" ]]; then
            return 0
        fi
    done
    return 1
}

find_install_dir() {
    local candidate

    if [[ -n "$PREFERRED_INSTALL_DIR" ]]; then
        # Use the user-specified directory
        candidate="$PREFERRED_INSTALL_DIR"
        if [[ ! -d "$candidate" ]]; then
            mkdir -p "$candidate" || {
                error "Cannot create directory: ${candidate}"
                exit 1
            }
        fi
        echo "$candidate"
        return
    fi

    # Try user-space first
    candidate="$(user_bin_dir)"
    if on_path "$candidate" || [[ -d "$candidate" ]]; then
        echo "$candidate"
        return
    fi

    # Fall back to system-wide
    candidate="$(system_bin_dir)"
    echo "$candidate"
}

INSTALL_DIR="$(find_install_dir)"
info "Install directory: ${INSTALL_DIR}"

# ── version checking ────────────────────────────────────────────────────────
get_current_version() {
    local bin_name output version
    bin_name="$(get_binary_name)"
    output="$(${bin_name} version 2>&1)" || true
    # Extract semver: "0.1.0+dev (commit: abc123)" -> "0.1.0"
    # (the old multi-flag fallback was removed — only `granite-cli version` works now)
    version="$(echo "$output" | awk '{for(i=1;i<=NF;i++) if($i ~ /^[0-9]+\.[0-9]+\.[0-9]+$/) {print $i; exit}}')"
    if [[ -n "$version" ]]; then
        echo "$version"
        return 0
    fi
    return 1
}

# Compare two semver strings. Returns 0 if v1 > v2, 1 if v1 == v2, 2 if v1 < v2.
compare_versions() {
    local v1="$1" v2="$2"

    # Normalize: strip leading 'v'
    v1="${v1#v}"
    v2="${v2#v}"

    local IFS='.'
    local -a parts1=($v1) parts2=($v2)

    local i
    for i in 0 1 2; do
        local p1="${parts1[$i]:-0}"
        local p2="${parts2[$i]:-0}"
        if (( p1 > p2 )); then
            return 0  # v1 is newer
        elif (( p1 < p2 )); then
            return 2  # v1 is older
        fi
    done
    return 1  # versions equal
}

check_existing_install() {
    local bin_name="$(get_binary_name)"
    local current_version latest_version

    # Only check if the (platform-appropriate) binary is on PATH
    if ! command -v "${bin_name}" &>/dev/null; then
        return 1
    fi

    current_version="$(get_current_version 2>/dev/null)" || {
        warn "Could not read current granite-cli version; proceeding with fresh install."
        return 1
    }

    latest_version="$(echo "$VERSION" | tr -d 'v')"

    info "Found existing ${bin_name} at $(command -v "${bin_name}") (version ${current_version})"

    if [[ "$current_version" == "$latest_version" ]]; then
        ok "Already up to date (version ${current_version}). Nothing to do."
        exit 0
    fi

    local newer_msg=""
    if compare_versions "$latest_version" "$current_version" 2>/dev/null; then
        newer_msg="newer"
    elif compare_versions "$current_version" "$latest_version" 2>/dev/null; then
        newer_msg="older"
    else
        warn "Version comparison inconclusive; proceeding with install."
        return 1
    fi

    if [[ "$newer_msg" == "older" ]]; then
        info "Current version (${current_version}) is newer than latest release (${latest_version})."
        warn "You may be on a development/build version."
    else
        info "Latest release version: ${latest_version}"
        warn "Current version (${current_version}) is outdated."
    fi

    # Decide whether to update
    if is_ci; then
        ok "CI mode detected — will update to version ${latest_version}."
        return 0
    fi

    # Interactive mode: ask user
    # Use /dev/tty to ensure we read from the terminal even if stdin is redirected
    if [ -e /dev/tty ]; then
        printf "\n${YELLOW}Do you want to update to version ${latest_version}? [Y/n]${NC} " >/dev/tty 2>/dev/null || true
        if ! read -r response </dev/tty 2>/dev/null; then
            info "Could not read response; skipping update."
            exit 0
        fi
    else
        printf "\n${YELLOW}Do you want to update to version ${latest_version}? [Y/n]${NC} "
        read -r response
    fi
    case "$response" in
        [yY][eE][sS]|[yY]|'')
            ok "Updating to version ${latest_version}."
            return 0
            ;;
        [nN][oO]|[nN])
            info "Skipping update."
            exit 0
            ;;
        *)
            info "Unrecognized response; skipping update."
            exit 0
            ;;
    esac
}

# ── prebuilt binary install ──────────────────────────────────────────────────
install_from_release() {
    local asset_url bin_name tmp_dir

    asset_url="$(get_asset_url)"
    bin_name="$(get_binary_name)"
    tmp_dir="$(mktemp -d)"
    local dest_bin="${tmp_dir}/${bin_name}"

    info "Downloading release from: ${asset_url}"

    if command -v curl &>/dev/null; then
        curl -fsSL --progress-bar -o "${dest_bin}" "${asset_url}" \
            || { error "Download failed"; cleanup_tmp "$tmp_dir"; return 1; }
    elif command -v wget &>/dev/null; then
        wget -q --show-progress -O "${dest_bin}" "${asset_url}" \
            || { error "Download failed"; cleanup_tmp "$tmp_dir"; return 1; }
    else
        error "Neither curl nor wget found. Cannot download binary."
        cleanup_tmp "$tmp_dir"
        return 1
    fi

    # Verify the download succeeded (release binaries are non-empty executables)
    if [[ ! -s "$dest_bin" ]]; then
        error "Downloaded file is empty — asset may not exist for this platform"
        cleanup_tmp "$tmp_dir"
        return 1
    fi

    # Ensure install directory exists and copy binary
    mkdir -p "$INSTALL_DIR"
    cp -f "${dest_bin}" "${INSTALL_DIR}/${bin_name}"
    chmod +x "${INSTALL_DIR}/${bin_name}"

    cleanup_tmp "$tmp_dir"

    ok "Installed to ${INSTALL_DIR}/${bin_name}"
    echo ""
    info "Make sure ${INSTALL_DIR} is on your PATH."

    # Check if it's on PATH and warn if not
    if ! on_path "$INSTALL_DIR"; then
        warn "WARNING: ${INSTALL_DIR} is NOT on your PATH."
        echo "   Add it with: export PATH=\"${INSTALL_DIR}:\$PATH\""
        echo "   Or add this line to your shell profile (~/.bashrc, ~/.zshrc, etc.)"
    fi

    return 0
}

# ── cargo install ─────────────────────────────────────────────────────────────
install_from_cargo() {
    info "Attempting cargo install…"

    if ! command -v cargo &>/dev/null; then
        error "cargo not found on PATH."
        return 1
    fi

    # Set up Termux-specific build environment if applicable
    if is_termux; then
        info "Setting up Termux build environment…"
        setup_termux_build_env
    fi

    local cargo_args=("--locked")
    if [[ -n "$VERSION" ]]; then
        cargo_args+=("--version" "$VERSION")
    fi

    info "Running: cargo install granite-cli ${cargo_args[*]}"

    CARGO_INSTALL_ROOT="$INSTALL_DIR" cargo install "${BIN_NAME}" "${cargo_args[@]}" 2>&1 || {
        error "cargo install failed"
        return 1
    }

    ok "Installed via cargo install"
    return 0
}

# ── build from source ─────────────────────────────────────────────────────────
setup_termux_build_env() {
    # Set up environment for building Rust crates in Termux.
    # Mirrors the logic in scripts/build-termux.sh to work around:
    #   1. openssl-sys vendoring failure → use system OpenSSL
    #   2. sys-info C code assuming glibc / C23 defaults → use gnu17 + headers

    local REQUIRED_PKGS=(
        "openssl:$PREFIX/lib/libssl.so"
        "openssl-tool:$PREFIX/bin/openssl"
        "pkg-config:$PREFIX/bin/pkg-config"
        "clang:$PREFIX/bin/clang"
        "perl:$PREFIX/bin/perl"
        "binutils:$PREFIX/bin/llvm-ar"
    )

    local MISSING=()
    for entry in "${REQUIRED_PKGS[@]}"; do
        local pkg="${entry%%:*}"
        local probe="${entry#*:}"
        if ! compgen -G "${probe}*" > /dev/null; then
            MISSING+=("$pkg")
        fi
    done

    if [ ${#MISSING[@]} -gt 0 ]; then
        info "Installing missing packages: ${MISSING[*]}"
        pkg install -y "${MISSING[@]}"
    else
        ok "All required build packages present."
    fi

    # OpenSSL: use the Termux system library, never vendor
    export OPENSSL_NO_VENDOR=1
    export OPENSSL_DIR="$PREFIX"
    export OPENSSL_INCLUDE_DIR="$PREFIX/include"
    export OPENSSL_LIB_DIR="$PREFIX/lib"
    export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

    # C toolchain: point cc-rs at Termux clang, relax C standard for bundled C
    export CC=clang
    export AR=llvm-ar
    export RANLIB=llvm-ranlib
    export CFLAGS="${CFLAGS:-} -std=gnu17 -Wno-implicit-function-declaration -include sys/sysinfo.h -include strings.h"
}

install_from_source() {
    info "Building from source…"

    if ! command -v cargo &>/dev/null; then
        error "cargo not found on PATH. Cannot build from source."
        error "Install Rust: https://rustup.rs"
        return 1
    fi

    # Set up Termux-specific build environment if applicable
    if is_termux; then
        info "Setting up Termux build environment…"
        setup_termux_build_env
    fi

    local git_url="https://github.com/${OWNER}/${REPO}.git"
    local tmp_dir
    tmp_dir="$(mktemp -d)"

    info "Cloning ${git_url}…"
    git clone --depth 1 "${git_url}" "${tmp_dir}/${BIN_NAME}" 2>&1 || {
        error "Failed to clone repository"
        cleanup_tmp "$tmp_dir"
        return 1
    }

    info "Building granite-cli…"
    (
        cd "${tmp_dir}/${BIN_NAME}"
        cargo build --release 2>&1
    ) || {
        error "Build failed"
        cleanup_tmp "$tmp_dir"
        return 1
    }

    mkdir -p "$INSTALL_DIR"
    cp -f "${tmp_dir}/${BIN_NAME}/target/release/${BIN_NAME}" "${INSTALL_DIR}/${BIN_NAME}"
    chmod +x "${INSTALL_DIR}/${BIN_NAME}"

    cleanup_tmp "$tmp_dir"

    ok "Built and installed to ${INSTALL_DIR}/${BIN_NAME}"
    echo ""
    info "Make sure ${INSTALL_DIR} is on your PATH."

    if ! on_path "$INSTALL_DIR"; then
        warn "WARNING: ${INSTALL_DIR} is NOT on your PATH."
        echo "   Add it with: export PATH=\"${INSTALL_DIR}:\$PATH\""
        echo "   Or add this line to your shell profile (~/.bashrc, ~/.zshrc, etc.)"
    fi

    return 0
}

# ── cleanup helper ────────────────────────────────────────────────────────────
cleanup_tmp() {
    local tmp_dir="$1"
    rm -rf "$tmp_dir" 2>/dev/null || true
}

# ── main ──────────────────────────────────────────────────────────────────────
main() {
    info "Installing ${BIN_NAME}…"
    echo ""

    # --- Check for existing installation and offer update ---
    # If an existing install is found and is outdated, user is prompted (or auto-updated in CI).
    # If already up-to-date, the function calls exit 0 — we never reach here.
    # If no existing install, the function returns 1 — we continue to install.
    check_existing_install || true

    # --- Attempt 1: prebuilt binary (skip on Termux — glibc binaries incompatible with Bionic) ---
    if ! is_termux; then
        if install_from_release; then
            echo "Installation complete!"
            exit 0
        fi

        warn "Prebuilt binary not available for ${OS}/${ARCH}."
        echo ""
    fi

    # --- Attempt 2: cargo install ---
    if install_from_cargo; then
        echo "Installation complete!"
        exit 0
    fi

    warn "cargo install failed or not available."
    echo ""

    # --- Attempt 3: build from source ---
    if install_from_source; then
        echo "Installation complete!"
        exit 0
    fi

    error "All installation methods failed."
    error "Try specifying a version: GRANITE_CLI_VERSION=<tag> $0"
    exit 1
}

main
