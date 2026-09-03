# Plan: Config Reference Integrity & Remediation

## Overview

Configured items reference each other by id (a capability's `model_id`, a
model's `provider_id`, a launcher's `enabled_capabilities`), and nothing
validates those references stay live.

Configuration may become inconsistent upon removal (see #47) which causes
dangling references that may prevent using a command until the issue is
fixed (see #90). It can also become inconsistent during creation, when a
nested "configure a new X" step silently fails to persist but is trusted
anyway.

A dangling capability reference is worse than a failed command today: model
resolution panics rather than returning an error, so an unrelated broken
entry can take down a command that had no interest in it.

This plan covers three aspects for the entire CLI:
- how to detect inconsistencies in configuration
- how to notify the user about them and help them fix such issues seamlessly
- how to prevent new inconsistencies from being introduced while configuring
  something else

## Proposal

A function checks whether a configured instance's references resolve.
Commands call it for the instance they were asked to act on and handle the
result according to the policy table below. Removal and the setup wizards
also call it before writing, so a broken reference is not created in the
first place.

The check reads config and registry metadata, so read-only commands can call
it. What it reads differs per reference, because the config stores the three
ids in different places:

```
┌──────────────────┐
│  LauncherConfig  │
└──────────────────┘
         │  enabled_capabilities: Vec<String>
         │  (wrapper field; no type knowledge needed)
         ▼
┌──────────────────┐
│ CapabilityConfig │ <─── Registry metadata().dependencies
└──────────────────┘      (declares dep.config_key for this type)
         │  config[dep.config_key]
         │  (opaque JSON; inspects key named by metadata)
         ▼
┌──────────────────┐
│    ModelConfig   │
└──────────────────┘
         │  provider_id: Option<String>
         │  (wrapper field; no type knowledge needed)
         ▼
┌──────────────────┐
│  ProviderConfig  │
└──────────────────┘
```

Two of the three are plain fields on the config wrapper structs, so checking
them needs no type at all. A capability instead stores its id inside its own
config JSON, so something has to say which key holds it. That something
already exists: `CapabilityMetadata` carries a static `dependencies` list
whose `config_key` names exactly that key, reachable by type name through
the registry. `setup` already reads it that way to collect model
requirements without building anything.

The three sections below follow the three aspects named in the Overview,
and every sub-task belongs to exactly one of them.

### 1. Detecting inconsistencies

Validation is one function over `Config` and registry metadata, recursing
one hop at a time (Sub-Task 1):

```rust
fn validate_ref(kind: RefKind, id: &str, config: &Config)
    -> Result<(), ValidationError>

enum ValidationError {
    NotConfigured { kind: RefKind, id: String, reason: String },
    Other(String),
}
```

The command drives the check, and it covers only what that command names,
walking transitively from there. A command never reports a problem in a part
of the configuration it was not asked about, so an unrelated broken entry
does not block the work in hand. Construction cannot be the driver instead,
because `ModelSource::from_config` is eager and is rebuilt inside
`ConfiguredModel::resolve()`, so building one instance already touches the
whole configuration.

```
capability setup chat     ->  chat, its model, that model's provider
launch claude             ->  claude, its enabled capabilities,
                              their models, those models' providers
model list                ->  every model (the command's whole subject)
provider remove ollama    ->  whatever currently points at ollama
```

The same check also runs inside construction, to remove the panic. The
factory's generated `construct()` calls it before building an instance and
returns an error if a reference does not resolve. Today that path aborts the
process instead: `ConfiguredModel::resolve()` panics when a capability names
a model that is no longer configured.

### 2. Notifying and remediating

What a command does about a broken reference depends on what the user was
doing.

| Command | On a broken reference | Implemented by |
|---|---|---|
| List (`model/capability/launcher/provider list`) | Inline annotation, never prompt | Sub-Task 3 |
| Info/detail (`model/capability info`) | Warning + offer remediation | Sub-Task 3 |
| Setup (create) | Remediation only for a dependency the wizard is about to use | Deferred, see [out of scope](#out-of-scope-future-work) |
| Setup (overwrite of an existing instance) | Visually flag defaults that are themselves broken | Sub-Task 5 |
| Launch | Offer remediation, abort by default if declined | Sub-Task 3 |
| Remove | Warn and offer a choice before creating a new dangling reference | Sub-Task 4 |

One shared prompt serves every command that offers a fix, so the choices
read the same wherever they appear (Sub-Task 2).

Fixing one reference can reveal another: choosing a replacement model for a
capability can point it at a model whose own provider is gone. A command that
remediates therefore re-runs its scoped validation after each accepted fix
and keeps prompting until the walk comes back clean, so one run of the
command ends with a configuration that resolves rather than asking the user
to run it again.

Remediation is interactive-only, gated on **both** `Ui::is_interactive()`
*and* the command not being in an auto/non-prompting mode (e.g. `setup
--auto`), since `is_interactive()` alone only reflects the output backend,
not a command-level flag. Warnings always show.

### 3. Preventing new inconsistencies

There are two points where a dangling reference gets created today, and both
can check before writing.

The first is removal: deleting an entry that something else points at strands
that reference, so the command offers a choice before it happens
(Sub-Task 4).

The second is creation: a nested "configure a new X" step can report success
without persisting anything, and an overwrite wizard can present a broken
current value as a default that the user accepts without noticing.
Both are handled by validating what was just produced rather than trusting
it (Sub-Task 5).

## Out of Scope (Future Work)

- Broken candidates in `setup` selection lists. `*Source::from_config`
  filters a dangling instance out of `instances()`, so a broken model never
  appears in "Select a model for this capability" and reads as one that was
  never configured. Listing it with `ui.warn_mark` and forcing a reconfigure
  when it is picked would say more, but it changes the candidate lists of
  every `setup` flow, which is wider than this refactor.
- Runtime liveness (#36). Whether a provider is actually reachable is a
  separate axis from whether config references resolve, and giving `Model`,
  `Capability` and `Launcher` a `health_check()` is its own piece of work.
  Keeping it out is what lets validation stay pure and I/O-free. Note that
  #36 proposes a status column at list time, the same surface Sub-Task 3
  annotates, so the two want one column carrying both signals rather than
  two columns.
- Warning stream routing across the UI backends (#118). Every text backend
  sends `warn()` to stdout and `error()` to stderr today, and the `Ui` trait
  specifies stderr only for `error`. Whether `warn` should join it is a
  policy question about the trait, not about config integrity.
- `ModelSource` eagerness (#58). It builds every configured model on each
  `ConfiguredModel::resolve()`, which is both wasted work and the reason
  construction cannot be scoped. #58's lazy memoised `construct_shared` is
  that fix; this plan works around it meanwhile by driving validation from
  the command layer.
- Malformed instance config (#59). `ConfigConstructable::new` deserialises
  with `unwrap_or_default()`, so a corrupt config silently becomes a default
  rather than an error. Validation here checks that references resolve, not
  that each instance's own config parses.
- Checking that a referenced model *satisfies* the `ModelRequirement` its
  dependent declares, rather than merely existing.
- Cross-process conflict resolution / file locking. granite-cli has no
  daemon; every invocation is load-run-exit, so this is a narrow race, not
  addressed here.

---

## Sub-Tasks

Sub-Task 1 detects, 2 and 3 notify and remediate, 4 and 5 prevent.

---

### Sub-Task 1 — Scoped reference validator

**Intent**
One shared way to ask whether a named instance's references resolve,
answering it from configuration and registry metadata alone, without
constructing anything and without straying outside what was asked about.

**Expected Outcomes**

A new module provides `validate_ref` and the `ValidationError` it returns
(see Proposal for both). It resolves one hop at a time and recurses, so a
launcher whose capability's model's provider is missing reports the missing
provider rather than stopping at the first level.

Each kind reads its references from where they actually live. A launcher
walks the ids in `enabled_capabilities`. A capability looks up its type's
static `dependencies` in the registry and, for each entry, reads the id from
its own config JSON under that entry's `config_key`. A model reads
`provider_id`. Providers have no outbound references and always pass.

A model with no `provider_id` at all fails, distinctly from one whose
`provider_id` points at nothing:

```
provider_id: None            ->  "no provider configured"
provider_id: Some(dangling)  ->  "provider 'ollama' is not configured"
```

Both are unusable today, since every path that reaches a model needs its
provider, but they are different problems and the messages should say so.

Alongside it, a helper answers the same question for a whole kind at once,
which is what a list command needs:

```rust
fn find_dangling(kind: RefKind, config: &Config) -> Vec<DanglingRef>

struct DanglingRef {
    kind: RefKind,
    instance_id: String,
    reason: String,     // the validation error, verbatim
}
```

Scanning a whole kind is the exception the scoping rule allows, because a
list command's subject genuinely is every instance of that kind.

Capability dependencies get a single declaration site. The instance method
`Capability::dependencies(&self)` is removed along with its four
hand-written implementations, two of which are inside macros. Production code
already prefers the static form, and the only callers of the instance form
are five tests, which move to metadata. This leaves `metadata()` as the one
place a capability says what it needs.

The factory's generated `construct()` calls `validate_ref` before building
and returns an error instead of proceeding. This stops
`ConfiguredModel::resolve()` panicking on a dangling reference, and gives
`*Source::from_config` the skip-and-warn outcome #90 asks for.

Its public signature is unchanged: it already returns `Result<_, String>`,
and the validation error converts into that string.

Tests cover: a healthy and a dangling instance of each kind, confirming only
the dangling one fails; a launcher → capability → model → provider chain with
only the provider missing, confirming the walk recurses rather than stopping
one hop deep; the two `provider_id` cases producing different messages;
`find_dangling` against a config seeded with several known-broken instances
returning exactly the expected list; and a config whose capability references
a removed model, driven through `construct()`, returning an error where it
previously panicked.

**Relevant Context**
- `src/capabilities/base.rs` (`CapabilityMetadata.dependencies`, `Dependency`, `Capability::dependencies`)
- `src/commands/capability.rs:196-198` (comment on preferring metadata over an instance)
- `src/commands/setup.rs:513-530` (existing static-metadata read)
- `src/models/base.rs:251-261` (`ConfiguredModel::resolve`, the panic)
- `src/registry/mod.rs` (`define_factory!`, generated `construct`)

**Status** — `[ ]` not started

---

### Sub-Task 2 — Remediation prompt + `Ui::is_interactive()`

**Intent**
One reusable prompt that every remediation-offering command drives the same
way.

**Expected Outcomes**

The `Ui` trait gains a method for whether the current session can prompt at
all, defaulting to true and overridden to false only for the JSON and
Markdown backends. This is simpler and less fragile than detecting
interactivity by pattern-matching on the existing "non-interactive" error
string.

```rust
fn is_interactive(&self) -> bool  // default: true
```

The prompt walks broken references one at a time, offering three choices:
reconfigure the instance, which drives the existing per-type setup command
pre-selected on the right instance; remove it, via the existing removal
command; or skip it and leave it as-is. Skip is also the automatic fallback
whenever prompting is not offered, either because the session is not
interactive or because the command is running in a non-prompting mode such as
`setup --auto`. `launch` is the exception, aborting by default instead of
skipping.

Remediation is a loop rather than a single pass. After a fix is accepted the
caller re-runs the same scoped validation and prompts for whatever is still
broken, including anything the fix itself introduced. The loop ends when
validation comes back clean, or when a whole pass changes nothing, which is
what a run of skips produces, so a user who keeps declining is never asked
about the same reference twice.

```
⚠ Configuration issue (1 of 2)

  Capability 'chat' (agent-model) depends on model
  'granite-3.1-8b-instruct', which is no longer configured.

  [1] Reconfigure 'chat' now — pick a different model
  [2] Remove 'chat'
  [3] Skip for now — 'chat' stays disabled until fixed
>
```

Tests cover: canned answers confirming that reconfigure invokes setup
pre-selected on the right instance, that remove calls the right removal
function, and that neither a non-interactive session nor an auto-mode flag
ever reaches the underlying prompt call; a fix that repairs one reference
while exposing a second, confirming the loop re-validates and prompts again
before returning; and a pass in which every problem is skipped, confirming
the loop stops instead of re-offering the same choices.

**Relevant Context**
- `src/commands/capability.rs` (`CapabilityCommands::setup`, reused by reconfigure)

**Status** — `[ ]` not started

---

### Sub-Task 3 — Wiring list, info/detail, and launch

**Intent**
Drive the shared prompt from the commands that should offer it, and surface
a broken reference where the user would look for it, without turning a
read-only command into an interruption.

**Expected Outcomes**

List commands, for models, capabilities, launchers, and providers, gain a
status column or inline suffix populated from `find_dangling()`:

```
ID       PROVIDER   NOTES
model-1  ollama
model-2  ollama     ⚠ invalid provider
model-3  lm-studio
```

They never prompt, whatever they find. A list reports that a problem exists;
acting on it is left to a command the user chooses to run next.

Info and detail commands validate the instance they were asked about, show a
warning, and offer the remediation prompt from Sub-Task 2 when the session
allows prompting.

`launch` validates the launcher it was asked to run, the capabilities it has
enabled, and what those resolve to, before starting anything. A broken
reference is offered the same prompt, but declining aborts the launch rather
than skipping, because a capability that cannot bind would fail later during
the launch itself. This check runs before the existing binary check, so a
config problem is reported before anything about the environment.

Remediation during a fresh `setup`, for a dependency the wizard is about to
use, is not built here. It depends on broken candidates being visible in the
wizard's selection lists, which is out of scope for this plan.

Tests cover: a UI double that panics on `select`/`confirm`, driven through a
list containing a broken entry, confirming the list never prompts; for info
and detail, canned answers for each remediation choice against a broken
instance, confirming the resulting configuration is correct; and, for
`launch`, that declining remediation aborts before any subprocess is spawned,
while accepting it proceeds with the repaired configuration.

**Relevant Context**
- `src/commands/model.rs`, `capability.rs`, `launcher.rs`, `provider.rs`
  (`list` and `info` functions)
- `src/commands/launcher.rs` (current `launch` pre-flight: `validate_command()` only)

**Status** — `[ ]` not started

---

### Sub-Task 4 — Remove-time dependency check

**Intent**
Stop `Remove` from stranding whatever pointed at what it just deleted.

**Expected Outcomes**

Before any of the four removal methods on `Config` deletes an entry, we scan
the other configuration maps for anything that depends on the id being
removed. If something does, the command layer, not `Config` itself per Spec
0001, offers a choice: remove both together, cancel, or remove only what was
asked for. Non-interactive backends default to removing only what was asked,
with a warning.

```
⚠ Removing 'granite-3.1-8b-instruct' will break:
  - capability 'chat' (agent-model)

  [1] Remove 'granite-3.1-8b-instruct' and 'chat' together
  [2] Cancel — keep 'granite-3.1-8b-instruct'
  [3] Remove only 'granite-3.1-8b-instruct' — fix 'chat' later
>
```

Tests cover: a model with one dependent capability producing the right final
configuration for each of the three outcomes, including the non-interactive
default.

**Relevant Context**
- `src/config/mod.rs:318-418` (`remove_model`/`remove_provider`/`remove_capability`/`remove_launcher`, all currently unconditional)

**Status** — `[ ]` not started

---

### Sub-Task 5 — Creation-time integrity

**Intent**
A configure step that did not actually persist, or a default the user never
looked at, must not become a new dangling reference.

**Expected Outcomes**

Every insert method on `Config` updates its in-memory map before attempting
to save to disk, and every caller treats a save failure as a non-fatal
warning rather than propagating it, so a nested setup step can report success
even though nothing was written. The fix does not need a bespoke check: the
two "configure a new X" helpers, the one resolving a capability's model
dependency and the one selecting a provider for a model, validate the id they
were just given once the nested setup call returns, instead of trusting
`Ok(())` alone.

Separately, the provider-selection helper does not re-validate the id it
returns at all. If a user picks "configure a new provider," types a name that
collides with an existing provider, and declines to overwrite it, the helper
still returns that existing provider's id, even if it does not satisfy what
the model needs. It should get the same re-validation.

The overwrite wizard is the third case. When `setup` presents an existing
instance's current values as defaults, a default that is itself a dangling
reference is flagged inline, so pressing Enter through the wizard cannot
silently re-save it:

```
Model for 'chat' [current: granite-4.2-8b — ⚠ no longer configured]:
  > granite-vision
    granite-3.1-8b
```

Tests cover: inserting a provider into an unwritable configuration directory
returns an error while the entry still appears in memory, which is the root
cause this sub-task addresses; declining an overwrite onto a mismatched
existing provider is rejected or re-prompted rather than silently reused; and
an overwrite wizard whose current value is dangling renders the flag rather
than offering it as a clean default.

**Relevant Context**
- `src/config/mod.rs:313-413` (`insert_model`/`insert_provider`/`insert_capability`/`insert_launcher`, `save()`)
- `src/commands/capability.rs:334-352` (`resolve_model_dependency`)
- `src/commands/model.rs:722-770` (`select_provider`)

**Status** — `[ ]` not started
