#!/usr/bin/env bash
# A smoke check of hardware detection: on an empty configuration, the first
# command that estimates fit saves `local` from this machine, and with
# `-o json` the notice goes to stderr, so stdout stays the command's result.
# The checks hold on any machine; the figures detected are not checked.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

fresh

step "the first model recommend saves local and reports it on stderr"
gcli model recommend -o json
expect_rc 0
expect_json 'map(.message // "") | all(contains("Saved hardware profile") | not)' "stdout has no notice"
expect_err_contains "Saved hardware profile 'local' from this machine"
expect_file "$GRANITE_CLI_HOME/hardware/local.yaml"

step "local follows detection and has memory for models"
gcli hardware info local -o json
expect_rc 0
expect_json 'detail("Hardware Profile '"'"'local'"'"'").Mode | startswith("auto")' "mode is auto"
expect_json 'detail("Hardware Profile '"'"'local'"'"'")["Usable for models"] | split(" ") | .[0] | tonumber > 0' \
    "usable memory is positive"

step "a second run does not report it again"
gcli model recommend -o json
expect_rc 0
if grep -qF "Saved hardware profile" "$ERR"; then fail "notice repeated"; else pass "no notice"; fi

finish
