#!/usr/bin/env bash
# update-capability-table.sh
#
# Parses launcher and capability source files to generate the
# Capability Support matrix in README.md.
#
# Usage:
#   ./scripts/update-capability-table.sh           # update README.md in place
#   ./scripts/update-capability-table.sh --check   # exit 1 if README.md is stale
#
# The script rewrites the block between:
#   <!-- capability-table-start -->
#   <!-- capability-table-end -->

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

CHECK_MODE=false
if [[ "${1:-}" == "--check" ]]; then
    CHECK_MODE=true
fi

README="README.md"
LAUNCHERS_MOD="src/launchers/mod.rs"
CAPABILITIES_MOD="src/capabilities/mod.rs"
LAUNCHERS_DIR="src/launchers"
CAPABILITIES_DIR="src/capabilities"

# ---------------------------------------------------------------------------
# Helper: extract quoted id from a factory.register line
#   factory.register::<FooType>("some-id");  =>  some-id
# ---------------------------------------------------------------------------
extract_id() {
    echo "$1" | sed 's/.*register::<[^>]*>("\([^"]*\)").*/\1/'
}

# ---------------------------------------------------------------------------
# Parallel-array helpers (bash 3.x associative-array replacement)
#
# Each "map" is represented as two indexed arrays — one for keys, one for
# values — plus a lookup function that iterates them.
#
# Usage:
#   map_set  <keys_var> <vals_var> <key> <value>
#   map_get  <keys_var> <vals_var> <key>   # prints value or empty string
#
# Values are passed to `eval` by reference (`$_val`), never spliced into the
# eval string, so quotes, `$` and backticks in a description are safe.
# ---------------------------------------------------------------------------
map_set() {
    local _keys_var="$1" _vals_var="$2" _key="$3" _val="$4"
    local _i
    # Overwrite if key already exists
    eval "local _len=\${#${_keys_var}[@]}"
    for (( _i=0; _i<_len; _i++ )); do
        eval "local _k=\"\${${_keys_var}[${_i}]}\""
        if [[ "$_k" == "$_key" ]]; then
            eval "${_vals_var}[${_i}]=\$_val"
            return
        fi
    done
    # New entry
    eval "${_keys_var}+=(\"\$_key\")"
    eval "${_vals_var}+=(\"\$_val\")"
}

map_get() {
    local _keys_var="$1" _vals_var="$2" _key="$3"
    local _i
    eval "local _len=\${#${_keys_var}[@]}"
    for (( _i=0; _i<_len; _i++ )); do
        eval "local _k=\"\${${_keys_var}[${_i}]}\""
        if [[ "$_k" == "$_key" ]]; then
            eval "printf '%s' \"\${${_vals_var}[${_i}]}\""
            return
        fi
    done
    # Key not found — return empty
    printf '%s' ''
}

# ---------------------------------------------------------------------------
# Source discovery: nothing below knows about any particular launcher or
# capability. Everything is read from the code the registry points at.
# ---------------------------------------------------------------------------

# The module a `factory.register::<module::Type>("id")` line names.
#   factory.register::<speech_to_text::SpeechToTextCapability>("x")  =>  speech_to_text
extract_module() {
    echo "$1" | sed -n 's/.*register::<\([A-Za-z0-9_]*\)::.*/\1/p'
}

# The source file for a module under <dir>: <dir>/<module>.rs or <dir>/<module>/mod.rs.
# Prints nothing when neither exists.
module_src_file() {
    local dir="$1" module="$2"
    if [[ -f "${dir}/${module}.rs" ]]; then
        printf '%s' "${dir}/${module}.rs"
    elif [[ -f "${dir}/${module}/mod.rs" ]]; then
        printf '%s' "${dir}/${module}/mod.rs"
    fi
}

