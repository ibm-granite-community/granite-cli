#!/usr/bin/env bash
# `model remove` with and without `--dependents refuse|leave|cascade` on a
# model that a capability uses, and `provider remove --dependents
# cascade|discovered` on a provider with discovered models.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

step "--dependents refuse: exits 1 and removes nothing"
use_fixture basic
gcli model remove granite-4.2-3b --dependents refuse -o json
expect_rc 1
expect_file "$M/granite-4.2-3b.yaml"
expect_file "$C/sub-agent-explore.yaml"

step "--dependents leave: removes the model, the capability is left with a note"
use_fixture basic
gcli model remove granite-4.2-3b --dependents leave -o json
expect_rc 0
expect_json 'messages("warn") == ["These stay, and will not work until fixed:"]' "one warning heads what breaks"
expect_json 'messages("info") | any(. == "  - capability '"'sub-agent-explore'"': its model '"'granite-4.2-3b'"' is removed")' "names the capability and why it breaks"
expect_no_file "$M/granite-4.2-3b.yaml"
expect_file "$C/sub-agent-explore.yaml"
gcli capability list -o json
expect_json 'row("sub-agent-explore").NOTES | contains("granite-4.2-3b")' "capability notes the missing model"
expect_json 'row("agent-model").NOTES == ""' "the other capability is unaffected"

step "--dependents cascade: removes the model and everything that depends on it"
use_fixture basic
gcli model remove granite-4.2-3b --dependents cascade -o json
expect_rc 0
expect_no_file "$M/granite-4.2-3b.yaml"
expect_no_file "$C/sub-agent-explore.yaml"
# The cascade is recursive: opencode enables sub-agent-explore, so it goes too.
expect_no_file "$L/opencode.yaml"
expect_file "$C/agent-model.yaml"
expect_file "$M/granite-4.2-8b.yaml"

step "no --dependents in a run that cannot ask: exits 1 and removes nothing"
# gcli runs with stdin from /dev/null and stderr to a file, so the binary
# cannot prompt whether or not the test runner has a terminal.
use_fixture basic
gcli model remove granite-4.2-3b -o plain
expect_rc 1
expect_err_contains "--dependents cascade, --dependents leave, or --dependents refuse"
expect_file "$M/granite-4.2-3b.yaml"
expect_file "$C/sub-agent-explore.yaml"
expect_file "$L/opencode.yaml"

step "a model nothing depends on is removed without a flag"
use_fixture basic
printf 'model_id: spare\nmodel_type: granite-4.2-3b\nprovider_id: ollama\nvariant: GGUF/Q8_0\nconfig: {}\n' >"$M/spare.yaml"
gcli model remove spare -o json
expect_rc 0
expect_no_file "$M/spare.yaml"
expect_file "$M/granite-4.2-3b.yaml"

step "provider remove --dependents cascade: removes the models the provider discovered, with discovery on"
use_fixture model-provider
printf 'provider_id: ollama\ntype: ollama\nconfig:\n  base_url: http://127.0.0.1:9\n  discover_models: true\n' >"$P/ollama.yaml"
gcli model remove discovered-unknown-context --dependents cascade -o json
expect_rc 1
expect_json 'messages("error") | any(contains("which has discovery on, so the next sync would add it back"))' "refuses a model the provider manages"
expect_file "$M/discovered-unknown-context.yaml"
gcli provider remove ollama --dependents cascade -o json
expect_rc 0
expect_no_file "$P/ollama.yaml"
expect_no_file "$M/discovered-unknown-context.yaml"
expect_no_file "$M/granite-4.2-3b.yaml"
expect_no_file "$M/granite-4.2-8b.yaml"
expect_json 'messages("info") | map(select(startswith("Removing "))) == ["Removing provider '"'ollama'"' and what depends on it:"]' "prints one plan for the whole chain"
expect_json 'messages("warn") == []' "breaks nothing"

step "provider remove --dependents discovered: one plan, with what stays and why"
use_fixture model-provider
printf 'provider_id: ollama\ntype: ollama\nconfig:\n  base_url: http://127.0.0.1:9\n  discover_models: true\n' >"$P/ollama.yaml"
gcli provider remove ollama --dependents discovered -o json
expect_rc 0
expect_no_file "$P/ollama.yaml"
expect_no_file "$M/discovered-unknown-context.yaml"
expect_file "$M/granite-4.2-3b.yaml"
expect_file "$M/granite-4.2-8b.yaml"
expect_file "$C/model-provider.yaml"
expect_json 'messages("info") | map(select(startswith("Removing "))) == ["Removing provider '"'ollama'"' and the models discovered from it:"]' "prints one plan for the whole chain"
expect_json 'messages("warn") == ["These stay, and will not work until fixed:"]' "one warning heads what breaks"
expect_json 'messages("info") | map(select(startswith("  - "))) | sort == [
  "  - capability '"'model-provider'"': its model '"'discovered-unknown-context'"' is removed",
  "  - model '"'discovered-unknown-context'"'",
  "  - model '"'granite-4.2-3b'"': its provider '"'ollama'"' is removed",
  "  - model '"'granite-4.2-8b'"': its provider '"'ollama'"' is removed"
]' "lists what is removed, and what stays with the reason"
expect_json 'messages("info") | any(contains("granite-cli model setup") and contains("granite-cli capability setup"))' "names a fix command for each kind left broken"
expect_json 'messages("info") | any(contains("launcher setup")) | not' "names no fix command for a kind nothing broke"

finish
