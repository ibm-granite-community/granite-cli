# Data Model

granite-cli reads two sets of data. The configuration holds the providers,
models, capabilities and launchers a user has set up, as one YAML file per
instance. The compiled-in data ships in the binary: the model catalog
generated from `resources/models.yaml`, and the metadata each registered
implementation type declares. A configured instance names other configured
instances by id, and names its own implementation type by registry key.

Update this when a configuration struct gains a field that names something, or
when a registry gains or loses a type.

```
CONFIGURATION                                   COMPILED INTO THE BINARY
one YAML file per instance                      registries and their metadata

┌────────────────────────┐
│ LauncherConfig         │
│   launcher_id          │                      ┌──────────────────────────────┐
│   launcher_type      ──┼── type ─────────────▶│ LAUNCHER_REGISTRY            │
│   enabled_capabilities │                      │   supported_capabilities     │
│   config               │                      └──────────────────────────────┘
└───────────┬────────────┘                                      ┊
            │ id, per entry                                     ┊ binding types
            ▼                                                   ┊
┌────────────────────────┐                                      ┊
│ CapabilityConfig       │                                      ┊
│   capability_id        │                      ┌──────────────────────────────┐
│   capability_type    ──┼── type ─────────────▶│ CAPABILITY_REGISTRY          │
│   config               │                      │   supported_binding_types    │
│     [config_key] ◀─────┼─── names key ────────┤   dependencies[].config_key  │
└───────────┬────────────┘                      │   dependencies[].requirement │
            │ id, under config_key              └──────────────────────────────┘
            ▼                                                   ┊ requirement
┌────────────────────────┐                                      ┊
│ ModelConfig            │                                      ┊
│   model_id             │                      ┌──────────────────────────────┐
│   model_type         ──┼── type ─────────────▶│ MODEL_REGISTRY               │
│   provider_id          │                      │   built from models.yaml     │
│   variant            ──┼── format/precision ─▶│   variants[]                 │
│   config               │                      │   family, model_type, size,  │
└───────────┬────────────┘                      │   context_length, tags,      │
            │ id                                │   supported_functions        │
            ▼                                   └──────────────────────────────┘
┌────────────────────────┐                                      ┊ variant format
│ ProviderConfig         │                                      ┊
│   provider_id          │                      ┌──────────────────────────────┐
│   provider_type      ──┼── type ─────────────▶│ PROVIDER_REGISTRY            │
│   config               │                      │   supported_formats          │
└────────────────────────┘                      │   supported_api_types        │
                                                └──────────────────────────────┘
```

The diagram shows three kinds of reference.

- **Id references**, the vertical arrows on the left, point from one configured
  instance to another. The user writes both ends, so a `remove` command or a
  hand edit can break one.
- **Type references**, the horizontal arrows, point from an instance's `*_type`
  field to a key in that kind's registry. A new build breaks one when it drops
  or renames a registered type, for example a model id removed from
  `resources/models.yaml`.
- **Compatibility references**, the dotted lines on the right, compare
  compiled-in values at the two ends of an id reference: what a capability type
  requires of a model against the model it names, a capability's binding types
  against the launcher that enables it, and a model variant's format against the
  provider serving it. The id reference can resolve while the two ends do not
  fit, and a new build can change either end.

A capability's model id is stored inside its `config` blob, under the key that
its type's `CapabilityMetadata.dependencies[].config_key` names. Reading that id
needs the capability's type reference to resolve first.

## Provider model discovery

`ProviderConfig.config` may carry `discover_models: bool` for provider types
whose listing API granite-cli understands (Ollama, LM Studio, llama.cpp). It is
a setting, not a reference. When it is on, a successful listing from that
provider is authoritative for the model entries discovery created from it.

Those entries add a field the diagram does not show:

```yaml
# models/<id>.yaml
model_id: llama3:8b
model_type: custom
provider_id: ollama
variant: ~
discovered:
  provider_id: ollama
  provider_model_id: llama3:8b
config: { ... }        # CustomModelConfig fields
```

`discovered` is provenance, not a second reference: `ModelConfig.provider_id`
remains the required id reference. Its presence is what lets sync remove the
entry when the provider stops reporting it, and what makes `model remove`
refuse while that provider's discovery is on. Catalog entries and hand-made
custom models have no `discovered` field and are never touched by sync.

In YAML, `provider_type`, `capability_type` and `launcher_type` are written as
`type`. `model_type` keeps its name.
