# shellcheck shell=bash
# shellcheck disable=SC2317 # 'return 1 2>/dev/null || exit 1' exits only when executed, not sourced
# Shared helpers for granite-cli end-to-end tests and manual test guides.
#
# Source it from a test script, or from an interactive bash shell when
# following a manual test guide:
#
#   . tests/e2e/lib.sh
#
# Sourcing it:
#   - sets E2E_DIR (this directory) and E2E_REPO (the repository root);
#   - sets B to the binary under test: $GCLI_BIN when set, otherwise
#     target/debug/granite-cli, built with `cargo +<rust-version> build
#     --locked` if missing;
#   - creates a scratch directory E2E_TMP, removed on exit unless KEEP=1;
#   - checks that jq is installed.
#
# Nothing here reads or writes the user's own configuration: every helper
# that runs the binary works on a GRANITE_CLI_HOME and HOME under E2E_TMP.
#
# Written for bash 3.2 (the macOS /bin/bash) and later.

E2E_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
E2E_REPO=$(cd "$E2E_DIR/../.." && pwd)
E2E_FIXTURES="$E2E_DIR/fixtures"

# Exit status a test uses to say "skipped" (the automake convention).
E2E_SKIP=77

E2E_STEP=0
E2E_FAILED=0
E2E_CHECKS=0

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    _e2e_red=$'\033[31m'
    _e2e_green=$'\033[32m'
    _e2e_bold=$'\033[1m'
    _e2e_reset=$'\033[0m'
else
    _e2e_red=
    _e2e_green=
    _e2e_bold=
    _e2e_reset=
fi

# jq functions available to every expect_json expression. The binary's
# `-o json` output is newline-delimited JSON; expect_json slurps it into one
# array, so these functions take that array as input.
#
#   table             the first {"type":"table"} object
#   col("NAME")       the values of one column of that table, in row order
#   ids               the first column (the instance or catalog id)
#   row("id")         the row whose first cell is "id", as an object keyed
#                     by header
#   detail("title")   the "fields" object of the detail block with that title
#   messages("kind")  the text of every {"type": kind} message
# shellcheck disable=SC2016 # jq variables, not shell variables
E2E_JQ_DEFS='
def table: map(select(.type == "table")) | first;
def col($name): table as $t | ($t.headers | index($name)) as $i | $t.rows | map(.[$i]);
def ids: table.rows | map(.[0]);
def row($id): table as $t | $t.rows | map(select(.[0] == $id)) | first
  | if . == null then null else [$t.headers, .] | transpose | map({(.[0]): .[1]}) | add end;
def detail($title): map(select(.type == "detail" and .title == $title)) | first | .fields;
def messages($kind): map(select(.type == $kind) | .message);
'

_e2e_cleanup() {
    if [ "${KEEP:-0}" = 1 ]; then
        echo "kept scratch directory: $E2E_TMP" >&2
    else
        rm -rf "$E2E_TMP"
    fi
}

if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required by tests/e2e/lib.sh; install it (brew install jq, apt-get install jq)" >&2
    return 1 2>/dev/null || exit 1
fi

E2E_TMP=$(mktemp -d "${TMPDIR:-/tmp}/gcli-e2e.XXXXXX")
trap _e2e_cleanup EXIT

if [ -n "${GCLI_BIN:-}" ]; then
    B=$GCLI_BIN
else
    B="$E2E_REPO/target/debug/granite-cli"
    if [ ! -x "$B" ]; then
        echo "building $B" >&2
        _e2e_rust=$(grep "rust-version" "$E2E_REPO/Cargo.toml" | cut -d'=' -f2 | sed 's,[ "],,g')
        (cd "$E2E_REPO" && cargo "+$_e2e_rust" build --locked >&2) || { return 1 2>/dev/null || exit 1; }
    fi
fi
if [ ! -x "$B" ]; then
    echo "not an executable: $B (set GCLI_BIN)" >&2
    return 1 2>/dev/null || exit 1
