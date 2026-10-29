#!/usr/bin/env bash
# `launch --dry-run` prints the launcher config it would write and the
# command it would run, and writes nothing. CI has no opencode, so a stub
# stands in for it on PATH.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

use_fixture basic
fresh_home
stub_bin opencode

step "launch opencode --dry-run prints the OpenCode config"
gcli launch opencode --dry-run -o json
expect_rc 0
expect_json 'messages("info") | any(startswith("Would write OpenCode config to "))'
# The second message is the opencode.json the launch would write.
cfg=$(json 'messages("info") | map(select(startswith("{"))) | first')
printf '%s' "$cfg" >"$E2E_TMP/opencode.json"
if jq -e '.model == "ollama/hf.co/ibm-granite/granite-4.2-8b-GGUF:Q8_0"' "$E2E_TMP/opencode.json" >/dev/null; then
    pass "agent-model sets the main model"
else
    fail "agent-model sets the main model"
fi
if jq -e '.agent.explore.model == "ollama/hf.co/ibm-granite/granite-4.2-3b-GGUF:Q8_0"' "$E2E_TMP/opencode.json" >/dev/null; then
    pass "sub-agent-explore sets the explore agent's model"
else
    fail "sub-agent-explore sets the explore agent's model"
fi
if jq -e '.provider.ollama.options.baseURL == "http://127.0.0.1:9/v1"' "$E2E_TMP/opencode.json" >/dev/null; then
    pass "provider base URL is the configured one"
else
    fail "provider base URL is the configured one"
fi
expect_json 'messages("info") | any(startswith("Would exec: ") and endswith("/opencode"))' "names the opencode command"

step "the dry run writes no launcher state and runs nothing"
expect_no_file "$GRANITE_CLI_HOME/launcher-state"
expect_no_file "$E2E_TMP/bin/opencode.args"
if [ -z "$(ls -A "$HOME")" ]; then pass "HOME is empty"; else fail "HOME has files: $(ls -A "$HOME")"; fi

step "without opencode on PATH the dry run fails"
PATH=/usr/bin:/bin gcli launch opencode --dry-run -o json
expect_rc 1
expect_json 'messages("error") | any(contains("not found on PATH"))'

finish
