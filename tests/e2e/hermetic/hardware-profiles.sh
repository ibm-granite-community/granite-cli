#!/usr/bin/env bash
# The hardware command group and `model recommend --hardware`, run through
# the binary: subcommand and argument routing, exit codes, what a run that
# cannot prompt refuses, the profile files written and removed, and the
# providers `hardware remove --dependents` takes with them. The
# fixture's `local` is manual with no recorded detection, so no result here
# depends on the machine the test runs on. The one step that lets detection
# write `local` checks only which stream each line goes to.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

hw_dir() { echo "$GRANITE_CLI_HOME/hardware"; }

step "read-only commands show the profiles and leave the files as they were"
use_fixture hardware
rm -rf "$E2E_TMP/hardware-before"
cp -R "$(hw_dir)" "$E2E_TMP/hardware-before"
gcli hardware list -o json
expect_rc 0
expect_json 'ids | sort == ["box", "local", "spark"]' "the three profiles"
gcli hardware info box -o json
expect_rc 0
expect_json 'detail("Hardware Profile '"'"'box'"'"'").Type == "custom"' "info box"
gcli hardware catalog --family AMD -o json
expect_rc 0
expect_json 'col("FAMILY") | unique == ["AMD"]' "--family keeps one family"
gcli model recommend --hardware spark -p all -o json
expect_rc 0
expect_json 'table.title | contains("hardware '"'"'spark'"'"'")' "recommends for spark"
if [ -s "$ERR" ]; then fail "stderr is not empty"; else pass "stderr is empty"; fi
expect_same_tree "$E2E_TMP/hardware-before" "$(hw_dir)"

step "an id that is not configured exits 1"
use_fixture hardware
gcli hardware info nope -o json
expect_rc 1
gcli model recommend --hardware nope -o json
expect_rc 1
expect_json 'messages("error") | any(contains("hardware setup custom --id nope"))' "names the command that creates it"
gcli hardware catalog --family nope -o json
expect_rc 1

step "setup writes a profile that info reads back"
use_fixture hardware
gcli hardware setup custom --kind discrete --memory 16 --id gpu -o json
expect_rc 0
expect_file "$(hw_dir)/gpu.yaml"
gcli hardware setup nvidia-rtx-4090 --memory 24 --id card -o json
expect_rc 0
gcli hardware list -o json
expect_json 'ids | sort == ["box", "card", "gpu", "local", "spark"]' "both new profiles are listed"
gcli hardware info gpu -o json
expect_json 'detail("Hardware Profile '"'"'gpu'"'"'").Memory == "discrete 16.0 GB"' "gpu has the figures given"

step "setup that cannot prompt refuses an existing id and missing flags, and writes nothing"
use_fixture hardware
rm -rf "$E2E_TMP/hardware-before"
cp -R "$(hw_dir)" "$E2E_TMP/hardware-before"
gcli hardware setup custom --kind discrete --memory 8 --id box -o json
expect_rc 1
expect_json 'messages("error") | any(contains("already configured"))' "box is already configured"
gcli hardware setup custom --memory 8 --id other -o json
expect_rc 1
expect_json 'messages("error") | any(contains("--kind"))' "names the missing --kind"
gcli hardware setup nvidia-rtx-4090 --memory 7 --id other -o json
expect_rc 1
expect_same_tree "$E2E_TMP/hardware-before" "$(hw_dir)"

step "remove deletes a profile; local cannot be removed"
use_fixture hardware
gcli hardware remove spark -o json
expect_rc 0
expect_no_file "$(hw_dir)/spark.yaml"
gcli hardware remove local -o json
expect_rc 1
expect_file "$(hw_dir)/local.yaml"

step "remove of a profile a provider runs on, without --dependents or with refuse: exits 1 and removes nothing"
use_fixture hardware
gcli hardware remove box -o plain
expect_rc 1
expect_err_contains "--dependents cascade, --dependents leave, or --dependents refuse"
gcli hardware remove box --dependents refuse -o json
expect_rc 1
expect_file "$(hw_dir)/box.yaml"
expect_file "$P/lab.yaml"

step "--dependents leave removes only the profile"
use_fixture hardware
gcli hardware remove box --dependents leave -o json
expect_rc 0
expect_no_file "$(hw_dir)/box.yaml"
expect_file "$P/lab.yaml"

step "--dependents cascade removes the profile and the provider"
use_fixture hardware
gcli hardware remove box --dependents cascade -o json
expect_rc 0
expect_no_file "$(hw_dir)/box.yaml"
expect_no_file "$P/lab.yaml"
expect_file "$(hw_dir)/spark.yaml"

step "markdown output keeps notices and warnings on stderr"
fresh
gcli hardware list -o markdown
expect_rc 0
expect_out_contains "## Configured Hardware"
expect_err_contains "Saved hardware profile 'local' from this machine"
if grep -qF "Saved hardware profile" "$OUT"; then fail "the notice is on stdout"; else
    pass "the notice is not on stdout"
fi
use_fixture hardware
sed 's/mode: manual/mode: bogus/' "$E2E_FIXTURES/hardware/hardware/local.yaml" >"$(hw_dir)/local.yaml"
gcli hardware setup custom --kind discrete --memory 8 --id gpu -o markdown
expect_rc 0
expect_out_contains "Hardware profile 'gpu' configured"
expect_err_contains "Warning: Hardware profile 'local' cannot be read"
if grep -qF "cannot be read" "$OUT"; then fail "the warning is on stdout"; else
    pass "the warning is not on stdout"
fi

finish
