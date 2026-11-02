#!/usr/bin/env bash
set -euo pipefail

# Generates YAML from model metadata JSON
# Input: models.json
# Output: models.yaml

MODELS_FILE="${1:-data/models.json}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UTILS_DIR="${SCRIPT_DIR}/utils"

if [ ! -f "$MODELS_FILE" ]; then
    echo "Error: Models file not found: $MODELS_FILE" >&2
    exit 1
fi

# Check for JSON tool (jq or python)
if ! command -v jq &> /dev/null; then
    if ! command -v python3 &> /dev/null && ! command -v python &> /dev/null; then
        echo "Error: Neither jq nor Python is available. Please install one of them." >&2
        exit 1
    fi
fi

generate_model_entry() {
    local model="$1"

    # Extract fields
    local repo=$(echo "$model" | jq -r '.repo')
    local id=$(basename "$repo")
    local family=$(echo "$model" | jq -r '.family')
    local version=$(echo "$model" | jq -r '.version')
    local size=$(echo "$model" | jq -r '.size')
    local context_length=$(echo "$model" | jq -r '.context_length')
    local model_type=$(echo "$model" | jq -r '.model_type')
    local native_dtype=$(echo "$model" | jq -r '.native_dtype // "unknown"')
    local architecture=$(echo "$model" | jq -c '.architecture')
    local variants=$(echo "$model" | jq -c '
        .variants + (
            if .ollama_info and (.ollama_info | length > 0) then
                [.ollama_info[] | {format: "Ollama", url: .url, precision: .precision, size_gb: .size_gb}]
            else
                []
            end
        ) + (
            if .lmstudio_info and (.lmstudio_info | length > 0) then
                [.lmstudio_info[] | {format: "LMStudio", url: .url, precision: .precision, size_gb: .size_gb}]
            else
                []
            end
        ) + (
            if .openrouter_info and (.openrouter_info | length > 0) then
                [.openrouter_info[] | {format: "OpenRouter", url: .url, precision: "", size_gb: null}]
            else
                []
            end
        )
    ')

    # Infer supported functions
    local functions=$(echo "$model" | "${UTILS_DIR}/infer-functions.sh")

    # Generate description
    local description=$(echo "$model" | jq -r .description || echo "")
    if [ "$description" == "" ]; then
        description=$("${UTILS_DIR}/format-description.sh" "$id" "$family" "$version" "$model_type")
    fi
    description="$(echo "$description" | sed 's,^,    ,g')" # Indent 4 spaces for multiline yaml

    # Suggest tags
    local tags=$(echo "$model" | "${UTILS_DIR}/suggest-tags.sh")

    # Generate YAML entry
    cat <<EOF
- id: ${id}
  family: "${family}"
  version: "${version}"
  size: ${size}
  context_length: ${context_length}
  model_type: "${model_type}"
  huggingface_repo: "${repo}"
  native_dtype: "${native_dtype}"
  architecture:
EOF
    echo "$architecture" | jq -r '
        "    num_hidden_layers: \(.num_hidden_layers)",
        "    hidden_size: \(.hidden_size)",
        "    num_attention_heads: \(.num_attention_heads)",
        "    num_key_value_heads: \(.num_key_value_heads)",
        "    head_dim: \(.head_dim)",
        "    layer_types:"'
    echo "$architecture" | jq -r '.layer_types[] |
        "      - kind: \(.kind)\n        count: \(.count)"
        + (if .window then "\n        window: \(.window)" else "" end)
        + (if .mamba then "\n        mamba: { d_conv: \(.mamba.d_conv), d_state: \(.mamba.d_state), d_inner: \(.mamba.d_inner), n_groups: \(.mamba.n_groups) }" else "" end)'
    cat <<EOF
  supported_functions:
EOF
    echo "$functions" | jq -r '.[] | "    - \(.)"'
    local efforts=$(echo "$model" | jq -c '.supported_reasoning_efforts // []')
    if [ "$efforts" != "[]" ]; then
        echo "  supported_reasoning_efforts:"
        echo "$efforts" | jq -r '.[] | "    - \(.)"'
    fi
    cat <<EOF
  variants:
EOF
    echo "$variants" | jq -r '.[] | "    - format: \(.format)\n      precision: \(.precision // null)\n      size_gb: \(.size_gb // null)\n      url: \"\(.url)\""'
    cat <<EOF
  description: |
${description}
  tags:
EOF
    echo "$tags" | jq -r '.[] | "    - \(.)"'
    echo ""
}

# Header
echo "# Granite Models Catalog"
echo "# Generated: $(date -u +"%Y-%m-%d %H:%M:%S UTC")"
echo "# Source: HuggingFace ibm-granite organization"
echo ""

# Process all models
jq -r -c '. | sort_by(.repo) | .[]' "$MODELS_FILE" | while read -r model; do
    generate_model_entry "$model"
done