# Prints the `BindingType` variants named by a `supported_*:` field, in
# source order, space separated. Handles a one-line `HashSet::from([..])`
# as well as one spread over several lines.
#   $1 file   $2 field name (supported_capabilities | supported_binding_types)
#   $3 optional: a `macro_rules!` name -- read only that macro's body
#      (otherwise read from the `fn metadata() -> ...` that follows)
binding_types_in() {
    awk -v field="$2" -v macro="${3:-}" '
        macro != "" && $0 ~ ("macro_rules! " macro "([^A-Za-z0-9_]|$)") { active = 1; next }
        macro != "" && /macro_rules!/ { active = 0 }
        macro == "" && /fn metadata\(\) -> / && !/-> \$/ { active = 1 }
        active && index($0, field ":") { collecting = 1 }
        collecting {
            line = $0
            while (match(line, /BindingType::[A-Za-z0-9_]+/)) {
                tok = substr(line, RSTART + 13, RLENGTH - 13)
                printf "%s%s", (n++ ? " " : ""), tok
                line = substr(line, RSTART + RLENGTH)
            }
            if ($0 ~ /\]/) { exit }
        }
    ' "$1"
}

# Prints the capability `name` or `description` declared in a source file,
# reading (in order of preference) a `declare_sub_agent_*!(..)` call, then
# `fn metadata()`. A value may be a string literal or the name of a
# `const X: &str = "..";` in the same file.
#   $1 file   $2 field (name | description)
metadata_field_in() {
    awk -v field="$2" '
        # Pass 1: collect `const NAME: &str = "..."` (value may be on the next line).
        FNR == NR {
            if (pending != "" && match($0, /"[^"]*"/)) {
                consts[pending] = substr($0, RSTART + 1, RLENGTH - 2); pending = ""
            } else if (match($0, /^(pub(\([a-z]+\))? )?const [A-Za-z0-9_]+: &str =/)) {
                head = substr($0, RSTART, RLENGTH)
                sub(/^(pub(\([a-z]+\))? )?const /, "", head); sub(/:.*/, "", head)
                if (match($0, /"[^"]*"/)) consts[head] = substr($0, RSTART + 1, RLENGTH - 2)
                else pending = head
            }
            next
        }
        # The value on a `key: ...` line: a literal, else a const name.
        function value_of(s,   id) {
            if (match(s, /"[^"]*"/)) return substr(s, RSTART + 1, RLENGTH - 2)
            sub(/^[^:]*:[[:space:]]*/, "", s)
            if (match(s, /^[A-Za-z_][A-Za-z0-9_]*/)) {
                id = substr(s, RSTART, RLENGTH)
                if (id in consts) return consts[id]
            }
            return ""
        }
        # declare_sub_agent_*!( Type Config "Name"; "Description"; ...
        /^declare_sub_agent_[a-z_]+!\(/ { inv = 1; q = 0; next }
        inv && /^\)/ { inv = 0 }
        inv && /^[[:space:]]*("|[A-Za-z_])/ && /;[[:space:]]*$/ {
            q++
            if ((field == "name" && q == 1) || (field == "description" && q == 2)) {
                v = value_of($0); if (v != "") { print v; exit }
            }
        }
        # fn metadata() -> CapabilityMetadata { name: ..., description: ..., }
        # Macro templates (`-> $crate::..`, `name: $name_cap`) are not metadata.
        /fn metadata\(\) -> / && !/-> \$/ { meta = 1; next }
        meta && want {
            v = value_of($0); if (v != "") { print v; exit }
        }
        meta && $0 ~ ("^[[:space:]]*" field ":[[:space:]]*[^$[:space:]]") {
            v = value_of($0)
            if (v != "") { print v; exit }
            want = 1
        }
    ' "$1" "$1"
}

# The file defining the `macro_rules! <name>` a capability source invokes
# (`declare_sub_agent_basic!(..)`), or nothing for a file with its own metadata.
macro_invoked_in() {
    sed -n 's/^\(declare_[a-z_]*\)!(.*/\1/p' "$1" | head -1
}

macro_defined_in() {
    grep -rlE "macro_rules! $1([^A-Za-z0-9_]|$)" "$CAPABILITIES_DIR" 2>/dev/null | head -1
}

# ---------------------------------------------------------------------------
# 1. Extract launcher ids and their modules (in registration order)
# ---------------------------------------------------------------------------
launcher_ids=()
launcher_mod_keys=()
launcher_mod_vals=()
while IFS= read -r line; do
    id=$(extract_id "$line")
    [[ -n "$id" && "$id" != "$line" ]] || continue
    launcher_ids+=("$id")
    map_set launcher_mod_keys launcher_mod_vals "$id" "$(extract_module "$line")"
done < <(grep 'factory\.register' "$LAUNCHERS_MOD")

# ---------------------------------------------------------------------------
# 2. Extract capability ids and their modules (in registration order)
# ---------------------------------------------------------------------------
capability_ids=()
cap_mod_keys=()
cap_mod_vals=()
while IFS= read -r line; do
    id=$(extract_id "$line")
    [[ -n "$id" && "$id" != "$line" ]] || continue
    capability_ids+=("$id")
    map_set cap_mod_keys cap_mod_vals "$id" "$(extract_module "$line")"
done < <(grep 'factory\.register' "$CAPABILITIES_MOD")

# ---------------------------------------------------------------------------
# 3. For each launcher, extract its supported BindingTypes
#    Source: supported_capabilities: HashSet::from([BindingType::X, ...])
# ---------------------------------------------------------------------------
launcher_type_keys=()   # parallel arrays: launcher_type_keys[i] / launcher_type_vals[i]
launcher_type_vals=()

for id in "${launcher_ids[@]}"; do
    src=$(module_src_file "$LAUNCHERS_DIR" "$(map_get launcher_mod_keys launcher_mod_vals "$id")")
    if [[ -z "$src" ]]; then
        echo "WARNING: source file not found for launcher '${id}'" >&2
        map_set launcher_type_keys launcher_type_vals "$id" ""
        continue
    fi
    map_set launcher_type_keys launcher_type_vals "$id" \
        "$(binding_types_in "$src" supported_capabilities)"
done

# ---------------------------------------------------------------------------
# 4. Capability metadata: binding types, display name, description
#    All read from the module the registry names -- see the helpers above.
# ---------------------------------------------------------------------------
cap_btype_keys=()   # parallel arrays for the binding types a capability fills
cap_btype_vals=()

cap_dname_keys=()   # parallel arrays for cap_display_name
cap_dname_vals=()

cap_desc_keys=()    # parallel arrays for cap_description
cap_desc_vals=()

for id in "${capability_ids[@]}"; do
    src=$(module_src_file "$CAPABILITIES_DIR" "$(map_get cap_mod_keys cap_mod_vals "$id")")
    if [[ -z "$src" ]]; then
        echo "WARNING: source file not found for capability '${id}'" >&2
        map_set cap_btype_keys cap_btype_vals "$id" ""
        map_set cap_desc_keys  cap_desc_vals  "$id" ""
        map_set cap_dname_keys cap_dname_vals "$id" "$id"
        continue
    fi

    # A capability declared through a macro (declare_sub_agent_*!) gets its
    # binding types from the macro's definition; one with its own
    # `fn metadata()` states them there.
    macro=$(macro_invoked_in "$src")
    if [[ -n "$macro" ]]; then
        macro_src=$(macro_defined_in "$macro")
        btypes=$([[ -n "$macro_src" ]] && binding_types_in "$macro_src" supported_binding_types "$macro")
    else
        btypes=$(binding_types_in "$src" supported_binding_types)
    fi
    name=$(metadata_field_in "$src" name)
    desc=$(metadata_field_in "$src" description)

    [[ -n "$btypes" ]] || echo "WARNING: no supported_binding_types found for capability '${id}' (${src})" >&2
    [[ -n "$name" ]]   || echo "WARNING: no metadata name found for capability '${id}' (${src})" >&2
    [[ -n "$desc" ]]   || echo "WARNING: no metadata description found for capability '${id}' (${src})" >&2

    map_set cap_btype_keys cap_btype_vals "$id" "$btypes"
    map_set cap_dname_keys cap_dname_vals "$id" "${name:-$id}"
    map_set cap_desc_keys  cap_desc_vals  "$id" "$desc"
done

# ---------------------------------------------------------------------------
# 4b. Binding types, in declaration order, from the `define_bindings!` block
# ---------------------------------------------------------------------------
BINDINGS_SRC="${CAPABILITIES_DIR}/base.rs"
binding_type_names=()
while IFS= read -r name; do
    [[ -n "$name" ]] && binding_type_names+=("$name")
done < <(awk '
    /^define_bindings! \{/ { in_block = 1; next }
    in_block && /^\}/ { exit }
    in_block && /^    [A-Za-z0-9_]+ \{/ { sub(/^    /, ""); sub(/ .*/, ""); print }
' "$BINDINGS_SRC")

# What each binding type means, as prose. A binding type with no entry here
# is still listed (by name) so a new one is never silently left out.
binding_type_blurb() {
    case "$1" in
        AgentModel)  printf '%s' "passes a model's connection details directly to the launcher" ;;
        Mcp)         printf '%s' "exposes the capability as an MCP server the launcher can call" ;;
        ModelProvider) printf '%s' "populates the launcher's model provider config with one entry per granite-cli provider, routed through the local proxy" ;;
        SubAgent)    printf '%s' "exposes the capability as a named sub-agent the launcher can delegate to" ;;
        VoiceInput)  printf '%s' "passes a transcription model's connection details to the launcher's voice input mode" ;;
        *)           printf '%s' "" ;;
    esac
}

