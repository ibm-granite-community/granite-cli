#!/usr/bin/env bash
# Spec 0033 use cases 7, 8, 9 and 18: the host command group run through the
# binary. host setup with --address and --hardware, host list and host info,
# a host with unknown hardware, the HOSTS column of hardware list, an address
# change in a run that cannot prompt, and host remove.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

hosts_dir() { echo "$GRANITE_CLI_HOME/hosts"; }

step "read-only commands list the hosts and leave the files as they were"
use_fixture hosts
# The first load creates the empty per-kind directories; snapshot after it.
gcli host list -o json
rm -rf "$E2E_TMP/before"
cp -R "$GRANITE_CLI_HOME" "$E2E_TMP/before"
gcli host list -o json
expect_rc 0
expect_json 'ids | sort == ["box", "lab", "local", "pgx", "pgx2"]' "every host"
expect_json 'row("box").DEVICES | contains("2 × rtx-4090")' "box has two devices"
expect_json 'row("lab").DEVICES == "unknown"' "lab's hardware is unknown"
expect_json 'row("box").PROVIDERS | split(", ") | sort == ["llama-cpp-box", "openai-box", "vllm-box"]' \
    "box's providers"
gcli host info pgx -o json
expect_rc 0
expect_json 'detail("Host '"'"'pgx'"'"'").Address == "pgx.lan"' "pgx's address"
expect_json 'detail("Host '"'"'pgx'"'"'").Providers == "llama-cpp-pgx"' "pgx's providers"
gcli hardware list -o json
expect_rc 0
expect_json 'row("dgx-spark").HOSTS | split(", ") | sort == ["pgx", "pgx2"]' \
    "dgx-spark is used by pgx and pgx2"
expect_json 'row("rtx-4090").HOSTS == "box"' "rtx-4090 is used by box"
if [ -s "$ERR" ]; then fail "stderr is not empty"; else pass "stderr is empty"; fi
expect_same_tree "$E2E_TMP/before" "$GRANITE_CLI_HOME"

step "host info of an unknown host exits 1"
gcli host info nope -o json
expect_rc 1

step "host setup with --address and --hardware writes a host"
use_fixture hosts
gcli host setup --address gpu.lan --hardware rtx-4090 --hardware rtx-4090 -o json
expect_rc 0
expect_file "$(hosts_dir)/gpu.yaml"
gcli host info gpu -o json
expect_json 'detail("Host '"'"'gpu'"'"'").Address == "gpu.lan"' "the address given"
expect_json 'detail("Host '"'"'gpu'"'"'").Devices | contains("2 × rtx-4090")' "two devices"

step "host setup with an IP address defaults the id to the hardware profile"
gcli host setup --address 10.0.0.30 --hardware dgx-spark -o json
expect_rc 0
expect_file "$(hosts_dir)/dgx-spark.yaml"

step "host setup without --hardware saves a host with unknown hardware"
gcli host setup --address mystery.lan -o json
expect_rc 0
gcli host list -o json
expect_json 'row("mystery").DEVICES == "unknown"' "mystery's hardware is unknown"

step "host setup refuses what it cannot save, and writes nothing"
use_fixture hosts
# The first load creates the empty per-kind directories; snapshot after it.
gcli host list -o json
rm -rf "$E2E_TMP/before"
cp -R "$GRANITE_CLI_HOME" "$E2E_TMP/before"
gcli host setup local --address example.lan -o json
expect_rc 1
gcli host setup bad/id --address bad.lan -o json
expect_rc 1
gcli host setup --address other.lan --hardware local -o json
expect_rc 1
expect_json 'messages("error") | any(contains("local"))' "a local profile is refused"
gcli host setup other --address box.lan -o json
expect_rc 1
expect_json 'messages("error") | any(contains("box"))' "names the host with that address"
gcli host setup other --address other.lan --hardware nope -o json
expect_rc 1
gcli host setup --address box.example --hardware rtx-4090 -o json
expect_rc 1
expect_json 'messages("error") | any(contains("already configured"))' \
    "the default id box is taken, and the run cannot ask for another"
expect_same_tree "$E2E_TMP/before" "$GRANITE_CLI_HOME"

step "naming an existing host changes it; a run that cannot prompt keeps the base URLs, and provider list warns"
use_fixture hosts
gcli host setup pgx --address 10.0.0.12 -o json
expect_rc 0
gcli host info pgx -o json
expect_json 'detail("Host '"'"'pgx'"'"'").Address == "10.0.0.12"' "the new address"
expect_json 'detail("Host '"'"'pgx'"'"'").Devices | contains("dgx-spark")' "the devices are kept"
if grep -q 'base_url: http://pgx.lan:9797' "$P/llama-cpp-pgx.yaml"; then
    pass "the base URL is unchanged"
else fail "the base URL changed"; fi
gcli provider list -o json
expect_rc 0
expect_json 'row("llama-cpp-pgx").NOTES | contains("pgx.lan")' "the mismatch is noted"

step "host remove of a host with providers needs --dependents"
use_fixture hosts
gcli host remove box -o plain
expect_rc 1
expect_err_contains "--dependents cascade, --dependents leave, or --dependents refuse"
gcli host remove box --dependents cascade -o json
expect_rc 0
expect_no_file "$(hosts_dir)/box.yaml"
expect_no_file "$P/llama-cpp-box.yaml"
expect_file "$GRANITE_CLI_HOME/hardware/rtx-4090.yaml"

step "local cannot be removed"
gcli host remove local -o json
expect_rc 1
expect_file "$(hosts_dir)/local.yaml"

finish
