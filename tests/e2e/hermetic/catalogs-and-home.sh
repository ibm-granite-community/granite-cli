#!/usr/bin/env bash
# Catalog commands work on an empty configuration, the configuration
# directories are created on first use, and an unusable GRANITE_CLI_HOME is
# reported.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

fresh
rmdir "$P" "$M" "$C" "$L"

step "catalogs list the built-in types"
gcli provider catalog -o json
expect_rc 0
expect_json '[ids[] | select(. == "ollama" or . == "openrouter")] | length == 2' "ollama and openrouter"
gcli model catalog -o json
expect_rc 0
expect_json 'ids | index("granite-4.2-8b") != null' "granite-4.2-8b"
gcli capability catalog -o json
expect_rc 0
expect_json 'ids | index("agent-model") != null' "agent-model"
gcli launcher catalog -o json
expect_rc 0
expect_json 'ids | index("opencode") != null' "opencode"

step "the first command creates the configuration directories"
for d in providers models capabilities launchers; do
    expect_file "$GRANITE_CLI_HOME/$d"
done

step "empty lists"
gcli provider list -o json
expect_rc 0
expect_json 'table.rows == []'

step "a GRANITE_CLI_HOME whose parent does not exist is an error"
GRANITE_CLI_HOME="$E2E_TMP/missing/home" gcli provider list -o json
expect_rc 1
expect_json 'messages("error") | any(contains("Invalid GRANITE_CLI_HOME"))'

step "provider health reports an unreachable server and exits 0"
use_fixture basic
gcli provider health ollama -o json
expect_rc 0
expect_json 'map(select(.type == "status")) | first | .ok == false and .label == "ollama"'

finish
