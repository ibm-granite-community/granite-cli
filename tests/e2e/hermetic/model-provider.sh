#!/usr/bin/env bash
# A `model-provider` capability turns every configured chat model into one
# launcher provider entry per granite-cli provider, all pointing at the
# session proxy. A dry run has no proxy, so it prints the documented
# placeholder in the URL. CI has neither opencode nor pi, so stubs stand in
# for them on PATH.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

use_fixture model-provider
fresh_home
stub_bin opencode
stub_bin pi

step "launch opencode --dry-run writes one provider entry named after the gcli provider"
gcli launch opencode --dry-run -o json
expect_rc 0
cfg=$(json 'messages("info") | map(select(startswith("{"))) | first')
printf '%s' "$cfg" >"$E2E_TMP/opencode.json"
if jq -e '.provider.ollama.npm == "@ai-sdk/openai-compatible"' "$E2E_TMP/opencode.json" >/dev/null; then
    pass "the provider uses the OpenAI-compatible SDK"
else
    fail "the provider uses the OpenAI-compatible SDK"
fi
if jq -e '.provider.ollama.options.baseURL | startswith("http://127.0.0.1:<session-proxy>/v1")' "$E2E_TMP/opencode.json" >/dev/null; then
    pass "the provider points at the session proxy"
else
    fail "the provider points at the session proxy"
fi
if jq -e '.provider.ollama.models | keys == ["discovered-unknown-context", "hf.co/ibm-granite/granite-4.2-3b-GGUF:Q8_0", "hf.co/ibm-granite/granite-4.2-8b-GGUF:Q8_0"]' "$E2E_TMP/opencode.json" >/dev/null; then
    pass "every configured model is offered once under the provider"
else
    fail "every configured model is offered once under the provider"
fi
if jq -e 'has("model") | not' "$E2E_TMP/opencode.json" >/dev/null; then
    pass "granite-cli selects no default model"
else
    fail "granite-cli selects no default model"
fi
expect_json 'messages("info") | any(startswith("Would exec: ") and endswith("/opencode"))' "names the opencode command"

step "launch pi --dry-run writes one provider entry named after the gcli provider"
gcli launch pi --dry-run -o json
expect_rc 0
entry=$(json 'messages("info") | map(select(startswith("{"))) | first')
printf '%s' "$entry" >"$E2E_TMP/pi-provider.json"
if jq -e '.api == "openai-completions"' "$E2E_TMP/pi-provider.json" >/dev/null; then
    pass "the provider uses the OpenAI-completions dialect"
else
    fail "the provider uses the OpenAI-completions dialect"
fi
if jq -e '.baseUrl | startswith("http://127.0.0.1:<session-proxy>/v1")' "$E2E_TMP/pi-provider.json" >/dev/null; then
    pass "the provider points at the session proxy"
else
    fail "the provider points at the session proxy"
fi
if jq -e '[.models[].id] | sort == ["discovered-unknown-context", "hf.co/ibm-granite/granite-4.2-3b-GGUF:Q8_0", "hf.co/ibm-granite/granite-4.2-8b-GGUF:Q8_0"]' "$E2E_TMP/pi-provider.json" >/dev/null; then
    pass "every configured model is offered once under the provider"
else
    fail "every configured model is offered once under the provider"
fi
if jq -e '.models[] | select(.id == "discovered-unknown-context") | has("contextWindow") | not' "$E2E_TMP/pi-provider.json" >/dev/null; then
    pass "a model without a reported context omits contextWindow"
else
    fail "a model without a reported context omits contextWindow"
fi
if jq -e '.models[] | select(.id == "hf.co/ibm-granite/granite-4.2-8b-GGUF:Q8_0") | .contextWindow == 131072' "$E2E_TMP/pi-provider.json" >/dev/null; then
    pass "a model with a reported context keeps contextWindow"
else
    fail "a model with a reported context keeps contextWindow"
fi
if jq -e '.apiKey == "$GRANITE_CLI_PI_API_KEY"' "$E2E_TMP/pi-provider.json" >/dev/null; then
    pass "the key stays an environment reference"
else
    fail "the key stays an environment reference"
fi
expect_json 'messages("info") | any(contains("--provider")) | not' "pi selects no default model"
expect_json 'messages("info") | any(startswith("Would exec: ") and endswith("/pi"))' "names the pi command"

step "the dry runs write no launcher state and run nothing"
expect_no_file "$GRANITE_CLI_HOME/launcher-state"
expect_no_file "$E2E_TMP/bin/opencode.args"
expect_no_file "$E2E_TMP/bin/pi.args"

finish
