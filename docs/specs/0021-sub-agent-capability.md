# Plan: Sub-Agent Capability

## Overview

[Issue #72](https://github.com/ibm-granite-community/granite-cli/issues/72) asks
for a new `Capability` that defines a sub-agent — a named agent with its own
prompt, tool allow-list, and `Model`/`Provider` — that a launched coding agent
can delegate to, independent of whatever model the main session is using. The
issue calls out `claude` as the launcher to start with, and flags the real
challenge: Claude Code has exactly **one** `ANTHROPIC_BASE_URL` for an entire
session — the main agent's calls *and* every sub-agent's calls all go through
it. A sub-agent's `model` field, when it's an arbitrary literal string, is
sent verbatim in the Messages API request body for that sub-agent's turns. So
giving a sub-agent its own provider means standing up a small reverse-proxy
("mini-router," per the issue) in front of `ANTHROPIC_BASE_URL` that inspects
each request's `model` field: a name that belongs to a configured sub-agent
routes to that model's real provider; anything else (the main agent's own
traffic) routes to the normal upstream, unchanged.

The load-bearing Claude Code CLI behaviors were confirmed against current
official docs and against the `claude` binary (v2.1.238) installed in the
development environment:

- **`--agents '<json>'` exists and is session-scoped**, e.g. `claude --agents
  '{"reviewer": {"description": "...", "prompt": "...", "tools": [...],
  "model": "..."}}'` — confirmed directly via `claude --help`. This is the
  exact analog of the `mcp add-json`/`mcp remove --scope local` pattern
  `src/launchers/shared/mcp_cli.rs` already uses for MCP servers, except
  simpler: no register/remove around the launch is needed, it's just one CLI
  arg.
- **Custom model strings pass through unvalidated** once `ANTHROPIC_BASE_URL`
  is non-default — Claude Code only validates model strings against the
  direct Anthropic API; behind a custom base URL, the provider/gateway
  defines the model names and Claude Code passes any string through
  unchecked. So the router can freely use any resolved provider-side model
  name (the same `model_name` `AgentModelCapability`/`VisionMCPCapability`
  already compute) as the dispatch key.
- **Auth headers aren't forced into one mode.** Claude Code attaches whatever
  credential it has (OAuth subscription session, `ANTHROPIC_API_KEY`,
  `ANTHROPIC_AUTH_TOKEN`, etc.) to requests against a custom base URL the same
  way it would against the real API; a gateway must forward those headers
  unchanged to preserve subscription-based auth. This means the "normal
  upstream" (unknown model) branch of the router must **pass headers through
  untouched** rather than injecting its own credentials — this transparently
  preserves whichever auth mode the user already has configured, including
  subscription billing, with zero credential-handling code of our own.
- **Default real upstream** is `https://api.anthropic.com`, overridable via
  `ANTHROPIC_BASE_URL` — this is the "read from env with fallback to a
  well-known default" the issue asks for, applied to granite-cli's *own*
  process environment (so a user who already points `ANTHROPIC_BASE_URL` at a
  corporate gateway keeps working).

The result: no dialect translation is needed, no credential-juggling for the
subscription case, and no Anthropic-specific request/response parsing beyond
reading one JSON field to pick a destination.

### Key design decisions

- **`ConfiguredModel` helper (`src/models/base.rs`)**: `AgentModelCapability`
  and `VisionMCPCapability` each duplicated "resolve `model_id` through
  `ModelSource`, remember any pinned variant, resolve that variant, then
  check provider/api-type/function support and compute the provider-specific
  model name." Factored into `ConfiguredModel`, shared by all three
  model-backed capabilities, so `SubAgentCapability` doesn't add a third
  copy. `resolve_provider_endpoint` takes `required_function` and
  `endpoint_function` as separate parameters specifically because
  `VisionMCPCapability` needs them to differ (model must support
  `ImageUnderstanding`, but the endpoint is looked up via `Chat`).
- **`SubAgentBinding` reuses `AgentModelBinding` by composition** rather than
  duplicating connection fields (base_url, model_name, api_key, verify_ssl,
  endpoint_path, context_length).
- **The mini-router (`ModelRouter`, `src/launchers/shared/model_router.rs`)
  is launcher-side plumbing, not capability-side** — only a launcher sees
  every bound capability at once, so only it can build the full
  model-name-to-target routing table. Placed alongside `mcp_cli.rs`, the
  existing precedent for launcher-side helpers that turn one `BindingType`'s
  bindings into something a downstream CLI tool understands.
