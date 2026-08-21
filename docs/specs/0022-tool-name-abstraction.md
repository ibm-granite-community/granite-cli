# Plan: Launcher-Agnostic Tool-Name Abstraction + Schema-Driven Enum Prompting

## Overview

`SubAgentCapability` (spec [0021](0021-sub-agent-capability.md)) carried
`tools: Vec<String>` straight through from user config into `ClaudeLauncher`'s
`--agents` JSON, unmodified — Claude Code-specific strings baked into a
capability that's meant to be launcher-agnostic. This plan replaces that with
a canonical `ToolName` enum (with an `Other(String)` escape hatch) mapped to
each launcher's own native strings via a per-launcher function — the same
shape `pi.rs` already uses for `ApiType` (`pi_api_name`), generalized and
made a proper `Launcher` trait extension point since more launchers are
expected to implement it over time. It also matters for where the project is
headed: `SubAgentCapability` is meant to become the generic fallback once
pre-baked sub-agents (e.g. "Explore," "Research") exist, and those need to
specify a sensible tool set in Rust, independent of whichever launcher ends
up running them.

Making `ToolName` a real config field type (`Vec<ToolName>`, not a
stringly-typed workaround with a parsing layer) surfaced a genuine gap: the
schema-driven setup wizard (`src/utils/ui/prompt.rs`) had no rendering path
for enums at all — a plain enum degraded to free-text entry (the schema's
`enum` constraint silently ignored), and a mixed enum with data-carrying
variants degraded further to broken `null` array entries. Rather than route
around this, the wizard itself was fixed: generic Select/Multi-Select
rendering for enum-shaped schemas, driven by the name↔string mapping the
schema already carries. This benefits every future enum-typed config field,
not just this one.

### Key design decisions

- **`map_tool_name` returns `Option<String>`, not `Result`** (unlike
  `pi_api_name`), because a sub-agent's `tools` is a list — one unmappable
  entry shouldn't sink the rest. The caller (`ClaudeLauncher::build_agents_json`)
  skips-with-a-warning per entry instead of failing the whole bind.
- **Exact schema shape confirmed by reading the vendored `schemars_derive`
  1.2.1 source** (not guessed): a mixed externally-tagged enum renders as
  `oneOf: [{"type":"string","enum":[<all unit variants>]}, {"type":"object","properties":{"<Variant>":<fields>},"required":["<Variant>"]}, ...]` —
  one alternative for all unit variants "grouped together, one per
  data-carrying variant, each a single-property object keyed by its own
  variant name. A pure unit-only enum skips the `oneOf` wrapper and *is*
  that first alternative directly. This uniform shape is what let one
  generic detector (`enum_choices`) handle both the plain and mixed cases,
  and was verified empirically via a `CaptureUi`-driven round-trip test
  against a real `#[derive(schemars::JsonSchema)]` enum, not just schema
  literals.
- **The wizard's new enum support is fully generic**, not `ToolName`-specific:
  any future enum-typed config field (unit-only, or mixed with tagged
  variants) gets Select/Multi-Select rendering for free.
- **`ToolName`'s starter set is deliberately small** (`FileRead`,
  `FileWrite`, `FileEdit`, `Search`, `FileSearch`, `Shell`, `WebFetch`,
  `WebSearch`, plus `Mcp{server,tool}` and `Other`) — enough for a generic
  coding sub-agent and for "Explore"/"Research"-shaped pre-baked agents
  (read/search/web, no write/shell), expected to grow incrementally rather
  than trying to be exhaustive now.

## Out of Scope (Future Work)

- `pi`/`bob`/`opencode` implementing `BindingType::SubAgent` or their own
  `map_tool_name` — no launcher-specific subagent mechanism exists yet in
  those tools' integration code to map onto.
- Cross-checking that an `Mcp{server, ..}` reference matches a capability
  actually bound on the same launcher — a typo'd server name silently
  resolves to nothing downstream, same as today's raw strings.
- Other wizard gaps: enum-keyed maps, untagged/internally-tagged enums.
  Only the externally-tagged shape schemars derives by default is handled.
- Pre-baked sub-agent capabilities (Explore/Research) themselves.

---

## Sub-Tasks

---

### Sub-Task 1 — Generic enum prompting (`src/utils/ui/prompt.rs`)

