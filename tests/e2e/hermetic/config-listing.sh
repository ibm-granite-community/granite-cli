#!/usr/bin/env bash
# The list and info commands show a configuration read from YAML files, and
# reading it does not rewrite the files.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

use_fixture basic
cp -R "$GRANITE_CLI_HOME" "$E2E_TMP/before"

step "provider list shows the configured provider and its base URL"
gcli provider list -o json
expect_rc 0
expect_json 'ids == ["ollama"]'
expect_json 'row("ollama").TYPE == "ollama"'
expect_json 'row("ollama")["BASE URL"] == "http://127.0.0.1:9"'
expect_json 'row("ollama").NOTES == ""' "no notes"

step "model list shows both models bound to the provider"
gcli model list -o json
expect_rc 0
expect_json 'ids | sort == ["granite-4.2-3b", "granite-4.2-8b"]' "both models, in any order"
expect_json 'col("PROVIDER") | unique == ["ollama"]'
expect_json 'row("granite-4.2-8b").SIZE == "8B"'
expect_json 'col("NOTES") | unique == [""]' "no notes"

step "capability and launcher lists have no notes"
gcli capability list -o json
expect_rc 0
expect_json 'ids | sort == ["agent-model", "sub-agent-explore"]'
expect_json 'col("NOTES") | unique == [""]' "no capability notes"
gcli launcher list -o json
expect_rc 0
expect_json 'ids == ["opencode"]'
expect_json 'col("NOTES") | unique == [""]' "no launcher notes"

step "info commands show the instance block"
gcli capability info agent-model -o json
expect_rc 0
expect_json '[.[] | select(.title == "agent-model")] | .[0].fields["Config: model_id"] == "granite-4.2-8b"'
gcli launcher info opencode -o json
expect_rc 0
expect_json '[.[] | select(.title == "opencode")] | .[0].fields["Enabled Capabilities"] | contains("agent-model")'

step "info for an id that is not configured exits 1"
gcli provider info nope -o json
expect_rc 1
expect_json 'messages("error") | length == 1' "one error message"
gcli model info nope -o json
expect_rc 1

step "read-only commands leave the config files as they were"
expect_same_tree "$E2E_TMP/before/providers" "$GRANITE_CLI_HOME/providers"
expect_same_tree "$E2E_TMP/before/models" "$GRANITE_CLI_HOME/models"
expect_same_tree "$E2E_TMP/before/capabilities" "$GRANITE_CLI_HOME/capabilities"
expect_same_tree "$E2E_TMP/before/launchers" "$GRANITE_CLI_HOME/launchers"

finish
