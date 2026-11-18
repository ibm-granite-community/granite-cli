#!/usr/bin/env bash
# Pi rejects a `models.json` whose `contextWindow` is 0 or null, and then
# ignores every provider in the file. A discovered model can report no context
# length, so granite-cli omits the field for it and Pi applies its own default.
# This checks the real Pi against a config granite-cli generated.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

require_cmd pi
PI_BIN=$(command -v pi)

use_fixture model-provider
fresh_home
# Isolate both the generated config and Pi's source directory from the user's.
unset PI_CODING_AGENT_DIR
stub_bin pi

step "launch pi writes a config that keeps the discovered model without a context length"
gcli launch pi -o plain
expect_rc 0
state="$GRANITE_CLI_HOME/launcher-state/pi"
expect_file "$state/models.json"
if grep -qF '"discovered-unknown-context"' "$state/models.json"; then
    pass "the discovered model is written"
else
    fail "the discovered model is written"
fi
if grep -qF '"contextWindow": 0' "$state/models.json"; then
    fail "no model carries a zero contextWindow"
else
    pass "no model carries a zero contextWindow"
fi

step "the real pi accepts the generated models.json"
GRANITE_CLI_PI_API_KEY=granite-cli \
    PI_CODING_AGENT_DIR="$state" \
    "$PI_BIN" --list-models --offline --no-extensions --no-skills \
    --no-prompt-templates --no-mcp \
    >"$E2E_TMP/pi-list.out" 2>"$E2E_TMP/pi-list.err"
rc=$?
if [ "$rc" -eq 0 ]; then pass "pi --list-models exits 0"; else
    fail "pi --list-models exits $rc"
    sed 's/^/       /' "$E2E_TMP/pi-list.err" | head -20
fi
if grep -qF 'Invalid models.json schema' "$E2E_TMP/pi-list.err"; then
    fail "pi reports no schema error"
    sed 's/^/       /' "$E2E_TMP/pi-list.err" | head -20
else
    pass "pi reports no schema error"
fi
if grep -qF 'ollama' "$E2E_TMP/pi-list.out" \
    && grep -qF 'discovered-unknown-context' "$E2E_TMP/pi-list.out"; then
    pass "pi lists the generated provider and the discovered model"
else
    fail "pi lists the generated provider and the discovered model"
    sed 's/^/       /' "$E2E_TMP/pi-list.out" | head -20
fi

finish
