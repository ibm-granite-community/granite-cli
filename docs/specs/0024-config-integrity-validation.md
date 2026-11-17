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
         │  provider_id: String
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

struct ValidationError {
    /// What did not resolve.
    target: (RefKind, String),
    problem: Problem,
    /// The configured instance holding the broken reference, absent when the
    /// instance asked about is itself the problem.
    referrer: Option<(RefKind, String)>,
}

enum Problem {
    NotConfigured,
    UnknownType { type_name: String },
    MissingDependency { config_key: String },
}
```

The referrer is there because a caller acting on a failure acts on the
instance that holds the reference, not on the missing thing. `launch claude`
finding that `chat`'s model is gone offers to reconfigure `chat`, and the
prompt in Sub-Task 2 names it. `Problem` is an enum rather than a message so
that caller branches on a variant instead of on text.

A caller names what it is about to use and branches on the failure to decide
what to offer:

```rust
// The launch check: the launcher, its enabled capabilities, their models,
// and those models' providers.
validate_ref(RefKind::Launcher, "claude", &config)?;

// A failure names what to act on as well as what is missing, so a caller
// offering a fix reconfigures the capability rather than the model.
if let Err(e) = validate_ref(RefKind::Capability, "chat", &config) {
    match (&e.problem, &e.referrer) {
        (Problem::NotConfigured, Some((kind, id))) => reconfigure(*kind, id),
        _ => ui.warn(&e.to_string()),
    }
}
```

The command drives the check, and it covers only what that command names,
walking transitively from there. A command never reports a problem in a part
of the configuration it was not asked about, so an unrelated broken entry
does not block the work in hand. Construction cannot be the driver instead,
because `construct()` serves two kinds of caller and cannot tell them apart.
One builds an instance drawn from the configuration. The other builds a
transient instance of something that is deliberately not configured, which is
how the setup wizard probes provider and launcher types and how `model setup`
builds a model before saving it. Only the caller knows which it is.

```
capability setup chat     ->  chat, its model, that model's provider
launch claude             ->  claude, its enabled capabilities,
                              their models, those models' providers