- **The router is deliberately not Anthropic-specific in its mechanics** — it
  only reads a top-level JSON `"model"` string, a shape OpenAI/Ollama-style
  bodies share too, even though Claude/Anthropic is its first and only
  consumer.
- **`v1` requires the sub-agent's provider to itself support
  `ApiType::Anthropic`** — same restriction `AgentModelCapability` already
  has for the main model. No Anthropic Messages ⟷ OpenAI Chat Completions
  translation is attempted.
- **Usage tracking composes underneath this for free.** Because
  `SubAgentCapability` resolves its model through `ConfiguredModel::resolve`
  → `ModelSource::take`, exactly like `AgentModelCapability`, a
  usage-tracking session (spec 0020) transparently wraps it the same way —
  no changes needed there.

## Out of Scope (Future Work)

**Dialect translation.** Serving a sub-agent off an OpenAI-only provider by
translating Anthropic Messages ⟷ OpenAI Chat Completions on the fly. Today
that means Ollama, LM Studio, and llama.cpp (the only providers that
advertise `ApiType::Anthropic`) are the usable backends for a sub-agent.

**Bedrock/Vertex/Foundry auth modes** for the "real upstream" fallback — only
the direct-Anthropic-API credential precedence is exercised; passthrough
mode doesn't care which credential scheme it's carrying, but genuinely
cloud-provider-specific request signing is not something this proxy
attempts.

**Any launcher other than `claude`.** `pi`, `bob`, and `opencode` are
unaffected — they simply don't declare `BindingType::SubAgent` in their
`supported_capabilities`, so the existing subset check in
`bind_capability`/`select_capabilities` rejects enabling a `SubAgentCapability`
for them, the same way it already rejects any other unsupported binding
type.

**Cost/pricing accounting for subscription vs. API-key billing** — per the
issue's own caveat.

---

## Sub-Tasks

---

### Sub-Task 1 — `ConfiguredModel` shared helper (`src/models/base.rs`)

**Intent**
Deduplicate the model-resolution logic `AgentModelCapability` and
`VisionMCPCapability` each carried inline before `SubAgentCapability` would
have become a third copy.

**Expected Outcomes**
- `ConfiguredModel { pub model: Arc<dyn Model>, configured_variant: Option<String> }`.
- `ConfiguredModel::resolve(model_id, global_config)` — same
  `ModelSource::from_config` + `take` + panic-on-missing resolution every
  capability's `ConfigConstructable::new` did inline.
- `ConfiguredModel::resolve_variant()` — case-insensitive `"format/precision"`
  lookup against the model's catalog variants.
- `ConfiguredModel::resolve_provider_endpoint(model_id, api_type,
  required_function, endpoint_function) -> anyhow::Result<(Box<dyn Provider>,
  ApiEndpoint, String)>` — resolves the provider, checks `api_type` and
  `required_function` support, finds the `endpoint_function`/`api_type`
  endpoint, and computes the provider-specific model name/alias.
- `AgentModelCapability` and `VisionMCPCapability` updated to hold a
  `configured_model: ConfiguredModel` field and call
  `resolve_provider_endpoint` instead of their own inline checks. One
  defensive check (`provider.supports_function`, redundant with a successful
  endpoint lookup) is dropped as part of unifying the two shapes; two
  existing tests' expected error substrings were updated to match the
  resulting wording (still an error on the same condition, just reworded).

**Status** — `[x] done`

---

### Sub-Task 2 — `BindingType::SubAgent` (`src/capabilities/base.rs`)

**Intent**
Declare the new binding surface alongside `AgentModel`/`Mcp`.

**Expected Outcomes**
- `SubAgentBindingRequest { api_type: ApiType }` — mirrors
  `AgentModelBindingRequest`.
- `SubAgentBinding { description: String, prompt: String, tools: Vec<String>,
  model: AgentModelBinding }`.
- Added to the `define_bindings!` macro invocation.

**Status** — `[x] done`

---

### Sub-Task 3 — `SubAgentCapability` (`src/capabilities/sub_agent.rs`)

**Intent**
The capability itself: config, dependency declaration, and `bind()`.

**Expected Outcomes**
- Config: `{ description, prompt, tools: Vec<String> (default empty),
  model_id }`, all but `tools` required (`min_length = 1`).
