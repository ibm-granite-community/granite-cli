---
name: update-models
description: Use this skill any time you need to update the set of models surfaced to the CLI in resources/models.yaml
---

# Update Models Skill

This skill automates the process of updating `resources/models.yaml` with the latest Granite models from HuggingFace, Ollama, and LM Studio. It combines automated data collection with manual review to ensure accuracy and completeness.

## When to Use This Skill

- New Granite model versions are released
- New model families are added to the IBM Granite organization
- Quantized variants are published
- Ollama adds new Granite model support
- LM Studio adds new Granite model support
- Model metadata needs to be updated or corrected

## Prerequisites

- `curl` - for HTTP requests
- `jq` - for JSON parsing
- `python3` - for parsing LM Studio's embedded model.yaml manifests and deep YAML validation
- Standard Unix tools: `grep`, `sed`, `awk`
- Internet connection to access HuggingFace, Ollama, and LM Studio

## Quick Start

```bash
# Run the complete workflow
.agents/skills/update-models/run-update.sh

# Review and merge
git diff -- resources/models.yaml
```

## Workflow Overview

### Phase 1: Perform Automated Update

```bash
# Run the complete workflow (this can take 5-10 minutes)
.agents/skills/update-models/run-update.sh
```

### Phase 2: Review

```bash
git diff -- resources/models.yaml
```

**Review Checklist:**
- [ ] All model IDs are unique
- [ ] Descriptions are accurate and informative prose
- [ ] Tags are appropriate and consistent
- [ ] Supported functions match model type and use cases
- [ ] Variant sizes are reasonable
- [ ] No duplicate entries
- [ ] New models are properly categorized
- [ ] Existing models are not modified

After your review, direct the user to use ./agent/skills/update-models/review-models.py to walk through the new models registry.

### Phase 3: Build

Before completing, make sure `cargo build` runs cleanly. The `models.yaml` file is consumed at build time to create the static model catalog, and if any of the entries are incorrectly formatted, the build will fail.

## Field Mapping Reference

### ModelMetadata Structure
```rust
pub struct ModelMetadata {
    pub id: String,                              // Unique identifier
    pub family: String,                          // Model family name
    pub version: String,                         // Version number
    pub size: u64,                              // Parameters count
    pub context_length: u64,                    // Max context tokens
    pub model_type: ModelType,                  // Text/Vision/Speech/Embedding
    pub huggingface_repo: String,               // HF repo path
    pub variants: Vec<ModelVariant>,            // Available formats
    pub description: Option<String>,            // Human-readable description
    pub tags: Vec<String>,                      // Categorization tags
    pub supported_functions: Vec<ModelFunction>, // Logical capabilities
}

pub struct ModelVariant {
    pub format: String,    // File format (eg GGUF, safetensors)
    pub precision: String, // Precision label (eg Q4_K_M, bfloat16)
    pub size_gb: f64,      // Size in GB to 3 decimal places
    pub url: String,       // URL to the model
}
```

### MLX Variant Discovery

