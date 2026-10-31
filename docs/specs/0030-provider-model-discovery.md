# Plan: Provider Model Discovery (config-driven, full mirror)

## Overview

Opt in per provider **type config** with a `discover_models` field. When it is
true, a successful model listing is authoritative: sync adds models the
provider reports, removes models it no longer reports, and never touches a
model the user authored.

The opt-in lives in the provider's own config struct, not the
`ProviderConfig` wrapper. That gives each provider type its own support and
default: local providers can default on, hosted providers can be absent or
default off, and `prompt_from_schema` handles the question for free.

v1 supports three providers:

- **Ollama** — `GET /api/tags`, enriched with `POST /api/show`.
- **LM Studio** — `GET /api/v1/models`.
- **llama.cpp** — `GET /models` (the router endpoint, not `/v1/models`).

`openai-compatible` and `openrouter` are deliberately out of scope for v1. They
have no `discover_models` field and no `list_models` implementation yet. When
OpenRouter is added it should default `false`; its catalog is hundreds of
hosted models.

There is no `model discover` / `model import` command. A read-only
`provider models [<provider-id>]` inspection command may remain, but it is not
the opt-in mechanism.

What "true" means:

> A successful listing from this provider defines the set of models
> granite-cli created from it. Sync adds new ones, removes missing ones, and
> leaves catalog and hand-made models alone.

A failed or partial listing changes nothing. Only a complete, parsed response
may remove anything.

## What changed from the previous draft

- The flag moved from `ProviderConfig` to each provider type's config struct.
  No `ProviderConfig` literal churn; defaults are per type and schema-prompted.
- No `ProviderMetadata.supports_model_discovery` and no
  `DiscoveryUnsupported` validation problem: a type that does not have the
  field cannot sync.
- No tombstones, no `discover_ignore`, no `missing` marker. A successful
  listing is trusted; absent means removed.
- v1 is three local providers with provider-specific parsers. No shared
  OpenAI-shape parser and no `models_endpoint` setting.
- `model remove` refuses discovery-managed models while discovery is on, so
  removal has exactly one owner.
- `ModelConfig.discovered` is the only provenance mechanism (unchanged).

## Constraints found in the code

1. **Config loading is synchronous and offline.** `Config::new()`
   (`src/config/mod.rs:479`) reads one YAML file per instance through
   `load_dir`; `AppContext`/`ProviderSource`/`ModelSource` are also built
   synchronously (`src/sources.rs`, `src/providers/mod.rs`,
   `src/models/mod.rs`). Sync cannot run on load, on `sources()`, or at every
   command start without putting network I/O into every list command and test.

2. **`Config::save()` rewrites every kind's files** (`src/config/mod.rs:569`).
   Adding/removing discovered models one at a time with `insert_model` /
   `remove_model` would rewrite the whole config per model. Sync mutates in
   memory and saves once.

3. **Async entry points already exist where the CLI contacts providers.**
   `ProviderCommands::setup` and `health` are async; `run_launch` reloads config
   and calls `LauncherCommands::prelaunch` before starting the proxy
   (`src/main.rs:879-883`, `src/commands/launcher.rs:328`); the setup wizard is
   async. Those are the sync points.

4. **`ModelConfig` is built with struct literals.** There are 51
   `ModelConfig { ... }` sites in `src/` (almost all tests). Adding
   `discovered` touches every one. `#[serde(default)]` keeps old YAML loading
   but does not help Rust literals. `ProviderConfig` is **not** touched, so the
   other 68 sites are unaffected.

5. **Provenance must be explicit.** Sync must be able to tell a model it
   created from one the user made, or removal would delete user work. A naming
   convention is brittle: users can type the same id, and catalog models can
   have provider aliases that do not look like their instance id.

6. **Still no registered Anthropic provider.** `src/providers/anthropic.rs` is
   untracked and not compiled. Anthropic discovery is out of scope.

## Configuration

Each supporting provider's config struct gains a field (default `true` for
these local providers):

