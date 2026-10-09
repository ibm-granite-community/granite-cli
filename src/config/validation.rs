//! Answers whether a configured instance can be used, by resolving it: the
//! names it holds, the settings it carries, and what its type requires of
//! what it names.
//!
//! Three things can be wrong. An id names another configured instance that
//! is not there, a `*_type` name is not a key in that kind's registry, and an
//! instance's own settings cannot be read as its type's config. A capability
//! has a fourth: the model it names is configured and does not meet what the
//! capability type requires of it.
//!
//! The first two are read from the configuration and the registries. The
//! other two are answered by building the instance through the source that
//! owns its kind, which is the same call a command makes to use it, so this
//! walk and the command cannot disagree. What is built here stays in the
//! source's cache, and the command gets that same instance. A launch is the
//! exception: starting its session proxy discards the set the check built,
//! and the launch builds what it uses again with providers pointed at the
//! proxy. Asking who points at an instance stays a plain read, since a
//! removal needs the answer before anything is built.
//!
//! One walk covers every kind. Each of the four config types implements
//! [`Validatable`] to say what its type name is and which ids it points at,
//! and [`validate`] does the rest.

use std::collections::HashMap;

// TODO: This is a circular dependency that needs to be untangled
use crate::capabilities::Dependency;
use crate::config::{
    CapabilityConfig, Config, ConfigId, LauncherConfig, ModelConfig, ProviderConfig,
};
use crate::sources::Sources;

/*-- public --------------------------------------------------------------------*/

/// The four kinds of configured instance that reference each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RefKind {
    Launcher,
    Capability,
    Model,
    Provider,
}

/// What went wrong with a reference. Callers branch on this to decide what to
/// offer the user, rather than matching on a rendered message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Problem {
    /// The named instance is not in the configuration.
    NotConfigured,
    /// The instance's `*_type` is not a key in its kind's registry.
    UnknownType { type_name: String },
    /// The instance's own settings cannot be read as its type's config.
    UnreadableSettings { detail: String },
    /// A capability's model is configured, and does not meet what the
    /// capability type requires of it.
    UnmetRequirement { model_id: String, unmet: String },
    /// The instance builds, and a name it holds does not resolve although
    /// the walk found every name it reads valid. This is the case where a
    /// type reads an id from a different setting than its dependency
    /// metadata declares.
    Unresolved { detail: String },
}

impl From<crate::registry::ConstructError> for Problem {
    fn from(error: crate::registry::ConstructError) -> Self {
        match error {
            crate::registry::ConstructError::UnknownType { type_name } => {
                Self::UnknownType { type_name }
            }
            crate::registry::ConstructError::Settings { detail } => {
                Self::UnreadableSettings { detail }
            }
        }
    }
}

impl From<crate::sources::SourceError> for Problem {
    fn from(error: crate::sources::SourceError) -> Self {
        match error {
            crate::sources::SourceError::NotConfigured => Self::NotConfigured,
            crate::sources::SourceError::Construct(error) => error.into(),
            crate::sources::SourceError::UnmetRequirement { model_id, unmet } => {
                Self::UnmetRequirement { model_id, unmet }
            }
            crate::sources::SourceError::Unresolved(detail) => Self::Unresolved { detail },
        }
    }
}

/// A reference that does not resolve.
///
/// `referrer` is the configured instance that holds the broken reference, and
/// is what a caller offering a fix acts on: `launch claude` finding that
/// `chat`'s model is gone reconfigures `chat`, not the missing model. It is
/// absent when the instance asked about is itself the problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidationError {
    pub(crate) target: (RefKind, String),
    pub(crate) problem: Problem,
    pub(crate) referrer: Option<(RefKind, String)>,
}

/// One instance with a broken reference, as returned by [`find_dangling`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DanglingRef {
    pub(crate) kind: RefKind,
    pub(crate) instance_id: String,
    /// The validation error, rendered.
    pub(crate) reason: String,
}

/// Validates that `id`'s references resolve, one hop at a time, recursing
/// through whatever it finds. A launcher walks its enabled capabilities,
/// their models, and those models' providers, so a missing provider four
/// levels down is reported rather than the walk stopping at the first level.
///
/// The walk covers only what it was asked about. Nothing here reads a part of
/// the configuration the caller did not name.
pub(crate) fn validate_ref(
    kind: RefKind,
    id: &str,
    config: &Config,
    sources: &Sources,
) -> Result<(), ValidationError> {
    validate(kind, id, config, sources, None)
}