# ---------------------------------------------------------------------------
# 5. Generate the replacement block
# ---------------------------------------------------------------------------

# True when the two space-separated lists of binding types share a member.
shares_binding_type() {
    local want have
    for want in $1; do
        for have in $2; do
            [[ "$want" == "$have" ]] && return 0
        done
    done
    return 1
}

generate_block() {
    echo "<!-- capability-table-start -->"
    echo ""

    # Header
    header="| Capability | Type"
    separator="|---|---"
    for id in "${launcher_ids[@]}"; do
        header+=" | \`${id}\`"
        separator+="|:---:"
    done
    echo "${header} |"
    echo "${separator}|"

    # Data rows
    for cap_id in "${capability_ids[@]}"; do
        btypes=$(map_get cap_btype_keys cap_btype_vals "$cap_id")
        display=$(map_get cap_dname_keys cap_dname_vals "$cap_id")
        [[ -z "$display" ]] && display="$cap_id"
        row="| \`${cap_id}\` | ${display}"
        for l_id in "${launcher_ids[@]}"; do
            ltypes=$(map_get launcher_type_keys launcher_type_vals "$l_id")
            if shares_binding_type "$btypes" "$ltypes"; then
                row+=" | ✅"
            else
                row+=" | ❌"
            fi
        done
        echo "${row} |"
    done

    echo ""
    echo "**Capability descriptions:**"
    echo ""
    echo "| Capability | Description |"
    echo "|---|---|"
    for cap_id in "${capability_ids[@]}"; do
        desc=$(map_get cap_desc_keys cap_desc_vals "$cap_id")
        echo "| \`${cap_id}\` | ${desc} |"
    done

    echo ""
    echo "**Binding types** used to determine compatibility:"
    for bt in "${binding_type_names[@]}"; do
        blurb=$(binding_type_blurb "$bt")
        if [[ -n "$blurb" ]]; then
            echo "- **${bt}** — ${blurb}"
        else
            echo "- **${bt}**"
        fi
    done
    echo ""
    echo "<!-- capability-table-end -->"
}