```rust
#[serde(default = "default_discover_models")]
pub(crate) discover_models: bool,

fn default_discover_models() -> bool { true }
```

- `OllamaProviderConfig`
- `LMStudioProviderConfig`
- `LlamaCppProviderConfig`

`OpenAIProviderConfig`, `OpenRouterProviderConfig`, and any future Anthropic
provider do not have it in v1. When OpenRouter is added, default `false`.

Consequences of putting it in the type config:

- `prompt_from_schema` asks it during `provider setup`, using the existing
  config as defaults when reconfiguring. No extra prompt code and no separate
  CLI flag.
- `--auto` gets the right default for free via
  `PROVIDER_REGISTRY.default_config(type)`: local true, everything else
  unsupported.
- `provider info` shows `Config: discover_models` automatically through
  `config_fields`; `provider list` can read `cfg.config["discover_models"]`.
- A type without the field simply cannot sync. A hand-edited
  `discover_models: true` in an unsupported type is an ignored unknown key
  (configs do not use `deny_unknown_fields`), not an error.
- `provider catalog` can optionally show a column by checking whether the
  type's `config_schema` has a `discover_models` property; no metadata field
  needed.

Trait surface:

```rust
#[async_trait]
pub(crate) trait Provider {
    /// Whether this instance is configured to mirror its reported models.
    /// Defaults to unsupported/off; the three v1 providers return their
    /// config field.
    fn discover_models(&self) -> bool {
        false
    }

    /// Models this provider instance currently serves. The default says the
    /// provider cannot be asked, so callers can tell "unsupported" from
    /// "supported and empty".
    async fn list_models(&self) -> Result<Vec<ProviderModel>, ProviderError> {
        Err(ProviderError::Unsupported("model discovery".to_string()))
    }
}
```

Add `ProviderError::Unsupported(String)`. `ProxiedProvider::list_models`
delegates to `inner` (whose base URL is the real upstream, not the session
proxy) and `ProxiedProvider::discover_models` delegates too. `FakeProvider`
and other test doubles keep compiling on the defaults.

## Data model: model provenance

Add a config-level field to `ModelConfig`, not `CustomModelConfig`:

```rust
pub(crate) struct ModelConfig {
    pub(crate) model_id: String,
    pub(crate) model_type: String,
    pub(crate) provider_id: String,
    pub(crate) variant: Option<String>,
    /// Present only on entries provider discovery created. Absent for
    /// catalog models and hand-made custom models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) discovered: Option<DiscoveredModel>,
    pub(crate) config: serde_json::Value,
}

pub(crate) struct DiscoveredModel {
    /// Provider instance that created this entry.
    pub(crate) provider_id: String,
    /// The name requests must use upstream.
    pub(crate) provider_model_id: String,
}
```

Why here rather than `CustomModelConfig`:

- `prompt_from_schema` rebuilds an object from schema properties only
  (`src/utils/ui/prompt.rs:63-82`), so a `#[schemars(skip)]` field in
  `CustomModelConfig` would be silently dropped whenever `model setup` runs.
  A `ModelConfig` field is preserved by `ModelCommands::setup` explicitly.
- Hand-authored `custom` setup UX stays unchanged (no discovery questions).
- Cost: ~51 `ModelConfig` literal edits.

There is no `missing` field: if the entry is absent from a successful listing,
the entry is gone.

### Wire-name resolution

`ConfiguredModel::resolve_provider_endpoint` currently sends the instance id
unless `Provider::model_alias` maps it (`src/models/base.rs:337-339`). Extend
it with the stored wire name, without adding a `Model` trait method:

```
provider.model_alias(instance_id, variant)   // explicit user override
  .or(discovered.provider_model_id)          // set by sync
  .unwrap_or(instance_id)
```

`ModelSource::resolve` already assembles `ConfiguredModel` from the
`ModelConfig`; pass the discovered wire name through. This also fixes
variantless custom models generally, which today cannot map a local id to a
server id at all.

