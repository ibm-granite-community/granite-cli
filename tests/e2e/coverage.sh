#!/usr/bin/env bash
# Run the end-to-end tests against a granite-cli built with coverage
# instrumentation, and write the line coverage of those runs as LCOV.
#
# Usage:
#   tests/e2e/coverage.sh OUT [RUN_ARGS...]
#
#   OUT       LCOV file to write
#   RUN_ARGS  passed to tests/e2e/run.sh, for example `--local local/`
#
# Needs cargo-llvm-cov and the llvm-tools-preview rustup component for the
# toolchain named by rust-version in Cargo.toml. The instrumented build goes
# to target/debug with its own RUSTFLAGS, so the next plain build rebuilds
# the crate.
#
# The report is written even when a test fails. Exits with run.sh's status,
# or 1 when the build or the report fails.

set -u

if [ $# -lt 1 ]; then
    sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
fi

E2E_DIR=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$E2E_DIR/../.." && pwd)
out=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
shift

cd "$REPO" || exit 1
rust_version=$(grep "rust-version" Cargo.toml | cut -d'=' -f2 | sed 's,[ "],,g')
cov() { cargo "+$rust_version" llvm-cov "$@"; }

# Sets RUSTFLAGS and LLVM_PROFILE_FILE, so that the build below is
# instrumented and every run of the binary writes a profile.
env_sh=$(cov show-env --sh) || exit 1
eval "$env_sh"
cov clean --workspace || exit 1
cargo "+$rust_version" build --locked || exit 1

status=0
GCLI_BIN="$REPO/target/debug/granite-cli" bash "$E2E_DIR/run.sh" "$@" || status=$?

cov report --lcov --output-path "$out" || exit 1
echo "wrote $out"
exit "$status"
