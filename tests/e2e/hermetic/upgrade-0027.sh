#!/usr/bin/env bash
# Spec 0033 use case 19: a provider file written by spec 0027, with
# `hardware:`, does not load, and the error names the file and the release
# notes. scripts/convert-0027-hosts.sh writes the hosts and rewrites the
# providers; estimates stay the same, and a second run changes nothing.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

skip "spec 0033 sub-task 2"

convert() {
    printf '$ scripts/convert-0027-hosts.sh %s\n' "$1"
    "$E2E_REPO/scripts/convert-0027-hosts.sh" "$1" >"$E2E_TMP/convert.out" 2>&1
    RC=$?
    OUT="$E2E_TMP/convert.out"
}

step "a spec 0027 provider file does not load"
use_fixture spec0027
gcli provider list -o json
expect_rc 1
expect_json 'messages("error") | any(contains("providers/") and contains(".yaml"))' "names a provider file"
expect_json 'messages("error") | any(contains("'"'"'hardware'"'"' was replaced by '"'"'host'"'"'"))' "says what replaced it"
expect_json 'messages("error") | any(contains("/releases/"))' "points at the release notes"

step "the script converts the configuration"
use_fixture spec0027
convert "$GRANITE_CLI_HOME"
expect_rc 0
expect_file "$GRANITE_CLI_HOME/hosts/spark.yaml"
expect_file "$GRANITE_CLI_HOME/hosts/card.yaml"
expect_file "$GRANITE_CLI_HOME/hosts/card-2.yaml"
expect_no_file "$GRANITE_CLI_HOME/hosts/local.yaml"
if grep -q '^hardware:' "$P"/*.yaml; then fail "a provider still has hardware:"; else
    pass "no provider has hardware:"
fi

step "the converted configuration loads, with each provider on its host"
gcli provider list -o json
expect_rc 0
expect_json 'row("ollama").HOST == "local"' "ollama on local"
expect_json 'row("openrouter").HOST == "hosted"' "openrouter not specified"
expect_json 'row("ollama-spark").HOST == "spark" and row("llama-cpp-spark").HOST == "spark"' \
    "one host for two providers at one address"
expect_json '[row("llama-cpp-card-a").HOST, row("llama-cpp-card-b").HOST] | sort == ["card", "card-2"]' \
    "one host per address for one profile"
expect_json 'col("NOTES") | all(. == "")' "no notes"
gcli host info spark -o json
expect_rc 0
expect_json 'detail("Host '"'"'spark'"'"'").Address == "spark.lan"' "spark's address from the base URLs"

step "estimates on a converted host are those of its hardware profile"
gcli model recommend --hardware spark -o json
expect_rc 0
by_hardware=$(json '[col("ID"), col("VARIANT")] | transpose | sort')
expect_json 'col("PROVIDERS") | map(split(", ")) | add | unique == ["llama-cpp-spark", "ollama-spark"]' \
    "--hardware spark lists the providers on hosts with that profile"
gcli model recommend --host spark -o json
expect_rc 0
expect_json "([col(\"ID\"), col(\"VARIANT\")] | transpose | sort) == $by_hardware" \
    "--host spark recommends the same models and variants"

step "a second run changes nothing"
rm -rf "$E2E_TMP/converted"
cp -R "$GRANITE_CLI_HOME" "$E2E_TMP/converted"
convert "$GRANITE_CLI_HOME"
expect_rc 0
expect_same_tree "$E2E_TMP/converted" "$GRANITE_CLI_HOME"

finish