/// Validates every configured instance of one kind, returning those that
/// fail. This is what a list command needs, whose subject genuinely is every
/// instance of its kind.
///
/// # Examples
///
/// ```ignore
/// // The status column of `model list`. One scan covers the whole table, and
/// // reports only models even when what is actually missing is a provider.
/// let broken = find_dangling(RefKind::Model, &config, &Sources::build(&config, None));
/// for row in &mut rows {
///     if let Some(d) = broken.iter().find(|d| d.instance_id == row.id) {
///         row.notes = format!("{} {}", ui.warn_mark(), d.reason);
///     }
/// }
/// ```
pub(crate) fn find_dangling(kind: RefKind, config: &Config, sources: &Sources) -> Vec<DanglingRef> {
    config_entries(config, kind)
        .into_iter()
        .filter_map(|entry| {
            let id = entry.config_id();
            validate_ref(kind, id, config, sources)
                .err()
                .map(|e| DanglingRef {
                    kind,
                    instance_id: id.to_string(),
                    reason: e.to_string(),
                })
        })
        .collect()
}

/// The configured instances that point at `(kind, id)`, which is what
/// removing it would strand. Sorted, so a caller listing them is stable.
///
/// This is [`Validatable::refs`] read backwards: an instance depends on the
/// target when the target appears among the references it declares.
///
/// # Examples
///
/// ```ignore
/// // Before `model remove granite-3.1-8b-instruct` deletes anything.
/// let stranded = dependents(RefKind::Model, "granite-3.1-8b-instruct", &config);
/// // -> [(RefKind::Capability, "chat")]
/// ```
pub(crate) fn dependents(kind: RefKind, id: &str, config: &Config) -> Vec<(RefKind, String)> {
    let mut found: Vec<(RefKind, String)> = [
        RefKind::Launcher,
        RefKind::Capability,
        RefKind::Model,
        RefKind::Provider,
    ]
    .into_iter()
    .flat_map(|referrer_kind| {
        config_entries(config, referrer_kind)
            .into_iter()
            .map(move |entry| (referrer_kind, entry))
    })
    .filter(|(_, entry)| {
        entry
            .refs()
            .iter()
            .any(|(target_kind, target_id)| *target_kind == kind && *target_id == id)
    })
    .map(|(referrer_kind, entry)| (referrer_kind, entry.config_id().to_string()))
    .collect();

    found.sort_by(|a, b| a.0.to_string().cmp(&b.0.to_string()).then(a.1.cmp(&b.1)));
    found
}

/// The `*_type` of a configured instance: the registry key that its setup
/// command needs to reconfigure it. `None` when `id` names nothing.
pub(crate) fn type_name<'a>(kind: RefKind, id: &str, config: &'a Config) -> Option<&'a str> {
    config_entry(config, kind, id).map(Validatable::type_name)
}

impl std::fmt::Display for RefKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            RefKind::Launcher => "launcher",
            RefKind::Capability => "capability",
            RefKind::Model => "model",
            RefKind::Provider => "provider",
        };
        f.write_str(s)
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, id) = &self.target;
        match &self.referrer {
            Some((referrer_kind, referrer_id)) => write!(
                f,
                "{referrer_kind} '{referrer_id}' depends on {kind} '{id}', which "
            )?,
            None => write!(f, "{kind} '{id}' ")?,
        }
        match &self.problem {
            Problem::NotConfigured => write!(f, "is not configured"),
            Problem::UnknownType { type_name } => {
                write!(f, "has an unknown {kind} type '{type_name}'")
            }
            Problem::UnreadableSettings { detail } => {
                write!(f, "has settings that are not valid: {detail}")
            }
            Problem::UnmetRequirement { model_id, unmet } => {
                write!(
                    f,
                    "names model '{model_id}', which does not meet its requirement: {unmet}"
                )
            }
            Problem::Unresolved { detail } => write!(f, "does not resolve: {detail}"),
        }
    }
}

impl std::error::Error for ValidationError {}

/*-- private -------------------------------------------------------------------*/

/// What the walk needs from a configured instance: the name of its
/// implementation type, whether that name is registered, and the ids it
/// points at. Everything else about validating an instance is the same for
/// every kind and lives in [`validate`].
pub(crate) trait Validatable: ConfigId {
    /// The `*_type` field: the registry key this instance was configured
    /// from.
    fn type_name(&self) -> &str;

