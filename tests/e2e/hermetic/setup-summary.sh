#!/usr/bin/env bash
# Spec 0033 use case 15: when a launcher ends up with no capability, the
# summary of `setup --auto` names it, the capability that did not resolve,
# and the command that binds one. A stub opencode is on PATH, and
# GRANITE_CLI_TEST_DEFAULT_BASE_URL points every provider type's default URL
# at a port where nothing listens, so no provider answers, no model can be
# bound, and nothing reaches the network.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

export GRANITE_CLI_TEST_DEFAULT_BASE_URL=http://127.0.0.1:9

step "setup --auto with a launcher and no provider names the launcher"
fresh
fresh_home
stub_bin opencode
# Only the stub launcher, so launchers installed on the machine take no part.
PATH="$E2E_TMP/bin:/usr/bin:/bin" gcli setup --auto --skip-pull
expect_rc 0
expect_out_contains "=== Setup Complete ==="
# Warnings go to stderr.
expect_err_contains "Launcher 'opencode' has no capability: its recommended 'agent-model' did not resolve"
expect_err_contains "Run \`granite-cli capability setup\` to bind one."

step "no provider was added"
if [ -z "$(ls -A "$P")" ]; then pass "providers/ is empty"; else fail "providers/ has: $(ls -A "$P")"; fi

finish