**Intent**
Make the schema-driven setup wizard render Select/Multi-Select for
enum-shaped schema nodes, so `ToolName` (and any future enum-typed config
field) doesn't need a stringly-typed workaround.

**Expected Outcomes**
- `EnumChoice { Literal(String), Tagged { key: String, schema: Value } }` and
  `fn enum_choices(root: &Value, node: &Value) -> Option<Vec<EnumChoice>>`,
  detecting a plain `{"type":"string","enum":[...]}` node or a `oneOf` mixing
  that shape with single-property tagged object alternatives. `None` for
  anything else.
- `prompt_enum_scalar` (Select one choice; `Tagged` recurses into
  `prompt_value` for its sub-schema, wrapping the result as `{key: value}`)
  and `prompt_enum_array` (one-shot Multi-Select over an all-`Literal`
  array).
- `prompt_value` dispatches to `prompt_enum_scalar` before the existing
  `get_promptable_type` match; `prompt_object`'s per-property skip check no
  longer drops enum-typed properties; `prompt_array` uses `prompt_enum_array`
  for a pure-literal items schema, or `prompt_enum_scalar` per iteration of
  the existing "Add another?" loop for a mixed one.
- Zero behavior change for any non-enum schema (verified: all 10 pre-existing
  tests in this file still pass unmodified).

**Status** — `[x] done`

---

### Sub-Task 2 — `ToolName` enum (`src/capabilities/base.rs`)

**Intent**
The canonical, launcher-agnostic tool-name representation.

**Expected Outcomes**
- `ToolName` enum (see starter set above), deriving `Serialize`,
  `Deserialize`, `schemars::JsonSchema` — no custom parsing needed, since
  Sub-Task 1 makes it round-trip through config/schema directly.
- `SubAgentBinding.tools` and `SubAgentCapabilityConfig.tools` both
  `Vec<ToolName>` (`sub_agent.rs`'s `bind()` needed no other change —
  `tools: self.config.tools.clone()` already worked once both sides agreed
  on the element type).

**Status** — `[x] done`

---

### Sub-Task 3 — `Launcher::map_tool_name` (`src/launchers/base.rs`)

**Intent**
The per-launcher mapping extension point.

**Expected Outcomes**
- `fn map_tool_name(&self, tool: &ToolName) -> Option<String>` on the
  `Launcher` trait, default: `Other` passes through verbatim, every other
  variant returns `None` — mirrors `env_overlay`'s "no-op until you actually
  support the feature" default, so `pi`/`bob`/`opencode` need no changes.

**Status** — `[x] done`

---

### Sub-Task 4 — `ClaudeLauncher` wiring (`src/launchers/claude.rs`)

**Intent**
Give Claude Code's sub-agents real, correct tool names instead of an opaque
passthrough.

**Expected Outcomes**
- `map_tool_name` override covering all 8 canonical variants plus
  `Mcp{server,tool}` → `mcp__<server>[__<tool>]` and `Other` → itself.
- `build_agents_json` (previously `serde_json::json!(binding.tools)`
  verbatim) now maps each `ToolName` via `self.map_tool_name`, skipping (and
  `ui.warn`-ing, per sub-agent) anything with no mapping — needed `ui: &dyn
  Ui` threaded in from `launch()`.

**Status** — `[x] done`

---

### Sub-Task 5 — Tests

**Intent**
Cover the new wizard mechanics end-to-end (not just the detection heuristic
in isolation), the enum itself, the trait default, and the launcher mapping.

**Expected Outcomes**
- `prompt.rs`: unit tests on `enum_choices` (pure enum, mixed with tagged
  variants, non-enum schemas, malformed `oneOf` alternatives), plus two
  `CaptureUi`-driven round-trip tests against a real derived mixed test enum
  (one picking a tagged variant and filling its sub-fields, one picking a
  plain literal) and one against a pure unit-only test enum proving the
  one-shot Multi-Select path.
- `sub_agent.rs`: `bind()` test updated to construct `Vec<ToolName>`
  (a canonical name, and the `Other` escape hatch) and assert the binding
  carries them.
- `launchers/base.rs`: `map_tool_name`'s default behavior via the existing
  `FakeLauncher` double.
- `launchers/claude.rs`: every canonical variant's mapping, `Mcp` formatting
  with/without a specific tool, `Other` passthrough; `build_agents_json`
  tests updated to construct `ToolName` values.

**Status** — `[x] done`
