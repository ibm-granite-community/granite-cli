#!/usr/bin/env bash
# Spec 0033 use case 19: a provider file written by spec 0027, with
# `hardware:`, does not load, and the error names the file and the release
# notes. scripts/convert-0027-hosts.sh writes the hosts and rewrites the
# providers; estimates stay the same, and a second run changes nothing.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

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
expect_json 'messages("error") | any(contains("/releases"))' "points at the release notes"
expect_json 'messages("error") | any(contains("scripts/convert-0027-hosts.sh"))' "names the script"

step "the script converts the configuration"
use_fixture spec0027
convert "$GRANITE_CLI_HOME"
expect_rc 0
expect_file "$GRANITE_CLI_HOME/hosts/spark.yaml"
expect_file "$GRANITE_CLI_HOME/hosts/card.yaml"
expect_file "$GRANITE_CLI_HOME/hosts/card-2.yaml"
expect_file "$GRANITE_CLI_HOME/hosts/card-3.yaml"
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
expect_json '[row("llama-cpp-card-a").HOST, row("llama-cpp-card-b").HOST, row("llama-cpp-card-v6").HOST] | sort == ["card", "card-2", "card-3"]' \
    "one host per address for one profile, IPv6 included"
expect_json 'col("NOTES") | all(. == "")' "no notes"
if grep -q '^address: spark.lan$' "$GRANITE_CLI_HOME/hosts/spark.yaml"; then
    pass "spark's address is taken from the base URLs"
else fail "spark's address: $(cat "$GRANITE_CLI_HOME/hosts/spark.yaml")"; fi

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

step "a link that is not a plain id is reported and left alone"
fresh
printf 'provider_id: odd\ntype: llama-cpp\nconfig:\n  base_url: http://odd.lan:8080\nhardware: ../x\n' >"$P/odd.yaml"
printf 'provider_id: empty\ntype: llama-cpp\nconfig:\n  base_url: http://e.lan:8080\nhardware: ""\n' >"$P/empty.yaml"
printf 'provider_id: star\ntype: llama-cpp\nconfig:\n  base_url: http://*.lan:8080\nhardware: card\n' >"$P/star.yaml"
printf 'provider_id: ok\ntype: ollama\nconfig:\n  base_url: http://localhost:9\nhardware: local # comment\r\n' >"$P/ok.yaml"
chmod 600 "$P/ok.yaml"
convert "$GRANITE_CLI_HOME"
expect_rc 1
expect_no_file "$GRANITE_CLI_HOME/x.yaml"
expect_no_file "$GRANITE_CLI_HOME/hosts/.yaml"
expect_no_file "$GRANITE_CLI_HOME/hosts/card.yaml"
if grep -q '^hardware: \.\./x$' "$P/odd.yaml" && grep -q '^hardware: ""$' "$P/empty.yaml" &&
    grep -q '^hardware: card$' "$P/star.yaml"; then
    pass "the odd link, the empty link and the bad address are unchanged"
else fail "a skipped provider changed"; fi
if grep -q '^host: local$' "$P/ok.yaml"; then pass "the other provider is converted"; else
    fail "ok.yaml: $(cat "$P/ok.yaml")"; fi
if [ "$(ls -l "$P/ok.yaml" | cut -c1-10)" = "-rw-------" ]; then pass "its permissions are kept"; else
    fail "permissions: $(ls -l "$P/ok.yaml")"; fi
if ls "$P" | grep -qv '\.yaml$'; then fail "temporary files left: $(ls "$P")"; else
    pass "no temporary file is left"; fi

step "a second run changes nothing"
use_fixture spec0027
convert "$GRANITE_CLI_HOME"
rm -rf "$E2E_TMP/converted"
cp -R "$GRANITE_CLI_HOME" "$E2E_TMP/converted"
convert "$GRANITE_CLI_HOME"
expect_rc 0
expect_same_tree "$E2E_TMP/converted" "$GRANITE_CLI_HOME"

finish