    /// Whether [`Self::type_name`] is a key in this kind's registry.
    fn type_is_registered(&self) -> bool;

    /// The instances this one points at, for the walk to follow.
    ///
    /// An id it does not hold is not reported here: whether a setting has to
    /// be there is what building the instance answers, so an entry with
    /// nothing under a required key names nothing and construction reports
    /// it.
    fn refs(&self) -> Vec<(RefKind, &str)>;
}

/// Reaching the four maps by [`RefKind`] rather than by name. These live with
/// the walk because [`Validatable`] and [`RefKind`] are the only reason to
/// want them. The arms spell the four fields out because the maps have four
/// different value types and `kind` is only known at run time.
fn config_entry<'a>(config: &'a Config, kind: RefKind, id: &str) -> Option<&'a dyn Validatable> {
    match kind {
        RefKind::Launcher => lookup(&config.launchers, id),
        RefKind::Capability => lookup(&config.capabilities, id),
        RefKind::Model => lookup(&config.models, id),
        RefKind::Provider => lookup(&config.providers, id),
    }
}

fn config_entries(config: &Config, kind: RefKind) -> Vec<&dyn Validatable> {
    match kind {
        RefKind::Launcher => erase(&config.launchers),
        RefKind::Capability => erase(&config.capabilities),
        RefKind::Model => erase(&config.models),
        RefKind::Provider => erase(&config.providers),
    }
}

/// One entry of a single kind's map, as the walk sees it.
fn lookup<'a, T: Validatable>(
    map: &'a HashMap<String, T>,
    id: &str,
) -> Option<&'a dyn Validatable> {
    map.get(id).map(|entry| entry as &dyn Validatable)
}

/// Every entry of a single kind's map, as the walk sees it.
fn erase<T: Validatable>(map: &HashMap<String, T>) -> Vec<&dyn Validatable> {
    map.values()
        .map(|entry| entry as &dyn Validatable)
        .collect()
}

/// The recursive body of [`validate_ref`], and the whole of what validating
/// one instance means: it is configured, its type name resolves, it builds
/// from its own settings, and every id it points at validates in turn.
///
/// `referrer` is the instance whose reference brought the walk here, and
/// rides along so that a failure names the instance a caller would act on
/// rather than only the missing thing.
fn validate(
    kind: RefKind,
    id: &str,
    config: &Config,
    sources: &Sources,
    referrer: Option<(RefKind, &str)>,
) -> Result<(), ValidationError> {
    let entry = config_entry(config, kind, id)
        .ok_or_else(|| err(kind, id, Problem::NotConfigured, referrer))?;

    if !entry.type_is_registered() {
        return Err(err(
            kind,
            id,
            Problem::UnknownType {
                type_name: entry.type_name().to_string(),
            },
            referrer,
        ));
    }

    // What an instance points at is checked before the instance is built.
    // Building a capability resolves its model, and a model that is missing
    // or broken is reported at the model, with this instance as the
    // referrer the remediation prompt acts on.
    for (target_kind, target_id) in entry.refs() {
        validate(target_kind, target_id, config, sources, Some((kind, id)))?;
    }

    resolves(kind, id, sources).map_err(|problem| err(kind, id, problem, referrer))
}

/// What the source that owns `kind` says about the entry under `id`: whether
/// its settings are valid, and for a capability whether the model it names
/// meets what its type requires. What is built here stays in that source's
/// cache, so the command that goes on to use it does not build it again.
fn resolves(kind: RefKind, id: &str, sources: &Sources) -> Result<(), Problem> {
    match kind {
        RefKind::Launcher => sources.launchers().build(id).map(|_| ()),
        RefKind::Capability => sources.capabilities().build(id).map(|_| ()),
        RefKind::Model => sources.models().build(id).map(|_| ()),
        RefKind::Provider => sources.providers().build(id).map(|_| ()),
    }
    .map_err(Problem::from)
}

impl Validatable for LauncherConfig {
    fn type_name(&self) -> &str {
        &self.launcher_type
    }

    fn type_is_registered(&self) -> bool {
        crate::launchers::LAUNCHER_REGISTRY
            .get(&self.launcher_type)
            .is_some()
    }

    fn refs(&self) -> Vec<(RefKind, &str)> {
        self.enabled_capabilities
            .iter()
            .map(|id| (RefKind::Capability, id.as_str()))
            .collect()
    }
}

impl Validatable for CapabilityConfig {
    fn type_name(&self) -> &str {
        &self.capability_type
    }