In addition to the GGUF/safetensors variants found on the `ibm-granite` org itself, `find_mlx_variants` in `scripts/03-fetch-quantized.sh` looks for MLX conversions published by the `mlx-community` org (surfaced by Apple's MLX-based local inference tools, e.g. LM Studio on macOS). Rather than parsing the `<model-name>-mlx-<precision>` naming convention `mlx-community` mostly follows (repo names are inconsistent enough — `-MLX` suffixes, `-DWQ` suffixes, mixed case — to make that fragile), it queries HF's tag-based search directly:

```
${HF_API}/models?filter=base_model:<ibm-granite-repo>&author=mlx-community&limit=100
```

`mlx-community` tags every conversion with a `base_model:<owner>/<repo>` pointing back at the model it was converted from, so this reliably finds every conversion regardless of naming. Results are additionally filtered to `library_name == "mlx"` and non-private.

**Precision** is derived, in order of preference:
1. A repo name suffix matching `(mx|nv)fp\d+` (e.g. `mxfp4`, `nvfp4`, `mxfp8`) - these micro-scaled/NVIDIA float formats carry the same `"<bits>-bit"` tag as plain integer quantization at the same bit width (e.g. both `granite-4.1-30b-mxfp4` and `granite-4.1-30b-4bit` are tagged `4-bit`), so the tag alone can't distinguish them from plain int-N quantization, and neither the tags nor `config.quantization_config` name the scheme anywhere else. The repo name suffix is the only signal available.
2. A `"<bits>-bit"` tag (e.g. `4-bit` → `4bit`) for quantized repos not covered by rule 1.
3. `config.quantization_config.bits` (fallback if untagged).
4. The dtype of the (single) key in `.safetensors.parameters` for full-precision repos (`BF16` → `bfloat16`, `F16`/`FP16` → `float16`, `F32` → `float32`).

A repo where none of these resolve is skipped with a diagnostic on stderr.

**Size** is computed from `.safetensors.parameters` (a dtype → element-count map on the HF model detail endpoint), not `.safetensors.total` — the latter is a *parameter count*, not a byte size, and using it directly (without multiplying by bytes-per-element) undercounts non-8-bit dtypes. Size in GB is `sum(count * bytes_per_element(dtype)) / 1e9` across every entry in the map.

**Mistagged upstream repos**: `mlx-community`'s `base_model` tag is occasionally wrong (a repo named after one Granite variant has been seen tagged as derived from a different one). As a guard, a candidate is only accepted if its own repo name (minus an optional `ibm-` prefix) starts with the base model's name; mismatches are skipped with a diagnostic rather than silently attributed to the wrong model or silently dropped from both.

### Supported Functions Mapping

Functions are inferred from two sources by `scripts/utils/infer-functions.sh`.

Base functions come from `model_type`:

| model_type | supported_functions |
|---|---|
| Text | `[Chat]` |
| Vision | `[Chat, ImageUnderstanding]` |
| Speech | `[Chat, Transcription]` |
| Embedding | `[Embeddings]` |

`ToolCalling` and `Thinking` are detected from the model's HF chat template (fetched by `scripts/utils/fetch-chat-template.sh` and analyzed by `detect_chat_template_signals` in `scripts/02-fetch-all-models.sh`), since that's the actual mechanism inference clients use to gate these behaviors:

- **ToolCalling**: the template has a Jinja `if`/`elif` conditional on the bare `tools` variable (e.g. `{%- if tools %}`). A plain substring match on "tools" is not used — Granite Guardian's template mentions "tools" in role checks and risk-definition prose without actually gating on it, which would otherwise be a false positive.
- **Thinking**: the template has a Jinja `if`/`elif` conditional on a bare `thinking` variable (Granite's own convention, e.g. `{%- elif thinking %}`), or contains `enable_thinking`, `reasoning_content`, or the literal `<think>` token (conventions used by other model families, kept for forward compatibility).

The chat template lives in one of two places depending on model age: a standalone `chat_template.jinja` file at the repo root (newer models), or the `chat_template` field inside `tokenizer_config.json` (older models).

**Granite Guardian is a special case** (`scripts/utils/infer-functions.sh`): Guardian models reuse their upstream instruct model's chat template purely to format the transcript being judged, so any `tools`/`thinking` gate the template happens to contain is an inherited artifact, not a real capability — Guardian models can't perform tool calls, and where Guardian's own thinking output exists (e.g. 4.1) it isn't driven by the chat template and requires client-side code, not automatic detection. Guardian models also can't hold a standard back-and-forth chat. So any model whose `family` is `"Granite Guardian"` gets `supported_functions: [Guardian]` only, overriding the `model_type`/chat-template-derived rules above entirely.

**Granite Switch adapters** (`scripts/utils/infer-functions.sh`): Granite Switch models embed named adapters in their `config.json` under `adapter_names` (carried through as `.config.adapter_names` in `models.json`). Not every Switch model ships the same adapters (e.g. `granite-switch-4.1-3b-preview` has `guardian-core` but no decision adapter), and the names are free-form strings rather than an enum, so they are matched heuristically with a case-insensitive substring check: any name containing `decision` adds `Decision`, and any name containing `guardian` (e.g. `guardian-core`) adds `Guardian`. These are added on top of the base `Chat` function (Switch is not a Guardian-family model, so it keeps its other functions). Models without `adapter_names` are unaffected. If new adapter naming conventions appear, extend the patterns there.

These are still reviewable/correctable during Phase 2 if a model's template is unusual or malformed.

## Error Handling

### API Rate Limits
If you encounter rate limits there are two things to try:

1. Ask the user to provide a huggingface token (`export HF_TOKEN=<token>`)
2. Increase delay between calls (`export HF_REQUEST_DELAY=2`)

### Private Models
When `HF_TOKEN` belongs to an account with org access, the collections API surfaces private/unreleased repos too. `scripts/02-fetch-all-models.sh` skips any collection item with `private: true` by default so unreleased models don't leak into `resources/models.yaml`. To include them anyway (e.g. prepping the catalog ahead of a launch), set `export HF_INCLUDE_PRIVATE=true`.

### Validation Failures
```bash
# Validate generated YAML
./scripts/07-validate-yaml.sh data/models-new.yaml

# Common issues:
# - Duplicate IDs: Rename with qualifier
# - Missing required fields: Add manually
# - Invalid format: Check YAML syntax
```

## Advanced Usage

### Dry Run (No File Changes)
```bash
# Generate YAML without overwriting
./scripts/06-generate-yaml.sh data/models.json | tee data/models-preview.yaml
```

### Scheduled Check

`.github/workflows/models-update.yaml` runs the update daily against `main`
and checks whether `resources/models.yaml` changed. A run whose only change is
the `# Generated:` timestamp passes; any other change fails the run and keeps
the diff as the `models-update-patch` artifact. To turn a failure into a PR:

```bash
gh run download <run-id> -n models-update-patch
git apply models.yaml.patch
# review (Phase 2 above) and run `cargo build` (Phase 3) before committing
```

The workflow reads an optional `HF_TOKEN` repository secret. Without it the
HuggingFace requests are unauthenticated and more likely to be rate limited;
re-run the job or dispatch it from the Actions tab with a larger
`request_delay` input if that happens.

## Troubleshooting

### Issue: Script fails with "jq: command not found"
**Solution:** Request that the user install jq: `brew install jq` (macOS) or `apt-get install jq` (Linux)

### Issue: HuggingFace API returns 403
**Solution:** Check if repo is private or rate-limited. Wait and retry.

### Issue: Ollama search returns no results
**Solution:** Model may not be on Ollama yet. Skip Ollama integration for that model.

### Issue: LM Studio search returns no results
**Solution:** Model may not be in LM Studio's catalog yet (coverage is currently limited to the newest generations). Skip LM Studio integration for that model.

### Issue: LM Studio variant's precision looks wrong or is `null`
LM Studio's model page only exposes `compatibilityTypes` (a packaging *format* like `gguf`, not a quantization) and `minMemoryUsageBytes` — there's no real per-quant precision in the server-rendered page. `scripts/05-query-lmstudio.sh` infers precision by matching `minMemoryUsageBytes` against the closest-sized GGUF variant already fetched from HF's `-GGUF` sibling repo (by `scripts/03-fetch-quantized.sh`, which must run before this step). If a model has no GGUF variants yet, or LM Studio's manifest couldn't be parsed, precision comes back `null` — set it manually during review.

### Issue: A model has no MLX variants even though one exists on hf.co/mlx-community
The base model's `ibm-granite` repo may not be tagged as the `base_model` on the `mlx-community` conversion (upstream metadata issue), or the conversion may not set `library_name: mlx`. Check `https://huggingface.co/mlx-community/models?search=<model-name>` manually and add the variant to `resources/models.yaml` by hand if so.

### Issue: Generated YAML has syntax errors
**Solution:** Run `./scripts/07-validate-yaml.sh` to identify issues. Common causes:
- Unescaped special characters in descriptions
- Missing quotes around strings with colons
- Incorrect indentation

## Script Reference

### Core Scripts

| Script | Purpose | Input | Output |
|--------|---------|-------|--------|
| `01-list-collections.sh` | List HF collections | None | `collections.json` |
| `02-fetch-all-models.sh` | Fetch model metadata | `collections.json` | `models.json` |
| `03-fetch-quantized.sh` | Add quantized variants | `models.json` | Enriched `models.json` |
| `04-01-query-ollama.sh` | Add Ollama info | `models.json` | Enriched `models.json` |
| `04-02-query-lmstudio.sh` | Add LM Studio info | `models.json` | Enriched `models.json` |
| `04-03-query-openrouter.sh` | Add OpenRouter info | `models.json` | Enriched `models.json` |
| `05-generate-yaml.sh` | Generate YAML | `models.json` | `models-new.yaml` |
| `06-validate-yaml.sh` | Validate YAML | `models-new.yaml` | Validation report |

### Utility Scripts

| Script | Purpose |
|--------|---------|
| `infer-functions.sh` | Infer supported model functions from model type and chat-template signals |
| `format-description.sh` | Generate description template |
| `suggest-tags.sh` | Suggest tags based on model type |
| `hf-curl.sh` | Run a curl call against huggingface with HF_TOKEN if available |
| `fetch-chat-template.sh` | Fetch a model's chat template (jinja file or tokenizer_config.json) |

## Best Practices

1. **Always backup** `resources/models.yaml` before updating
2. **Review all flagged entries** marked with `[NEEDS REVIEW]` or `[SUGGESTED]`
3. **Test with subset** of models first to validate workflow
4. **Commit incrementally** - one model family per commit
5. **Document changes** in commit messages
6. **Validate YAML** before committing
7. **Check for duplicates** - ensure unique IDs
8. **Maintain consistency** - follow existing description/tag patterns

## Maintenance

### Updating the Skill
When HuggingFace or Ollama APIs change:
1. Update relevant scripts in `scripts/`
2. Test with current models
3. Update this documentation
4. Commit changes with clear description

### Adding New Model Types
To support new model types (e.g., "Granite Audio"):
1. Add to collection category mapping in `01-list-collections.sh`
2. Add ModelType variant in `src/models/base.rs`
3. Add ModelFunction variant in `src/models/base.rs`
4. Update field mapping rules in `06-generate-yaml.sh` and `infer-functions.sh`
5. Update this documentation

## Related Documentation

- [ModelMetadata Structure](../../src/models/base.rs)
- [Provider Capabilities](../../src/providers/base.rs)
- [Registry System](../../docs/specs/0003-model-registry-implementation.md)

## Support

For issues or questions:
1. Check [Troubleshooting](#troubleshooting) section
2. Review script output for error messages
3. Consult [PLAN.md](./PLAN.md) for detailed workflow
4. Open an issue in the project repository
