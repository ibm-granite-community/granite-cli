#!/usr/bin/env bash
# Spec 0033 use cases 4 and 5: `model recommend --host` estimates fit on a
# host's devices. On `box`, two rtx-4090 cards of 24 GB, llama-cpp-box and
# vllm-box (splits_across_devices: true) use both cards, and openai-box,
# which keeps the openai-compatible default, uses one.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

skip "spec 0033 sub-task 4"

# The rows of the first table as objects keyed by header.
# shellcheck disable=SC2016 # jq variables
ROWS='(table as $t | $t.rows | map([$t.headers, .] | transpose | map({(.[0]): .[1]}) | add))'
on() { echo "$ROWS | map(select(.PROVIDERS | split(\", \") | index(\"$1\")))"; }

step "model recommend --host box"
use_fixture hosts
gcli model recommend --host box -o json
expect_rc 0
expect_json 'table.title | contains("host '"'"'box'"'"'")' "the table names the host"
expect_json "$(on llama-cpp-box) | length > 0 and all(.FIT | contains(\"2 × 24 GB\"))" \
    "llama-cpp-box is estimated on both cards"
expect_json "$(on vllm-box) | length > 0 and all(.FIT | contains(\"2 × 24 GB\"))" \
    "vllm-box is estimated on both cards"
expect_json "$(on openai-box) | length > 0 and all(.FIT | (contains(\"24 GB\") and (contains(\"2 ×\") | not)))" \
    "openai-box is estimated on one card"
fitting() { echo "$(on "$1") | map(select(.FIT | startswith(\"None\") | not)) | map(.ID) | unique"; }
expect_json "($(fitting vllm-box) | length) > ($(fitting openai-box) | length)" \
    "more models fit on two cards than on one"

step "--host and --hardware cannot be combined"
gcli model recommend --host box --hardware rtx-4090 -o json
expect_rc 2

step "a host with unknown hardware has no estimate"
gcli model recommend --host lab -o json
expect_rc 0
expect_json 'messages("info") | any(contains("fit not available"))' "says fit is not available"

step "an unknown host exits 1"
gcli model recommend --host nope -o json
expect_rc 1

finish
