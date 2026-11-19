#!/usr/bin/env bash
# Convert a granite-cli configuration written before hosts (spec 0027) to
# hosts (spec 0033).
#
# Usage:
#   scripts/convert-0027-hosts.sh [CONFIG_DIR]
#
# CONFIG_DIR is the granite-cli configuration directory, the one that holds
# providers/ and hardware/. It defaults to $GRANITE_CLI_HOME. The error
# granite-cli gives for a provider file that still has `hardware:` names
# that file, under CONFIG_DIR/providers/.
#
# A provider saved by spec 0027 links to a hardware profile with
# `hardware: <id>`. This script rewrites that line as `host: <id>`:
#
#   - `hardware: local` and `hardware: unspecified` become `host: local` and
#     `host: unspecified`.
#   - For each other hardware profile, it writes a host under hosts/ with
#     that profile as its one device and the address from the providers'
#     base URLs. The host has the profile's id. When providers linked to one
#     profile use different addresses, each address gets its own host:
#     `<id>`, `<id>-2`, `<id>-3`, in the order of the provider file names.
#
# It reads files as granite-cli writes them: `hardware:` at the start of a
# line, and `base_url:` indented under `config:`. A comment after a value and
# Windows line endings are ignored. A provider whose link is empty, or names
# an id that is not a plain name (letters, digits, `.`, `_`, `-`), or whose
# base URL has no usable host name or IP address, is left as it is and
# reported; the script then exits 1 after converting the others.
#
# It prints each host it writes and each provider it rewrites. Run a second
# time, it finds no `hardware:` line and changes nothing. Provider files are
# rewritten in place, keeping their permissions; keep a copy of CONFIG_DIR if
# you want to go back.
#
# Written for bash 3.2 (the macOS /bin/bash) and later, with sed and grep.

set -eu

dir=${1:-${GRANITE_CLI_HOME:-}}
if [ -z "$dir" ]; then
    echo "usage: $0 CONFIG_DIR (or set GRANITE_CLI_HOME)" >&2
    exit 2
fi
if [ ! -d "$dir/providers" ]; then
    echo "$dir has no providers directory; nothing to convert" >&2
    exit 0
fi

hosts_dir="$dir/hosts"
tmp=""
trap 'if [ -n "$tmp" ]; then rm -f "$tmp"; fi' EXIT

# Removes a carriage return, a trailing comment, surrounding blanks and
# quotes from a YAML scalar on stdin.
clean() {
    sed -e 's/\r$//' -e 's/^[[:space:]]*#.*$//' -e 's/[[:space:]]#.*$//' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' \
        -e "s/^[\"']//" -e "s/[\"']\$//"
}

# The value of a top-level `KEY: value` line in FILE.
top_value() {
    sed -n "s/^$1:[[:space:]]*//p" "$2" | head -n 1 | clean
}

# The host part of the provider's base URL: scheme, user, port and path
# removed, brackets kept for an IPv6 address. Empty when there is none.
address_of() {
    local url
    url=$(sed -n 's/^[[:space:]][[:space:]]*base_url:[[:space:]]*//p' "$1" | head -n 1 | clean)
    url=${url#*://}
    url=${url%%/*}
    url=${url##*@}
    case "$url" in
        \[*) url=${url%%]*}"]" ;;
        *) url=${url%%:*} ;;
    esac
    printf '%s' "$url"
}

# Whether ADDRESS is a host name, an IPv4 address or a bracketed IPv6
# address that can be written in a host file as it is.
plain_address() {
    printf '%s' "$1" | grep -Eq '^([A-Za-z0-9][A-Za-z0-9.-]*|\[[0-9A-Fa-f:.]+\])$'
}

# Whether ID is a plain name granite-cli can use as a file name.
plain_id() {
    printf '%s' "$1" | grep -Eq '^[A-Za-z0-9][A-Za-z0-9._-]*$'
}

# The hardware profile ids a host file lists, one per line.
devices_of() {
    sed -n '/^hardware:/,$p' "$1" | sed -n 's/^-[[:space:]]*//p' | clean
}

# Prints the id of the host for PROFILE at ADDRESS: an existing host file
# with that address and that profile as its only device, or a new one,
# which it writes.
host_for() {
    local profile=$1 address=$2 id n f
    for f in "$hosts_dir"/*.yaml; do
        [ -e "$f" ] || continue
        if [ "$(top_value address "$f")" = "$address" ] &&
            [ "$(devices_of "$f" | tr '\n' ' ')" = "$profile " ]; then
            top_value host_id "$f"
            return
        fi
    done
    id=$profile
    n=1
    while [ -e "$hosts_dir/$id.yaml" ] || [ "$id" = local ] || [ "$id" = unspecified ]; do
        n=$((n + 1))
        id="$profile-$n"
    done
    mkdir -p "$hosts_dir"
    # An IPv6 address in brackets would read as a YAML list; quote it.
    printf "host_id: %s\naddress: '%s'\nhardware:\n- %s\n" "$id" "$address" "$profile" |
        sed "s/^address: '\([^[].*\)'\$/address: \1/" >"$hosts_dir/$id.yaml"
    echo "wrote host '$id' (address $address, hardware $profile): hosts/$id.yaml" >&2
    printf '%s' "$id"
}

changed=0
skipped=0
for file in "$dir"/providers/*.yaml; do
    [ -e "$file" ] || continue
    grep -q '^hardware:' "$file" || continue
    name=$(basename "$file" .yaml)
    link=$(top_value hardware "$file")
    if [ -z "$link" ] || ! plain_id "$link"; then
        echo "skipped provider $name: 'hardware: $link' is not a hardware profile id; edit providers/$name.yaml by hand" >&2
        skipped=$((skipped + 1))
        continue
    fi
    case "$link" in
        local | unspecified) host=$link ;;
        *)
            address=$(address_of "$file")
            if [ -z "$address" ]; then
                echo "note: provider $name has no base_url; host for '$link' gets the address localhost" >&2
                address=localhost
            fi
            if ! plain_address "$address"; then
                echo "skipped provider $name: '$address' in its base_url is not a host name or IP address; edit providers/$name.yaml by hand" >&2
                skipped=$((skipped + 1))
                continue
            fi
            host=$(host_for "$link" "$address")
            ;;
    esac
    tmp=$(mktemp "$file.XXXXXX")
    sed "s/^hardware:.*\$/host: $host/" "$file" >"$tmp"
    # Writing through the existing file keeps its permissions, and the
    # target of a symbolic link.
    cat "$tmp" >"$file"
    rm -f "$tmp"
    tmp=""
    echo "rewrote provider $name: hardware: $link -> host: $host" >&2
    changed=$((changed + 1))
done

if [ "$changed" -eq 0 ] && [ "$skipped" -eq 0 ]; then
    echo "no provider has 'hardware:'; nothing to convert" >&2
else
    echo "converted $changed provider(s), skipped $skipped" >&2
fi
if [ "$skipped" -gt 0 ]; then
    exit 1
fi
