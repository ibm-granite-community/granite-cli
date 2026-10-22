#!/usr/bin/env bash
# `model remove` with and without `--dependents refuse|leave|cascade` on a
# model that a capability uses.
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
expect_json 'messages("warn") | any(contains("will break"))' "warns about what breaks"
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

finish
