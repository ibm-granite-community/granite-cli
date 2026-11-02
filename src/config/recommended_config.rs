//! Recommended configuration schema for `granite-cli setup --auto`.
//!
//! A recommended configuration says, per launcher, which capabilities can be
//! auto-enabled and which specific catalog models (at which quantization
//! precisions) are recommended to fill each of that capability's model
//! dependency slots.

use alog::{MessageLevel, alog_channel, use_channel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use_channel!("RCFG");

// Embedded built-in recommended configurations (from build.rs)
include!(concat!(
    env!("OUT_DIR"),
    "/generated_recommended_configs.rs"
));

/// Built-in recommended configurations parsed from the YAML files embedded
/// by `build.rs`.  Each file becomes one entry; parse errors are logged and
/// the offending file is skipped.
pub(crate) static BUILTIN_RECOMMENDED_CONFIGS: std::sync::LazyLock<Vec<RecommendedConfiguration>> =
    std::sync::LazyLock::new(|| {
        RECOMMENDED_CONFIG_SOURCES
            .iter()
            .filter_map(|(name, yaml)| {
                match serde_yaml::from_str::<RecommendedConfiguration>(yaml) {
                    Ok(cfg) => Some(cfg),
                    Err(e) => {
                        alog_channel!(
                            MessageLevel::Warning,
                            "Failed to parse recommended config '{}': {}",
                            name,
                            e
                        );
                        None
                    }
                }
            })
            .collect()
    });

// -- Schema structs -------------------------------------------------------------

/// One launcher's curated auto-setup recommendation. `launcher` is the
/// LAUNCHER_REGISTRY key (e.g. "claude"), or the reserved wildcard "*"
/// applied to any launcher without its own specific entry.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecommendedConfiguration {
    pub(crate) launcher: String,
    pub(crate) capabilities: Vec<RecommendedCapability>,
}

/// One capability a launcher can auto-enable, and how to fill each of its
/// model dependency slots.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecommendedCapability {
    /// CAPABILITY_REGISTRY key (e.g. "agent-model").
    pub(crate) capability: String,
    /// Keyed by the capability's own `Dependency::Model.config_key` (e.g.
    /// "model_id") -- a capability may have more than one model dependency
    /// slot, each independently recommended.
    pub(crate) models: HashMap<String, RecommendedModelSet>,
}

/// A slot's requirements plus its ordered list of admissible candidates.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecommendedModelSet {
    /// Minimum useful *achieved* context length for this slot, independent
    /// of which candidate is chosen (e.g. a coding-agent capability
    /// inherently needs more headroom than a lightweight one). Meant to be
    /// checked by callers against the *effective* context after hardware
    /// fit, not a model's native max -- that's the caller's job, not this
    /// module's; this module just carries the number.
    pub(crate) min_context_length: Option<u64>,
    /// Ordered by preference; callers should try candidates in order and
    /// take the first that resolves.
    pub(crate) models: Vec<RecommendedModel>,
}

/// One candidate model and which of its variants are acceptable.
///
/// A variant is admitted when it matches `variant_formats` (or that list is
/// empty) *and* matches `variant_precisions` (or that list is empty) -- AND
/// logic between the two fields, OR logic within each. To express
/// alternatives that don't fit one AND (e.g. "any precision on Ollama, OR
/// Q4_K_M-or-better on GGUF"), list the same `model` more than once in the
/// enclosing `RecommendedModelSet.models` with different constraints --
/// candidates are already tried in order, first admissible one wins, so
/// this composes for free rather than needing a third list here.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecommendedModel {
    pub(crate) model: StringMatch,
    /// Per-model format allow-list (e.g. "GGUF", "Ollama", "OpenRouter");
    /// empty means any format is acceptable. Lets a config say "just use
    /// the Ollama-served build" (`variant_formats: [Ollama]`, no precision
    /// constraint) or restrict a hosted/API provider format that has no
    /// quantization precision to speak of at all.
    #[serde(default)]
    pub(crate) variant_formats: Vec<StringMatch>,
    /// Per-model precision allow-list (e.g. "Q4_K_M", "IQ4_XS"); empty means
    /// any variant is acceptable.
    #[serde(default)]
    pub(crate) variant_precisions: Vec<StringMatch>,
}

