#!/usr/bin/env bash
# Run the granite-cli end-to-end tests and report pass, fail or skip for each.
#
# Usage:
#   tests/e2e/run.sh [--local] [--no-skip] [-v] [NAME_FILTER]
#
#   (default)    run tests/e2e/hermetic/*.sh, the set CI runs
#   --local      also run tests/e2e/local/*.sh, which need a local
#                environment (for example a running Ollama) and skip
#                themselves when it is missing
#   --no-skip    count a test that skips itself as failed
#   -v           print every test's output, not only that of failed tests
#   NAME_FILTER  run only the tests whose file name contains this text
#
# Environment:
#   GCLI_BIN     binary to test (default: target/debug/granite-cli, built
#                once with `cargo +<rust-version> build --locked` before the
#                tests run)
#   KEEP=1       keep each test's scratch directory
#
# Exits 0 when no test failed.

set -u

E2E_DIR=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$E2E_DIR/../.." && pwd)

dirs="hermetic"
verbose=0
no_skip=0
filter=""
for arg in "$@"; do
    case "$arg" in
        --local) dirs="hermetic local" ;;
        --no-skip) no_skip=1 ;;
        -v) verbose=1 ;;
        -h | --help)
            sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        -*)
            echo "unknown option: $arg (see --help)" >&2
            exit 2
            ;;
        *) filter=$arg ;;
    esac
done

if [ -z "${GCLI_BIN:-}" ]; then
    # Build with the toolchain named by rust-version in Cargo.toml, as
    # scripts/fmt.sh does.
    rust_version=$(grep "rust-version" "$REPO/Cargo.toml" | cut -d'=' -f2 | sed 's,[ "],,g')
    (cd "$REPO" && cargo "+$rust_version" build --locked) || exit 1
    GCLI_BIN="$REPO/target/debug/granite-cli"
fi
export GCLI_BIN

passed=0
failed=0
skipped=0
failed_names=""
log=$(mktemp "${TMPDIR:-/tmp}/gcli-e2e-log.XXXXXX")
trap 'rm -f "$log"' EXIT

for dir in $dirs; do
    for test in "$E2E_DIR/$dir"/*.sh; do
        [ -e "$test" ] || continue
        name="$dir/$(basename "$test")"
        case "$name" in *"$filter"*) ;; *) continue ;; esac
        bash "$test" >"$log" 2>&1
        status=$?
        case "$status" in
            0)
                passed=$((passed + 1))
                printf 'PASS  %s\n' "$name"
                ;;
            77)
                reason=$(grep -m1 '^SKIP: ' "$log" | sed 's/^SKIP: //')
                if [ "$no_skip" = 1 ]; then
                    failed=$((failed + 1))
                    failed_names="$failed_names $name"
                    printf 'FAIL  %s (skipped with --no-skip: %s)\n' "$name" "$reason"
                    # Shows the test's output below, as for any failure.
                    status=1
                else
                    skipped=$((skipped + 1))
                    printf 'SKIP  %s (%s)\n' "$name" "$reason"
                fi
                ;;
            *)
                failed=$((failed + 1))
                failed_names="$failed_names $name"
                printf 'FAIL  %s (exit %s)\n' "$name" "$status"
                ;;
        esac
        if [ "$verbose" = 1 ] || { [ "$status" != 0 ] && [ "$status" != 77 ]; }; then
            sed 's/^/    /' "$log"
        fi
    done
done

echo
echo "$passed passed, $failed failed, $skipped skipped"
if [ "$failed" -gt 0 ]; then
    echo "failed:$failed_names"
    exit 1
fi