    fn type_is_registered(&self) -> bool {
        crate::capabilities::CAPABILITY_REGISTRY
            .get(&self.capability_type)
            .is_some()
    }

    /// A capability stores its dependency ids inside its own config JSON, and
    /// only its type's static metadata says which keys hold them. A type the
    /// registry does not have names nothing, which the walk reports as an
    /// unknown type before it asks.
    fn refs(&self) -> Vec<(RefKind, &str)> {
        crate::capabilities::CAPABILITY_REGISTRY
            .get(&self.capability_type)
            .map(|metadata| dependency_refs(&self.config, &metadata.dependencies))
            .unwrap_or_default()
    }
}

impl Validatable for ModelConfig {
    fn type_name(&self) -> &str {
        &self.model_type
    }

    fn type_is_registered(&self) -> bool {
        crate::models::MODEL_REGISTRY
            .get(&self.model_type)
            .is_some()
    }

    /// `provider_id` is required, so a model always names a provider. Whether
    /// that name resolves is the walk's business, like any other reference.
    fn refs(&self) -> Vec<(RefKind, &str)> {
        vec![(RefKind::Provider, &self.provider_id)]
    }
}

impl Validatable for ProviderConfig {
    fn type_name(&self) -> &str {
        &self.provider_type
    }

    fn type_is_registered(&self) -> bool {
        crate::providers::PROVIDER_REGISTRY
            .get(&self.provider_type)
            .is_some()
    }

    /// A provider references no other configured instance.
    fn refs(&self) -> Vec<(RefKind, &str)> {
        Vec::new()
    }
}