model list                ->  every model (the command's whole subject)
provider remove ollama    ->  whatever currently points at ollama
```

Validation runs where something is about to be used, and nowhere else,
so a broken entry blocks only the commands that would have used it.
A list command is the one case that walks every instance of a kind,
because its subject is every instance of that kind.


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
  filters a dangling instance out of `instances()`, so a broken model or
  capability never appears in "Select a model for this capability" and reads
  as one that was never configured. Listing it with `ui.warn_mark` and
  forcing a reconfigure when it is picked would say more, but it changes the
  candidate lists of every `setup` flow, which is wider than this refactor.
  Launcher setup does say what it could not offer, and carries a
  previously-enabled id through rather than dropping it, so editing a launcher
  never disables what the walk would otherwise report.
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
  `ConfiguredModel::resolve()`, which is wasted work. #58's lazy memoised
  `construct_shared` is that fix; this plan leaves the eager build in place
  and drives validation from the command layer.
- The `ConfiguredModel::resolve` panic itself (#90). This plan detects a
  dangling reference and tells the user about it, and `CapabilitySource`
  skips an instance whose references do not resolve rather than constructing
  it, so the construction sites no longer reach the abort. The panic is still
  there for any caller that resolves a model directly.
  Fixing that requires making `ConfigConstructable::new` fallible. Once 
  `global_config` is removed from `ConfigConstructable::new`, the next step
  will be to make it fallible and fix the panic in #90
  with `unwrap_or_default()`, so a corrupt config silently becomes a default
  rather than an error. Validation here checks that references resolve, not
  that each instance's own config parses.
- Checking that a referenced model *satisfies* the `ModelRequirement` its
  dependent declares, rather than merely existing.
- Cross-process conflict resolution / file locking. granite-cli has no
  daemon; every invocation is load-run-exit, so this is a narrow race, not
  addressed here.
- Re-typing a broken instance in place (issue to file). When an instance's
  `*_type` is not in the registry the only repair offered is removal, and
  removing a model breaks every capability that points at it. Re-typing would
  keep the instance id, and so every inbound reference, which is what a
  catalog rename actually calls for. It needs a type picker per kind, which
  does not exist as a reusable piece today, and it leaves behind an instance
  id that may still name the old type.
- The second confirmation when reconfiguring (issue to file). Choosing to
  reconfigure an instance leads into setup, which then asks whether to
  overwrite the instance that is already configured, defaulting to no. That
  is two confirmations for one decision, and accepting the default leaves the
  reference broken. Fixing it means changing how the four setup commands
  treat an explicit instance id, which affects every caller of them.
- A flag for non-interactive removal (issue to file). A session with nobody
  to ask always removes only what was asked and warns about what that broke.
  A script has no way to say that it wants the dependents removed too, or
  that it wants the removal refused when anything depends on the target. A
  flag on the four removal commands would give it both, and would give the
  refusal a non-zero exit to act on.
- Removing a capability without breaking or deleting its launcher (issue to
  file). A launcher depends on a capability by listing it in
  `enabled_capabilities`, so the removal prompt's three answers are: delete
  the launcher too, do nothing, or leave the launcher pointing at something
  that is gone. None of them deletes exactly what was asked and leaves the
  launcher working. A fourth choice would drop the id from that list as part
  of the removal. Reaching that end state today takes two commands: remove
  only the capability, then accept the disable the next `launch` offers.
  The behaviour exists in `remediation::disable` and needs extracting to
  take an explicit launcher and capability id.
- Sibling problems a declined prompt hides (issue to file). Remediation
  reports one problem at a time and stops when the user declines, so a
  launcher with two broken capabilities only ever names the first. Reporting
  the second would mean gathering every error rather than stopping at the
  first, which is the collect-all walk Sub-Task 1 deliberately does not have.
- Launching without a broken capability (issue to file). A launcher with
  several enabled capabilities cannot start while any one of them is broken:
  declining the prelaunch aborts, and there is no way to run with the rest.
  Acknowledging the problem is not enough on its own, because the capability
  stays enabled and model resolution panics when it is constructed (#90). The
  repair is to drop the broken id from `enabled_capabilities` for that run
  only, which `run_launch` can do on the clone it already takes, and which
  re-validation then reports clean. It needs a fourth action on the prelaunch
  prompt, a line naming what was dropped so the reduced session is visible,
  and a decision about what to do when every capability is broken.

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
`provider_id`. Providers have no outbound references between instances.

Each of the four kinds also names its implementation type (`*_type` fields),
and that reference is checked too. 

A capability's dependency is validated whenever it holds an id, whether the
dependency is declared required or not, so a dangling optional dependency is
reported exactly like a dangling required one.

`ModelConfig.provider_id` is required, so a model always names a provider and
the only thing that can be wrong with it is that the name resolves to nothing,
which reads like any other dangling reference: "provider 'ollama' is not
configured".

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



Tests cover: a healthy and a dangling instance of each kind, confirming only
the dangling one fails; a launcher → capability → model → provider chain with
only the provider missing, confirming the walk recurses rather than stopping
one hop deep; `find_dangling` against a config seeded with several
known-broken instances returning exactly the expected list.

**Relevant Context**
- `src/capabilities/base.rs` (`CapabilityMetadata.dependencies`, `Dependency`, `Capability::dependencies`)
- `src/commands/capability.rs:196-198` (comment on preferring metadata over an instance)
- `src/commands/setup.rs:513-530` (existing static-metadata read)
- `src/commands/setup.rs:122-138`, `:335-350` (discovery constructing types
  that are deliberately not configured)
- `src/commands/model.rs:610-615` (a live instance built before anything is
  written)
- `src/models/mod.rs:35-59` (`ModelSource::from_config`, which constructs a
  model whether or not its provider resolves)
- `src/registry/mod.rs:203-231` (the `construct` doc comment)
- `src/capabilities/base.rs:346-376` (`Display for Dependency`, which already
  distinguishes required from optional to the user)
- `src/commands/capability.rs:278-300`, `:420` (`resolve_model_dependency` and
  `resolve_provider_dependency`, where an unsatisfiable optional dependency is
  left unset)

**Status** — `[x] done`

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
pre-selected on the right instance; remove it; or skip it and leave it as-is.
Skip is also the automatic fallback whenever prompting is not offered, either
because the session is not interactive or because the command is running in a
non-prompting mode such as `setup --auto`. `launch` is the exception,
aborting by default instead of skipping.

What removal means depends on how remediation was reached. A caller that
named the instance itself, such as `capability info chat`, is offered deletion
through the existing removal command. Remediation reached through a launcher
is offered disabling instead: the id is dropped from that launcher's
`enabled_capabilities` and the capability stays configured. A capability may
be enabled by several launchers, so `launch claude` offering to delete one
would change more than the launcher it was asked about. Both shapes a launcher
produces are covered, whether the enabled capability is itself broken or is
not configured at all.

An unknown type name is the one case with two choices rather than three.
Setup cannot run a type that is not in the registry, so offering to
reconfigure would only produce a failed fix, and removal is the only repair
on offer.

Remediation is a loop rather than a single pass. After a fix is accepted the
caller re-runs the same scoped validation and prompts for whatever is still
broken, including anything the fix itself introduced. It ends in one of three
ways: validation comes back clean, the user declines, or every repair on offer
has been tried against the same problem.

A repair that leaves the problem exactly as it was is dropped from the choices
rather than ending the run. Reconfiguring is the case that matters: a user who
walks out of the wizard, or who goes in to change something else, comes back to
the same question with the remaining repairs still reachable instead of having
to re-run the command. A different problem starts over with all of them.

Declining ends the whole loop rather than moving to the next problem. The
scoped walk is deterministic and reports one problem at a time, so the next
pass would report the reference just declined. This means a launcher with two
broken capabilities only ever names the first, which a list command shows in
full.

Removing the instance the caller asked about leaves it unresolved, since what
the caller named no longer validates. For `launch` that is the right answer
anyway, and for an info or detail command it means there is nothing left to
show.

There is no "1 of N" counter. A scoped walk stops at the first problem, so a
total is not knowable without gathering every error, which Sub-Task 1
deliberately does not do.

```
⚠ Configuration issue: capability 'chat' depends on model
  'granite-3.1-8b-instruct', which is not configured

? What would you like to do?
  [1] Reconfigure capability 'chat' now
  [2] Remove capability 'chat'
> [3] Skip for now, 'chat' stays broken until fixed
```

Reached through `launch claude`, the same problem offers the launcher's own
list rather than the shared instance:

```
⚠ Configuration issue: capability 'chat' depends on model
  'granite-3.1-8b-instruct', which is not configured

? What would you like to do?
  [1] Reconfigure capability 'chat' now
  [2] Remove capability 'chat' from launcher 'claude'
> [3] Cancel
```

Tests cover: canned answers confirming that reconfigure invokes setup
pre-selected on the right instance, that remove calls the right removal
function, and that neither a non-interactive session nor an auto-mode flag
ever reaches the underlying prompt call; a fix that repairs one reference
while exposing a second, confirming the loop re-validates and prompts again
before returning; a fix that returns having changed nothing, confirming it
drops out of the choices while the rest stay reachable, and that a launch can
still disable after one; a declined
problem, confirming the loop stops instead of re-offering the same choices;
an unknown type, confirming removal is offered without reconfiguration; a
launcher root, confirming disabling is offered in place of deletion for both
a broken capability and one that is not configured, that it leaves the
capability configured, and that a caller naming the capability is still
offered deletion; and the JSON and Markdown backends answering that they
cannot prompt.

**Relevant Context**
- `src/commands/capability.rs` (`CapabilityCommands::setup`, reused by reconfigure)

**Status** — `[x] done`

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

`model list` builds its rows from constructed instances, so a model whose
type is not in the registry never reaches the table and would disappear at
exactly the moment something is wrong with it. It gets a row of its own,
carrying the id and the reason with the remaining columns empty. A list
filtered by model type leaves those rows out, since there is no metadata to
filter them on.

Info commands validate the instance they were asked about and offer the
remediation prompt from Sub-Task 2 when the session allows prompting, then
render the state that results, so what is shown reflects any repair. An id
that names a catalog type rather than a configured instance is being
browsed, not diagnosed, and is neither validated nor annotated. Removing the
instance is one of the offered choices, and leaves the command with nothing
to show, so it returns without rendering.

`launch` validates the launcher it was asked to run, the capabilities it has
enabled, and what those resolve to, before starting anything. A broken
reference is offered the same prompt, but declining aborts the launch rather
than skipping, because a capability that cannot bind would fail later during
the launch itself. This check runs before the existing binary check, so a
config problem is reported before anything about the environment. It is a
`prelaunch` function on the launcher commands, called by `run_launch` after
the configuration is loaded and before the launcher is constructed.

Remediation during a fresh `setup`, for a dependency the wizard is about to
use, is not built here. It depends on broken candidates being visible in the
wizard's selection lists, which is out of scope for this plan.

Tests cover: a list containing a broken entry, confirming the annotation
appears and that no prompt is reached; a model whose type is unknown, which
keeps its row; for info, canned answers for reconfigure and for remove
against a broken instance, confirming the resulting configuration is
correct, and a catalog id, confirming it is not diagnosed; and, for the
launch prelaunch, that declining aborts before anything is constructed
while accepting proceeds with the repaired configuration.

**Relevant Context**
- `src/commands/model.rs`, `capability.rs`, `launcher.rs`, `provider.rs`
  (`list` and `info` functions)
- `src/commands/launcher.rs` (current `launch` prelaunch: `validate_command()` only)

**Status** — `[x] done`

---

### Sub-Task 4 — Remove-time dependency check

**Intent**
Stop `Remove` from stranding whatever pointed at what it just deleted.

**Expected Outcomes**

Before any of the four removal commands deletes an entry, we scan the other
configuration maps for anything that depends on the id being removed. If
something does, the command layer, not `Config` itself per Spec 0001, offers
a choice: remove both together, cancel, or remove only what was asked for.
Non-interactive backends default to removing only what was asked, with a
warning.

Finding the dependents is `Validatable::refs` read backwards: an instance
depends on the target when the target appears among the references it
declares. It lives beside the walk in the validation module and needs no
knowledge of its own about which field holds what.

Cancelling is the default answer, since this is the destructive prompt and
the other two both delete something. It reports what was kept and succeeds
rather than failing: the user made a deliberate choice, and a script never
reaches it, since a session with nobody to ask does not prompt.

The prompt lists everything that removing them together would take: the
direct dependents and, at every further level, what depends on those. It is
the one question for the whole chain. The reference graph runs launcher to
capability to model to provider, so the chain is at most that deep.

```
⚠ Other instances depend on model 'granite-3.1-8b-instruct':
  - capability 'chat'
  - launcher 'claude'

? What would you like to do?
  [1] Remove model 'granite-3.1-8b-instruct' and the 2 instances listed above
> [2] Cancel, keep model 'granite-3.1-8b-instruct'
  [3] Remove only model 'granite-3.1-8b-instruct', fix the rest later
```

The whole chain is worked out before anything is printed or removed, and an
instance reached on two paths is listed once. Everything in the plan is then
removed without asking again, dependents before what they depend on, and each
removal prints its own line (`Launcher 'claude' removed.`).

When a removal leaves instances in place that point at something removed, by
the third answer or by `--dependents leave` or `--dependents discovered`, one
section lists each of them with the removed instance that breaks it, and the
setup command for each kind left broken:

```
⚠ These stay, and will not work until fixed:
  - capability 'chat' (agent-model): its model 'granite-3.1-8b-instruct' is removed
  Fix it with `granite-cli capability setup <type> --id <id>`.
Model 'granite-3.1-8b-instruct' removed.
```

A listed instance shows its type when the type differs from its id, since
setup takes the type. `--dependents cascade` prints the plan as
`Removing model 'granite-3.1-8b-instruct' and what depends on it:` followed by
the same list the prompt shows, and has no section of instances left broken.

Tests cover: a model with one dependent capability producing the right final
configuration for each of the three outcomes, including the non-interactive
default; a removal with no dependents, confirming it does not prompt; and the
dependents scan itself, in each direction and for a target nothing points
at.

**Relevant Context**
- `src/config/mod.rs:318-418` (`remove_model`/`remove_provider`/`remove_capability`/`remove_launcher`, all currently unconditional)

**Status** — `[x] done`

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
the model needs. It gets the same re-validation, by reading what is
configured under that id after setup returns and comparing its type against
the one being configured. A mismatch is rejected with an error naming the
collision, rather than re-prompted: the wizard has no loop to return to, and
the message says what to do differently.

The overwrite wizard is the third case. When `setup` presents an existing
instance's current values as defaults, a default that is itself a dangling
reference is flagged inline, so pressing Enter through the wizard cannot
silently re-save it:

```
Select a model for this capability [current: 'granite-4.2-8b', ⚠ no longer resolves]
  > granite-vision
    granite-3.1-8b
    Configure a new model...
```

The flag says the reference no longer resolves rather than naming which of
the four problems it is, since the prompt is asking for a replacement either
way. A current value that still resolves is named without a flag, so the
wizard always says what pressing Enter would be replacing. The same applies
to a model's provider, whose current value comes from the instance being
overwritten.

Tests cover: inserting a provider into an unwritable configuration directory
returns an error while the entry still appears in memory, which is the root
cause this sub-task addresses; declining an overwrite onto a mismatched
existing provider is rejected rather than silently reused; and an overwrite
wizard whose current value is dangling renders the flag, while one that still
resolves is named without it.

**Relevant Context**
- `src/config/mod.rs:313-413` (`insert_model`/`insert_provider`/`insert_capability`/`insert_launcher`, `save()`)
- `src/commands/capability.rs:334-352` (`resolve_model_dependency`)
- `src/commands/model.rs:722-770` (`select_provider`)

**Status** — `[x] done`

---

## Implementation Summary

Measured against `main` at bf117bb, the commit merged into the branch carrying
Sub-Tasks 1 to 3. Every production figure excludes the file's own `#[cfg(test)]`
items. The figures cover everything the two branches carry, which includes
making `ModelConfig.provider_id` required, a change that came out of the review
of Sub-Tasks 1 to 3.

### New modules

| Module | Production | Tests | Line coverage |
|---|---:|---:|---:|
| `src/config/validation.rs` | 436 | 412 | 98.64% |
| `src/commands/shared/remediation.rs` | 430 | 552 | 97.92% |

The validator answers three questions, `validate_ref`, `find_dangling` and
`dependents`, over one `Validatable` implementation per config type.
Remediation drives the prompts: `remediate`, `confirm_removal`,
`dangling_notes` and `prompt_with_current`. A third new file,
`src/commands/shared/mod.rs`, declares the remediation module in four lines.

### Existing code

| | Added | Removed |
|---|---:|---:|
| Production | 382 | 110 |
| Tests | 376 | 145 |

Twenty-four files: the five command modules and the `mod.rs` that declares
them, the capability registrations, the `Ui` trait with its JSON and Markdown
backends and the TUI app, `Config`, the model source, the registry macro,
`main`, and the two launchers whose test doubles implemented the removed
`dependencies` method.

### Tests

55 new, 27 updated, 8 removed or renamed away. The suite goes from 692 test
functions to 739, and from 720 passing tests to 767. The gap between the two
counts is the macro-generated output-contract tests each `Ui` backend gets.

### Coverage

| | Before | After |
|---|---:|---:|
| Lines | 78.79% | 80.38% |
| Regions | 78.45% | 79.91% |
| Functions | 77.14% | 78.75% |

Measured with `cargo llvm-cov`. Per-file coverage counts a file's own
`#[cfg(test)]` module, which runs in full, so a file that is half tests reads
higher than its production half alone. Where the misses sit is exact, since
test code always executes: `validation.rs` leaves 7 of 516 instrumented lines
uncovered and `remediation.rs` 14 of 672, and all of those are production
lines.

### Other

27 Rust files changed, plus this document, and no new dependencies. The
declaration sites for a capability's `model_id` config key drop from eight to
four, one per capability type, which is what removing
`Capability::dependencies(&self)` was for. Neither new module carries
`#![allow(dead_code)]`: every item has a production caller.