## Provider API and parsers

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderModel {
    pub(crate) id: String,                     // wire name, required
    pub(crate) display_name: Option<String>,
    pub(crate) context_length: Option<u64>,
    pub(crate) parameter_count: Option<u64>,
    pub(crate) disk_size_bytes: Option<u64>,
    pub(crate) model_type: Option<ModelType>,
    pub(crate) supported_functions: Vec<ModelFunction>,
}
```

No `owned_by`/`created`: none of the three v1 APIs need them.

### Ollama

`GET {base_url}/api/tags`:

```json
{ "models": [ {
  "name": "gemma4:latest", "model": "gemma4:latest",
  "modified_at": "...", "size": 9608350245, "digest": "...",
  "details": {
    "format": "gguf", "family": "gemma4", "families": ["gemma4"],
    "parameter_size": "8.0B", "quantization_level": "Q4_K_M"
  }
} ] }
```

- `id` = `name` (fallback `model`).
- `display_name` = `family` + `parameter_size` (or the id).
- `disk_size_bytes` = `size`.
- `parameter_count` = parsed `details.parameter_size` (`8.0B` → 8e9; only if it
  parses as a single number; `26B-A4B` stays `None`).
- `context_length` and `supported_functions`: `POST /api/show` with
  `{"model": "<id>"}` per model. N+1 requests, local and cheap. Read
  `capabilities` (`completion`, `tools`, `vision`, `thinking`, `embedding`) and
  the `model_info` key ending in `.context_length` (e.g.
  `llama.context_length`). Older builds without `capabilities` fall back to
  `[Chat, ToolCalling]` for `completion` models.
- `model_type`: `Embedding` when `capabilities` has `embedding`, otherwise
  `Text` (Ollama does not distinguish vision at the type level; the function is
  enough).
- Failure of one `/api/show` degrades that entry (no context/capabilities);
  failure of `/api/tags` fails the whole plan.

### LM Studio

`GET {base_url}/api/v1/models` (current REST API; v0 is superseded):

```json
{ "models": [ {
  "type": "llm",
  "publisher": "google",
  "key": "google/gemma-4-26b-a4b",
  "display_name": "Gemma 4 26B A4B",
  "max_context_length": 262144,
  "size_bytes": 17990911801,
  "params_string": "26B-A4B",
  "format": "gguf",
  "capabilities": {
    "vision": true,
    "trained_for_tool_use": true,
    "reasoning": { "allowed_options": ["off", "on"], "default": "on" }
  }
} ] }
```

- `id` = `key`; `display_name` = `display_name`.
- `context_length` = `max_context_length` (may be null).
- `disk_size_bytes` = `size_bytes`.
- `parameter_count` = parsed `params_string` when it is a plain number
  (`"7B"`), else `None` (`"26B-A4B"`).
- `type`: `llm` → `Text`, `embedding` → `Embedding` (+ `Embeddings`),
  `decision` → `Text` (+ `Decision`).
- Functions: always `Chat` for `llm`; `vision` → `ImageUnderstanding`;
  `trained_for_tool_use` → `ToolCalling`; `reasoning` present → `Thinking`.
- Auth reuses the provider's optional `api_key` (`Authorization: Bearer`).

### llama.cpp

`GET {base_url}/models` — the router endpoint the user pointed at
(`http://pgx:9797/models`), **not** `/v1/models`. Observed response:

```json
{ "data": [ {
  "id": "Qwen-3.8-27b-d4-UD-Q4_K_XL",
  "aliases": [],
  "tags": [],
  "object": "model",
  "owned_by": "llamacpp",
  "created": 1793415305,
  "status": { "value": "unloaded", "args": ["..."] },
  "architecture": { "input_modalities": ["text"], "output_modalities": ["text"] },
  "source": "preset",
  "can_remove": false
} ] }
```