- No separate `name` field — the capability's own `instance_id` is the
  sub-agent's name (the key in the `--agents` JSON map), the same convention
  `VisionMCPCapability` uses for its MCP server name.
- `dependencies()`: one `Dependency::Model` requiring `[Chat, ToolCalling]`.
- `bind()` calls `configured_model.resolve_provider_endpoint(model_id,
  api_type, Chat, Chat)` and wraps the result into `SubAgentBinding`.
- Registered as `"sub-agent"` in `CAPABILITY_REGISTRY`.

**Status** — `[x] done`

---

### Sub-Task 4 — Mini-router (`src/launchers/shared/model_router.rs`)

**Intent**
The reverse-proxy that makes per-sub-agent routing possible under Claude
Code's single `ANTHROPIC_BASE_URL`.

**Expected Outcomes**
- `UpstreamTarget { base_url, verify_ssl, auth: UpstreamAuth }`,
  `UpstreamAuth::Inject(Option<Secret>)` (strip client auth, inject this) or
  `UpstreamAuth::Passthrough` (forward client auth headers byte-for-byte).
- `ModelRouter::start(default: UpstreamTarget, routes: HashMap<String,
  UpstreamTarget>) -> anyhow::Result<Self>` — synchronous, built on
  `SubServer::spawn`. Exposes `local_base_url`; `shutdown(self)` is async.
- The handler buffers the request body, best-effort parses it as JSON and
  reads a top-level `"model"` string; a match against `routes` selects that
  target, anything else (parse failure, missing field, no match) falls
  through to `default`. Forwards method/path/query/headers (minus
  hop-by-hop headers) to the target, applying its `auth` mode; streams the
  response back unmodified.

**Status** — `[x] done`

---

### Sub-Task 5 — `ClaudeLauncher` wiring (`src/launchers/claude.rs`)

**Intent**
Bind `SubAgentCapability` instances, build the `--agents` JSON, and start the
router only when at least one sub-agent is bound.

**Expected Outcomes**
- `bound_sub_agents: Vec<(String, SubAgentBinding)>` field; `bind_capability`
  gains a `BindingType::SubAgent` branch (checked before the `Mcp` branch and
  the `AgentModel` fallthrough).
- `LauncherMetadata::supported_capabilities` gains `BindingType::SubAgent`.
- `launch()`: builds the overlay via the existing `env_overlay()`
  (unchanged), then — when `bound_sub_agents` is non-empty and not
  `ctx.dry_run` — builds the router's `default` target from
  `bound_agent_model` if present (`Inject`) else from the ambient
  `ANTHROPIC_BASE_URL` env var or the well-known fallback (`Passthrough`),
  builds `routes` from every bound sub-agent's `model.model_name`, starts the
  `ModelRouter`, and overrides just the `ANTHROPIC_BASE_URL` overlay entry
  with the router's local address. `--agents <json>` is prepended to the exec
  args when any sub-agent is bound; the router is shut down after the
  process exits.
- Under `--dry-run`, no socket is started, but the overlay still gets a
  placeholder value and `--agents` is still shown, so the dry-run output
  stays informative.

**Status** — `[x] done`

---

### Sub-Task 6 — Tests

**Intent**
Cover the new resolution helper, the capability, the router's dispatch
logic, and the launcher wiring — without needing a real provider or a real
`claude` invocation.

**Expected Outcomes**
- `src/models/base.rs`: `ConfiguredModel::resolve_provider_endpoint` tests
  covering the success path, each failure mode, the
  required-function-vs-endpoint-function-differ case (mirroring
  `VisionMCPCapability`), and alias resolution.
- `src/capabilities/sub_agent.rs`: `bind()` success (carries
  description/prompt/tools and connection details), failure modes, binding
  type/dependency/metadata checks — mirroring `agent_model.rs`'s test-double
  pattern.
- `src/launchers/shared/model_router.rs`: end-to-end tests with two real
  fake upstream HTTP servers verifying model-based dispatch with injected
  auth, default fallback for an unknown model, passthrough leaving client
  auth untouched, and graceful fallback for a non-JSON body.
- `src/launchers/claude.rs`: `build_agents_json` shape (tools omitted when
  empty, included when present, multiple sub-agents), `default_upstream_target`
  (well-known fallback, ambient env override, main-model binding takes
  precedence over the ambient env), `set_env_binding` (overwrite vs. append),
  and `bind_capability` pushing a `SubAgent` binding via a minimal
  `Capability` test double.

**Status** — `[x] done`
