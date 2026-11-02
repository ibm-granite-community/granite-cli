#!/usr/bin/env bash
# `setup --auto --skip-pull` with a running Ollama configures a complete,
# consistent setup. Needs Ollama on localhost:11434; skipped otherwise.
#
# setup works back from launchers: it configures the capabilities a
# launcher on PATH recommends and the models those need. A stub opencode on
# PATH gives it a launcher on any machine. Which capabilities and models it
# picks still depends on the detected memory, so the checks name none of
# them; a machine with too little memory for any recommended model, below
# about 16 GB without a GPU, fails the model and capability checks.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

require_url http://localhost:11434/api/tags

fresh
fresh_home
stub_bin opencode

step "setup --auto --skip-pull"
gcli setup --auto --skip-pull
expect_rc 0
expect_out_contains "=== Setup Complete ==="

step "ollama is configured and every model is bound to a configured provider"
gcli provider list -o json
expect_json 'ids | index("ollama") != null' "ollama provider"
providers=$(json 'ids')
gcli model list -o json
expect_json 'ids | length > 0' "at least one model"
expect_json "(col(\"PROVIDER\") - $providers) == []" "every PROVIDER is configured"

step "the stub launcher and at least one capability are configured"
gcli launcher list -o json
expect_json 'ids | index("opencode") != null' "opencode launcher"
gcli capability list -o json
expect_json 'ids | length > 0' "at least one capability"

step "no list has notes"
for kind in provider model capability launcher; do
    gcli "$kind" list -o json
    expect_json 'col("NOTES") | unique | . == [""] or . == []' "$kind list"
done

step "setup wrote nothing under HOME"
if [ -z "$(ls -A "$HOME")" ]; then pass "HOME is empty"; else fail "HOME has files: $(ls -A "$HOME")"; fi

finish