- `id` = `id` (the router uses the alias as the request name).
- `display_name` = `id` (no separate display name).
- `model_type` and functions from `architecture.input_modalities`:
  - `image` → `ModelType::Vision`, functions `[Chat, ToolCalling, ImageUnderstanding]`;
  - `audio` → `ModelType::Speech`, functions `[Chat, Transcription]` (observed
    on `ibm-granite/granite-4.0-1b-speech-GGUF:Q4_K_M` on the example router);
  - otherwise `ModelType::Text`, functions `[Chat, ToolCalling]`.
  The endpoint exposes no tool capability beyond modalities, so `ToolCalling`
  is the same optimistic default as the generic fallback for text/vision.
  Audio drops it: a speech model is not a tool caller.
- `context_length`, `parameter_count`, `disk_size_bytes`: not in the router
  list. Leave `None`/0. (`/v1/models` in a single-server build sometimes has
  `meta.n_ctx_train`/`meta.n_params`; do not depend on it.)
- `status.value` (`loaded`/`unloaded`/`loading`/`sleeping`/`failed`) is
  informational. In router mode with `models_autoload: true`, unloaded entries
  are still servable, so do not filter on it. If a future build exposes a
  clearly unusable `failed` state, revisit.
- No pagination.

Coverage caveat: llama.cpp's `model_alias` builds
`<hf_repo>:<precision>` from the configured variant URL, which matches the
router ids that are HF refs (`ibm-granite/granite-4.1-8b-GGUF:Q4_K_M`). Router
ids that are local presets/aliases (`Qwen-3.8-27b-d4-UD-Q4_K_XL`) have no
catalog counterpart to match, so sync adds them as discovered models. That is
expected.

## Synchronization

### Ownership

A configured model is **managed by provider P** when:

```
model_type == "custom"
  && discovered.is_some()
  && discovered.provider_id == P
```

Everything else is user-owned: catalog models and hand-made custom models.
Sync never removes, renames, or re-points a user-owned model.

### The diff

For each provider with `discover_models()`, one plan:

```
reported:  HashMap<wire_id, ProviderModel>
managed:   HashMap<wire_id, model_id>   // from config, ownership rule
covered:   HashMap<wire_id, model_id>   // any configured model for this provider

added   = reported - covered
present = managed ∩ reported
removed = managed - reported
```

Removal applies **only** to `managed`; a user-owned model that happens to match
a wire id is never removed (it is `covered`). Instance ids for additions:

1. the provider's `id` (the natural name users expect);
2. if that is taken by a different configured instance (user model, another
   provider's managed model, or a catalog id), use `<provider-id>/<id>`
   (`Config::id_to_filename` already maps `/` to `---`);
3. if that is also taken, append a numeric suffix and report it.

Provider ids are processed in sorted order, so two providers reporting
`gpt-4o` produce deterministic ids (`gpt-4o` for the first, `other/gpt-4o` for
the second).

Wire-name coverage uses the same resolution as requests:

1. `discovered.provider_model_id`, when present;
2. otherwise the provider alias for the configured variant (this is how an
   Ollama catalog model with a variant URL maps to `granite4.1:8b`);
3. otherwise the instance id.

Rule 2 prevents duplicate imports when a user already configured the catalog
model that the provider reports under its native name.

### Additions

Sync creates a `custom` model with defaults. No prompt: sync runs from
`provider health`, `setup`, and launch preflight, where blocking is wrong.

| field | default |
|---|---|
| `model_type` | `"custom"` |
| `family` | `display_name` else wire id |
| `context_length` | reported else `0` (unknown) |
| `size` | reported parameter count else `0` |
| `model_type` (capability) | inferred else `Text` |
| `supported_functions` | inferred, else `[Chat, ToolCalling]` |
| `tags` | `["discovered", <provider type>]` |
| `description` | `"Discovered from provider '<id>'."` (no timestamp: deterministic tests) |
| `variants` | empty — already served, nothing to pull |
| `huggingface_repo`, `native_dtype`, `version` | empty |
| `supported_reasoning_efforts` | empty |
| `discovered` | `{ provider_id, provider_model_id: wire_id }` |

Inference is per provider, as described above. There is no generic fallback
parser in v1: OpenRouter and `openai-compatible` do not sync.

### Removals

A managed model absent from a successful listing is removed from config. What
happens to anything pointing at it:

- Sync writes a removal summary (count, plus the model ids when the count is
  small). For each removed model that something references, it also warns with
  the model id and the capabilities/launchers from
  `validation::dependents(RefKind::Model, id, config)`. It does not print one
  line per removed model when a local catalog drops many.
- References are **not** cascaded. The capability stays configured and
  dangling; existing validation surfaces it in `model list` notes,
  `capability list`, `launch` preflight, and remediation. That is the same
  state a manual `model remove --dependents leave` produces today.
- Removal is batched with additions and saved once.

Why warnings rather than confirmation:

- A successful listing is authoritative by definition, and sync often runs
  non-interactively (`launch`, `--auto`, `--output json`).
- A confirmation would make the outcome depend on whether the terminal is
  interactive, which is exactly the drift the flag is meant to remove.
- The user can inspect first with `provider models` or `provider health`.

Empty successful list: every managed model is removed. That is correct for a
local server with nothing installed, and follows from trusting a successful
call. The response shape must be the expected one (`data`/`models` array); a
malformed or failed call is not empty.

### `model remove` and discovered models

`model remove` refuses a discovery-managed model **while its provider's
`discover_models` is true**:

```
Model 'llama3:8b' is managed by provider 'ollama', which has model discovery
on. Remove it at the provider instead: turn discovery off (provider setup
ollama), or remove the provider. Re-running with discovery on would only
discover it again.
```

- The check is `Provider::discover_models()` on the configured provider
  instance. If `discover_models` is false, removal is allowed. That is the
  escape hatch: turn discovery off, remove the model, leave discovery off
  (re-enabling re-adds it, which is expected).
- If the provider is not configured (dangling `provider_id`) or constructing
  it fails, removal is allowed: no sync can re-add it.
- The guard is in the `model remove` command, not in `Config::remove_model`.
  Sync calls the config layer directly.
- `model setup` on a discovered model is allowed and preserves
  `ModelConfig.discovered` (copy it from `existing_config` like the variant).
  It refuses to change `provider_id`, since that would leave the model owned by
  a provider that does not serve it; the message points at disabling discovery
  first. Sync does not rewrite its fields (v1), so edits stick until the model
  is removed or the provider stops reporting it.

### Metadata drift

**v1: sync never rewrites an existing model's fields.** It only adds and
removes. There is no reliable way to tell a user edit from a stale discovered
value without storing a fingerprint of the last discovered values; silently
overwriting `context_length` or `supported_functions` would undo corrections
and could change which capabilities admit the model.

Refresh path: the model is removed and re-added by sync when it disappears and
reappears, or the user edits it with `model setup`. Phase 2 can add a
fingerprint-based refresh.

### Failure and partial results

- Network/HTTP failure → no writes for that provider; report unavailable.
- 404/405 (endpoint absent) → `Unsupported`; no writes.
- Ollama `/api/tags` succeeds but a `/api/show` fails → keep the entry with
  whatever `/api/tags` gave; do not fail the plan.
- Pagination or any multi-page listing: if any page fails, abort the whole
  provider's plan. (None of the three paginate today; this is for the future.)
- Empty list with the expected shape → authoritative; remove managed models.
- Malformed body / unexpected top-level shape → error; no writes.
- Write failure → warn, keep the in-memory mutation (existing behavior), and
  report that on-disk config may be stale.

### Writing

Sync cannot use `insert_model`/`remove_model` per model: each saves the whole
config, and `save()` alone would leave the YAML files of removed models behind
(it writes entries that exist; it does not delete orphans). Add one batch
method, e.g.:

```rust
impl Config {
    /// Apply one provider's full mirror: add and remove model entries,
    /// delete the files of removed ones, and save once.
    pub(crate) fn apply_model_changes(
        &mut self,
        additions: Vec<(String, ModelConfig)>,
        removals: &[String],
    ) -> Result<()> { /* map + file deletion + one save */ }
}
```

1. compute every provider's plan (reads config and network, no writes);
2. apply additions and removals to `config.models` in memory, deleting files
   for removals;
3. save once;
4. sources are invalidated by `config_mut()` on first mutation, so subsequent
   reads rebuild.

Skip the call when the diff is empty, to avoid rewriting YAML (and touching
mtimes) on every `provider health` run.

`Config::remove_model` already has the file-deletion logic; the batch method
should share it rather than duplicate the path-building.

### When sync runs

| Point | Async? | Already contacts provider? | Decision |
|---|---|---|---|
| `Config::new()` / `sources()` | no | no | **No.** Would put network in every command and test. |
| Every command start in `main` | yes | no | **No.** Every `model list` pays N timeouts and may write. |
| `provider setup` | yes | yes (health) | **Yes.** `prompt_from_schema` already asked `discover_models`; sync after health and print the summary. |
| `setup` wizard / `--auto` | yes | yes (provider discovery health) | **Yes**, after providers are configured. `default_config` gives local types `discover_models: true`; hosted types do not have the field. |
| `provider health [<id>]` | yes | yes | **Yes.** The manual "refresh now" path. `--no-sync` for scripts. |
| `launch` preflight | yes | already resolving models | **Yes**, limited to providers backing the models this launcher uses. Fail-open on network errors. |
| `model list` / `provider list` | no | no | **No.** Show `discover_models` from config; no network. |
| TUI list views | — | — | No; TUI setup inherits `provider setup`. |

A `--refresh` flag on `model list` can be added later if live listing is wanted.

### Launch preflight in detail

`run_launch` reloads config and calls `prelaunch` before starting the session
proxy (`src/main.rs:879-883`), so it sees fresh config and can write without the
proxy being involved.

1. Collect the provider ids backing the launcher's enabled capabilities: walk
   each capability's declared `Dependency::Model` config keys to model ids,
   then `ModelConfig.provider_id`; dedupe.
2. For each such provider whose `discover_models()` is true, sync (short
   connect timeout; failure is a warning, not a launch failure).
3. If sync removed a model the launch uses, the capability is now dangling;
   the existing `prelaunch` validation/remediation handles it
   (`OnDecline::Abort` stops the launch unless the user fixes it). No new
   `Problem` variant is needed for this.
4. Never sync providers the launch does not use. This bounds launch latency and
   avoids importing unrelated catalogs.

### Concurrency

HTTP calls can run concurrently (bounded; `futures_util` is already a
dependency) into per-provider plans; applying plans and saving stays
sequential. `provider health` may be sequential for simpler output; the launch
path benefits most from parallel listing.

## UX changes

### `provider setup`

- `prompt_from_schema` asks `Discover models...` for the three supporting
  types, defaulting to true. Reconfiguring preserves the current value
  automatically because the existing config is passed as defaults.
- After the health check, if `discover_models` is true, sync and print the
  summary (`added N, removed N`). No second confirmation: the schema prompt was
  the consent.
- No new CLI flag. `--auto` uses the registry defaults.
- `ProviderCommands::setup` needs no signature change.

### `provider health`

- Keep the health table; add `DISCOVERY`, `ADDED`, `REMOVED`, `NOTE` columns,
  or print a sync summary after the table.
- Sync only healthy providers whose `discover_models()` is true; `--no-sync`
  disables.
- Unhealthy/unreachable → `DISCOVERY = skipped`, no writes.
- Removal warnings name dependents (see Removals).

### `setup` wizard / `--auto`

- Interactive: provider schema prompt handles the flag.
- `--auto`: `configure_all` already writes `PROVIDER_REGISTRY.default_config`,
  so the three local types get `discover_models: true` and hosted types get no
  field. After `configure_all`, sync the newly configured providers and print
  counts.
- Do not feed discovered models into `--auto`'s model/capability selection in
  v1; selection is catalog- and hardware-fit-driven. Phase 2 can consider
  discovered local models as candidates.

### `provider list` / `provider info` / `model list` / `model info`

- `provider list`: `DISCOVERY` column read from `cfg.config["discover_models"]`
  (`yes` / `-`).
- `provider info`: `Config: discover_models` already appears via
  `config_fields`; optionally show managed model count from config.
- `model list`: `NOTES` already exists (`dangling_notes`). Optionally add a
  `SOURCE` column (`catalog` / `custom` / `discovered`) from
  `ModelConfig.discovered`.
- `model info`: `Config: Discovered from`, `Config: Wire name`.

### Read-only inspection

Keep `provider models [<provider-id>]` as a live, read-only listing (no writes,
no sync). It shows what a provider offers before enabling the flag, and what a
sync would add/remove. It is not the opt-in and can be deferred to Phase 2 if
no new commands are wanted.

## Validation and remediation

No new `Problem` variants. A type without the field cannot sync, and a removed
model leaves ordinary dangling references that the existing validation and
remediation already report (a capability's model is `NotConfigured`; `launch`
can reconfigure, remove, or decline).

Turning `discover_models` off:

- Freezes the current set; no deletion, no marker changes.
- Managed models keep their `discovered` marker, so re-enabling resumes sync.
- `model remove` is allowed while the flag is off (the escape hatch above).

Deleting a provider flows through `dependents` (models reference
`provider_id`), so its managed models are handled by the existing
`--dependents` policy. `--dependents discovered` removes only the dependents
provider discovery created, leaving catalog and hand-made models (and any
dependents of those) in place. It is also the explicit escape hatch for
removing a discovered model directly while its provider still has discovery
on.

## Edge cases

- **User model with the same id as a reported model.** Covered by the wire-name
  match; if a different model owns the id, the managed copy is namespaced
  (`<provider>/<id>`) and reported as a conflict.
- **Two providers report the same wire id.** Deterministic namespacing by
  sorted provider id.
- **Wire id contains `/` or `:`** (llama.cpp `org/model:Q4_K_M`, Ollama tags).
  Both are legal in instance ids; `Config::id_to_filename` maps the separator
  and escapes Windows-invalid characters so config file names remain usable.
- **Provider reports the same id twice.** First wins; warn.
- **Ollama `/api/show` fails for one model.** Keep the `/api/tags` entry; the
  next sync retries.
- **llama.cpp lists `unloaded`/`loading` models.** They are still servable in
  router mode; do not filter.
- **Empty successful list.** All managed models removed, with warnings. Correct
  for a local server with nothing installed.
- **User removes a hand-made model that covers a reported wire id.** Allowed
  (it is user-owned); the next sync re-adds it as a discovered model. The
  provider still serves it, which is the point of mirror mode.
- **Sync writes while another command holds a `Sources` snapshot.**
  `config_mut` invalidates; the launch already holds its own snapshot
  deliberately. Apply sync before any long-lived snapshot is taken (prelaunch
  is before proxy start).
- **Timeouts.** Use the provider's `timeout_secs`; launch preflight should use
  a shorter cap so a dead provider does not stall launch. Both fail open.

## Tests

No test may reach the internet or depend on locally running servers
(`AGENTS.md`). Use `127.0.0.1:0` axum test servers for HTTP, and
`TestConfigHome` for config writes.

Provider parsers (fixture JSON, no network):

- Ollama `/api/tags` + `/api/show`: name/model fallback, parameter size parse,
  context key, capability mapping, show failure degrades.
- LM Studio `/api/v1/models`: `key`, `type` mapping, vision/tools/reasoning,
  `max_context_length`, `params_string` parse.
- llama.cpp `/models`: the observed router shape; image/audio modalities →
  vision/transcription; unloaded entries included; no context.

Pure logic (new module, no network):

- ownership: only `custom` + `discovered.provider_id == P` is managed; user
  models untouched;
- diff: added / present / removed / conflicts;
- wire-name matching via `discovered.provider_model_id` and via provider alias
  for a catalog model variant;
- id naming: raw id, collision → `<provider>/<id>`, second collision suffix;
- two providers same id → deterministic names;
- empty reported set → all managed removed, user models kept;
- `discover_models()` false → no plan.

Sync/command tests (local server + temp home):

- `provider health` adds/removes for a healthy flag-on provider; `--no-sync`
  does not; one persist per sync and removed YAML files are deleted;
- unhealthy or 500 → no writes; 404 → unsupported, no writes;
- malformed body → no writes;
- removal warnings name dependents and do not cascade;
- empty successful list removes all managed models;
- save failure warns and keeps memory;
- `model remove` refuses a managed model while the provider flag is on, and
  allows it when the flag is off;
- `model setup` on a discovered model preserves `discovered` and refuses a
  provider change;
- `provider setup` schema prompt includes `discover_models` for the three
  types and not for the others; reconfiguring preserves it;
- `--auto` writes the type default;
- launch preflight syncs only used providers (assert request counts on two test
  servers), fails open on network error, and a removed used model aborts via
  existing validation;
- `provider list/info`/`model list/info` show discovery state and source.

## Docs

- This spec.
- `docs/data-model.md`: `ModelConfig.discovered` (provenance naming a provider;
  no new reference kind). Note `discover_models` now lives in the provider
  type config, so the diagram's `ProviderConfig.config` carries it.
- `provider setup`/`health` help text.
- README: no launcher/capability change, so the Capability Support table and
  `resources/models.yaml` are untouched.

## Work breakdown

1. `ModelConfig.discovered` + `DiscoveredModel`; update ~51 literals; serde
   defaults and skip rules.
2. `ProviderModel`, `Provider::discover_models`, `Provider::list_models`,
   `ProviderError::Unsupported`; `ProxiedProvider` delegation.
3. `discover_models` field + default true in Ollama, LM Studio, llama.cpp
   configs; schema/default tests.
4. Three parser/list implementations with fixture tests (Ollama `/api/tags` +
   `/api/show`, LM Studio `/api/v1/models`, llama.cpp `/models`).
5. `ConfiguredModel` wire-name resolution + `ModelSource` plumbing.
6. Pure diff/plan module + tests (additions, removals, conflicts).
7. Sync orchestration: collect plans, apply additions/removals, one
   `apply_model_changes`, removal warnings with dependents.
8. `provider health` sync + `--no-sync`.
9. `prelaunch` targeted sync.
10. `setup`/`--auto` post-configure sync; no model-selection integration in v1.
11. `model remove` guard for managed models (flag-on only); `model setup`
    refuses provider changes on discovered models.
12. `provider list/info`, `model list/info`.
13. Optional read-only `provider models` (deferred to Phase 2).
14. Docs and `./scripts/fmt.sh`.

## Resolved decisions

1. **Local defaults.** `discover_models` defaults to `true` for Ollama, LM
   Studio, and llama.cpp.
2. **Capability defaults.** Use the provider-reported functions where they
   exist, and the documented `[Chat, ToolCalling]` / modality fallbacks where
   they do not.
3. **Removal safety.** Warn and apply. The summary lists the removed ids when
   there are at most five; dependent references are reported per removal and
   are never cascaded.
4. **Launch preflight.** Sync only the providers backing this launch, bounded
   to four concurrent listings with a three-second per-provider cap, and fail
   open.
5. **Read-only inspection command.** `provider models [<id>]` is deferred to
   Phase 2 rather than added in v1.
6. **`--auto` behavior.** Enable local discovery by type default and do not
   feed discovered models into model/capability selection.
7. **`model remove` escape hatch.** Refuse while the flag is on, with advice to
   disable discovery first. If the provider is missing or cannot be built,
   removal is allowed because no sync can re-add the model.
