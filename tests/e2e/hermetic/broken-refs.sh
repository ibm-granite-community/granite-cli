#!/usr/bin/env bash
# A reference to an instance that is not configured, or to an unknown type,
# is reported in the NOTES column of the list that holds the referring
# instance. The list command still exits 0.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

use_fixture broken-refs

step "model list notes the missing provider and the unknown model type"
gcli model list -o json
expect_rc 0
expect_json 'row("orphan").NOTES | startswith("⚠ ") and contains("provider '"'"'gone'"'"', which is not configured")' \
    "orphan: provider 'gone' is not configured"
expect_json 'row("unknown-type").NOTES | contains("unknown model type '"'"'no-such-type'"'"'")' \
    "unknown-type: unknown model type"

step "capability list notes the missing model"
gcli capability list -o json
expect_rc 0
expect_json 'row("sub-agent-explore").NOTES | contains("model '"'"'missing-model'"'"', which is not configured")'

step "launcher list notes the missing capability"
gcli launcher list -o json
expect_rc 0
expect_json 'row("opencode").NOTES | contains("capability '"'"'agent-model'"'"', which is not configured")'



finish