/// The ids a capability's config holds under its declared dependencies'
/// `config_key`s.
///
/// A dependency contributes a reference whenever it holds an id, whether or
/// not it is declared required, so a dangling optional dependency is walked
/// like any other. A key holding nothing, or holding an empty string, names
/// nothing: whether the setting had to be there is what building the
/// capability answers, since its config type is what declares that.
fn dependency_refs<'a>(
    capability_config: &'a serde_json::Value,
    dependencies: &[Dependency],
) -> Vec<(RefKind, &'a str)> {
    let mut refs = Vec::new();

    for dependency in dependencies {
        let (kind, config_key) = match dependency {
            Dependency::Model { config_key, .. } => (RefKind::Model, config_key),
            Dependency::Provider { config_key, .. } => (RefKind::Provider, config_key),
            // An external tool is a shell command, not a configured instance.
            Dependency::ExternalTool { .. } => continue,
        };

        let id = capability_config
            .get(config_key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        if id.is_empty() {
            continue;
        }

        refs.push((kind, id));
    }

    refs
}

fn err(
    kind: RefKind,
    id: &str,
    problem: Problem,
    referrer: Option<(RefKind, &str)>,
) -> ValidationError {
    ValidationError {
        target: (kind, id.to_string()),
        problem,
        referrer: referrer.map(|(k, i)| (k, i.to_string())),
    }
}

/*-- tests ---------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{ModelRequirement, ShellCommandRequirement};

    fn provider(id: &str, provider_type: &str) -> ProviderConfig {
        ProviderConfig {
            provider_id: id.to_string(),
            provider_type: provider_type.to_string(),
            config: serde_json::json!({}),
        }
    }

    fn model(id: &str, model_type: &str, provider_id: Option<&str>) -> ModelConfig {
        ModelConfig {
            model_id: id.to_string(),
            model_type: model_type.to_string(),
            provider_id: provider_id.unwrap_or("ollama").to_string(),
            variant: None,
            // The walk builds the model now, and compares it with what the
            // capability naming it requires: `family` is what a custom
            // model's settings have to state, and the functions are what
            // `agent-model` asks of the model it names.
            config: serde_json::json!({
                "family": "Test",
                "supported_functions": ["Chat", "ToolCalling"],
            }),
        }
    }

    /// `validate_ref` over a configuration and the sources built from it,
    /// which is how every caller reaches it.
    fn check(kind: RefKind, id: &str, config: &Config) -> Result<(), ValidationError> {
        validate_ref(kind, id, config, &Sources::build(config, None))
    }

    fn capability(id: &str, capability_type: &str, model_id: &str) -> CapabilityConfig {
        CapabilityConfig {
            capability_id: id.to_string(),
            capability_type: capability_type.to_string(),
            config: serde_json::json!({ "model_id": model_id }),
        }
    }

    fn launcher(id: &str, launcher_type: &str, enabled: &[&str]) -> LauncherConfig {
        LauncherConfig {
            launcher_id: id.to_string(),
            launcher_type: launcher_type.to_string(),
            enabled_capabilities: enabled.iter().map(|s| s.to_string()).collect(),
            config: serde_json::json!({}),
        }
    }

    /// A configuration in which every reference resolves: launcher `claude`
    /// enables capability `chat`, which uses model `m1`, which uses provider
    /// `p1`.
    fn healthy() -> Config {
        let mut config = Config::default();
        config
            .providers
            .insert("p1".into(), provider("p1", "ollama"));
        config
            .models
            .insert("m1".into(), model("m1", "custom", Some("p1")));
        config
            .capabilities
            .insert("chat".into(), capability("chat", "agent-model", "m1"));
        config
            .launchers
            .insert("claude".into(), launcher("claude", "claude", &["chat"]));
        config
    }

    #[test]
    fn healthy_instance_of_each_kind_passes() {
        let config = healthy();
        for (kind, id) in [
            (RefKind::Provider, "p1"),
            (RefKind::Model, "m1"),
            (RefKind::Capability, "chat"),
            (RefKind::Launcher, "claude"),
        ] {
            assert!(
                check(kind, id, &config).is_ok(),
                "{kind} '{id}' should validate"
            );
        }
    }

    #[test]
    fn unconfigured_instance_of_each_kind_fails() {
        let config = healthy();
        for kind in [
            RefKind::Provider,
            RefKind::Model,
            RefKind::Capability,
            RefKind::Launcher,
        ] {
            let err = check(kind, "nope", &config).expect_err("should fail");
            assert_eq!(err.problem, Problem::NotConfigured);
            assert_eq!(err.target, (kind, "nope".to_string()));
            assert_eq!(err.referrer, None);
        }
    }

    #[test]
    fn dangling_instance_of_each_kind_fails_while_the_healthy_one_passes() {
        let mut config = healthy();
        config
            .models
            .insert("m-broken".into(), model("m-broken", "custom", Some("gone")));
        config.capabilities.insert(
            "cap-broken".into(),
            capability("cap-broken", "agent-model", "gone"),
        );
        config.launchers.insert(
            "launcher-broken".into(),
            launcher("launcher-broken", "claude", &["gone"]),
        );

        assert!(check(RefKind::Model, "m1", &config).is_ok());
        assert!(check(RefKind::Model, "m-broken", &config).is_err());
        assert!(check(RefKind::Capability, "chat", &config).is_ok());
        assert!(check(RefKind::Capability, "cap-broken", &config).is_err());
        assert!(check(RefKind::Launcher, "claude", &config).is_ok());
        assert!(check(RefKind::Launcher, "launcher-broken", &config).is_err());
    }

    #[test]
    fn walk_recurses_from_launcher_to_the_missing_provider() {
        let mut config = healthy();
        config.providers.remove("p1");

        let err = check(RefKind::Launcher, "claude", &config).expect_err("should fail");

        // The walk reached the provider rather than stopping at the launcher
        // or the capability, both of which are themselves configured.
        assert_eq!(err.target, (RefKind::Provider, "p1".to_string()));
        assert_eq!(err.problem, Problem::NotConfigured);
        // And it names the model as what to act on, not the launcher that
        // started the walk.
        assert_eq!(err.referrer, Some((RefKind::Model, "m1".to_string())));
    }

    #[test]
    fn error_names_the_capability_holding_a_missing_model() {
        let mut config = healthy();
        config.models.remove("m1");

        let err = check(RefKind::Launcher, "claude", &config).expect_err("should fail");

        assert_eq!(err.target, (RefKind::Model, "m1".to_string()));
        assert_eq!(
            err.referrer,
            Some((RefKind::Capability, "chat".to_string()))
        );
        assert_eq!(
            err.to_string(),
            "capability 'chat' depends on model 'm1', which is not configured"
        );
    }

    #[test]
    fn an_unknown_type_name_fails_for_each_kind() {
        let mut config = healthy();
        config
            .providers
            .insert("p-bad".into(), provider("p-bad", "not-a-provider"));
        config
            .models
            .insert("m-bad".into(), model("m-bad", "not-a-model", Some("p1")));
        config.capabilities.insert(
            "cap-bad".into(),
            capability("cap-bad", "not-a-capability", "m1"),
        );
        config.launchers.insert(
            "launcher-bad".into(),
            launcher("launcher-bad", "not-a-launcher", &[]),
        );

        for (kind, id, type_name) in [
            (RefKind::Provider, "p-bad", "not-a-provider"),
            (RefKind::Model, "m-bad", "not-a-model"),
            (RefKind::Capability, "cap-bad", "not-a-capability"),
            (RefKind::Launcher, "launcher-bad", "not-a-launcher"),
        ] {
            let err = check(kind, id, &config).expect_err("should fail");
            assert_eq!(
                err.problem,
                Problem::UnknownType {
                    type_name: type_name.to_string()
                },
                "{kind} '{id}'"
            );
        }
    }

    #[test]
    fn an_optional_dependency_contributes_a_ref_only_when_it_holds_an_id() {
        // No capability type declares an optional dependency today, so the
        // dependency list is supplied directly rather than through a type.
        let optional = |key: &str| {
            vec![Dependency::Model {
                config_key: key.to_string(),
                requirement: ModelRequirement::default(),
                resolved_id: None,
                required: false,
            }]
        };

        // Absent: nothing to check.
        assert_eq!(
            dependency_refs(&serde_json::json!({}), &optional("model_id")),
            vec![]
        );
        // Present but empty counts as absent.
        assert_eq!(
            dependency_refs(
                &serde_json::json!({ "model_id": "" }),
                &optional("model_id")
            ),
            vec![]
        );
        // Present: walked like any other reference, whether or not it
        // resolves. `gone` is not configured, and the walk is what reports
        // that.
        assert_eq!(
            dependency_refs(
                &serde_json::json!({ "model_id": "m1" }),
                &optional("model_id")
            ),
            vec![(RefKind::Model, "m1")]
        );
        assert_eq!(
            dependency_refs(
                &serde_json::json!({ "model_id": "gone" }),
                &optional("model_id")
            ),
            vec![(RefKind::Model, "gone")]
        );
    }

    #[test]
    fn an_absent_required_dependency_names_nothing_and_construction_reports_it() {
        let required = vec![Dependency::Model {
            config_key: "model_id".to_string(),
            requirement: ModelRequirement::default(),
            resolved_id: None,
            required: true,
        }];

        // Whether a setting has to be there is what the capability's own
        // config type declares, so an entry with nothing under the key names
        // nothing here and is reported where it is built.
        assert_eq!(dependency_refs(&serde_json::json!({}), &required), vec![]);

        let mut config = healthy();
        config
            .capabilities
            .insert("chat".into(), capability("chat", "agent-model", ""));

        let err = check(RefKind::Capability, "chat", &config).expect_err("should fail");
        assert!(
            matches!(err.problem, Problem::UnreadableSettings { .. }),
            "expected unreadable settings, got {:?}",
            err.problem
        );
    }

    #[test]
    fn every_required_dependency_is_enforced_by_its_config_type() {
        // The walk reports an empty required id only through construction,
        // so a capability type that declares a dependency `required` has to
        // reject an empty value for it in its config type's validation. This
        // checks every registered type, so a new one that declares the
        // dependency without the validation fails here.
        let mut checked = 0;
        for (type_name, metadata) in crate::capabilities::CAPABILITY_REGISTRY.entries() {
            for dependency in &metadata.dependencies {
                let config_key = match dependency {
                    Dependency::Model {
                        config_key,
                        required: true,
                        ..
                    }
                    | Dependency::Provider {
                        config_key,
                        required: true,
                        ..
                    } => config_key,
                    _ => continue,
                };
                let mut settings = crate::capabilities::CAPABILITY_REGISTRY.default_config(type_name);
                settings[config_key.as_str()] = serde_json::json!("");

                let err = crate::capabilities::CAPABILITY_REGISTRY
                    .construct(type_name, type_name, &settings)
                    .err()
                    .unwrap_or_else(|| {
                        panic!("'{type_name}' builds with an empty required '{config_key}'")
                    });
                assert!(
                    err.to_string().contains(&format!("{config_key}:")),
                    "'{type_name}' failed for another reason: {err}"
                );
                checked += 1;
            }
        }
        assert!(
            checked > 0,
            "no capability type declares a required dependency"
        );
    }

    #[test]
    fn a_capabilitys_own_problem_names_the_instance_that_reached_it() {
        let mut config = healthy();
        // `agent-model` requires a model id, and `setup` leaves an empty
        // string behind when none was selected.
        config
            .capabilities
            .insert("chat".into(), capability("chat", "agent-model", ""));

        let err = check(RefKind::Launcher, "claude", &config).expect_err("should fail");

        assert_eq!(err.target, (RefKind::Capability, "chat".to_string()));
        assert!(
            matches!(err.problem, Problem::UnreadableSettings { .. }),
            "expected unreadable settings, got {:?}",
            err.problem
        );
        assert_eq!(
            err.referrer,
            Some((RefKind::Launcher, "claude".to_string()))
        );
        assert_eq!(
            err.to_string(),
            "launcher 'claude' depends on capability 'chat', \
             which has settings that are not valid: model_id: no model is selected"
        );
    }

    #[test]
    fn a_model_that_does_not_meet_the_requirement_is_reported_with_what_is_unmet() {
        let mut config = healthy();
        // `m1` states Chat and ToolCalling, which is what `agent-model`
        // asks of the model it names. `vision-mcp` asks for a vision model
        // that understands images, which `m1` is not.
        config
            .capabilities
            .insert("vision".into(), capability("vision", "vision-mcp", "m1"));

        assert!(
            check(RefKind::Capability, "chat", &config).is_ok(),
            "the same model satisfies the capability whose requirement it meets"
        );

        let err = check(RefKind::Capability, "vision", &config).expect_err("should fail");
        assert_eq!(err.target, (RefKind::Capability, "vision".to_string()));
        let Problem::UnmetRequirement { model_id, unmet } = &err.problem else {
            panic!("expected an unmet requirement, got {:?}", err.problem);
        };
        assert_eq!(model_id, "m1");
        assert!(
            unmet.contains("Image Understanding"),
            "the unmet part is named: {unmet}"
        );
    }

    #[test]
    fn a_custom_model_is_judged_by_its_own_settings() {
        let mut config = healthy();
        config.capabilities.insert(
            "vision".into(),
            capability("vision", "vision-mcp", "m-vision"),
        );

        // A `custom` model's registry entry is a placeholder, so what it can
        // do is what its own settings say.
        config.models.insert(
            "m-vision".into(),
            ModelConfig {
                model_id: "m-vision".to_string(),
                model_type: "custom".to_string(),
                provider_id: "p1".to_string(),
                variant: None,
                config: serde_json::json!({
                    "family": "Test",
                    "model_type": "Vision",
                    "supported_functions": ["Chat", "ImageUnderstanding"],
                }),
            },
        );
        assert!(check(RefKind::Capability, "vision", &config).is_ok());

        config
            .models
            .get_mut("m-vision")
            .unwrap()
            .config
            .as_object_mut()
            .unwrap()
            .insert(
                "supported_functions".to_string(),
                serde_json::json!(["Chat"]),
            );
        let err = check(RefKind::Capability, "vision", &config).expect_err("should fail");
        assert!(matches!(err.problem, Problem::UnmetRequirement { .. }));
    }

    #[test]
    fn a_mismatch_reached_through_a_launcher_names_the_launcher_as_referrer() {
        let mut config = healthy();
        config
            .capabilities
            .insert("vision".into(), capability("vision", "vision-mcp", "m1"));
        config.launchers.insert(
            "claude".into(),
            launcher("claude", "claude", &["chat", "vision"]),
        );

        let err = check(RefKind::Launcher, "claude", &config).expect_err("should fail");
        assert_eq!(err.target, (RefKind::Capability, "vision".to_string()));
        assert_eq!(
            err.referrer,
            Some((RefKind::Launcher, "claude".to_string()))
        );
        assert!(
            err.to_string().contains("does not meet its requirement"),
            "got: {err}"
        );
    }

    #[test]
    fn a_model_that_is_not_configured_is_still_reported_as_not_configured() {
        let mut config = healthy();
        config.models.remove("m1");

        let err = check(RefKind::Capability, "chat", &config).expect_err("should fail");
        assert_eq!(err.target, (RefKind::Model, "m1".to_string()));
        assert_eq!(err.problem, Problem::NotConfigured);
    }

    #[test]
    fn settings_that_cannot_be_read_are_reported_at_the_instance_that_holds_them() {
        let mut config = healthy();
        config.providers.insert(
            "p1".into(),
            ProviderConfig {
                provider_id: "p1".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({ "timeout_secs": "ten" }),
            },
        );

        let err = check(RefKind::Launcher, "claude", &config).expect_err("should fail");
        assert_eq!(err.target, (RefKind::Provider, "p1".to_string()));
        let Problem::UnreadableSettings { detail } = &err.problem else {
            panic!("expected unreadable settings, got {:?}", err.problem);
        };
        assert!(detail.contains("invalid type"), "got: {detail}");
    }

    #[test]
    fn an_external_tool_dependency_is_not_a_config_reference() {
        let deps = vec![Dependency::ExternalTool {
            requirement: ShellCommandRequirement {
                command: "ffmpeg".to_string(),
            },
            required: true,
        }];
        assert_eq!(dependency_refs(&serde_json::json!({}), &deps), vec![]);
    }

    #[test]
    fn find_dangling_returns_exactly_the_broken_instances_of_a_kind() {
        let mut config = healthy();
        config.models.insert(
            "m-no-provider".into(),
            model("m-no-provider", "custom", None),
        );
        config
            .models
            .insert("m-gone".into(), model("m-gone", "custom", Some("gone")));
        config
            .models
            .insert("m-bad-type".into(), model("m-bad-type", "nope", Some("p1")));

        let mut broken: Vec<String> =
            find_dangling(RefKind::Model, &config, &Sources::build(&config, None))
                .into_iter()
                .map(|d| d.instance_id)
                .collect();
        broken.sort();
        assert_eq!(broken, ["m-bad-type", "m-gone", "m-no-provider"]);

        assert!(
            find_dangling(RefKind::Provider, &config, &Sources::build(&config, None)).is_empty()
        );
        assert_eq!(
            find_dangling(RefKind::Capability, &config, &Sources::build(&config, None)).len(),
            0
        );
    }

    #[test]
    fn find_dangling_returns_nothing_for_a_healthy_config() {
        let config = healthy();
        for kind in [
            RefKind::Provider,
            RefKind::Model,
            RefKind::Capability,
            RefKind::Launcher,
        ] {
            assert!(
                find_dangling(kind, &config, &Sources::build(&config, None)).is_empty(),
                "{kind}"
            );
        }
    }

    #[test]
    fn find_dangling_only_reports_the_kind_it_was_asked_about() {
        let mut config = healthy();
        // Breaking the provider breaks the model, the capability and the
        // launcher that reach it, but each scan reports only its own kind.
        config.providers.remove("p1");

        for (kind, expected) in [
            (RefKind::Provider, Vec::<&str>::new()),
            (RefKind::Model, vec!["m1"]),
            (RefKind::Capability, vec!["chat"]),
            (RefKind::Launcher, vec!["claude"]),
        ] {
            let found: Vec<String> = find_dangling(kind, &config, &Sources::build(&config, None))
                .into_iter()
                .map(|d| d.instance_id)
                .collect();
            assert_eq!(found, expected, "{kind}");
            assert!(
                find_dangling(kind, &config, &Sources::build(&config, None))
                    .iter()
                    .all(|d| d.kind == kind)
            );
        }
    }

    #[test]
    fn dependents_are_the_instances_pointing_at_the_target() {
        let config = healthy();

        assert_eq!(
            dependents(RefKind::Provider, "p1", &config),
            vec![(RefKind::Model, "m1".to_string())]
        );
        assert_eq!(
            dependents(RefKind::Model, "m1", &config),
            vec![(RefKind::Capability, "chat".to_string())]
        );
        assert_eq!(
            dependents(RefKind::Capability, "chat", &config),
            vec![(RefKind::Launcher, "claude".to_string())]
        );
        // Nothing points at a launcher, and nothing points at what is not
        // configured.
        assert!(dependents(RefKind::Launcher, "claude", &config).is_empty());
        assert!(dependents(RefKind::Model, "gone", &config).is_empty());
    }

    #[test]
    fn dependents_lists_every_referrer_of_one_target() {
        let mut config = healthy();
        config
            .capabilities
            .insert("second".into(), capability("second", "agent-model", "m1"));

        assert_eq!(
            dependents(RefKind::Model, "m1", &config),
            vec![
                (RefKind::Capability, "chat".to_string()),
                (RefKind::Capability, "second".to_string()),
            ]
        );
    }

    #[test]
    fn find_dangling_reports_the_rendered_reason() {
        let mut config = healthy();
        config.providers.remove("p1");

        let dangling = find_dangling(RefKind::Model, &config, &Sources::build(&config, None));
        assert_eq!(dangling.len(), 1);
        assert_eq!(dangling[0].kind, RefKind::Model);
        assert_eq!(dangling[0].instance_id, "m1");
        assert_eq!(
            dangling[0].reason,
            "model 'm1' depends on provider 'p1', which is not configured"
        );
    }
}