fi

# fresh: point GRANITE_CLI_HOME at a new empty directory and create the
# per-kind config directories. Sets P, M, C and L to the providers, models,
# capabilities and launchers directories, for writing files by hand.
fresh() {
    GRANITE_CLI_HOME=$(mktemp -d "$E2E_TMP/home.XXXXXX")
    export GRANITE_CLI_HOME
    P="$GRANITE_CLI_HOME/providers"
    M="$GRANITE_CLI_HOME/models"
    C="$GRANITE_CLI_HOME/capabilities"
    L="$GRANITE_CLI_HOME/launchers"
    mkdir -p "$P" "$M" "$C" "$L"
}

# fresh_home: point HOME at a new empty directory. Use it before `setup` or
# `launch`, which can write launcher settings under HOME.
fresh_home() {
    HOME=$(mktemp -d "$E2E_TMP/user-home.XXXXXX")
    export HOME
}

# use_fixture NAME: fresh, then copy tests/e2e/fixtures/NAME into the new
# GRANITE_CLI_HOME. The fixture directory itself is never modified: a
# command that saves the configuration rewrites every file it loaded.
use_fixture() {
    local src="$E2E_FIXTURES/$1"
    if [ ! -d "$src" ]; then
        echo "no such fixture: $src" >&2
        return 1
    fi
    fresh
    cp -R "$src/." "$GRANITE_CLI_HOME/"
}

# stub_bin NAME [EXIT_CODE]: put an executable NAME on PATH that records its
# arguments in $E2E_TMP/bin/NAME.args and exits with EXIT_CODE (default 0).
# Use it for launcher commands (opencode, claude, ...) that CI does not have.
stub_bin() {
    local name=$1 code=${2:-0}
    mkdir -p "$E2E_TMP/bin"
    printf '#!/bin/sh\necho "$@" > "%s/bin/%s.args"\nexit %s\n' "$E2E_TMP" "$name" "$code" \
        > "$E2E_TMP/bin/$name"
    chmod +x "$E2E_TMP/bin/$name"
    case ":$PATH:" in
        *":$E2E_TMP/bin:"*) ;;
        *) PATH="$E2E_TMP/bin:$PATH" ;;
    esac
    export PATH
}

# step "text": start a numbered step. Steps only label the output.
step() {
    E2E_STEP=$((E2E_STEP + 1))
    printf '\n%s== %s. %s%s\n' "$_e2e_bold" "$E2E_STEP" "$*" "$_e2e_reset"
}

# gcli ARGS...: run the binary under test. Stdout goes to $OUT, stderr to
# $ERR, the exit status to $RC. Never fails by itself; check with expect_rc.
gcli() {
    OUT="$E2E_TMP/out"
    ERR="$E2E_TMP/err"
    printf '$ granite-cli %s\n' "$*"
    "$B" "$@" >"$OUT" 2>"$ERR" </dev/null
    RC=$?
}

# try ARGS...: for manual test guides. Print the command, run the binary
# with `-o plain` and stdin from /dev/null, so it never prompts, and print
# its exit status. Output goes to the terminal and records no check. Use it
# for a batch of error cases, followed in the guide by the captured output.
try() {
    # shellcheck disable=SC2016 # a literal '$ ' prompt, not a variable
    printf '$ granite-cli'
    printf ' %q' "$@"
    echo
    "$B" "$@" -o plain </dev/null
    echo "exit $?"
}

# pass / fail "text": record one check.
pass() {
    E2E_CHECKS=$((E2E_CHECKS + 1))
    printf '  %sok%s   %s\n' "$_e2e_green" "$_e2e_reset" "$*"
}
fail() {
    E2E_CHECKS=$((E2E_CHECKS + 1))
    E2E_FAILED=$((E2E_FAILED + 1))
    printf '  %sFAIL%s %s\n' "$_e2e_red" "$_e2e_reset" "$*"
}

