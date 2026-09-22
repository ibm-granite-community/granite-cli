# Plan: Instance Identity, Sources and Reference Resolution

## Overview

Providers, models, capabilities and launchers point at each other by name: a
capability names a model, a model names a provider, a launcher lists the
capabilities it enables. Turning those names into objects is the job of
`ConfigConstructable::new`, which receives the whole application
configuration to do it.

The way this works today:

- The constructor does the lookups, so every implementation takes the whole
  application configuration. Most of them ignore it, and two call sites pass
  `Config::default()` because they have nothing else to pass.
- The constructor cannot report a failure. A name that resolves to nothing
  calls `panic!` (#90), a settings blob that does not parse is replaced by
  defaults with nothing said (#59 item 2), and a provider whose HTTP client
  cannot be built calls `.expect`, which ends the process where an error would
  have named the provider.
- Each lookup builds a whole collection, takes the one instance it wanted and
  discards the rest, warning about unrelated broken entries in a place where
  nobody can act on them. One dangling capability stops `launcher setup` from
  running at all (#90).
- A collection is built per call site, so one run holds several objects for
  one configured id, and what an instance copied at construction can be out of
  date by the time it is used: a model carries its provider's settings by
  value under a string key that `src/models/mod.rs` and `build.rs` have to
  agree on (#51, #59 item 6).
- A capability's `ModelRequirement` is checked once, when setup picks the
  model. The requirement and the model's metadata both ship in the binary
  while the pairing sits in the user's configuration, so an upgrade can move
  either one and leave a configuration that `capability list` reports as
  healthy and that binds without complaint.

### Principles

Seven rules decide the design. Each names the issues it answers.

1. **One object per configured instance, per configuration snapshot.** A
   source builds an instance the first time its id is asked for and returns
   the same object every time after that. This is what #51 asks for when it
   says instances should be held as singletons, and the memoisation half of
   #58.

2. **One live set of sources, owned by the application context.** The four
   sources are built together from the configuration in memory and discarded
   when configuration is written, so a name built for one caller is the object
   every later caller gets. Exactly one live set is also what makes the
   session proxy complete: while two sets coexist, a capability resolved from
   the wrong one binds the real upstream and its traffic leaves unrouted and
   uncounted. This is the second half of #51, and it is not a process global,
   since a test and a launch each build their own set.

3. **Construction takes an instance id and that instance's own settings,
   returns a result, and does no I/O.** The same signature for every registry.
   This is #58's single-argument constructor, #59 item 2's `unwrap_or_default`
   and #59 item 5's `Model::provider()`, which reaches into `PROVIDER_REGISTRY`
   with a default configuration from inside the model layer.

4. **References are resolved by the source that owns what is named**, and a
   name that does not resolve is an error naming both ends. This deletes the
   panic in #90 and the sidecar injection in #59 item 6.

5. **Validity has one definition, which is resolution**, and a command
   resolves everything it will use before it does anything with a side effect.
   Once a constructor can report a failure, a walk that reads configuration
   and constructs nothing can no longer see a settings blob that does not
   parse, so the check a command runs first and the build it runs later have
   to be the same code.

6. **A broken instance denies service only to what names it.** In #90 one
   capability naming a removed model took down a command that had no interest
   in that capability.

7. **An error names the instance, the setting and the repair, and is raised
   while the CLI can still offer that repair.** Spec 0024 shipped the prompt
   that offers repairs; every problem found here reaches it, and none of them
   are found after the launched process has taken the terminal.

## Proposal

### The source set and the snapshot

A source holds the settings for its kind, a cache of what it has built, and a
handle to the source it asks: capabilities ask models, models ask providers.
Building a source records settings and constructs nothing, so building the set
is cheap enough to do on the first ask of any command.

```
  AppContext
     │
     ├── configuration snapshot ───────────────────────────────────────┐
     │                                                                 │
     │    providers   models   capabilities   launchers                │
     │    built together, settings recorded, nothing constructed       │
     │                                                                 │
     │    ask "granite"   ──> constructed once ──> kept ──> a handle   │
     │    ask "granite"   ─────────────────────────────────> the same  │
     │                                                                 │
     └── configuration write, or the session proxy starting ───────────┘
              │
              │   the set is discarded, with everything it built
              ▼
          the next ask builds a new set from the new configuration
```

The set is owned by `AppContext`, which hands out handles and discards it
whenever configuration is written. A write is the only way to reach a mutable
`Config`: reads go through `AppContext::config()`, writes through
`AppContext::config_mut()`, and the second drops the set before it returns, so
the borrow checker enforces the invalidation rule. The session proxy handle moves out of `Config` onto the context
for the same reason, which makes starting the proxy one more event that ends a
snapshot and rebuilds the set with providers that point at the local proxy.

A handle taken before a write keeps the instance it names alive, which is what
a launch wants: it pins the set it resolved and runs against it. A caller that
outlives a write asks the context again, so the TUI's setup panes see the
configuration they just wrote.

### Building an instance

Construction is the same for every registry. A constructor reads its own
settings and nothing else: no application configuration, no registry lookup,
no network, no filesystem, no process spawn. What it can report is that its
settings do not parse, or do not satisfy the validation its settings type
declares, which is the report `unwrap_or_default` discards today.

```rust
fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError>;
```

Names are resolved by the source that owns what is named, one hop at a time,
and a failure anywhere in the chain is returned to whoever asked:

```
  CapabilitySource::get("chat")
     │
     ├─ built already? ─────────────────────────> the same handle as before
     │
     ├─ settings for "chat"? ── no ──> Err  capability 'chat' is not configured
     │
     ├─ construct("agent-model", "chat", settings)
     │        └─ Err  the settings for capability 'chat' do not parse: <detail>
     │
     └─ resolve its model through ModelSource
           │
           ├─ get("granite") ── no settings ──> Err  model 'granite' is not configured
           │
           ├─ meets the capability's ModelRequirement? ── no ──> Err  <what is unmet>
           │
           └─ provider_for("granite")
                 └─ ProviderSource::get("ollama")
                       └─ no settings ──> Err  model 'granite' names provider
                                               'ollama', which is not configured
     │
     ├─ Err ──> returned to the caller, nothing cached, nothing handed out
     └─ Ok  ──> cached, and the same handle from here on
```

`ModelSource` answers which provider a model names, and assembles a
`ConfiguredModel` out of the model, that provider and the variant the user
pinned, so a model no longer carries a copy of its provider's settings.
`CapabilitySource` constructs a capability and then hands it a model lookup,
which consumes it and returns the form that holds its model, comparing that
model with the `ModelRequirement` the capability's registry entry declares.
Binding is declared on that second form alone, so a capability without its
model has no `bind` to call. Both steps run while the source is still the only
owner of the capability, and the source caches and hands out a handle only
when both have succeeded. A launcher holds no capabilities: the ids in
`enabled_capabilities` are read from configuration by a launch.

The error a source returns carries the kind, the id and what went wrong, which
is what lets the remediation prompt offer reconfigure, remove or un-enable
without matching on rendered text.

### Resolving before acting

Asking whether a configuration is valid is asking whether it resolves. The
walk keeps its shape from spec 0024, following a launcher to its capabilities,
their models and those models' providers, and each hop is now a source lookup,
so it reports a settings blob that does not parse, a required setting left
empty, and a model that does not meet its capability's requirement, alongside a
name that resolves to nothing.
Because the set memoises, checking and then acting build each instance once,
so the check costs nothing the command was not going to spend and the two
cannot disagree about a configuration that has not changed between them.
Asking who points at an instance stays a configuration read, since a removal
needs it before anything is built.

A command resolves the whole closure of what it will use before it does
anything with a side effect. A launch is the longest such sequence:

```
  launch <id>                                     what has happened so far
  ───────────                                     ────────────────────────
  resolve the launcher's closure                  nothing
     │   launcher, its capabilities, their
     │   models, those models' providers
     │
     ✗ ──> prompt: reconfigure, remove, un-enable
     │        └─ a write ends the snapshot, and the retry
     │           resolves against the new one
     ▼
  start the session proxy, when one is needed     a local port, the set rebuilt
     │
  construct the launcher                          nothing outside the process
     │
  resolve every enabled capability                nothing bound
     │
  bind each resolved capability                   the launcher has changed
     │   on_setup, then bind_capability
     │
  on_pre_launch, then launch                      the agent owns the terminal
```

The proxy starts after the check because a configuration problem should not
cost a port, and the resolve pass that follows it repeats work the check
already did against the same configuration, so it can fail only if something
outside configuration changed under it.

Where a caller asks for every instance of a kind, which is what the list
commands and the setup wizard's selection lists do, a broken entry is reported
against that entry and left out of the list, and the rest are returned. Where a
caller names one instance, the error is returned to it. A name that does not
resolve costs the user the thing that names it and nothing else, which is
principle 6.

---

## Sub-Tasks

Each sub-task is one commit that leaves `main` working, with its tests. They
are ordered so that how instances are held changes before who resolves
references, and both before the constructor contract narrows. The last one
tidies a field the earlier ones leave wider than it needs to be.

---

### Sub-Task 1 — Sources hold, share and build on demand

**Intent**
Give a configured instance one owner that can share it, and build it when it
is asked for.

**Expected Outcomes**

Each source records the configuration it was built from and keeps a cache of
what it has built, so an id is constructed on its first ask and every later
ask returns the same handle. It records the whole configuration while
`construct` still takes one, and Sub-Task 10 narrows the field to its own
kind's map. `Configured::instances()` returns owned handles
and builds every configured entry, filling the same cache so a later ask
reuses what is there. The cache
finishes through the entry API, so two callers racing on one id cannot end up
with two objects for it. `ModelSource::take` becomes `get`, which no longer
empties the slot it reads, and returns an error naming the id when the
configuration has no such entry or its type is not registered.

The per-entry warning moves from build time to ask time, which is what stops a
command reporting problems in parts of the configuration nobody asked about.
`CapabilitySource` keeps spec 0024's reference gate for now, moved to the same
place, and `instances()` filters on it, which keeps a dangling capability out
of the setup wizard's selection lists exactly as before.

Holding and building on demand land together because splitting them leaves a
commit that regresses: handing out a shared model means the session proxy has
to wrap it once per id, and without a cache the only way to do that is to wrap
and route every configured model when the source is built, registering routes
for models no launch asked for.

Tests cover: two asks for one id returning pointer-equal handles, replacing
the test that asserts a second `take` returns `None`; a source over two
configured models building only the one that was asked for; an id absent from
the configuration and an id whose type is unknown each returning an error
naming it; and `instances()` followed by `get` returning the same object.

**Relevant Context**
- `src/dependency/mod.rs` (`Configured`, `resolve`)
- `src/models/mod.rs`, `src/providers/mod.rs`, `src/capabilities/mod.rs`,
  `src/launchers/mod.rs` (the four eager loops)
- `src/models/base.rs:256` (`ModelSource::from_config` rebuilt per resolve)

**Status** — `[x]` done

---

### Sub-Task 2 — A model reaches its provider through the provider source

**Intent**
Let a model reach its provider through the source that owns providers, so it
stops carrying a copy of that provider's settings.

**Expected Outcomes**

`ModelSource` holds a handle to a `ProviderSource` built from the same
configuration and answers the model-to-provider question itself, reading
`ModelConfig.provider_id` and distinguishing a model that is not configured
from one whose provider is gone. Three things go away with it: the
`provider_config` blob injected under a string key that `src/models/mod.rs`
and `build.rs` both have to know, the generated field and deserialisation that
receive it, and `Model::provider_config()` and `Model::provider()`, the second
of which builds a fresh provider from a default configuration on every call.

`ConfiguredModel` holds that provider alongside the model, so
`resolve_provider_endpoint` reads a field where it calls
`self.model.provider()` today. Its `resolve` keeps the panic it has until
Sub-Task 3 moves the assembly into the source's lookup.

The session proxy moves down a layer with it. `ProviderSource` holds the
handle and returns providers whose connection details point at the local
proxy, which is all `ProxiedModel` ever did, so that wrapper is deleted and
`provider_for` does not know whether a proxy is running. A route's upstream
target has to be read from behind that swap, so the source keeps two views of
one provider, `get` handing out the proxy-pointed one and `upstream` the one
carrying the real connection details, both cached per id. One source per kind
stays one source per kind, and no launch holds a second copy of every
provider.

Route registration does not move down with the proxy: it needs the model, its
variant, the real provider's details and the handle at once, so it moves to
the launch path, where Sub-Task 9 puts it after the capabilities resolve.

Tests cover: `provider_for` returning the configured provider for a healthy
model; the two failure messages, checked separately, so a catalog id that was
never configured reads differently from a configured model whose provider is
gone; and, with a proxy handle active, `provider_for` returning details
pointed at the local proxy for an id whose upstream view returns the real
ones, while `health_check` and `pull_model` still reach the real upstream.

**Relevant Context**
- `src/models/mod.rs` (the sidecar injection, route registration)
- `src/models/base.rs:161-183` (`provider_config`, `provider`)
- `build.rs:60-75` (the generated field and its deserialisation)
- `src/proxy/model_wrapper.rs` (`ProxiedModel`, `ProxiedProvider`)

**Status** — `[x]` done

---

### Sub-Task 3 — A capability's model is resolved by the capability source

**Intent**
Separate building a capability from wiring it to the model it names, and put
both inside the source, so the construction path reports a missing or
unsuitable model itself.

**Expected Outcomes**

The models layer publishes the narrow lookup a capability needs, which
`ModelSource` implements and which assembles a `ConfiguredModel` from the
model, its provider and the configured variant. What a capability reports
about itself splits from what it does: `Capability` declares taking that
lookup and returning the resolved form, and `ResolvedCapability` declares
`bind` and the launch hooks. The six model-backed capabilities gain a resolved
companion holding the `ConfiguredModel` outright, built only by that step, so
binding one that never resolved does not compile.

`CapabilitySource` constructs and then resolves, in that order, while it is
still the only owner of the capability, and caches and returns a handle only
when both succeeded, so no caller of the source holds a capability without its
model. `run_launch` builds its capabilities through the registry and calls the
method itself, which leaves that state reachable in one place until Sub-Task 9
has the launch ask the source. Resolution compares the model with the `ModelRequirement` the
capability's registry entry declares, using the same `admits_instance` call
the setup picker makes when it filters candidates, and names what is unmet
when it fails. `ConfiguredModel::resolve_provider_endpoint` loses its
`required_function` parameter, which asked the question the requirement now
answers, and keeps `endpoint_function`, which selects the endpoint to look up.

The panic in `ConfiguredModel::resolve` is deleted, which closes #90 in the
code as well as in the behaviour spec 0024 delivered by keeping both
production paths away from it. `Validatable::refs` stops being private to
`config::validation`, so an instance's outbound names have one declaration:
the walk and the remove-time scan read it, and so does the launch's route
registration, which reads the same `config_key` out of a capability's settings
itself today.

Tests cover: a capability configured against a removed model producing an
error through both `get` and `instances()`, with the healthy capability
alongside it still returned; an error that names both the capability and the
missing model; a capability whose model exists but whose provider is gone,
naming the provider; a model that does not meet its capability's requirement
failing resolution with the unmet part named, where the same model resolves
for a capability that does not require it; and, per capability type, that
every id `refs()` reports is one the resolve step consumes.

**Relevant Context**
- `src/models/base.rs:240-266` (`ConfiguredModel::resolve`, the panic)
- `src/capabilities/base.rs` (`Capability`, `Dependency`)
- `src/capabilities/agent_model.rs`, `vision_mcp/mod.rs`, `sub_agent.rs`
  (the two plain implementations and the two macros)
- `src/capabilities/requirement.rs` (`Requirement<dyn Model>`)

**Status** — `[x]` done

---

### Sub-Task 4 — One home for the test doubles

**Intent**
Collect the factory-constructed test doubles into one place, so the two
changes to the constructor contract that follow land once each.

**Expected Outcomes**

A `#[cfg(test)]` support module holds the doubles that exist more than once:
the seven identical `FakeProvider`s, and the six model doubles that differ
only in an instance id, a `ModelType`, a repository string and whether they
carry variants. The shared model double takes the functions it supports as an
argument and its variants through a second call, so each test still states
what it needs. `CaptureUi`, `FakeLauncher` and the two registry `TestImpl`s
stay where they are, each already the only one of its kind, as do the doubles
that implement no factory trait, which the next two sub-tasks do not reach.
Nothing about production code changes, and the tests that move keep asserting
what they assert now.

Sub-Tasks 5 and 6 then edit two implementations instead of thirteen spread
across seven files.

Tests cover: nothing new. This sub-task is the existing tests, passing
unchanged against the shared doubles, which is what tells us the move was
faithful.

**Relevant Context**
- `src/capabilities/agent_model.rs`, `vision_mcp/mod.rs`, `sub_agent*.rs`
  (`FakeProvider`, the `TestModel` shapes)
- `src/models/base.rs`, `src/launchers/base.rs`, `src/utils/ui/base.rs`,
  `src/registry/mod.rs` (the remaining doubles)

**Status** — `[x]` done

---

### Sub-Task 5 — Construction takes only the instance's own settings

**Intent**
Remove the application configuration from the constructor now that nothing
reads it.

**Expected Outcomes**

`ConfigConstructable::new` and both generated factory methods take an instance
id and a settings blob. Every implementation loses the parameter, including
the generated model constructor in `build.rs` and the four `Ui` backends,
whose factory takes a configuration it has never had anything to put in. The
`Config::default()` placeholder in `construct_ui` goes with it, the other
having left alongside `Model::provider()` in Sub-Task 2.

`ClaudeLauncher` is the one production consumer that is not a capability. Its
model proxy handle moves to `LaunchContext`, which the launch already builds
after starting the proxy server, and `Config.model_proxy` goes with it: the
provider source receives the handle when the launch builds the set, and no
constructor reads it from configuration.

Tests cover: the existing instance id round-trip tests across the seven
launchers, updated for the new arity, still reporting the configured id; and
the Claude overlay built from a `LaunchContext` carrying a handle matching
what it produces from the field today.

**Relevant Context**
- `src/registry/mod.rs` (the trait and the macro)
- `build.rs` (the generated implementation)
- `src/main.rs` (`construct_ui`, the launch path)
- `src/launchers/claude.rs`, `src/launchers/base.rs` (`LaunchContext`)

**Status** — `[x]` done

---

### Sub-Task 6 — Construction returns a result

**Intent**
Let a constructor report settings it cannot read, so a malformed instance is
named instead of silently becoming a default.

**Expected Outcomes**

`ConfigConstructable::new` returns a result, and the factory's `construct`
returns an error that says either that the type is not registered or that the
settings could not be read, so a caller can tell the two apart without reading
the message. The seventeen constructors that read their settings with
`serde_json::from_value(cfg.clone()).unwrap_or_default()` report the parse
error, and the seven `.expect("Failed to create HTTP client")` calls across the
five providers become errors, which removes the last panics on the
construction path.

The sources turn that error into the same kind of value they already return
for a name that does not resolve, so a malformed instance reaches the
remediation prompt by the route a dangling one does. Nothing else changes
about when construction runs, and a required id left empty still reads as a
missing dependency, because the reference gate inside the capability source
runs before construction does. Sub-Task 8 moves that verdict.

The `#[allow(unused)]` annotations in `define_factory!` come off the methods
that now have production callers, which is `construct`, `config_schema`,
`default_config`, `get` and `entries`, and any that stay keep a line saying
what is still unused and why. The macro's TODO goes with them.

Tests cover: a provider whose settings have a field of the wrong type
returning an error that names the instance and carries what the deserialiser
said, where the same settings produce a default provider today; a capability with unreadable settings
reported through `get` and omitted by `instances()` with the healthy ones
returned; a settings blob with an unknown key still constructing, so a
configuration written by an older version keeps working; and an unknown type
name still reported as an unknown type.

**Relevant Context**
- `src/registry/mod.rs` (the trait, the macro's `construct`)
- `src/providers/*.rs`, `src/launchers/*.rs`, `src/capabilities/*.rs`,
  `src/models/custom.rs`, `src/utils/ui/backends/*.rs` (the implementations)
- the `reqwest::Client::builder` calls in the five provider constructors

**Status** — `[x]` done

---

### Sub-Task 7 — One live set of sources on the application context

**Intent**
Hold one set of sources per configuration snapshot, so every caller in a run
gets the same instance for a given id, and a write ends the snapshot.

**Expected Outcomes**

`AppContext` owns the four sources, builds them on the first ask and hands out
handles. Its `config` field becomes private: reads go through an accessor and
writes through a second one that discards the set before returning the mutable
reference, so a command cannot mutate configuration without invalidating what
was built from it. Every call site that builds a source from `ctx.config`
today asks the context instead.

The session proxy handle lives on the context for the same reason, and setting
it discards the set, so the providers handed out after a launch starts its
proxy all point at the proxy and the ones that point at the real upstream are
unreachable.

Tests cover: two asks returning the same set and the same instance for one id;
a write between two asks producing a different instance built from the new
configuration; a handle taken before a write continuing to work against the
snapshot it came from; and setting a proxy handle producing providers pointed
at the proxy where the previous set pointed at the upstream.

**Relevant Context**
- `src/main.rs` (`AppContext`, `construct_context`)
- `src/commands/model.rs`, `capability.rs`, `launcher.rs`, `setup.rs`,
  `src/utils/ui/app.rs` (the call sites that build a source per command)
- `src/config/mod.rs` (`insert_*`, `remove_*`, `update_*`, `model_proxy`)

**Status** — `[ ]` not started

---

### Sub-Task 8 — Resolution is the check

**Intent**
Make one function answer whether a configured instance is usable, so a list, a
prompt and a launch agree, and so the answer covers everything a build can
find.

**Expected Outcomes**

The forward walk resolves each hop through the source set instead of reading
configuration and registry metadata, so it reports settings it cannot read and
a model that does not meet its capability's requirement next to a name that
resolves to nothing. It keeps its shape, its referrer bookkeeping and all
of its command-layer callers: the list annotations, the remediation prompt and
the launch's prelaunch check. Asking who points at an instance stays a
configuration read, because a removal needs the answer before anything is
built.

The problem vocabulary the prompt reads gains the two cases resolution adds,
settings that cannot be read and a model that does not meet its requirement,
and loses `MissingDependency`. Construction takes over that verdict by running
the validation its settings type declares, which the six capability config
structs already derive with `serde_valid` and nothing calls today, so
`#[validate(min_length = 1)]` on a model id starts deciding whether that id is
usable. `Validatable::refs` reports only the ids it finds, leaving whether a
required setting is present to the type that declares it. The two land in one
commit because they are one verdict changing hands.

The module documentation stops saying the walk constructs nothing, and says
which two questions it reads and which two it builds to answer.

The prompt gains a second repair for settings it cannot read: replace the
fields that cannot be read with the type's defaults, and keep everything else
the instance was configured with. The settings blob is valid JSON and it is
reading it as the type's config that failed, usually over one field, so each
field that differs from its default is tried on its own before any are
replaced together. The offer names the fields and the values they would take,
so nobody accepts a repair without knowing what it moves, and it appears only
when such a replacement builds: it does for a provider with a mistyped
timeout, and does not for an `agent-model` capability whose required model id
is empty, whose default is that same empty id.

```
⚠ Configuration issue: provider 'ollama' has settings that cannot be read:
  invalid type: string "ten", expected u64
What would you like to do?
  Reconfigure provider 'ollama' now
  Reset provider 'ollama' timeout_secs setting to its default value of 10
  Remove provider 'ollama'
> Cancel
```

A capability whose model does not meet its requirement shows up in a list the
way a dangling reference does:

```
Configured Capabilities (2 capabilities)
ID       TYPE          NOTES
chat     agent-model
vision   vision-mcp    ⚠ capability 'vision' names model 'granite-4.0-micro',
                         which does not meet its requirement: Image Understanding
```

Tests cover: a capability whose model lacks a required function failing the
check with the model and the function named, where the same model passes for a
capability type that does not require it; a capability whose model is not
configured still reported as not configured; a custom model judged by its own
settings, passing when they list the required function and failing when they
do not; the same mismatch reached through a launcher reported with the
launcher as referrer; `capability list` and `launcher list` annotating a
mismatch without prompting; a capability whose required model id is empty
reported by construction, with the reset repair absent from its prompt and
present for a provider whose settings have a field of the wrong type; the
offer naming that field and the value it takes; and accepting it replacing
that field while leaving a deliberately configured endpoint alone.

**Relevant Context**
- `src/config/validation.rs` (the walk, `Problem`, `Validatable::refs`)
- `src/commands/shared/remediation.rs` (`Fix::for_error`, `Choice`, `choose`)
- the `serde_valid::Validate` derives on the capability config structs
- `src/commands/capability.rs`, `src/commands/launcher.rs` (the list commands)
- `src/models/custom.rs` (the placeholder registry entry)

**Status** — `[ ]` not started

---

### Sub-Task 9 — A launch resolves everything before it binds anything

**Intent**
Reach the first `bind_capability` only once every capability the launcher
enables has resolved, so a failure part way through the list cannot leave
earlier ones bound.

**Expected Outcomes**

The launch runs the check, starts the proxy when it needs one, constructs the
launcher, then resolves every enabled capability through the capability source
before running `on_setup` and `bind_capability` over the resolved ones in the
order the launcher lists them. The first failure in the resolve pass aborts
with that capability named and nothing bound. Building capabilities through
the capability source gives the launch the resolution path the check used, and
registering one proxy route per resolved model happens here, where the handle,
the models and their real upstream details are all in hand.

Tests cover: a launcher enabling two capabilities where the second fails to
resolve, aborting with the second named and the launcher recording no bind;
a launcher enabling two healthy capabilities binding both in the order it
lists them; and one route registered per resolved model when a proxy is
running.

**Relevant Context**
- `src/main.rs` (`run_launch`: the capability loop, the proxy start)
- `src/commands/launcher.rs` (`prelaunch`)
- `src/launchers/base.rs` (`bind_capability`), `src/capabilities/base.rs`
  (`on_setup` and the other hooks)

**Status** — `[ ]` not started

---

### Sub-Task 10 — Each source holds its own kind's settings

**Intent**
Leave each source holding the settings it reads, now that nothing it calls
needs the rest.

**Expected Outcomes**

Each source's `config` field becomes the map for its own kind, which is all
any of them has read since Sub-Task 5 took the configuration out of
`construct`. Four clones of the whole configuration per snapshot become four
clones of one map each, and the field says what it holds.

Tests cover: nothing new. The sources' own tests pass unchanged, which is
what says the field was only ever read for its own kind.

**Relevant Context**
- `src/providers/mod.rs`, `src/models/mod.rs`, `src/capabilities/mod.rs`,
  `src/launchers/mod.rs` (the four fields)
- `src/sources.rs` (`Sources::build`, which hands each source its map)

**Status** — `[ ]` not started