// -- StringMatch ----------------------------------------------------------------

/// Exact match by default; explicit regex escape hatch so a plain
/// version-like string (e.g. "4.2") is never silently misinterpreted as a
/// pattern. In YAML: a bare string deserializes to `Exact`; a mapping
/// `{regex: "..."}` deserializes to `Regex`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub(crate) enum StringMatch {
    Exact(String),
    Regex { regex: String },
}

impl StringMatch {
    /// Whether `s` matches. An invalid regex pattern never matches (rather
    /// than panicking) -- log a warning via this crate's `alog` logging.
    pub(crate) fn matches(&self, s: &str) -> bool {
        match self {
            StringMatch::Exact(pattern) => s == pattern,
            StringMatch::Regex { regex: pattern } => match regex::Regex::new(pattern) {
                Ok(re) => re.is_match(s),
                Err(e) => {
                    alog_channel!(
                        MessageLevel::Warning,
                        "Invalid regex pattern '{}': {}",
                        pattern,
                        e
                    );
                    false
                }
            },
        }
    }
}

// -- Merge/lookup helpers -------------------------------------------------------

/// Look up one launcher key's effective config: a user-authored entry wins
/// outright over the built-in entry for that same key (whole-entry
/// replace, not a field-level merge).
fn resolve_entry<'a>(
    key: &str,
    builtin: &'a [RecommendedConfiguration],
    user: &'a HashMap<String, RecommendedConfiguration>,
) -> Option<&'a RecommendedConfiguration> {
    user.get(key)
        .or_else(|| builtin.iter().find(|c| c.launcher == key))
}

/// The effective capability list for one launcher: if the launcher has its
/// own entry (user- or builtin-authored), that entry is authoritative and
/// complete -- the wildcard ("*") entry is consulted only when the launcher
/// has no entry of its own at all. The two are never blended.
///
/// A launcher's own entry omitting a capability the wildcard recommends is a
/// deliberate statement, not a gap to fill in: e.g. `claude.yaml` not
/// listing `agent-model` means "claude sticks with its own upstream model
/// for the main agent," not "fall back to whatever the wildcard says."
/// Blending the two would silently reintroduce exactly that kind of
/// unwanted recommendation.
pub(crate) fn effective_capabilities(
    launcher_type: &str,
    builtin: &[RecommendedConfiguration],
    user: &HashMap<String, RecommendedConfiguration>,
) -> Vec<RecommendedCapability> {
    if let Some(specific) = resolve_entry(launcher_type, builtin, user) {
        return specific.capabilities.clone();
    }
    resolve_entry("*", builtin, user)
        .map(|wildcard| wildcard.capabilities.clone())
        .unwrap_or_default()
}