# expect_rc N: the last gcli call exited with N. On failure it prints the
# captured stdout and stderr.
expect_rc() {
    if [ "$RC" = "$1" ]; then
        pass "exit status $1"
    else
        fail "exit status $RC, expected $1"
        _e2e_dump
    fi
}

# expect_json 'EXPR' [description]: the jq expression, applied to the last
# gcli call's stdout slurped into an array, is true. E2E_JQ_DEFS functions
# (table, col, ids, row, detail, messages) are available.
expect_json() {
    local expr=$1 desc=${2:-$1}
    if jq -e -s "$E2E_JQ_DEFS $expr" "$OUT" >/dev/null 2>&1; then
        pass "$desc"
    else
        fail "$desc"
        printf '       jq: %s\n' "$expr"
        printf '       got: %s\n' "$(jq -c -s "$E2E_JQ_DEFS $expr" "$OUT" 2>&1 | head -c 400)"
    fi
}

# json 'EXPR': print the jq expression's value for the last gcli call, raw.
json() {
    jq -r -s "$E2E_JQ_DEFS $1" "$OUT"
}

# expect_out_contains TEXT / expect_err_contains TEXT: a fixed string is in
# the last gcli call's stdout or stderr.
expect_out_contains() {
    if grep -qF -- "$1" "$OUT"; then pass "stdout contains: $1"; else
        fail "stdout does not contain: $1"
        _e2e_dump
    fi
}
expect_err_contains() {
    if grep -qF -- "$1" "$ERR"; then pass "stderr contains: $1"; else
        fail "stderr does not contain: $1"
        _e2e_dump
    fi
}

# expect_file PATH / expect_no_file PATH: a file exists or does not.
expect_file() {
    if [ -e "$1" ]; then pass "exists: ${1#"$E2E_TMP"/}"; else fail "missing: ${1#"$E2E_TMP"/}"; fi
}
expect_no_file() {
    if [ -e "$1" ]; then fail "unexpected: ${1#"$E2E_TMP"/}"; else pass "absent: ${1#"$E2E_TMP"/}"; fi
}

# expect_same_tree DIR_A DIR_B: both directories hold the same files with
# the same content. Use it to check that a command did not rewrite config.
expect_same_tree() {
    if diff -r "$1" "$2" >/dev/null; then pass "unchanged: ${2#"$E2E_TMP"/}"; else
        fail "changed: ${2#"$E2E_TMP"/}"
        diff -r "$1" "$2" | head -20 | sed 's/^/       /'
    fi
}

# skip "reason": stop the test as skipped (exit 77). For tests under
# tests/e2e/local that need something the machine may not have.
skip() {
    echo "SKIP: $*"
    exit "$E2E_SKIP"
}

# require_cmd NAME / require_url URL: skip unless a command is on PATH, or
# unless an HTTP GET of URL succeeds within 2 seconds.
require_cmd() {
    command -v "$1" >/dev/null 2>&1 || skip "$1 is not on PATH"
}
require_url() {
    command -v curl >/dev/null 2>&1 || skip "curl is not on PATH"
    curl -fsS -m 2 -o /dev/null "$1" 2>/dev/null || skip "$1 is not reachable"
}

# finish: print the summary and exit 0 when every check passed, 1 otherwise.
# Call it as the last line of a test script.
finish() {
    echo
    if [ "$E2E_FAILED" -eq 0 ]; then
        printf '%sPASS%s %s checks\n' "$_e2e_green" "$_e2e_reset" "$E2E_CHECKS"
        exit 0
    fi
    printf '%sFAIL%s %s of %s checks failed\n' "$_e2e_red" "$_e2e_reset" "$E2E_FAILED" "$E2E_CHECKS"
    exit 1
}

_e2e_dump() {
    sed 's/^/       stdout: /' "$OUT" | head -20
    sed 's/^/       stderr: /' "$ERR" | head -20
}
