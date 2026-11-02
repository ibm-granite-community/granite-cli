#!/data/data/com.termux/files/usr/bin/bash
#
# build-termux.sh — build under Termux on Android (aarch64).
#
# Passes all arguments through to cargo while setting up Termux-specific
# environment variables (system OpenSSL, gnu17 C standard, etc.).
#
# Works around two Termux-specific build failures:
#   1. openssl-sys falling back to vendoring and compiling OpenSSL from source,
#      which fails in Termux ("expected absolute path: configdata.pm").
#      Fix: link against Termux's system OpenSSL instead.
#   2. sys-info's bundled C assuming glibc, which Clang 21 rejects under its
#      C23 default ("call to undeclared function 'get_nprocs' / 'index'").
#      Fix: build C deps as gnu17 with the missing headers force-included.
#
# Usage:
#   ./build-termux.sh                 # passthrough: cargo build --release
#   ./build-termux.sh build           # passthrough: cargo build
#   ./build-termux.sh build --debug   # passthrough: cargo build --debug
#   ./build-termux.sh run --example foo  # passthrough: cargo run --example foo
#   ./build-termux.sh --env-only      # just export, don't build
#
set -euo pipefail

# --- argument parsing -------------------------------------------------------

PROJECT_DIR="$PWD"
ENV_ONLY=0
CARGO_ARGS=()

for arg in "$@"; do
    case "$arg" in
        --env-only) ENV_ONLY=1 ;;
        -h|--help)
            sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) CARGO_ARGS+=("$arg") ;;
    esac
done

# Default to "build --release" if no cargo subcommand was specified.
if [ ${#CARGO_ARGS[@]} -eq 0 ]; then
    CARGO_ARGS=(build --release)
fi

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m warn:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# --- sanity checks ----------------------------------------------------------

[ -n "${PREFIX:-}" ] || die "PREFIX unset — this script is for Termux."
[ -d "$PREFIX/bin" ] || die "\$PREFIX/bin not found — is this really Termux?"

# --- package check ----------------------------------------------------------
# Keys are Termux package names; values are a file that proves it's installed.

REQUIRED_PKGS=(
    "openssl:$PREFIX/lib/libssl.so"
    "openssl-tool:$PREFIX/bin/openssl"
    "pkg-config:$PREFIX/bin/pkg-config"
    "clang:$PREFIX/bin/clang"
    "perl:$PREFIX/bin/perl"
    "binutils:$PREFIX/bin/llvm-ar"
)

MISSING=()
for entry in "${REQUIRED_PKGS[@]}"; do
    pkg="${entry%%:*}"
    probe="${entry#*:}"
    # openssl ships a versioned .so; glob for it rather than exact match
    if ! compgen -G "${probe}*" > /dev/null; then
        MISSING+=("$pkg")
    fi
done

if [ ${#MISSING[@]} -gt 0 ]; then
    log "Installing missing packages: ${MISSING[*]}"
    pkg install -y "${MISSING[@]}"
else
    log "All required packages present."
fi

command -v cargo > /dev/null || die "cargo not found. Run: pkg install rust"

# Warn if this is a rustup toolchain rather than Termux's packaged Rust.
# rustup's aarch64-linux-android target makes openssl-sys think it's
# cross-compiling and reach for tools like aarch64-linux-android-gcc.
CARGO_PATH="$(command -v cargo)"
case "$CARGO_PATH" in
    "$PREFIX"/bin/*) ;;
    *) warn "cargo at $CARGO_PATH is not Termux's packaged Rust." 
       warn "If the build reaches for aarch64-linux-android-gcc, try: pkg install rust" ;;
esac

# --- OpenSSL: use the system library, never vendor ---------------------------

export OPENSSL_NO_VENDOR=1
export OPENSSL_DIR="$PREFIX"
export OPENSSL_INCLUDE_DIR="$PREFIX/include"
export OPENSSL_LIB_DIR="$PREFIX/lib"
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

[ -f "$PREFIX/include/openssl/ssl.h" ] \
    || warn "openssl headers missing at $OPENSSL_INCLUDE_DIR — build may fail."

# --- C toolchain ------------------------------------------------------------
CLANG_TARGET="$(clang -dumpmachine 2>/dev/null || true)"
case "$CLANG_TARGET" in
    *android[0-9]*) ;;
    *) CLANG_TARGET="aarch64-linux-android24" ;;
esac

# Wrapper: force-include glibc-isms only for .c sources, never for .S/.s files.
WRAPPER_DIR="${TMPDIR:-$PREFIX/tmp}/termux-cc"
mkdir -p "$WRAPPER_DIR"
cat > "$WRAPPER_DIR/clang-c" <<'EOF'
#!/data/data/com.termux/files/usr/bin/bash
extra=()
for a in "$@"; do
    case "$a" in
        *.c) extra=(-include sys/sysinfo.h -include strings.h); break ;;
    esac
done
exec clang "${extra[@]}" "$@"
EOF
chmod +x "$WRAPPER_DIR/clang-c"

export CC="$WRAPPER_DIR/clang-c"
export AR=llvm-ar
export RANLIB=llvm-ranlib
export CFLAGS="${CFLAGS:-} --target=$CLANG_TARGET -std=gnu17 -Wno-implicit-function-declaration"

if [ "$ENV_ONLY" -eq 1 ]; then
    log "Environment exported. (Only persists if you sourced this script.)"
    return 0 2>/dev/null || exit 0
fi

# --- build ------------------------------------------------------------------

cd "$PROJECT_DIR"
[ -f Cargo.toml ] || die "No Cargo.toml in $PROJECT_DIR"

log "Building in $PROJECT_DIR"
cargo "${CARGO_ARGS[@]}"