// -- Tests ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- StringMatch tests --------------------------------------------------

    #[test]
    fn exact_match_matches() {
        let m = StringMatch::Exact("foo".to_string());
        assert!(m.matches("foo"));
    }

    #[test]
    fn exact_non_match_does_not_match() {
        let m = StringMatch::Exact("foo".to_string());
        assert!(!m.matches("bar"));
    }

    #[test]
    fn regex_pattern_matches() {
        let m = StringMatch::Regex {
            regex: "fo.*".to_string(),
        };
        assert!(m.matches("foobar"));
        assert!(m.matches("fo"));
    }

    #[test]
    fn regex_pattern_non_match() {
        let m = StringMatch::Regex {
            regex: "fo.*".to_string(),
        };
        assert!(!m.matches("baz"));
    }

    #[test]
    fn invalid_regex_returns_false_not_panic() {
        let m = StringMatch::Regex {
            regex: "[invalid".to_string(),
        };
        assert!(!m.matches("anything"));
    }

    #[test]
    fn exact_version_string_does_not_treat_dot_as_regex() {
        // The footgun-avoidance case: "4.2" as Exact must match only "4.2"
        // and must NOT match "4x2" (which a regex "." would match).
        let m = StringMatch::Exact("4.2".to_string());
        assert!(m.matches("4.2"));
        assert!(!m.matches("4x2"));
        assert!(!m.matches("42"));
        assert!(!m.matches("4.a2"));
    }

    // -- effective_capabilities tests -----------------------------------------

    fn make_config(launcher: &str, capability: &str, model_val: &str) -> RecommendedConfiguration {
        let mut models = HashMap::new();
        models.insert(
            "model_id".to_string(),
            RecommendedModelSet {
                min_context_length: None,
                models: vec![RecommendedModel {
                    model: StringMatch::Exact(model_val.to_string()),
                    variant_formats: vec![],
                    variant_precisions: vec![],
                }],
            },
        );
        RecommendedConfiguration {
            launcher: launcher.to_string(),
            capabilities: vec![RecommendedCapability {
                capability: capability.to_string(),
                models,
            }],
        }
    }

    #[test]
    fn user_entry_fully_replaces_builtin_for_same_launcher() {
        // Builtin has capability X with model "builtin-model".
        // User override for the same launcher has capability Y with model "user-model".
        // Result must be exactly Y, not X+Y.
        let builtin = vec![make_config("claude", "cap-x", "builtin-model")];
        let mut user = HashMap::new();
        user.insert(
            "claude".to_string(),
            make_config("claude", "cap-y", "user-model"),
        );

        let caps = effective_capabilities("claude", &builtin, &user);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].capability, "cap-y");
    }

    #[test]
    fn wildcard_applies_to_launcher_with_no_specific_entry() {
        let builtin = vec![make_config("*", "wildcap", "wildcard-model")];
        let user = HashMap::new();

        let caps = effective_capabilities("bob", &builtin, &user);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].capability, "wildcap");
    }

    #[test]
    fn specific_entry_wins_over_wildcard_when_both_define_the_same_capability() {
        // Both wildcard and specific define capability "agent-model". The
        // specific launcher's entry is authoritative, so its
        // RecommendedCapability must win.
        let mut specific_models = HashMap::new();
        specific_models.insert(
            "model_id".to_string(),
            RecommendedModelSet {
                min_context_length: None,
                models: vec![RecommendedModel {
                    model: StringMatch::Exact("specific-model".to_string()),
                    variant_formats: vec![],
                    variant_precisions: vec![],
                }],
            },
        );

        let mut wildcard_models = HashMap::new();
        wildcard_models.insert(
            "model_id".to_string(),
            RecommendedModelSet {
                min_context_length: None,
                models: vec![RecommendedModel {
                    model: StringMatch::Exact("wildcard-model".to_string()),
                    variant_formats: vec![],
                    variant_precisions: vec![],
                }],
            },
        );

        let builtin = vec![
            RecommendedConfiguration {
                launcher: "claude".to_string(),
                capabilities: vec![RecommendedCapability {
                    capability: "agent-model".to_string(),
                    models: specific_models,
                }],
            },
            RecommendedConfiguration {
                launcher: "*".to_string(),
                capabilities: vec![RecommendedCapability {
                    capability: "agent-model".to_string(),
                    models: wildcard_models,
                }],
            },
        ];
        let user = HashMap::new();

        let caps = effective_capabilities("claude", &builtin, &user);
        assert_eq!(caps.len(), 1);
        // Verify the specific launcher's model value wins
        let model_slot = caps[0].models.get("model_id").expect("model_id present");
        assert_eq!(
            model_slot.models[0].model,
            StringMatch::Exact("specific-model".to_string())
        );
    }

    #[test]
    fn a_launchers_own_entry_excludes_wildcard_capabilities_entirely() {
        // Specific launcher defines only "agent-model"; wildcard separately
        // defines "sub-agent-cap". A launcher's own entry is authoritative
        // and complete -- the wildcard must NOT be blended in, so
        // "sub-agent-cap" must not appear even though the wildcard lists it.
        // (This is exactly the claude.yaml case: it deliberately omits
        // agent-model to mean "don't use it," not "fall back to default.")
        let builtin = vec![
            make_config("claude", "agent-model", "specific-model"),
            make_config("*", "sub-agent-cap", "wildcard-model"),
        ];
        let user = HashMap::new();

        let caps = effective_capabilities("claude", &builtin, &user);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].capability, "agent-model");
    }

    #[test]
    fn no_builtin_no_user_returns_empty() {
        let builtin: Vec<RecommendedConfiguration> = vec![];
        let user = HashMap::new();
        let caps = effective_capabilities("claude", &builtin, &user);
        assert!(caps.is_empty());
    }

    // -- BUILTIN_RECOMMENDED_CONFIGS tests ------------------------------------

    #[test]
    fn builtin_recommended_configs_is_non_empty() {
        assert!(
            !BUILTIN_RECOMMENDED_CONFIGS.is_empty(),
            "BUILTIN_RECOMMENDED_CONFIGS must contain at least one entry"
        );
    }

    #[test]
    fn builtin_contains_claude_sub_agent_code() {
        // The built-in configs must include a claude entry with its own
        // sub-agent-code recommendation (claude.yaml deliberately doesn't
        // define agent-model itself, and -- since a launcher's own entry is
        // authoritative -- that means claude never gets agent-model at all,
        // not that it falls back to the wildcard; check a capability
        // claude.yaml actually owns instead).
        let mut found_claude = false;
        for cfg in &*BUILTIN_RECOMMENDED_CONFIGS {
            if cfg.launcher == "claude" {
                found_claude = true;
                let sub_agent_code = cfg
                    .capabilities
                    .iter()
                    .find(|c| c.capability == "sub-agent-code");
                assert!(
                    sub_agent_code.is_some(),
                    "claude config must define 'sub-agent-code' capability"
                );
                let sub_agent_code = sub_agent_code.unwrap();
                assert!(
                    sub_agent_code.models.contains_key("model_id"),
                    "claude sub-agent-code must have a 'model_id' slot"
                );
            }
        }
        assert!(
            found_claude,
            "BUILTIN_RECOMMENDED_CONFIGS must contain a 'claude' launcher entry"
        );
    }

    #[test]
    fn claude_never_gets_agent_model_from_the_wildcard() {
        // claude.yaml deliberately doesn't define agent-model itself,
        // meaning "claude sticks with its own upstream model for the main
        // agent" -- NOT "fall back to the wildcard's recommendation." Since
        // claude has its own entry, the wildcard must not be consulted at
        // all, so agent-model must be absent from claude's effective
        // capabilities.
        let caps = effective_capabilities("claude", &BUILTIN_RECOMMENDED_CONFIGS, &HashMap::new());
        assert!(
            !caps.iter().any(|c| c.capability == "agent-model"),
            "claude has its own recommended config, so it must not inherit agent-model \
             from the wildcard"
        );
    }

    #[test]
    fn every_builtin_recommended_config_parses() {
        // BUILTIN_RECOMMENDED_CONFIGS drops a file that fails to parse and
        // logs a warning, so a broken file under resources/recommended_configs
        // would otherwise only show up at run time.
        let names: Vec<&str> = RECOMMENDED_CONFIG_SOURCES
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            BUILTIN_RECOMMENDED_CONFIGS.len(),
            RECOMMENDED_CONFIG_SOURCES.len(),
            "some of {names:?} did not parse"
        );
    }

    #[test]
    fn unknown_fields_are_rejected() {
        // `variant_precision` is a misspelling of `variant_precisions`.
        // Ignoring it would admit every precision of the model.
        let result = serde_yaml::from_str::<RecommendedModel>(
            "model: granite-4.2-3b\nvariant_precision: [Q8_0]\n",
        );
        assert!(result.is_err());

        let result =
            serde_yaml::from_str::<RecommendedModelSet>("min_context_lenght: 65536\nmodels: []\n");
        assert!(result.is_err());
    }

    #[test]
    fn builtin_contains_wildcard() {
        assert!(
            BUILTIN_RECOMMENDED_CONFIGS
                .iter()
                .any(|c| c.launcher == "*"),
            "must have a wildcard (*) entry"
        );
    }

    #[test]
    fn builtin_contains_opencode() {
        assert!(
            BUILTIN_RECOMMENDED_CONFIGS
                .iter()
                .any(|c| c.launcher == "opencode"),
            "must have an 'opencode' launcher entry"
        );
    }
}