BLOCK_FILE=$(mktemp)
generate_block > "$BLOCK_FILE"

# ---------------------------------------------------------------------------
# 6. Replace sentinel block in README.md using a temp file for the block
# ---------------------------------------------------------------------------
UPDATED_FILE=$(mktemp)
awk -v block_file="$BLOCK_FILE" '
    /<!-- capability-table-start -->/ {
        in_block = 1
        while ((getline line < block_file) > 0) print line
        next
    }
    /<!-- capability-table-end -->/ { in_block = 0; next }
    !in_block { print }
' "$README" > "$UPDATED_FILE"

if $CHECK_MODE; then
    if diff -q "$README" "$UPDATED_FILE" > /dev/null 2>&1; then
        echo "✅ Capability table in ${README} is up to date."
        rm -f "$BLOCK_FILE" "$UPDATED_FILE"
        exit 0
    else
        echo "❌ Capability table in ${README} is out of date." >&2
        echo "   Run ./scripts/update-capability-table.sh to regenerate it." >&2
        diff "$README" "$UPDATED_FILE" >&2 || true
        rm -f "$BLOCK_FILE" "$UPDATED_FILE"
        exit 1
    fi
else
    cp "$UPDATED_FILE" "$README"
    rm -f "$BLOCK_FILE" "$UPDATED_FILE"
    echo "✅ ${README} capability table updated."
fi
