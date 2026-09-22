// Standard
use std::collections::{HashMap, HashSet};

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use anyhow::Result;

// Local
use crate::capabilities::{BindingType, CAPABILITY_REGISTRY, Dependency, ModelRequirement};
use crate::commands::capability::CapabilityCommands;
use crate::commands::launcher::LauncherCommands;
use crate::commands::model::ModelCommands;
use crate::commands::provider::ProviderCommands;
use crate::config::recommended_config;
use crate::dependency::{Configured, Requirement};
use crate::launchers::LAUNCHER_REGISTRY;
use crate::models::{
    ContextFit, MODEL_REGISTRY, ModelFunction, ModelMetadata, ModelType, ModelVariant,
};
use crate::providers::{HealthStatus, PROVIDER_REGISTRY, Provider};
use crate::utils::hardware::{HardwareProfile, detect_hardware};

use_channel!("SETUP");

/*-- public --*/

/// A single recommendation produced during discovery.
pub enum Recommendation {
    Provider {
        provider_type: &'static str,
        provider_name: String,
        health_healthy: bool,
        health_error: Option<String>,
    },
    Model {
        model_id: String,
        family: String,
        version: String,
        size: String,
        model_type: ModelType,
        best_variant: ModelVariant,
        context_fit: ContextFit,
        can_run_by: Vec<String>,
    },
    Launcher {
        launcher_type: String,
        launcher_name: String,
        binary_path: Option<String>,
    },
    Capability {
        capability_type: String,
        capability_name: String,
    },
}

/// The complete output of the discovery engine.
pub struct DiscoveryResult {
    pub recommendations: Vec<Recommendation>,
    /// Every unconfigured model with at least a partial hardware fit, one
    /// entry per catalog model_id (not deduplicated by family/version like
    /// `recommendations` is). Used by the "choose different models" escape
    /// hatch so a user can see and pick models that don't fully fit.
    pub all_model_candidates: Vec<Recommendation>,
    pub configured_provider_ids: Vec<String>,
    pub configured_model_ids: Vec<String>,
    pub configured_launcher_ids: Vec<String>,
    pub configured_capability_ids: Vec<String>,
}

/*-- private --*/

/// Discovers all available providers, models, launchers, and capabilities,
/// returning structured recommendations and a list of already-configured items.
struct Discover;

impl Discover {
    /// Run the full discovery pipeline against a given hardware profile.
    /// Split out from `run` so tests can pin the hardware profile instead of
    /// depending on `detect_hardware()`'s result on whatever machine the
    /// test happens to run on -- model-fit outcomes (and therefore which
    /// recommendations come out) are a direct function of the profile.
    async fn run_with_hardware(
        ctx: &crate::AppContext,
        profile: &HardwareProfile,
    ) -> DiscoveryResult {
        let (provider_recs, configured_providers) = Self::discover_providers(ctx).await;
        let model_recs = Self::discover_models(ctx, &configured_providers, profile);
        let all_model_candidates =
            Self::discover_all_model_candidates(ctx, &configured_providers, profile);
        let (launcher_recs, configured_launchers) = Self::discover_launchers(ctx).await;
        let (capability_recs, configured_capabilities) =
            Self::discover_capabilities(ctx, &provider_recs, &model_recs, &configured_providers);

        let mut recommendations: Vec<Recommendation> = Vec::new();
        recommendations.extend(provider_recs);
        recommendations.extend(model_recs);
        recommendations.extend(launcher_recs);
        recommendations.extend(capability_recs);

        // Sort for deterministic output
        recommendations.sort_by_key(display_name);

        DiscoveryResult {
            recommendations,
            all_model_candidates,
            configured_provider_ids: configured_providers,
            configured_model_ids: ctx.config.models.keys().cloned().collect(),
            configured_launcher_ids: configured_launchers,
            configured_capability_ids: configured_capabilities,
        }
    }

    // -- Provider discovery --------------------------------------------------

    async fn discover_providers(ctx: &crate::AppContext) -> (Vec<Recommendation>, Vec<String>) {
        let configured_ids: HashSet<&str> =
            ctx.config.providers.keys().map(|s| s.as_str()).collect();
        let mut configured: Vec<String> = Vec::new();
        let mut recommendations: Vec<Recommendation> = Vec::new();

        for (provider_type, metadata) in PROVIDER_REGISTRY.entries() {
            if configured_ids.contains(provider_type) {
                configured.push(provider_type.to_string());
                continue;
            }

            // Construct a transient instance with default config and run health check
            let default_config = PROVIDER_REGISTRY
                .default_config(provider_type)
                .unwrap_or_default();
            let result = PROVIDER_REGISTRY.construct(provider_type, provider_type, &default_config);

            match result {
                Ok(provider) => match Self::run_health_check(&*provider).await {
                    Ok(status) => recommendations.push(Recommendation::Provider {
                        provider_type,
                        provider_name: metadata.name.clone(),
                        health_healthy: status.healthy,
                        health_error: status.error,
                    }),
                    Err(e) => recommendations.push(Recommendation::Provider {
                        provider_type,
                        provider_name: metadata.name.clone(),
                        health_healthy: false,
                        health_error: Some(format!("Health check failed: {e}")),
                    }),
                },
                Err(_) => {
                    // Provider could not be constructed (e.g., missing schema).
                    // Still recommend it — user may need to configure it manually.
                    recommendations.push(Recommendation::Provider {
                        provider_type,
                        provider_name: metadata.name.clone(),
                        health_healthy: false,
                        health_error: Some(
                            "Could not construct provider with default config".to_string(),
                        ),
                    });
                }
            }
        }

        configured.sort();
        recommendations.sort_by_key(display_name);
        (recommendations, configured)
    }

    async fn run_health_check(
        provider: &dyn Provider,
    ) -> Result<HealthStatus, crate::providers::ProviderError> {
        provider.health_check().await
    }

    // -- Model discovery -----------------------------------------------------

    fn discover_models(
        ctx: &crate::AppContext,
        configured_provider_ids: &[String],
        profile: &HardwareProfile,
    ) -> Vec<Recommendation> {
        let configured_ids: HashSet<&str> = ctx.config.models.keys().map(|s| s.as_str()).collect();

        // Group models by family, keeping each model's real catalog id
        // alongside its metadata.
        let mut family_groups: HashMap<String, Vec<(String, ModelMetadata)>> = HashMap::new();
        for (model_id, model_md) in MODEL_REGISTRY.entries() {
            if configured_ids.contains(model_id) {
                continue;
            }
            let family = model_md.family.clone();
            family_groups
                .entry(family)
                .or_default()
                .push((model_id.to_string(), model_md));
        }

        let mut recommendations: Vec<Recommendation> = Vec::new();

        for models in family_groups.values() {
            // Find the latest version string present in this family (a
            // family can release several sizes at the same version, e.g.
            // granite-4.2-3b/8b/30b are all version "4.2").
            let Some((_, sample)) = find_latest_version(models) else {
                continue;
            };
            let latest_version = &sample.version;

            // Among every size released at that version, recommend only
            // the largest one that *fully* fits the current hardware.
            // Partially-fitting models are never auto-recommended here --
            // the user can still reach them via `select_models_manually`.
            let best = models
                .iter()
                .filter(|(_, md)| &md.version == latest_version)
                .filter_map(|(id, md)| {
                    Self::build_model_recommendation(
                        id,
                        md,
                        profile,
                        configured_provider_ids,
                        ctx,
                        true,
                    )
                })
                .max_by_key(|rec| match rec {
                    Recommendation::Model { size, .. } => parse_size(size),
                    _ => 0,
                });

            if let Some(rec) = best {
                recommendations.push(rec);
            }
        }

        sort_model_recommendations(&mut recommendations);
        recommendations
    }

    /// Every unconfigured model with at least a partial fit, one entry per
    /// catalog model_id -- the full pool "choose different models" picks
    /// from, unfiltered by the "only fully-fitting" / "one per family" rules
    /// `discover_models` applies for the default recommendation.
    fn discover_all_model_candidates(
        ctx: &crate::AppContext,
        configured_provider_ids: &[String],
        profile: &HardwareProfile,
    ) -> Vec<Recommendation> {
        let configured_ids: HashSet<&str> = ctx.config.models.keys().map(|s| s.as_str()).collect();

        let mut recommendations: Vec<Recommendation> = MODEL_REGISTRY
            .entries()
            .into_iter()
            .filter(|(model_id, _)| !configured_ids.contains(*model_id))
            .filter_map(|(model_id, md)| {
                Self::build_model_recommendation(
                    model_id,
                    &md,
                    profile,
                    configured_provider_ids,
                    ctx,
                    false,
                )
            })
            .collect();

        sort_model_recommendations(&mut recommendations);
        recommendations
    }

    /// Builds a `Recommendation::Model` for `model_id` using its best-fitting
    /// variant for `profile`. When `require_full_fit` is true, only a
    /// `ContextFit::Full` result is accepted (used for the default,
    /// one-per-family recommendation); otherwise any non-`None` fit is
    /// accepted (used for the full candidate pool).
    fn build_model_recommendation(
        model_id: &str,
        md: &ModelMetadata,
        profile: &crate::utils::hardware::HardwareProfile,
        configured_provider_ids: &[String],
        ctx: &crate::AppContext,
        require_full_fit: bool,
    ) -> Option<Recommendation> {
        let (variant, fit) = best_variant(md, profile)?;
        if require_full_fit && fit != ContextFit::Full {
            return None;
        }
        let can_run_by = Self::find_can_run_providers(&variant, configured_provider_ids, ctx);
        Some(Recommendation::Model {
            model_id: model_id.to_string(),
            family: md.family.clone(),
            version: md.version.clone(),
            size: format_size(md.size),
            model_type: md.model_type.clone(),
            best_variant: variant,
            context_fit: fit,
            can_run_by,
        })
    }

    fn find_can_run_providers(
        variant: &ModelVariant,
        configured_provider_ids: &[String],
        ctx: &crate::AppContext,
    ) -> Vec<String> {
        configured_provider_ids
            .iter()
            .filter_map(|pid| ctx.config.get_provider(pid))
            .filter_map(|pc| {
                PROVIDER_REGISTRY
                    .construct(&pc.provider_type, &pc.provider_id, &pc.config)
                    .ok()
                    .filter(|p| p.can_run_model(&variant.format, &variant.precision))
            })
            .map(|p| p.instance_id().to_string())
            .collect()
    }

    // -- Launcher discovery --------------------------------------------------

    async fn discover_launchers(ctx: &crate::AppContext) -> (Vec<Recommendation>, Vec<String>) {
        let configured_ids: HashSet<&str> =
            ctx.config.launchers.keys().map(|s| s.as_str()).collect();
        let mut configured: Vec<String> = Vec::new();
        let mut recommendations: Vec<Recommendation> = Vec::new();

        for (launcher_type, metadata) in LAUNCHER_REGISTRY.entries() {
            if configured_ids.contains(launcher_type) {
                configured.push(launcher_type.to_string());
                continue;
            }

            // Construct a transient instance with default config
            let default_config = LAUNCHER_REGISTRY
                .default_config(launcher_type)
                .unwrap_or_default();
            match LAUNCHER_REGISTRY.construct(launcher_type, launcher_type, &default_config) {
                Ok(launcher) => match launcher.validate_command() {
                    Ok(path) => recommendations.push(Recommendation::Launcher {
                        launcher_type: launcher_type.to_string(),
                        launcher_name: metadata.name.clone(),
                        binary_path: Some(path.to_string_lossy().to_string()),
                    }),
                    Err(_) => recommendations.push(Recommendation::Launcher {
                        launcher_type: launcher_type.to_string(),
                        launcher_name: metadata.name.clone(),
                        binary_path: None,
                    }),
                },
                Err(_) => {
                    // Could not construct — still recommend but without binary info
                    recommendations.push(Recommendation::Launcher {
                        launcher_type: launcher_type.to_string(),
                        launcher_name: metadata.name.clone(),
                        binary_path: None,
                    });
                }
            }
        }

        configured.sort();
        recommendations.sort_by_key(display_name);
        (recommendations, configured)
    }

    // -- Capability discovery ------------------------------------------------

    fn discover_capabilities(
        ctx: &crate::AppContext,
        _provider_recs: &[Recommendation],
        _model_recs: &[Recommendation],
        _configured_provider_ids: &[String],
    ) -> (Vec<Recommendation>, Vec<String>) {
        let configured_ids: Vec<&str> =
            ctx.config.capabilities.keys().map(|s| s.as_str()).collect();
        let configured: Vec<String> = configured_ids.iter().copied().map(String::from).collect();

        let mut recommendations: Vec<Recommendation> = Vec::new();

        for (capability_type, metadata) in CAPABILITY_REGISTRY.entries() {
            if configured_ids.contains(&capability_type) {
                continue;
            }

            recommendations.push(Recommendation::Capability {
                capability_type: capability_type.to_string(),
                capability_name: metadata.name.clone(),
            });
        }

        recommendations.sort_by_key(display_name);
        (recommendations, configured)
    }
}

/// A re-evaluator that filters recommendations based on user selections from
/// earlier wizard sections. This implements the backward-from-capabilities
/// dependency flow.
struct Revaluator;

impl Revaluator {
    /// Filter launcher recommendations to only those that support at least one
    /// of the selected capability binding types.
    fn for_launchers<'a>(
        discovery: &'a DiscoveryResult,
        selected_cap_types: &HashSet<String>,
    ) -> Vec<&'a Recommendation> {
        // Determine which binding types are needed by selected capabilities
        let needed_types: HashSet<BindingType> = selected_cap_types
            .iter()
            .filter_map(|cap_type| CAPABILITY_REGISTRY.get(cap_type))
            .flat_map(|m| m.supported_binding_types.clone().into_iter())
            .collect();

        if needed_types.is_empty() {
            // No binding types needed — any launcher is fine
            return discovery
                .recommendations
                .iter()
                .filter(|r| matches!(r, Recommendation::Launcher { .. }))
                .collect();
        }

        discovery
            .recommendations
            .iter()
            .filter(move |r| {
                if let Recommendation::Launcher { launcher_type, .. } = r {
                    if let Some(launcher_meta) = LAUNCHER_REGISTRY.get(launcher_type) {
                        return launcher_meta
                            .supported_capabilities
                            .iter()
                            .any(|bt| needed_types.contains(bt));
                    }
                    // If we can't look up the launcher, include it anyway
                    return true;
                }
                false
            })
            .collect()
    }

    /// Filter provider recommendations to only those that can run at least one
    /// of the selected model variants.
    fn for_providers<'a>(
        discovery: &'a DiscoveryResult,
        selected_model_ids: &HashSet<String>,
        _ctx: &crate::AppContext,
    ) -> Vec<&'a Recommendation> {
        let provider_recs: Vec<_> = discovery
            .recommendations
            .iter()
            .filter(|r| matches!(r, Recommendation::Provider { .. }))
            .collect();

        // If no models selected, return all provider recommendations
        if selected_model_ids.is_empty() {
            return provider_recs;
        }

        // Build a set of selected model IDs for quick lookup
        let selected_ids: HashSet<&str> = selected_model_ids.iter().map(|s| s.as_str()).collect();

        // Check which providers can run the selected models by looking at the
        // can_run_by field in the model recommendations
        let providers_that_can_run: HashSet<String> = discovery
            .recommendations
            .iter()
            .filter_map(|r| match r {
                Recommendation::Model {
                    model_id,
                    can_run_by,
                    ..
                } => {
                    if selected_ids.contains(model_id.as_str()) {
                        Some(can_run_by.clone())
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .flatten()
            .collect();

        if providers_that_can_run.is_empty() {
            // If no provider info available, show all providers
            return provider_recs;
        }

        provider_recs
            .into_iter()
            .filter(move |r| {
                if let Recommendation::Provider { provider_type, .. } = r {
                    providers_that_can_run.contains(*provider_type)
                } else {
                    false
                }
            })
            .collect()
    }

    /// Filter model recommendations to only those that would be used by at least
    /// one selected capability (i.e., satisfy the capability's `ModelRequirement`).
    fn for_models<'a>(
        recommendations: &'a [Recommendation],
        selected_cap_types: &HashSet<String>,
    ) -> Vec<&'a Recommendation> {
        // Collect all model requirements from selected capabilities
        let all_requirements: Vec<ModelRequirement> = selected_cap_types
            .iter()
            .filter_map(|cap_type| CAPABILITY_REGISTRY.get(cap_type))
            .flat_map(|m| {
                m.dependencies
                    .iter()
                    .filter_map(|d| match d {
                        Dependency::Model {
                            requirement,
                            resolved_id: None,
                            ..
                        } => Some(requirement.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        if all_requirements.is_empty() {
            // No model requirements — show all model recommendations
            return recommendations
                .iter()
                .filter(|r| matches!(r, Recommendation::Model { .. }))
                .collect();
        }

        recommendations
            .iter()
            .filter(move |r| {
                if let Recommendation::Model { model_id, .. } = r {
                    // Look up the real catalog metadata (family/version/size
                    // alone can't tell us `supported_functions`, which most
                    // capability requirements actually key on).
                    match MODEL_REGISTRY.get(model_id) {
                        Some(md) => all_requirements
                            .iter()
                            .any(|req| admits_for_recommendation(req, &md)),
                        None => false,
                    }
                } else {
                    false
                }
            })
            .collect()
    }

    /// Filter capability recommendations to only those that could actually be
    /// used by at least one selected launcher (i.e., the launcher supports
    /// one of the capability's declared binding types). With no launchers
    /// selected, no capability has anywhere to bind, so none are shown.
    fn for_capabilities<'a>(
        discovery: &'a DiscoveryResult,
        selected_launcher_types: &HashSet<String>,
    ) -> Vec<&'a Recommendation> {
        if selected_launcher_types.is_empty() {
            return Vec::new();
        }

        let supported_types: HashSet<BindingType> = selected_launcher_types
            .iter()
            .filter_map(|lt| LAUNCHER_REGISTRY.get(lt))
            .flat_map(|m| m.supported_capabilities.clone().into_iter())
            .collect();

        discovery
            .recommendations
            .iter()
            .filter(move |r| {
                if let Recommendation::Capability {
                    capability_type, ..
                } = r
                {
                    if let Some(cap_meta) = CAPABILITY_REGISTRY.get(capability_type) {
                        return cap_meta
                            .supported_binding_types
                            .iter()
                            .any(|bt| supported_types.contains(bt));
                    }
                    // If we can't look up the capability, include it anyway
                    true
                } else {
                    false
                }
            })
            .collect()
    }
}

/*-- helpers -----------------------------------------------------------------*/

/// Compare semantic versions in descending order (higher versions first).
fn compare_versions_desc(a: &str, b: &str) -> std::cmp::Ordering {
    let parse_version =
        |v: &str| -> Vec<u32> { v.split('.').filter_map(|s| s.parse::<u32>().ok()).collect() };

    let va = parse_version(a);
    let vb = parse_version(b);

    for (a_part, b_part) in va.iter().zip(vb.iter()) {
        match b_part.cmp(a_part) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }

    vb.len().cmp(&va.len())
}

fn find_latest_version(models: &[(String, ModelMetadata)]) -> Option<&(String, ModelMetadata)> {
    // `compare_versions_desc` is a *reversed* comparator (higher version
    // sorts first when fed to `sort_by`/`sort_by_key`, i.e. it reports the
    // higher version as `Less`) -- so picking the "latest" requires
    // `min_by`, not `max_by`. `max_by` would select the lowest version in
    // the family instead.
    models
        .iter()
        .min_by(|(_, a), (_, b)| compare_versions_desc(&a.version, &b.version))
}

fn format_size(size: u64) -> String {
    match size {
        1_000_000_000.. => format!("{}B", size / 1_000_000_000),
        1_000_000.. => format!("{}M", size / 1_000_000),
        _ => size.to_string(),
    }
}

fn parse_size(size_str: &str) -> u64 {
    let size_str = size_str.trim();
    if let Some(num) = size_str.strip_suffix('B') {
        num.parse::<u64>().unwrap_or(0) * 1_000_000_000
    } else if let Some(num) = size_str.strip_suffix('M') {
        num.parse::<u64>().unwrap_or(0) * 1_000_000
    } else {
        size_str.parse::<u64>().unwrap_or(0)
    }
}

fn best_variant(
    model: &ModelMetadata,
    profile: &crate::utils::hardware::HardwareProfile,
) -> Option<(ModelVariant, ContextFit)> {
    best_variant_among(
        model.variants.iter(),
        model.context_length,
        &model.architecture,
        &model.native_dtype,
        profile,
    )
}

/// Rank an iterator of variants by hardware fit (best first).
/// `context_length` / `architecture` / `native_dtype` are used to compute
/// ContextFit for every variant; `profile` is the machine profile.
///
/// Returns the single best variant (highest fit rank, then smallest size as
/// tie-break) together with its ContextFit, or None if nothing fits.
fn best_variant_among<'a>(
    variants: impl Iterator<Item = &'a ModelVariant>,
    context_length: u64,
    architecture: &crate::models::ModelArchitecture,
    native_dtype: &str,
    profile: &crate::utils::hardware::HardwareProfile,
) -> Option<(ModelVariant, ContextFit)> {
    let fit_rank = |fit: &ContextFit| match fit {
        ContextFit::Full => 1,
        ContextFit::Partial(_) => 0,
        ContextFit::None => -1,
    };

    variants
        .map(|v| {
            let fit = crate::models::context_fit::estimate(
                context_length,
                architecture,
                native_dtype,
                v,
                profile,
            );
            (fit, v)
        })
        .filter(|(fit, _)| *fit != ContextFit::None)
        .max_by(|(fit_a, a), (fit_b, b)| {
            fit_rank(fit_a).cmp(&fit_rank(fit_b)).then_with(|| {
                a.size_gb
                    .partial_cmp(&b.size_gb)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        })
        .map(|(fit, v)| (v.clone(), fit))
}

/// Rank every variant that fits at all, best first, using the same
/// ordering `best_variant_among` picks its single winner with. Unlike
/// `best_variant_among`, returns the whole ranked list rather than just the
/// top entry -- needed by `resolve_model_set`, which must be able to fall
/// through to the next-best variant when the top-ranked one turns out to be
/// unusable for a reason unrelated to fit (e.g. no healthy provider can
/// actually serve its format).
fn rank_variants_among<'a>(
    variants: impl Iterator<Item = &'a ModelVariant>,
    context_length: u64,
    architecture: &crate::models::ModelArchitecture,
    native_dtype: &str,
    profile: &crate::utils::hardware::HardwareProfile,
) -> Vec<(ModelVariant, ContextFit)> {
    let fit_rank = |fit: &ContextFit| match fit {
        ContextFit::Full => 1,
        ContextFit::Partial(_) => 0,
        ContextFit::None => -1,
    };

    let mut ranked: Vec<(ModelVariant, ContextFit)> = variants
        .map(|v| {
            let fit = crate::models::context_fit::estimate(
                context_length,
                architecture,
                native_dtype,
                v,
                profile,
            );
            (v.clone(), fit)
        })
        .filter(|(_, fit)| *fit != ContextFit::None)
        .collect();

    ranked.sort_by(|(a, fit_a), (b, fit_b)| {
        fit_rank(fit_b).cmp(&fit_rank(fit_a)).then_with(|| {
            b.size_gb
                .partial_cmp(&a.size_gb)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });

    ranked
}

fn display_name(rec: &Recommendation) -> String {
    match rec {
        Recommendation::Provider { provider_name, .. } => provider_name.clone(),
        Recommendation::Model {
            family,
            version,
            size,
            ..
        } => format!("{family} {version} {size}"),
        Recommendation::Launcher { launcher_name, .. } => launcher_name.clone(),
        Recommendation::Capability {
            capability_name, ..
        } => capability_name.clone(),
    }
}

/// Extracts `(family, version, size)` from a model recommendation for
/// sorting purposes, without needing a full `ModelMetadata`.
fn model_sort_key(rec: &Recommendation) -> (&str, &str, u64) {
    match rec {
        Recommendation::Model {
            family,
            version,
            size,
            ..
        } => (family.as_str(), version.as_str(), parse_size(size)),
        _ => ("", "", 0),
    }
}

/// Sorts model recommendations by family, then version descending, then
/// size descending -- shared by every discovery pass that produces a list
/// of `Recommendation::Model`.
fn sort_model_recommendations(recommendations: &mut [Recommendation]) {
    recommendations.sort_by(|a, b| {
        let (a_family, a_version, a_size) = model_sort_key(a);
        let (b_family, b_version, b_size) = model_sort_key(b);
        a_family
            .cmp(b_family)
            .then_with(|| compare_versions_desc(a_version, b_version))
            .then_with(|| b_size.cmp(&a_size))
    });
}

/// Model functions that make a model "multi-modal" -- image or audio
/// input. A model reporting one of these can usually still `Chat`, but it's
/// not *intended* as a general chat model, so it shouldn't be recommended
/// for a capability that only asked for `Chat`/`ToolCalling`/etc.
const MULTIMODAL_FUNCTIONS: &[ModelFunction] = &[
    ModelFunction::ImageUnderstanding,
    ModelFunction::Transcription,
    ModelFunction::Translation,
    ModelFunction::SpeakerAttribution,
    ModelFunction::KeywordBiasing,
];

/// Given a `StringMatch`, return the sorted list of catalog model ids that
/// match. `Exact` resolves to at most one id; `Regex` resolves to every
/// matching registered id (sorted for determinism).
fn matching_catalog_ids(m: &recommended_config::StringMatch) -> Vec<String> {
    match m {
        recommended_config::StringMatch::Exact(id) => {
            if MODEL_REGISTRY.get(id).is_some() {
                vec![id.clone()]
            } else {
                vec![]
            }
        }
        recommended_config::StringMatch::Regex { regex: _pattern } => {
            let mut matches: Vec<String> = MODEL_REGISTRY
                .entries()
                .keys()
                .filter(|k| m.matches(k))
                .map(|s| s.to_string())
                .collect();
            matches.sort();
            matches
        }
    }
}

/// Whether the provider `provider_id` can run `variant`. A provider in
/// `ctx.config.providers` is constructed from its own config. Any other id is
/// taken as a provider type found by discovery and constructed with that
/// type's default config. Same lookup as `candidate_variants`.
fn provider_can_run(provider_id: &str, variant: &ModelVariant, ctx: &crate::AppContext) -> bool {
    let provider = match ctx.config.get_provider(provider_id) {
        Some(pc) => PROVIDER_REGISTRY.construct(&pc.provider_type, &pc.provider_id, &pc.config),
        None => {
            let default_config = PROVIDER_REGISTRY
                .default_config(provider_id)
                .unwrap_or_default();
            PROVIDER_REGISTRY.construct(provider_id, provider_id, &default_config)
        }
    };
    provider
        .ok()
        .is_some_and(|p| p.can_run_model(&variant.format, &variant.precision))
}

/// One capability's resolution against its `RecommendedCapability`: which
/// model+variant fills each of the capability's `Dependency::Model` config_key
/// slots. Only present when every *required* slot resolved.
struct ResolvedCapability {
    capability_type: String,
    /// config_key -> (model_id, variant)
    slots: HashMap<String, (String, ModelVariant)>,
}

/// What `setup --auto` passes to `configure_all`, built by
/// `SetupCommands::auto_selection`.
#[derive(Default)]
struct AutoSelection {
    capabilities: HashSet<String>,
    models: HashSet<String>,
    /// model id -> the variant its capability's recommendation resolved to
    variants: HashMap<String, ModelVariant>,
    providers: HashSet<String>,
    /// capability type -> config_key -> model id
    resolved_capability_models: HashMap<String, HashMap<String, String>>,
    /// launcher type -> the capability types its recommended config lists,
    /// whether or not they resolved
    recommended_capability_types_by_launcher: HashMap<String, HashSet<String>>,
}

/// Healthy provider types found among a discovery pass's recommendations.
/// Discovery skips configured providers, so none of them is in the result.
/// `select_capabilities` resolves recommended capabilities against this list;
/// `SetupCommands::auto_selection` adds the configured providers to it first.
fn healthy_provider_types(discovery: &DiscoveryResult) -> Vec<String> {
    discovery
        .recommendations
        .iter()
        .filter_map(|r| match r {
            Recommendation::Provider {
                provider_type,
                health_healthy: true,
                ..
            } => Some(provider_type.to_string()),
            _ => None,
        })
        .collect()
}

/// The real registry type for a launcher instance id. An escape-hatch- (or
/// previous-session-) configured launcher already has a live config entry
/// with its real type; an id with no entry yet is a not-yet-configured
/// recommended/auto-detected launcher, for which id IS the type (same
/// invariant the rest of this file already relies on).
fn resolved_launcher_type(ctx: &crate::AppContext, launcher_id: &str) -> String {
    ctx.config
        .get_launcher(launcher_id)
        .map(|l| l.launcher_type.clone())
        .unwrap_or_else(|| launcher_id.to_string())
}

/// The id `LauncherCommands::setup` just wrote to or updated in
/// `ctx.config.launchers`, found by diffing against a `before` snapshot
/// taken just before the call -- necessary because that wizard lets the
/// user free-type an instance name, so the resulting id isn't known ahead
/// of time. `None` when nothing changed (the user declined an overwrite
/// confirmation).
fn changed_launcher_id(
    before: &std::collections::HashMap<String, crate::config::LauncherConfig>,
    ctx: &crate::AppContext,
) -> Option<String> {
    ctx.config
        .launchers
        .iter()
        .find_map(|(id, cfg)| match before.get(id) {
            None => Some(id.clone()),
            Some(prev) => (serde_json::to_value(prev).ok() != serde_json::to_value(cfg).ok())
                .then(|| id.clone()),
        })
}

/// The id `ProviderCommands::setup` just wrote to or updated in
/// `ctx.config.providers`, found by diffing against a `before` snapshot
/// taken just before the call -- necessary because that wizard lets the
/// user free-type an instance name, so the resulting id isn't known ahead
/// of time. `None` when nothing changed (the user declined an overwrite
/// confirmation).
fn changed_provider_id(
    before: &std::collections::HashMap<String, crate::config::ProviderConfig>,
    ctx: &crate::AppContext,
) -> Option<String> {
    ctx.config
        .providers
        .iter()
        .find_map(|(id, cfg)| match before.get(id) {
            None => Some(id.clone()),
            Some(prev) => (serde_json::to_value(prev).ok() != serde_json::to_value(cfg).ok())
                .then(|| id.clone()),
        })
}

/// The id `CapabilityCommands::setup` just wrote to or updated in
/// `ctx.config.capabilities`, found by diffing against a `before` snapshot
/// taken just before the call -- necessary because that wizard lets the
/// user free-type an instance name, so the resulting id isn't known ahead
/// of time. `None` when nothing changed (the user declined an overwrite
/// confirmation).
fn changed_capability_id(
    before: &std::collections::HashMap<String, crate::config::CapabilityConfig>,
    ctx: &crate::AppContext,
) -> Option<String> {
    ctx.config
        .capabilities
        .iter()
        .find_map(|(id, cfg)| match before.get(id) {
            None => Some(id.clone()),
            Some(prev) => (serde_json::to_value(prev).ok() != serde_json::to_value(cfg).ok())
                .then(|| id.clone()),
        })
}

/// Resolve one capability's model slots against a `RecommendedCapability`.
/// Looks up the capability's `CAPABILITY_REGISTRY` metadata for its
/// `Dependency::Model` requirements, then tries each candidate in the
/// recommended set until one fully resolves.
fn resolve_capability(
    cap_type: &str,
    rec_cap: &recommended_config::RecommendedCapability,
    hardware: &crate::utils::hardware::HardwareProfile,
    provider_ids: &[String],
    ctx: &crate::AppContext,
) -> Option<ResolvedCapability> {
    let cap_meta = CAPABILITY_REGISTRY.get(cap_type)?;

    let mut result = ResolvedCapability {
        capability_type: cap_type.to_string(),
        slots: HashMap::new(),
    };

    for dep in &cap_meta.dependencies {
        let Dependency::Model {
            config_key,
            required,
            ..
        } = dep
        else {
            continue; // Provider / ExternalTool -- irrelevant for resolution
        };

        // Look up the recommended model set for this config_key slot
        let rec_model_set = match rec_cap.models.get(config_key.as_str()) {
            Some(set) => set,
            None => {
                if *required {
                    return None; // Required slot has no recommendation -> whole capability fails
                }
                continue; // Not required, leave unset
            }
        };

        // Resolve this slot against the recommended model set
        match resolve_model_set(rec_model_set, hardware, provider_ids, ctx) {
            Some((model_id, variant)) => {
                result.slots.insert(config_key.clone(), (model_id, variant));
            }
            None => {
                if *required {
                    return None;
                }
                // Not required, leave unset
            }
        }
    }

    Some(result)
}

/// Resolve a single `RecommendedModelSet` to a concrete `(model_id, variant)`
/// pair. Tries candidates in declared order (first-match-wins semantics);
/// within one candidate, tries every hardware-fitting variant that admits
/// (format, precision) allow-lists, best fit first, rather than stopping at
/// the single best-ranked one -- the best-ranked variant by fit/size alone
/// may turn out to be unrunnable by any healthy provider (e.g. a
/// `safetensors` variant when only Ollama is healthy) even though a
/// slightly-lower-ranked variant in the same allow-list would work fine.
fn resolve_model_set(
    set: &recommended_config::RecommendedModelSet,
    hardware: &crate::utils::hardware::HardwareProfile,
    provider_ids: &[String],
    ctx: &crate::AppContext,
) -> Option<(String, ModelVariant)> {
    for rec_model in &set.models {
        // Resolve the model match to actual catalog ids
        let catalog_ids = matching_catalog_ids(&rec_model.model);
        for model_id in catalog_ids {
            let md = match MODEL_REGISTRY.get(&model_id) {
                Some(md) => md,
                None => continue,
            };

            // Filter variants by the format and precision allow-lists (AND
            // between the two fields, OR within each; empty = wildcard),
            // then rank every one that fits, best first.
            let ranked = rank_variants_among(
                md.variants.iter().filter(|v| {
                    (rec_model.variant_formats.is_empty()
                        || rec_model
                            .variant_formats
                            .iter()
                            .any(|f| f.matches(&v.format)))
                        && (rec_model.variant_precisions.is_empty()
                            || rec_model
                                .variant_precisions
                                .iter()
                                .any(|p| p.matches(&v.precision)))
                }),
                md.context_length,
                &md.architecture,
                &md.native_dtype,
                hardware,
            );

            for (variant, fit) in ranked {
                // Compute effective context length from the fit
                let effective_context = match fit {
                    ContextFit::Full => md.context_length,
                    ContextFit::Partial(n) => n,
                    ContextFit::None => continue, // Already excluded by rank_variants_among
                };

                // Check min_context_length gate (against effective, not native)
                if effective_context < set.min_context_length.unwrap_or(0) {
                    continue;
                }

                // Check that at least one provider in `provider_ids` can run this variant
                if !provider_ids
                    .iter()
                    .any(|id| provider_can_run(id, &variant, ctx))
                {
                    continue;
                }

                return Some((model_id, variant));
            }
        }
    }

    None
}

/// The models the wizard binds to the capabilities in `selected_caps`, as
/// `capability type -> config_key -> model id`, for `configure_all`'s
/// `resolved_capability_models`. For each model slot, the model is the first
/// candidate listed in the slot's recommended `RecommendedModelSet.models`
/// whose catalog id is in `selected_models`. `launcher_types` are read in the
/// given order, and when two launchers' configs list the same capability, the
/// first launcher that fills a slot keeps it. A slot with no candidate in
/// `selected_models` is left out, so `configure_all` falls back to
/// `find_model_for_capability` for it.
fn recommended_models_for_selection(
    ctx: &crate::AppContext,
    launcher_types: &[String],
    selected_caps: &HashSet<String>,
    selected_models: &HashSet<String>,
) -> HashMap<String, HashMap<String, String>> {
    let mut result: HashMap<String, HashMap<String, String>> = HashMap::new();
    for launcher_type in launcher_types {
        let effective_caps = recommended_config::effective_capabilities(
            launcher_type,
            &recommended_config::BUILTIN_RECOMMENDED_CONFIGS,
            &ctx.config.recommended_configs,
        );
        for rec_cap in effective_caps {
            if !selected_caps.contains(&rec_cap.capability) {
                continue;
            }
            for (config_key, set) in &rec_cap.models {
                if result
                    .get(&rec_cap.capability)
                    .is_some_and(|slots| slots.contains_key(config_key))
                {
                    continue;
                }
                let kept = set
                    .models
                    .iter()
                    .flat_map(|m| matching_catalog_ids(&m.model))
                    .find(|id| selected_models.contains(id));
                if let Some(model_id) = kept {
                    result
                        .entry(rec_cap.capability.clone())
                        .or_default()
                        .insert(config_key.clone(), model_id);
                }
            }
        }
    }
    result
}

/// Whether `md` should be recommended for a capability declaring `req`.
/// Layers an extra rule on top of `ModelRequirement::admits_type`: if `req`
/// doesn't itself ask for any multi-modal function, a multi-modal model is
/// excluded even though it technically satisfies the requirement's function
/// list (e.g. a vision model supports `Chat` too, but a plain agent-model
/// binding isn't looking for a vision model).
fn admits_for_recommendation(req: &ModelRequirement, md: &ModelMetadata) -> bool {
    if !req.admits_type(md) {
        return false;
    }
    let req_wants_multimodal = req
        .supported_functions
        .iter()
        .any(|f| MULTIMODAL_FUNCTIONS.contains(f));
    req_wants_multimodal
        || !md
            .supported_functions
            .iter()
            .any(|f| MULTIMODAL_FUNCTIONS.contains(f))
}

/*-- SetupCommands -----------------------------------------------------------*/

pub struct SetupCommands;

impl SetupCommands {
    /// Entry point for `granite-cli setup`.
    /// `pull`: `Some(true)` => pull without prompting, `Some(false)` => skip
    /// pull, `None` => prompt in interactive mode / no pull in auto mode.
    pub async fn run(ctx: &mut crate::AppContext, auto: bool, pull: Option<bool>) -> Result<()> {
        if auto {
            Self::run_auto(ctx, pull).await
        } else {
            Self::run_wizard(ctx, pull).await
        }
    }

    /// Run the interactive wizard.
    async fn run_wizard(ctx: &mut crate::AppContext, pull: Option<bool>) -> Result<()> {
        let ui = &*ctx.ui;
        ui.info("=== granite-cli Setup Wizard ===\n");
        ui.info("Discovering available components...\n");

        // Detected once and threaded through the selection phases (not just
        // discovery) so `select_capabilities` can resolve recommended
        // capabilities against the same hardware profile discovery used,
        // rather than re-detecting (and potentially disagreeing).
        let hardware = detect_hardware();
        let discovery = Discover::run_with_hardware(ctx, &hardware).await;

        if discovery.recommendations.is_empty()
            && discovery.configured_provider_ids.is_empty()
            && discovery.configured_model_ids.is_empty()
            && discovery.configured_launcher_ids.is_empty()
            && discovery.configured_capability_ids.is_empty()
        {
            ui.info(
                "Nothing to configure. All components are either not available or already set up.",
            );
            return Ok(());
        }

        // Phase 1: Launchers selection (show all detected launchers)
        let selected_launchers = Self::select_launchers(ctx, &discovery).await?;

        // Phase 2: Capabilities selection (filtered by what the selected
        // launchers can actually bind, pre-selected only where a
        // recommendation actually resolves on this hardware)
        let selected_caps =
            Self::select_capabilities(ctx, &discovery, &selected_launchers, &hardware).await?;

        // Phase 3: Models selection (filtered by capability requirements)
        let selected_models =
            Self::select_models(ctx, &discovery, &selected_caps, &selected_launchers).await?;

        // Phase 4: Providers selection (only healthy, filtered by model compatibility)
        let selected_providers = Self::select_providers(ctx, &discovery, &selected_models).await?;

        // Phase 4.5: Variant selection (limited to formats the selected
        // providers can actually run, with a VRAM estimate at full context)
        let selected_variants =
            Self::select_variants(ctx, &discovery, &selected_models, &selected_providers).await?;

        // Phase 5: Configuration. Same per-launcher recommended-capability
        // tracking as `run_auto_with_hardware`, so a capability recommended
        // for one selected launcher doesn't also land on another selected
        // launcher just because both happen to support its binding type.
        // Keys are the RESOLVED type (matching what configure_all's enable-loop
        // looks up via `ctx.config.get_launcher(launcher_id).map(|l| l.launcher_type)`).
        let recommended_capability_types_by_launcher: HashMap<String, HashSet<String>> =
            selected_launchers
                .iter()
                .map(|launcher_id| {
                    let launcher_type = resolved_launcher_type(ctx, launcher_id);
                    let types = recommended_config::effective_capabilities(
                        &launcher_type,
                        &recommended_config::BUILTIN_RECOMMENDED_CONFIGS,
                        &ctx.config.recommended_configs,
                    )
                    .into_iter()
                    .map(|c| c.capability)
                    .collect();
                    (launcher_type, types)
                })
                .collect();
        // Bind each selected capability to the model its recommended config
        // lists for it, among the models the user kept in `select_models`.
        let mut launcher_types: Vec<String> = recommended_capability_types_by_launcher
            .keys()
            .cloned()
            .collect();
        launcher_types.sort();
        let resolved_capability_models = recommended_models_for_selection(
            ctx,
            &launcher_types,
            &selected_caps,
            &selected_models,
        );
        Self::configure_all(
            ctx,
            &discovery,
            &selected_caps,
            &selected_launchers,
            &selected_providers,
            &selected_models,
            &selected_variants,
            &resolved_capability_models,
            &recommended_capability_types_by_launcher,
        )
        .await?;

        // Phase 6: Pull (optional)
        match pull {
            Some(false) => {}
            Some(true) => Self::do_pull(ctx, &selected_models).await?,
            None => Self::prompt_pull(ctx, &selected_models).await?,
        }

        // Phase 7: Summary
        Self::print_summary(
            ctx,
            &selected_caps,
            &selected_launchers,
            &selected_providers,
            &selected_models,
        );

        Ok(())
    }

    /// Run auto mode — detect, configure everything with defaults.
    async fn run_auto(ctx: &mut crate::AppContext, pull: Option<bool>) -> Result<()> {
        Self::run_auto_with_hardware(ctx, &detect_hardware(), pull).await
    }

    /// Hardware-aware variant of `run_auto` for testability.
    async fn run_auto_with_hardware(
        ctx: &mut crate::AppContext,
        hardware: &crate::utils::hardware::HardwareProfile,
        pull: Option<bool>,
    ) -> Result<()> {
        let ui = &*ctx.ui;
        ui.info("=== granite-cli Auto Setup ===\n");
        ui.info("Auto-detecting and configuring all available components...\n");

        let discovery = Discover::run_with_hardware(ctx, hardware).await;

        if discovery.recommendations.is_empty() {
            ui.info("No components available to configure.");
            return Ok(());
        }

        // Auto-select everything that's recommended, following the same
        // Launchers → Capabilities → Models → Providers dependency chain as
        // the interactive wizard.
        let selected_launchers: HashSet<String> = discovery
            .recommendations
            .iter()
            .filter_map(|r| match r {
                Recommendation::Launcher {
                    launcher_type,
                    binary_path: Some(_),
                    ..
                } => Some(launcher_type.clone()),
                _ => None,
            })
            .collect();

        // Compute healthy provider types from discovery recommendations
        let healthy_provider_types = healthy_provider_types(&discovery);

        let selection =
            Self::auto_selection(ctx, &selected_launchers, &healthy_provider_types, hardware);

        // --auto is non-interactive, so there's no prompt for variant
        // selection -- `configure_all` falls back to discovery's
        // hardware-fit `best_variant` for every model.
        Self::configure_all(
            ctx,
            &discovery,
            &selection.capabilities,
            &selected_launchers,
            &selection.providers,
            &selection.models,
            &selection.variants,
            &selection.resolved_capability_models,
            &selection.recommended_capability_types_by_launcher,
        )
        .await?;

        // Only pull if explicitly requested via --pull; never prompt in auto mode.
        if pull == Some(true) {
            Self::do_pull(ctx, &selection.models).await?;
        }
        Self::print_summary(
            ctx,
            &selection.capabilities,
            &selected_launchers,
            &selection.providers,
            &selection.models,
        );

        Ok(())
    }

    /// Resolves the recommended config of each launcher in `launchers`
    /// against `hardware`, and returns what `run_auto_with_hardware` passes to
    /// `configure_all`. Models are resolved against `healthy_provider_types`
    /// and the providers already in `ctx.config.providers`. A capability in a
    /// launcher's config is resolved only when the launcher supports one of
    /// the capability's binding types, and only when `ctx.config.capabilities`
    /// has no entry with the capability type as its id.
    fn auto_selection(
        ctx: &crate::AppContext,
        launchers: &HashSet<String>,
        healthy_provider_types: &[String],
        hardware: &HardwareProfile,
    ) -> AutoSelection {
        let ui = &*ctx.ui;
        let mut selection = AutoSelection::default();

        // Discovery health-checks only providers that are not configured, so
        // `healthy_provider_types` never contains a configured provider.
        // Configured providers are added here, without a health check.
        let mut provider_ids: Vec<String> = healthy_provider_types.to_vec();
        let mut configured_provider_ids: Vec<String> =
            ctx.config.providers.keys().cloned().collect();
        configured_provider_ids.sort();
        for id in configured_provider_ids {
            if !provider_ids.contains(&id) {
                provider_ids.push(id);
            }
        }

        // Iterate launchers in sorted order for determinism, resolving
        // capabilities via the recommended config system.
        let mut sorted_launchers: Vec<String> = launchers.iter().cloned().collect();
        sorted_launchers.sort();

        for launcher_type in &sorted_launchers {
            let effective_caps = recommended_config::effective_capabilities(
                launcher_type,
                &recommended_config::BUILTIN_RECOMMENDED_CONFIGS,
                &ctx.config.recommended_configs,
            );
            // Recorded before the binding check below: `configure_all`
            // reads it to decide which launchers a capability is
            // recommended for.
            selection.recommended_capability_types_by_launcher.insert(
                launcher_type.clone(),
                effective_caps
                    .iter()
                    .map(|c| c.capability.clone())
                    .collect(),
            );
            let Some(launcher_meta) = LAUNCHER_REGISTRY.get(launcher_type) else {
                continue;
            };

            for rec_cap in &effective_caps {
                // `configure_all` enables a capability only on launchers
                // that support one of its binding types. Resolving one this
                // launcher cannot bind would configure the capability and
                // its model without enabling them anywhere, e.g.
                // `agent-model` from the wildcard config for `bob`, which
                // supports only `BindingType::Mcp`.
                let bindable =
                    CAPABILITY_REGISTRY
                        .get(&rec_cap.capability)
                        .is_some_and(|cap_meta| {
                            cap_meta
                                .supported_binding_types
                                .iter()
                                .any(|bt| launcher_meta.supported_capabilities.contains(bt))
                        });
                if !bindable {
                    continue;
                }
                // `configure_all` does not overwrite a configured capability,
                // so resolving it again would only configure a model that
                // the capability does not use.
                if ctx.config.get_capability(&rec_cap.capability).is_some() {
                    continue;
                }
                if let Some(resolved) =
                    resolve_capability(&rec_cap.capability, rec_cap, hardware, &provider_ids, ctx)
                {
                    selection
                        .capabilities
                        .insert(resolved.capability_type.clone());
                    for (config_key, (model_id, variant)) in resolved.slots {
                        // Check for conflict from an earlier launcher in the
                        // sorted iteration
                        if let Some(cap_slots) = selection
                            .resolved_capability_models
                            .get(&resolved.capability_type)
                            && let Some(existing_model) = cap_slots.get(&config_key)
                            && existing_model != &model_id
                        {
                            ui.warn(&format!(
                                "Capability '{}' config_key '{}' has a conflicting \
                                 recommendation between launchers '{}': keeping '{}', \
                                 ignoring '{}'.",
                                resolved.capability_type,
                                config_key,
                                launcher_type,
                                existing_model,
                                model_id
                            ));
                            continue;
                        }
                        selection
                            .resolved_capability_models
                            .entry(resolved.capability_type.clone())
                            .or_default()
                            .insert(config_key.clone(), model_id.clone());
                        selection.models.insert(model_id.clone());
                        selection.variants.insert(model_id, variant);
                    }
                }
            }
        }

        // Select every healthy provider type and every configured provider.
        // After writing the new providers to config, `configure_all` uses
        // `find_compatible_provider` to pick, for each model, a provider that
        // can run its variant. It does not write configured providers again.
        selection.providers = provider_ids.into_iter().collect();

        selection
    }

    /*-- Selection phases ----------------------------------------------------*/

    async fn select_capabilities(
        ctx: &mut crate::AppContext,
        discovery: &DiscoveryResult,
        selected_launchers: &HashSet<String>,
        hardware: &crate::utils::hardware::HardwareProfile,
    ) -> Result<HashSet<String>> {
        let ui = ctx.ui.clone(); // owned Arc<dyn Ui>: doesn't borrow `ctx`, so it stays usable across the later `&mut ctx` calls into CapabilityCommands::setup

        // Resolve every launcher id to its real registry type (an escape-hatch
        // or previous-session-configured launcher has a live config entry with
        // its type; a not-yet-configured recommended launcher's id IS its type).
        let resolved_launcher_types: HashSet<String> = selected_launchers
            .iter()
            .map(|id| resolved_launcher_type(ctx, id))
            .collect();

        // Get the base set from Revaluator (launcher-compatibility filter),
        // using resolved types so a custom-id launcher doesn't hit the wildcard.
        let all_caps: Vec<_> = Revaluator::for_capabilities(discovery, &resolved_launcher_types)
            .into_iter()
            .filter_map(|r| match r {
                Recommendation::Capability {
                    capability_type,
                    capability_name,
                } => Some((capability_type.clone(), capability_name.clone())),
                _ => None,
            })
            .collect();

        // Every capability type named by some selected launcher's effective
        // config, whether or not it actually resolves on this machine --
        // used only to decide which capabilities are shown at all (a
        // launcher's curated intent), never to decide their checkbox
        // default.
        let effective_caps_by_launcher: Vec<recommended_config::RecommendedCapability> =
            resolved_launcher_types
                .iter()
                .flat_map(|launcher_type| {
                    recommended_config::effective_capabilities(
                        launcher_type,
                        &recommended_config::BUILTIN_RECOMMENDED_CONFIGS,
                        &ctx.config.recommended_configs,
                    )
                })
                .collect();
        let declared_cap_types: HashSet<String> = effective_caps_by_launcher
            .iter()
            .map(|rec_cap| rec_cap.capability.clone())
            .collect();

        // Subset of the above that actually resolves to a concrete model
        // right now -- same resolution `run_auto_with_hardware` performs, so
        // a capability naming a model too large for this machine (or one no
        // healthy provider can serve) is never pre-selected, even though its
        // launcher's config still lists it.
        let healthy_provider_types = healthy_provider_types(discovery);
        let resolvable_cap_types: HashSet<String> = effective_caps_by_launcher
            .iter()
            .filter(|rec_cap| {
                resolve_capability(
                    &rec_cap.capability,
                    rec_cap,
                    hardware,
                    &healthy_provider_types,
                    ctx,
                )
                .is_some()
            })
            .map(|rec_cap| rec_cap.capability.clone())
            .collect();

        let caps: Vec<_> = if declared_cap_types.is_empty() {
            // No recommendations -- show all capabilities (original behavior)
            all_caps
        } else {
            all_caps
                .into_iter()
                .filter(|(cap_type, _)| declared_cap_types.contains(cap_type))
                .collect()
        };
        // NOTE: no early return here when caps.is_empty() -- the escape hatch
        // below is exactly what a user needs when nothing was auto-detected.

        let mut manual: Vec<String> = Vec::new(); // ids configured via the escape hatch this run

        loop {
            let mut items: Vec<String> = caps
                .iter()
                .map(|(id, name)| format!("{id} — {name}"))
                .collect();
            let mut defaults = if declared_cap_types.is_empty() {
                vec![true; items.len()]
            } else {
                caps.iter()
                    .map(|(cap_type, _)| resolvable_cap_types.contains(cap_type))
                    .collect()
            };
            for id in &manual {
                items.push(format!("{id} — manually configured"));
                defaults.push(true); // pre-checked: the user just configured it
            }
            if caps.is_empty() && manual.is_empty() {
                ui.info("No capabilities available for the selected launchers.");
            }
            items.push(Self::CONFIGURE_DIFFERENT_CAPABILITY_LABEL.to_string());
            defaults.push(false);
            let escape_idx = items.len() - 1;

            let selected =
                ui.multi_select("Select capabilities to configure", &items, &defaults)?;
            let chose_different = selected.contains(&escape_idx);
            let result: HashSet<String> = selected
                .iter()
                .filter(|&&i| i != escape_idx)
                .map(|&i| {
                    if i < caps.len() {
                        caps[i].0.clone()
                    } else {
                        manual[i - caps.len()].clone()
                    }
                })
                .collect();

            if !chose_different {
                return Ok(result);
            }

            let mut type_ids: Vec<String> = CAPABILITY_REGISTRY
                .entries()
                .keys()
                .map(|k| k.to_string())
                .collect();
            type_ids.sort();
            let type_items: Vec<String> = type_ids
                .iter()
                .map(|id| {
                    format!(
                        "{id} — {}",
                        CAPABILITY_REGISTRY
                            .get(id)
                            .map(|m| m.name.clone())
                            .unwrap_or_default()
                    )
                })
                .collect();
            let idx = ui.select("Configure which capability type?", &type_items, 0)?;
            let chosen_type = type_ids[idx].clone();

            let before: HashMap<String, crate::config::CapabilityConfig> =
                ctx.config.capabilities.clone();
            CapabilityCommands::setup(ctx, &chosen_type, None).await?;
            if let Some(new_id) = changed_capability_id(&before, ctx)
                && !manual.contains(&new_id)
                && !caps.iter().any(|(id, ..)| id == &new_id)
            {
                manual.push(new_id);
            }
            // loop again, re-rendering with `manual` folded in
        }
    }

    /// Labels for the extra rows appended to the default lists that hand off to
    /// manual selection.
    const CONFIGURE_DIFFERENT_CAPABILITY_LABEL: &'static str =
        "→ Configure a different capability…";
    const CONFIGURE_DIFFERENT_LAUNCHER_LABEL: &'static str = "→ Configure a different launcher…";
    const CONFIGURE_DIFFERENT_PROVIDER_LABEL: &'static str = "→ Configure a different provider…";
    const CONFIGURE_DIFFERENT_MODELS_LABEL: &'static str = "→ Configure a different models…";

    async fn select_launchers(
        ctx: &mut crate::AppContext,
        discovery: &DiscoveryResult,
    ) -> Result<HashSet<String>> {
        let ui = ctx.ui.clone(); // owned Arc<dyn Ui>: doesn't borrow `ctx`, so it stays usable across the later `&mut ctx` calls into LauncherCommands::setup

        let filtered: Vec<(String, String, String)> =
            Revaluator::for_launchers(discovery, &HashSet::new())
                .into_iter()
                .filter_map(|r| match r {
                    Recommendation::Launcher {
                        launcher_type,
                        launcher_name,
                        binary_path: Some(binary_path),
                    } => Some((
                        launcher_type.clone(),
                        launcher_name.clone(),
                        binary_path.clone(),
                    )),
                    _ => None,
                })
                .collect();
        // NOTE: no early return here when filtered.is_empty() -- the escape hatch
        // below is exactly what a user needs when nothing was auto-detected.

        let mut manual: Vec<String> = Vec::new(); // ids configured via the escape hatch this run

        loop {
            let mut items: Vec<String> = filtered
                .iter()
                .map(|(id, name, path)| format!("{id} — {name} ({path})"))
                .collect();
            let mut defaults = vec![false; items.len()];
            for id in &manual {
                items.push(format!("{id} — manually configured"));
                defaults.push(true); // pre-checked: the user just configured it
            }
            if filtered.is_empty() && manual.is_empty() {
                ui.info("No launchers detected on this system.");
            }
            items.push(Self::CONFIGURE_DIFFERENT_LAUNCHER_LABEL.to_string());
            defaults.push(false);
            let escape_idx = items.len() - 1;

            let selected = ui.multi_select("Select launchers to configure", &items, &defaults)?;
            let chose_different = selected.contains(&escape_idx);
            let result: HashSet<String> = selected
                .iter()
                .filter(|&&i| i != escape_idx)
                .map(|&i| {
                    if i < filtered.len() {
                        filtered[i].0.clone()
                    } else {
                        manual[i - filtered.len()].clone()
                    }
                })
                .collect();

            if !chose_different {
                return Ok(result);
            }

            let mut type_ids: Vec<String> = LAUNCHER_REGISTRY
                .entries()
                .keys()
                .map(|k| k.to_string())
                .collect();
            type_ids.sort();
            let type_items: Vec<String> = type_ids
                .iter()
                .map(|id| {
                    format!(
                        "{id} — {}",
                        LAUNCHER_REGISTRY
                            .get(id)
                            .map(|m| m.name.clone())
                            .unwrap_or_default()
                    )
                })
                .collect();
            let idx = ui.select("Configure which launcher type?", &type_items, 0)?;
            let chosen_type = type_ids[idx].clone();

            let before: HashMap<String, crate::config::LauncherConfig> =
                ctx.config.launchers.clone();
            LauncherCommands::setup(ctx, &chosen_type, None).await?;
            if let Some(new_id) = changed_launcher_id(&before, ctx)
                && !manual.contains(&new_id)
                && !filtered.iter().any(|(id, ..)| id == &new_id)
            {
                manual.push(new_id);
            }
            // loop again, re-rendering with `manual` folded in
        }
    }

    async fn select_providers(
        ctx: &mut crate::AppContext,
        discovery: &DiscoveryResult,
        selected_models: &HashSet<String>,
    ) -> Result<HashSet<String>> {
        let ui = ctx.ui.clone(); // owned Arc<dyn Ui>: doesn't borrow `ctx`, so it stays usable across the later `&mut ctx` calls into ProviderCommands::setup

        let filtered: Vec<_> = Revaluator::for_providers(discovery, selected_models, ctx)
            .into_iter()
            .filter_map(|r| match r {
                Recommendation::Provider {
                    provider_type,
                    provider_name,
                    health_healthy,
                    health_error,
                } if *health_healthy => Some((
                    provider_type.to_string(),
                    provider_name,
                    health_error.clone(),
                )),
                _ => None,
            })
            .collect();
        // NOTE: no early return here when filtered.is_empty() -- the escape hatch
        // below is exactly what a user needs when no providers are healthy.

        let mut manual: Vec<String> = Vec::new(); // ids configured via the escape hatch this run

        loop {
            let mut items: Vec<String> = filtered
                .iter()
                .map(|(id, name, error)| {
                    let status = if error.is_none() || error.as_ref().is_some_and(|e| e.is_empty())
                    {
                        "healthy".to_string()
                    } else if let Some(e) = &error {
                        format!("healthy ({e})")
                    } else {
                        "healthy".to_string()
                    };
                    format!("{id} — {name} ({status})")
                })
                .collect();
            let mut defaults = vec![false; items.len()];
            for id in &manual {
                items.push(format!("{id} — manually configured"));
                defaults.push(true); // pre-checked: the user just configured it
            }
            if filtered.is_empty() && manual.is_empty() {
                ui.info("No healthy providers available to configure.");
            }
            items.push(Self::CONFIGURE_DIFFERENT_PROVIDER_LABEL.to_string());
            defaults.push(false);
            let escape_idx = items.len() - 1;

            let selected = ui.multi_select("Select providers to configure", &items, &defaults)?;
            let chose_different = selected.contains(&escape_idx);
            let result: HashSet<String> = selected
                .iter()
                .filter(|&&i| i != escape_idx)
                .map(|&i| {
                    if i < filtered.len() {
                        filtered[i].0.clone()
                    } else {
                        manual[i - filtered.len()].clone()
                    }
                })
                .collect();

            if !chose_different {
                return Ok(result);
            }

            let mut type_ids: Vec<String> = PROVIDER_REGISTRY
                .entries()
                .keys()
                .map(|k| k.to_string())
                .collect();
            type_ids.sort();
            let type_items: Vec<String> = type_ids
                .iter()
                .map(|id| {
                    format!(
                        "{id} — {}",
                        PROVIDER_REGISTRY
                            .get(id)
                            .map(|m| m.name.clone())
                            .unwrap_or_default()
                    )
                })
                .collect();
            let idx = ui.select("Configure which provider type?", &type_items, 0)?;
            let chosen_type = type_ids[idx].clone();

            let before: HashMap<String, crate::config::ProviderConfig> =
                ctx.config.providers.clone();
            ProviderCommands::setup(ctx, &chosen_type, None).await?;
            if let Some(new_id) = changed_provider_id(&before, ctx)
                && !manual.contains(&new_id)
                && !filtered.iter().any(|(id, ..)| id == &new_id)
            {
                manual.push(new_id);
            }
            // loop again, re-rendering with `manual` folded in
        }
    }

    /// Phase 4.5: let the user pick a specific variant for each selected
    /// model, once both capabilities and providers are known. Options are
    /// limited to variants at least one selected provider can actually run,
    /// each annotated with an estimated VRAM/RAM footprint at the model's
    /// full configured context length. Models with only one compatible
    /// variant are auto-selected without prompting.
    async fn select_variants(
        ctx: &mut crate::AppContext,
        discovery: &DiscoveryResult,
        selected_models: &HashSet<String>,
        selected_providers: &HashSet<String>,
    ) -> Result<HashMap<String, ModelVariant>> {
        let mut chosen = HashMap::new();

        for model_id in selected_models {
            let Some(md) = MODEL_REGISTRY.get(model_id) else {
                continue;
            };

            let candidates = Self::candidate_variants(&md, selected_providers, ctx);
            if candidates.is_empty() {
                ctx.ui.warn(&format!(
                    "No selected provider can run any variant of '{model_id}'; leaving its variant unset."
                ));
                continue;
            }

            if candidates.len() == 1 {
                let (variant, gb) = &candidates[0];
                ctx.ui.info(&format!(
                    "Only one compatible variant for '{model_id}': {} — selected automatically.",
                    Self::format_variant_option(variant, *gb, md.context_length)
                ));
                chosen.insert(model_id.clone(), variant.clone());
                continue;
            }

            let recommended = discovery.recommendations.iter().find_map(|r| match r {
                Recommendation::Model {
                    model_id: rec_id,
                    best_variant,
                    ..
                } if rec_id == model_id => Some(best_variant.clone()),
                _ => None,
            });

            let default_idx = recommended
                .as_ref()
                .and_then(|rv| {
                    candidates.iter().position(|(v, _)| {
                        v.format.eq_ignore_ascii_case(&rv.format)
                            && v.precision.eq_ignore_ascii_case(&rv.precision)
                    })
                })
                .unwrap_or(0);

            let items: Vec<String> = candidates
                .iter()
                .map(|(v, gb)| Self::format_variant_option(v, *gb, md.context_length))
                .collect();

            let idx = ctx.ui.select(
                &format!("Select variant for {model_id}"),
                &items,
                default_idx,
            )?;
            chosen.insert(model_id.clone(), candidates[idx].0.clone());
        }

        Ok(chosen)
    }

    /// Variants of `md` that at least one selected provider can run, paired
    /// with their estimated required memory (GB) at `md.context_length`
    /// (i.e. full context). Prefers the real configured provider when one
    /// already exists in `ctx.config` (as is the case for an
    /// escape-hatch-configured provider, since its config was already written
    /// by the time `select_variants` runs), and falls back to constructing
    /// with registry defaults for not-yet-configured ids.
    fn candidate_variants(
        md: &ModelMetadata,
        selected_providers: &HashSet<String>,
        ctx: &crate::AppContext,
    ) -> Vec<(ModelVariant, f64)> {
        let providers: Vec<Box<dyn Provider>> = selected_providers
            .iter()
            .filter_map(|pid| {
                if let Some(pc) = ctx.config.get_provider(pid) {
                    PROVIDER_REGISTRY
                        .construct(&pc.provider_type, &pc.provider_id, &pc.config)
                        .ok()
                } else {
                    let default_config = PROVIDER_REGISTRY.default_config(pid).unwrap_or_default();
                    PROVIDER_REGISTRY.construct(pid, pid, &default_config).ok()
                }
            })
            .collect();

        md.variants
            .iter()
            .filter(|v| {
                providers
                    .iter()
                    .any(|p| p.can_run_model(&v.format, &v.precision))
            })
            .map(|v| {
                let gb = crate::models::required_gb(
                    &md.architecture,
                    v,
                    &md.native_dtype,
                    md.context_length,
                );
                (v.clone(), gb)
            })
            .collect()
    }

    fn format_variant_option(
        variant: &ModelVariant,
        required_gb: f64,
        context_length: u64,
    ) -> String {
        match variant.size_gb {
            Some(size) => format!(
                "{} / {} — {:.1} GB file, ~{:.1} GB VRAM @ full context ({context_length} tokens)",
                variant.format, variant.precision, size, required_gb
            ),
            None => format!(
                "{} / {} — ~{:.1} GB VRAM @ full context ({context_length} tokens)",
                variant.format, variant.precision, required_gb
            ),
        }
    }

    fn format_model_option(id: &str, size: &str, fit: ContextFit, providers: &[String]) -> String {
        let providers_str = if providers.is_empty() {
            "none".to_string()
        } else {
            providers.join(", ")
        };
        format!("{id} — {size} — Fit: {fit} ({providers_str})")
    }

    fn model_options(recs: Vec<&Recommendation>) -> Vec<(String, String, ContextFit, Vec<String>)> {
        recs.into_iter()
            .filter_map(|r| match r {
                Recommendation::Model {
                    model_id,
                    size,
                    context_fit,
                    can_run_by,
                    ..
                } => Some((
                    model_id.clone(),
                    size.clone(),
                    *context_fit,
                    can_run_by.clone(),
                )),
                _ => None,
            })
            .collect()
    }

    async fn select_models(
        ctx: &mut crate::AppContext,
        discovery: &DiscoveryResult,
        selected_caps: &HashSet<String>,
        selected_launchers: &HashSet<String>,
    ) -> Result<HashSet<String>> {
        let ui = &*ctx.ui;

        // Resolve every launcher id to its real registry type (an escape-hatch
        // or previous-session-configured launcher has a live config entry with
        // its type; a not-yet-configured recommended launcher's id IS its type).
        let resolved_launcher_types: HashSet<String> = selected_launchers
            .iter()
            .map(|id| resolved_launcher_type(ctx, id))
            .collect();

        // Filter the Revaluator output to only include models recommended by
        // the effective capabilities for the selected launchers.
        let mut recommended_ids: HashSet<String> = HashSet::new();
        for launcher_type in &resolved_launcher_types {
            for rec_cap in recommended_config::effective_capabilities(
                launcher_type,
                &recommended_config::BUILTIN_RECOMMENDED_CONFIGS,
                &ctx.config.recommended_configs,
            ) {
                if selected_caps.contains(&rec_cap.capability) {
                    for set in rec_cap.models.values() {
                        for m in &set.models {
                            for id in matching_catalog_ids(&m.model) {
                                recommended_ids.insert(id);
                            }
                        }
                    }
                }
            }
        }

        let filtered = if recommended_ids.is_empty() {
            // No recommendations available -- show all models (original
            // behavior), from the deduped one-per-family list.
            Self::model_options(Revaluator::for_models(
                &discovery.recommendations,
                selected_caps,
            ))
        } else {
            // `discovery.recommendations` keeps only the single largest
            // fully-fitting size per model family, so a recommendation
            // naming a smaller sibling (e.g. a 3b model for a lightweight
            // capability when an 8b of the same family also fits) would
            // never appear there at all. Source from the full candidate
            // pool instead -- every recommended id gets a chance to show up
            // -- then intersect with recommended_ids as before.
            Self::model_options(
                Revaluator::for_models(&discovery.all_model_candidates, selected_caps)
                    .into_iter()
                    .filter(|r| {
                        if let Recommendation::Model { model_id, .. } = r {
                            recommended_ids.contains(model_id)
                        } else {
                            false
                        }
                    })
                    .collect::<Vec<_>>(),
            )
        };

        let mut chosen: HashSet<String> = HashSet::new();
        let mut configure_different = filtered.is_empty();

        if filtered.is_empty() {
            ui.info("No models fully fit your hardware for the selected capabilities.");
        } else {
            let escape_hatch_idx = filtered.len();
            let mut items: Vec<String> = filtered
                .iter()
                .map(|(id, size, fit, providers)| {
                    Self::format_model_option(id, size, *fit, providers)
                })
                .collect();
            items.push(Self::CONFIGURE_DIFFERENT_MODELS_LABEL.to_string());

            let mut defaults = vec![true; filtered.len()];
            defaults.push(false);

            let selected = ui.multi_select("Select models to configure", &items, &defaults)?;

            configure_different = selected.contains(&escape_hatch_idx);
            chosen = selected
                .into_iter()
                .filter(|&i| i < escape_hatch_idx)
                .map(|i| filtered[i].0.clone())
                .collect();
        }

        if configure_different {
            let manual = Self::select_models_manually(ctx, discovery, selected_caps).await?;
            chosen.extend(manual);
        }

        Ok(chosen)
    }

    /// Escape hatch from `select_models`: lets the user pick directly from
    /// every model that satisfies the selected capabilities' requirements,
    /// regardless of family/version deduplication or hardware fit (so long
    /// as it fits at least partially) -- with each option's fit value shown,
    /// including partial fits `discover_models` excludes from the default
    /// recommendation.
    async fn select_models_manually(
        ctx: &mut crate::AppContext,
        discovery: &DiscoveryResult,
        selected_caps: &HashSet<String>,
    ) -> Result<HashSet<String>> {
        let ui = &*ctx.ui;

        let filtered = Self::model_options(Revaluator::for_models(
            &discovery.all_model_candidates,
            selected_caps,
        ));

        if filtered.is_empty() {
            ui.info("No candidate models satisfy the selected capabilities.");
            return Ok(HashSet::new());
        }

        let items: Vec<String> = filtered
            .iter()
            .map(|(id, size, fit, providers)| Self::format_model_option(id, size, *fit, providers))
            .collect();
        let defaults = vec![false; items.len()];

        let selected = ui.multi_select("Choose models directly", &items, &defaults)?;

        Ok(selected
            .into_iter()
            .map(|i| filtered[i].0.clone())
            .collect())
    }

    /*-- Configuration phase -------------------------------------------------*/

    #[allow(clippy::too_many_arguments)]
    async fn configure_all(
        ctx: &mut crate::AppContext,
        discovery: &DiscoveryResult,
        selected_caps: &HashSet<String>,
        selected_launchers: &HashSet<String>,
        selected_providers: &HashSet<String>,
        selected_models: &HashSet<String>,
        selected_variants: &HashMap<String, ModelVariant>,
        resolved_capability_models: &HashMap<String, HashMap<String, String>>,
        // launcher_type -> capability_types that launcher's own effective
        // recommended config lists. A capability_type absent from every
        // launcher's set here has no curated opinion anywhere, so the old
        // purely-binding-type-based enable behavior still applies to it
        // (preserves e.g. the wizard's manual-override capabilities); a
        // capability_type present for *some* launcher but not this one must
        // NOT be enabled here even if the binding type matches.
        recommended_capability_types_by_launcher: &HashMap<String, HashSet<String>>,
    ) -> Result<()> {
        let ui = &*ctx.ui;

        // Configure providers first
        for provider_id in selected_providers {
            // An id already configured (e.g. via a previous session or an
            // escape-hatch wizard invoked earlier in this same run) must be
            // left alone rather than clobbered with a freshly-built default.
            if ctx.config.get_provider(provider_id).is_some() {
                continue;
            }
            ui.info(&format!("\nConfiguring provider: {provider_id}..."));
            let default_config = PROVIDER_REGISTRY
                .default_config(provider_id)
                .unwrap_or_default();

            let provider_config = crate::config::ProviderConfig {
                provider_id: provider_id.clone(),
                provider_type: provider_id.to_string(),
                config: default_config,
            };

            if ctx
                .config
                .insert_provider(provider_id, provider_config)
                .is_err()
            {
                ui.warn(&format!(
                    "Failed to save provider config for '{provider_id}'"
                ));
            }
        }

        // Configure launchers
        for launcher_id in selected_launchers {
            // An id already configured (e.g. via a previous session or an
            // escape-hatch wizard invoked earlier in this same run) must be
            // left alone rather than clobbered with a freshly-built default.
            if ctx.config.get_launcher(launcher_id).is_some() {
                continue;
            }
            ui.info(&format!("\nConfiguring launcher: {launcher_id}..."));
            let default_config = LAUNCHER_REGISTRY
                .default_config(launcher_id)
                .unwrap_or_default();

            let launcher_config = crate::config::LauncherConfig {
                launcher_id: launcher_id.to_string(),
                launcher_type: launcher_id.to_string(),
                enabled_capabilities: Vec::new(),
                config: default_config,
            };

            if ctx
                .config
                .insert_launcher(launcher_id, launcher_config)
                .is_err()
            {
                ui.warn(&format!(
                    "Failed to save launcher config for '{launcher_id}'"
                ));
            }
        }

        // Configure models. Providers were just configured above, so
        // `ctx.config` already has real entries for every id in
        // `selected_providers` -- look one up and construct it live to
        // check actual format/precision compatibility, rather than relying
        // on discovery's `can_run_by` (which only reflects providers that
        // were *already* configured before this wizard run, and so is
        // always empty on a first-time setup).
        for model_id in selected_models {
            // A model that is already configured (e.g. by a previous session)
            // keeps its provider and variant, as the provider, launcher and
            // capability loops do for their entries.
            if ctx.config.get_model(model_id).is_some() {
                continue;
            }
            ui.info(&format!("\nConfiguring model: {model_id}..."));

            // Prefer the variant the user picked in the variant-selection
            // phase; fall back to discovery's hardware-fit recommendation
            // (e.g. in --auto mode, where that phase never runs).
            let chosen_variant = selected_variants.get(model_id).cloned().or_else(|| {
                discovery.recommendations.iter().find_map(|r| match r {
                    Recommendation::Model {
                        model_id: rec_id,
                        best_variant,
                        ..
                    } if rec_id == model_id => Some(best_variant.clone()),
                    _ => None,
                })
            });

            let (provider_id, variant) = match &chosen_variant {
                Some(v) => (
                    Self::find_compatible_provider(v, selected_providers, ctx)
                        .or_else(|| selected_providers.iter().next().cloned()),
                    Some(format!("{}/{}", v.format, v.precision)),
                ),
                None => (selected_providers.iter().next().cloned(), None),
            };
            if provider_id.is_none() {
                let err = format!("Failed to find provider for {model_id}/{chosen_variant:#?}");
                ui.warn(&err);
                anyhow::bail!(err);
            }

            let model_config = crate::config::ModelConfig {
                model_id: model_id.clone(),
                model_type: model_id.clone(),
                config: serde_json::json!({}),
                provider_id: provider_id.unwrap(),
                variant,
            };

            if ctx.config.insert_model(model_id, model_config).is_err() {
                ui.warn(&format!("Failed to save model config for '{model_id}'"));
            }
        }

        // Configure capabilities
        for cap_type in selected_caps {
            let dependencies = CAPABILITY_REGISTRY
                .get(cap_type)
                .map(|meta| meta.dependencies)
                .unwrap_or_default();

            // `CapabilitySource` skips a capability whose required model slot
            // is empty, with a warning, so such a capability is not written.
            let cap_cfg = crate::config::CapabilityConfig {
                capability_id: cap_type.to_string(),
                capability_type: cap_type.to_string(),
                config: CAPABILITY_REGISTRY
                    .default_config(cap_type)
                    .unwrap_or_default(),
            };
            let Some(cap_model_ids) = Self::capability_model_ids(
                cap_type,
                &dependencies,
                &cap_cfg,
                resolved_capability_models,
                selected_models,
            ) else {
                ui.warn(&format!(
                    "Skipping '{cap_type}': no compatible model available."
                ));
                continue;
            };

            // An id already configured (e.g. via a previous session or an
            // escape-hatch wizard invoked earlier in this same run) must be
            // left alone rather than clobbered with a freshly-built default.
            if ctx.config.get_capability(cap_type).is_some() {
                continue;
            }
            ui.info(&format!("\nConfiguring capability: {cap_type}..."));

            let mut config = CAPABILITY_REGISTRY
                .default_config(cap_type)
                .unwrap_or_default();

            // Set each resolved model dependency slot
            for (config_key, model_id) in &cap_model_ids {
                config[config_key.as_str()] = model_id.as_str().into();
            }

            let capability_config = crate::config::CapabilityConfig {
                capability_id: cap_type.clone(),
                capability_type: cap_type.clone(),
                config,
            };

            if ctx
                .config
                .insert_capability(cap_type, capability_config)
                .is_err()
            {
                ui.warn(&format!(
                    "Failed to save capability config for '{cap_type}'"
                ));
            }
        }

        // Every capability_type recommended for *some* selected launcher --
        // used below to tell "not recommended for this launcher because no
        // one has an opinion on it" (old, purely binding-type-based
        // behavior still applies) apart from "not recommended for this
        // launcher because it's recommended for a *different* one instead"
        // (must NOT be enabled here, even though the binding type matches --
        // this is exactly the issue #129 follow-up bug: `agent-model` being
        // a valid recommendation for e.g. `goose` must not make it land on
        // `claude` too, just because claude's launcher metadata also
        // supports the `AgentModel` binding).
        let recommended_anywhere: HashSet<&String> = recommended_capability_types_by_launcher
            .values()
            .flatten()
            .collect();

        // Enable every configured capability on each configured launcher
        // that supports it AND either recommends it specifically or has no
        // curated opinion on it at all. Must run after both loops above,
        // since it needs a live `CapabilitySource` built from the
        // now-populated capability configs.
        let capability_source = crate::capabilities::CapabilitySource::from_config(&ctx.config);
        for launcher_id in selected_launchers {
            let Some(launcher_type) = ctx
                .config
                .get_launcher(launcher_id)
                .map(|l| l.launcher_type.clone())
            else {
                continue;
            };
            let Some(launcher_meta) = LAUNCHER_REGISTRY.get(&launcher_type) else {
                continue;
            };
            let recommended_for_this = recommended_capability_types_by_launcher
                .get(&launcher_type)
                .cloned()
                .unwrap_or_default();

            let mut enabled: Vec<String> = capability_source
                .instances()
                .into_iter()
                .filter(|(id, cap)| {
                    let binding_ok = cap
                        .binding_types()
                        .iter()
                        .any(|bt| launcher_meta.supported_capabilities.contains(bt));
                    binding_ok
                        && (recommended_for_this.contains(id) || !recommended_anywhere.contains(id))
                })
                .map(|(id, _)| id)
                .collect();
            enabled.sort();

            if ctx
                .config
                .update_launcher(launcher_id, |l| l.enabled_capabilities = enabled.clone())
                .is_err()
            {
                ui.warn(&format!(
                    "Failed to enable capabilities for launcher '{launcher_id}'"
                ));
            }
        }

        Ok(())
    }

    /// Picks a selected provider that can actually run `variant`, by
    /// constructing each one and checking `can_run_model`. Assumes providers
    /// have already been written to `ctx.config` (as `configure_all` does,
    /// before configuring models).
    fn find_compatible_provider(
        variant: &ModelVariant,
        selected_providers: &HashSet<String>,
        ctx: &crate::AppContext,
    ) -> Option<String> {
        selected_providers
            .iter()
            .find(|pid| {
                ctx.config
                    .get_provider(pid)
                    .and_then(|pc| {
                        PROVIDER_REGISTRY
                            .construct(&pc.provider_type, &pc.provider_id, &pc.config)
                            .ok()
                    })
                    .is_some_and(|p| p.can_run_model(&variant.format, &variant.precision))
            })
            .cloned()
    }

    /// The model id for each model slot in `dependencies`, keyed by the
    /// slot's `config_key`. A slot gets the model `resolved_capability_models`
    /// lists for `cap_type` and that `config_key`, otherwise the result of
    /// `find_model_for_capability`. An optional slot with no model is left
    /// out. Returns `None` when any required slot has no model.
    fn capability_model_ids(
        cap_type: &str,
        dependencies: &[Dependency],
        cap_cfg: &crate::config::CapabilityConfig,
        resolved_capability_models: &HashMap<String, HashMap<String, String>>,
        selected_models: &HashSet<String>,
    ) -> Option<HashMap<String, String>> {
        // Base model slots from the capability's config (e.g. "model_id" →
        // "my-model") using the shared utility.
        let mut model_ids = crate::utils::capability_model_ids(cap_cfg, dependencies);

        // Overlay with resolved_capability_models (the wizard/auto-selection
        // output), then fall back to find_model_for_capability when a slot
        // is still missing.
        for dep in dependencies {
            let Dependency::Model {
                config_key,
                required,
                ..
            } = dep
            else {
                continue;
            };
            let model_id = resolved_capability_models
                .get(cap_type)
                .and_then(|slots| slots.get(config_key.as_str()).cloned())
                .or_else(|| Self::find_model_for_capability(cap_type, selected_models));
            match model_id {
                Some(id) => {
                    model_ids.insert(config_key.clone(), id);
                }
                None if *required => return None,
                None => {}
            }
        }
        Some(model_ids)
    }

    /// Find a model_id from selected_models that satisfies a capability's model
    /// requirement. Returns the first matching model id in sorted order, or
    /// None if no model matches.
    fn find_model_for_capability(
        cap_type: &str,
        selected_models: &HashSet<String>,
    ) -> Option<String> {
        let cap_meta = CAPABILITY_REGISTRY.get(cap_type)?;

        let all_requirements: Vec<ModelRequirement> = cap_meta
            .dependencies
            .iter()
            .filter_map(|d| match d {
                Dependency::Model {
                    requirement,
                    resolved_id: None,
                    ..
                } => Some(requirement.clone()),
                _ => None,
            })
            .collect();

        if all_requirements.is_empty() {
            return None;
        }

        // Check each selected model's real catalog metadata to see if it
        // satisfies any requirement. Sorted, so the result does not depend on
        // the iteration order of `selected_models`, which changes between runs.
        let mut candidates: Vec<&String> = selected_models.iter().collect();
        candidates.sort();
        candidates
            .into_iter()
            .find(|model_id| {
                MODEL_REGISTRY.get(model_id).is_some_and(|md| {
                    all_requirements
                        .iter()
                        .any(|req| admits_for_recommendation(req, &md))
                })
            })
            .cloned()
    }

    /*-- Pull phase ----------------------------------------------------------*/

    /// Pull all pullable models without prompting (used when `--pull` is set).
    async fn do_pull(ctx: &mut crate::AppContext, selected_models: &HashSet<String>) -> Result<()> {
        let ui = ctx.ui.clone();
        let pullable = Self::pullable_models(ctx, selected_models);
        if pullable.is_empty() {
            alog_channel!(MessageLevel::Debug2, "No pullable models");
            return Ok(());
        }
        alog_channel!(MessageLevel::Debug2, "Pullable models: {:#?}", pullable);
        for (model_id, _provider_id, _provider_type) in &pullable {
            ui.info(&format!("Pulling {model_id}..."));
            if let Err(e) = ModelCommands::pull(ctx, model_id).await {
                alog_channel!(MessageLevel::Warning, "Pull failed for '{model_id}': {e}");
            }
        }
        Ok(())
    }

    /// Prompt the user whether to pull, then pull if confirmed (used when
    /// neither `--pull` nor `--skip-pull` is set in interactive mode).
    async fn prompt_pull(
        ctx: &mut crate::AppContext,
        selected_models: &HashSet<String>,
    ) -> Result<()> {
        // Cloned (not `&*ctx.ui`) so it doesn't hold a borrow of `ctx` --
        // `ModelCommands::pull` below needs `&mut ctx` for the whole struct.
        let ui = ctx.ui.clone();

        let pullable = Self::pullable_models(ctx, selected_models);

        if pullable.is_empty() {
            alog_channel!(MessageLevel::Debug2, "No pullable models");
            return Ok(());
        }
        alog_channel!(MessageLevel::Debug2, "Pullable models: {:#?}", pullable);

        let items: Vec<String> = pullable
            .iter()
            .map(|(model, provider, ptype)| format!("{provider} → {model} ({ptype})"))
            .collect();

        let pull_now = ui.confirm("\n→ Pull model weights now?", !items.is_empty())?;

        if pull_now {
            for (model_id, _provider_id, _provider_type) in &pullable {
                ui.info(&format!("Pulling {model_id}..."));
                // `ModelCommands::pull` already reports success/failure to
                // `ctx.ui` itself; just keep going on error rather than
                // aborting the rest of the pull phase over one model.
                if let Err(e) = ModelCommands::pull(ctx, model_id).await {
                    alog_channel!(MessageLevel::Warning, "Pull failed for '{model_id}': {e}");
                }
            }
        }

        Ok(())
    }

    /// Returns the list of configured models that can be pulled, as
    /// `(model_id, provider_id, provider_type)` triples.
    fn pullable_models(
        ctx: &crate::AppContext,
        selected_models: &HashSet<String>,
    ) -> Vec<(String, String, String)> {
        selected_models
            .iter()
            .filter_map(|model_id| {
                ctx.config.get_model(model_id).and_then(|mc| {
                    ctx.config.get_provider(mc.provider_id.as_str()).map(|pc| {
                        (
                            model_id.clone(),
                            mc.provider_id.clone(),
                            pc.provider_type.clone(),
                        )
                    })
                })
            })
            .collect()
    }

    /*-- Summary phase -------------------------------------------------------*/

    fn print_summary(
        ctx: &crate::AppContext,
        selected_caps: &HashSet<String>,
        selected_launchers: &HashSet<String>,
        selected_providers: &HashSet<String>,
        selected_models: &HashSet<String>,
    ) {
        let ui = &*ctx.ui;

        ui.info("\n=== Setup Complete ===");
        ui.info(&format!(
            "Providers: {}",
            if selected_providers.is_empty() {
                "none".to_string()
            } else {
                selected_providers
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
        ui.info(&format!(
            "Models: {}",
            if selected_models.is_empty() {
                "none".to_string()
            } else {
                selected_models
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
        ui.info(&format!(
            "Launchers: {}",
            if selected_launchers.is_empty() {
                "none".to_string()
            } else {
                selected_launchers
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
        ui.info(&format!(
            "Capabilities: {}",
            if selected_caps.is_empty() {
                "none".to_string()
            } else {
                selected_caps.iter().cloned().collect::<Vec<_>>().join(", ")
            }
        ));

        ui.info("\nRun `granite-cli launcher list` to see configured launchers.");
        if !selected_launchers.is_empty() {
            let first_launcher = selected_launchers.iter().next().unwrap();
            ui.info(&format!(
                "Run `granite-cli launch {first_launcher}` to launch with Granite overlay.",
            ));
        }
    }
}

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ModelConfig, ProviderConfig};
    use crate::utils::ui::base::tests::CaptureUi;
    use std::sync::Arc;

    fn test_ctx() -> crate::AppContext {
        crate::AppContext {
            config: Config::default(),
            ui: Arc::new(CaptureUi::default()),
        }
    }

    /// A fixed hardware profile for discovery tests, in place of
    /// `detect_hardware()`'s result on whatever machine happens to run the
    /// test. Model-fit outcomes are a direct function of the hardware
    /// profile, so exercising the real detector here would make these tests
    /// pass or fail depending on the CI runner's actual RAM/VRAM rather than
    /// on the discovery logic under test.
    ///
    /// 32GB of usable memory (ram_gb / 2.0, no GPU) is calibrated against the
    /// real "Granite Language" 4.2 catalog entries: granite-4.2-8b's
    /// smallest GGUF variant fully fits at its full 131072-token context,
    /// while even granite-4.2-30b's smallest variant needs far more than
    /// that for full context and only ever partially fits.
    fn test_hardware_profile() -> HardwareProfile {
        HardwareProfile {
            os: "test".to_string(),
            cpu_cores: 8,
            cpu_arch: "test".to_string(),
            gpu_vendor: None,
            vram_gb: None,
            ram_gb: 64.0,
        }
    }

    async fn run_discovery(ctx: &crate::AppContext) -> DiscoveryResult {
        Discover::run_with_hardware(ctx, &test_hardware_profile()).await
    }

    fn ctx_with_provider(
        id: &str,
        provider_type: &str,
        config: serde_json::Value,
    ) -> crate::AppContext {
        let mut ctx = test_ctx();
        ctx.config.providers.insert(
            id.to_string(),
            ProviderConfig {
                provider_id: id.to_string(),
                provider_type: provider_type.to_string(),
                config,
            },
        );
        ctx
    }

    fn ctx_with_model(id: &str, provider_id: Option<&str>) -> crate::AppContext {
        let mut ctx = test_ctx();
        ctx.config.models.insert(
            id.to_string(),
            ModelConfig {
                model_id: id.to_string(),
                model_type: id.to_string(),
                config: serde_json::json!({}),
                provider_id: provider_id.unwrap_or("ollama").to_string(),
                variant: None,
            },
        );
        ctx
    }

    // -- discover_providers ----------------------------------------------------

    #[tokio::test]
    async fn discover_providers_skips_configured() {
        let ctx = ctx_with_provider("ollama", "ollama", serde_json::json!({}));
        let result = run_discovery(&ctx).await;
        assert!(
            result
                .configured_provider_ids
                .contains(&"ollama".to_string())
        );
    }

    #[tokio::test]
    async fn discover_providers_recommends_unconfigured() {
        let ctx = test_ctx();
        let result = run_discovery(&ctx).await;
        let provider_recs: Vec<_> = result
            .recommendations
            .iter()
            .filter(|r| matches!(r, Recommendation::Provider { .. }))
            .collect();
        // Should have at least some provider recommendations
        assert!(
            !provider_recs.is_empty(),
            "expected at least one provider recommendation"
        );
    }

    // -- discover_models -------------------------------------------------------

    #[tokio::test]
    async fn discover_models_groups_by_family() {
        let ctx = test_ctx();
        let result = run_discovery(&ctx).await;
        let model_recs: Vec<_> = result
            .recommendations
            .iter()
            .filter(|r| matches!(r, Recommendation::Model { .. }))
            .collect();
        // Should have at least one model recommendation
        assert!(
            !model_recs.is_empty(),
            "expected at least one model recommendation"
        );
    }

    #[tokio::test]
    async fn discover_models_recommendation_carries_real_registry_model_id() {
        let ctx = test_ctx();
        let result = run_discovery(&ctx).await;
        for rec in &result.recommendations {
            if let Recommendation::Model {
                model_id, family, ..
            } = rec
            {
                assert!(
                    MODEL_REGISTRY.get(model_id).is_some(),
                    "model_id '{model_id}' should be a real catalog key, not the family name"
                );
                assert_ne!(
                    model_id, family,
                    "model_id should be the specific model's catalog id, not its family"
                );
            }
        }
    }

    #[tokio::test]
    async fn discover_models_only_recommends_full_fit_and_picks_largest_full_fitting_size() {
        // Granite Language 4.2 ships three sizes (3b/8b/30b) at the same
        // version. The 30b only partially fits typical hardware -- it must
        // never be the default recommendation, and whichever size *is*
        // recommended must be a full fit.
        let ctx = test_ctx();
        let result = run_discovery(&ctx).await;

        let rec = result.recommendations.iter().find(
            |r| matches!(r, Recommendation::Model { family, .. } if family == "Granite Language"),
        );

        if let Some(Recommendation::Model {
            model_id,
            context_fit,
            ..
        }) = rec
        {
            assert_eq!(
                *context_fit,
                ContextFit::Full,
                "the default recommendation must fully fit, got {model_id} with {context_fit}"
            );
            assert_ne!(
                model_id, "granite-4.2-30b",
                "the 30b variant only partially fits on typical hardware and must not be auto-recommended"
            );
        }
        // If no size fully fits, no recommendation for the family is also
        // acceptable -- the important thing is a partial fit is never
        // silently promoted to the default recommendation.
    }

    #[tokio::test]
    async fn discover_all_model_candidates_includes_every_size_with_its_own_fit() {
        let ctx = test_ctx();
        let result = run_discovery(&ctx).await;

        let granite_4_2: Vec<_> = result
            .all_model_candidates
            .iter()
            .filter_map(|r| match r {
                Recommendation::Model {
                    model_id,
                    family,
                    version,
                    ..
                } if family == "Granite Language" && version == "4.2" => Some(model_id.as_str()),
                _ => None,
            })
            .collect();

        assert!(
            granite_4_2.contains(&"granite-4.2-8b"),
            "expected granite-4.2-8b among all-candidates, got {granite_4_2:?}"
        );
        assert!(
            granite_4_2.contains(&"granite-4.2-30b"),
            "the 30b size should still appear in the full candidate pool despite only partially fitting, got {granite_4_2:?}"
        );
    }

    #[tokio::test]
    async fn discover_models_skips_configured() {
        let ctx = ctx_with_model("granite-3.1-8b-instruct", Some("ollama"));
        let result = run_discovery(&ctx).await;
        assert!(
            result
                .configured_model_ids
                .contains(&"granite-3.1-8b-instruct".to_string())
        );
    }

    // -- discover_launchers ----------------------------------------------------

    #[tokio::test]
    async fn discover_launchers_skips_configured() {
        let mut ctx = test_ctx();
        ctx.config.launchers.insert(
            "claude".to_string(),
            crate::config::LauncherConfig {
                launcher_id: "claude".to_string(),
                launcher_type: "claude".to_string(),
                enabled_capabilities: vec![],
                config: serde_json::json!({}),
            },
        );
        let result = run_discovery(&ctx).await;
        assert!(
            result
                .configured_launcher_ids
                .contains(&"claude".to_string())
        );
    }

    #[tokio::test]
    async fn discover_launchers_recommends_unconfigured() {
        let ctx = test_ctx();
        let result = run_discovery(&ctx).await;
        let launcher_recs: Vec<_> = result
            .recommendations
            .iter()
            .filter(|r| matches!(r, Recommendation::Launcher { .. }))
            .collect();
        assert!(
            !launcher_recs.is_empty(),
            "expected at least one launcher recommendation"
        );
    }

    // -- discover_capabilities -------------------------------------------------

    #[tokio::test]
    async fn discover_capabilities_recommends_unconfigured() {
        let ctx = test_ctx();
        let result = run_discovery(&ctx).await;
        let cap_recs: Vec<_> = result
            .recommendations
            .iter()
            .filter(|r| matches!(r, Recommendation::Capability { .. }))
            .collect();
        assert!(
            !cap_recs.is_empty(),
            "expected at least one capability recommendation"
        );
    }

    // -- version comparison ----------------------------------------------------

    #[test]
    fn compare_versions_desc_simple() {
        assert_eq!(
            compare_versions_desc("3.1", "3.0"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_versions_desc("3.0", "3.1"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_versions_desc("3.1", "3.1"),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn compare_versions_desc_multi_part() {
        assert_eq!(
            compare_versions_desc("3.1.1", "3.1.0"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_versions_desc("3.1", "3.1.0"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn compare_versions_desc_major_difference() {
        assert_eq!(
            compare_versions_desc("4.0", "3.1"),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn find_latest_version_picks_the_highest_version_not_the_lowest() {
        // Regression test: `compare_versions_desc` is a reversed comparator
        // (by design, for descending display sorts), so `find_latest_version`
        // must pair it with `min_by`, not `max_by` -- using `max_by` silently
        // picked the *lowest* version in the family instead.
        fn md(version: &str) -> ModelMetadata {
            ModelMetadata {
                family: "Test Family".to_string(),
                version: version.to_string(),
                size: 0,
                context_length: 0,
                model_type: ModelType::Text,
                huggingface_repo: String::new(),
                native_dtype: String::new(),
                architecture: crate::models::ModelArchitecture {
                    num_hidden_layers: 0,
                    hidden_size: 0,
                    num_attention_heads: 0,
                    num_key_value_heads: 0,
                    head_dim: 0,
                    layer_types: vec![],
                },
                variants: vec![],
                description: None,
                tags: vec![],
                supported_functions: vec![],
            }
        }
        let models = vec![
            ("a".to_string(), md("4.0")),
            ("b".to_string(), md("3.1")),
            ("c".to_string(), md("4.1")),
            ("d".to_string(), md("3.3")),
        ];
        let (id, latest) = find_latest_version(&models).expect("non-empty input");
        assert_eq!(id, "c");
        assert_eq!(latest.version, "4.1");
    }

    // -- size helpers ----------------------------------------------------------

    #[test]
    fn format_size_billion() {
        assert_eq!(format_size(8_000_000_000), "8B");
    }

    #[test]
    fn format_size_million() {
        assert_eq!(format_size(2_000_000), "2M");
    }

    #[test]
    fn parse_size_billion() {
        assert_eq!(parse_size("8B"), 8_000_000_000);
    }

    #[test]
    fn parse_size_million() {
        assert_eq!(parse_size("2M"), 2_000_000);
    }

    // -- Revaluator ------------------------------------------------------------

    #[tokio::test]
    async fn revaluator_for_models_filters_by_capability_requirements() {
        let ctx = test_ctx();
        let discovery = run_discovery(&ctx).await;

        // agent-model requires Chat + ToolCalling support.
        let selected_caps: HashSet<String> = ["agent-model".to_string()].into_iter().collect();
        let filtered = Revaluator::for_models(&discovery.recommendations, &selected_caps);

        assert!(
            !filtered.is_empty(),
            "expected at least one granite model to satisfy agent-model's Chat+ToolCalling requirement"
        );
        // Every surviving recommendation's real catalog metadata must
        // actually admit the requirement -- this is the regression check for
        // the bug where a hand-rolled mock always reported empty
        // `supported_functions`, so nothing ever matched.
        for rec in &filtered {
            if let Recommendation::Model { model_id, .. } = rec {
                let md = MODEL_REGISTRY.get(model_id).expect("real catalog entry");
                assert!(
                    md.supported_functions
                        .contains(&crate::models::ModelFunction::Chat),
                    "{model_id} should support Chat"
                );
            }
        }
    }

    #[test]
    fn admits_for_recommendation_excludes_unrequested_multimodal_functions() {
        fn md_with_functions(functions: Vec<ModelFunction>) -> ModelMetadata {
            ModelMetadata {
                family: "Test".to_string(),
                version: "1.0".to_string(),
                size: 0,
                context_length: 8192,
                model_type: ModelType::Text,
                huggingface_repo: String::new(),
                native_dtype: String::new(),
                architecture: crate::models::ModelArchitecture {
                    num_hidden_layers: 0,
                    hidden_size: 0,
                    num_attention_heads: 0,
                    num_key_value_heads: 0,
                    head_dim: 0,
                    layer_types: vec![],
                },
                variants: vec![],
                description: None,
                tags: vec![],
                supported_functions: functions,
            }
        }

        let chat_only_req = ModelRequirement {
            supported_functions: vec![ModelFunction::Chat, ModelFunction::ToolCalling],
            ..Default::default()
        };
        let vision_req = ModelRequirement {
            supported_functions: vec![
                ModelFunction::Chat,
                ModelFunction::ToolCalling,
                ModelFunction::ImageUnderstanding,
            ],
            ..Default::default()
        };

        let text_model = md_with_functions(vec![ModelFunction::Chat, ModelFunction::ToolCalling]);
        let vision_model = md_with_functions(vec![
            ModelFunction::Chat,
            ModelFunction::ToolCalling,
            ModelFunction::ImageUnderstanding,
        ]);
        let speech_model =
            md_with_functions(vec![ModelFunction::Chat, ModelFunction::Transcription]);

        assert!(admits_for_recommendation(&chat_only_req, &text_model));
        assert!(
            !admits_for_recommendation(&chat_only_req, &vision_model),
            "a plain Chat/ToolCalling requirement should exclude a vision model even though it can chat"
        );
        assert!(
            !admits_for_recommendation(&chat_only_req, &speech_model),
            "a plain Chat/ToolCalling requirement should exclude a speech model even though it can chat"
        );
        // A requirement that explicitly wants vision should still admit it.
        assert!(admits_for_recommendation(&vision_req, &vision_model));
    }

    #[tokio::test]
    async fn revaluator_for_models_excludes_multimodal_for_plain_chat_capability() {
        let ctx = test_ctx();
        let discovery = run_discovery(&ctx).await;

        // agent-model only requires Chat + ToolCalling -- no multi-modal
        // function -- so vision/speech models must not show up even though
        // `all_model_candidates` (unlike the deduped default list) is
        // guaranteed to contain some.
        let selected_caps: HashSet<String> = ["agent-model".to_string()].into_iter().collect();
        let filtered = Revaluator::for_models(&discovery.all_model_candidates, &selected_caps);

        assert!(!filtered.is_empty());
        for rec in &filtered {
            if let Recommendation::Model { model_id, .. } = rec {
                let md = MODEL_REGISTRY.get(model_id).expect("real catalog entry");
                assert!(
                    !md.supported_functions
                        .iter()
                        .any(|f| MULTIMODAL_FUNCTIONS.contains(f)),
                    "{model_id} is multi-modal and should be excluded from a plain-chat capability's recommendations"
                );
            }
        }
    }

    #[tokio::test]
    async fn revaluator_for_models_still_includes_vision_for_vision_mcp() {
        let ctx = test_ctx();
        let discovery = run_discovery(&ctx).await;

        let selected_caps: HashSet<String> = ["vision-mcp".to_string()].into_iter().collect();
        let filtered = Revaluator::for_models(&discovery.all_model_candidates, &selected_caps);

        assert!(
            filtered.iter().any(|r| matches!(r, Recommendation::Model { model_id, .. } if model_id.contains("vision"))),
            "vision-mcp explicitly requires ImageUnderstanding, so vision models must still be recommended"
        );
    }

    #[tokio::test]
    async fn revaluator_for_models_with_no_requirements_returns_all() {
        let ctx = test_ctx();
        let discovery = run_discovery(&ctx).await;
        let filtered = Revaluator::for_models(&discovery.recommendations, &HashSet::new());
        let all_models: Vec<_> = discovery
            .recommendations
            .iter()
            .filter(|r| matches!(r, Recommendation::Model { .. }))
            .collect();
        assert_eq!(filtered.len(), all_models.len());
    }

    #[tokio::test]
    async fn revaluator_for_launchers_filters_by_binding_types() {
        let ctx = test_ctx();
        let discovery = run_discovery(&ctx).await;

        // agent-model needs the AgentModel binding, which bob does not support.
        let selected_caps: HashSet<String> = ["agent-model".to_string()].into_iter().collect();
        let filtered = Revaluator::for_launchers(&discovery, &selected_caps);
        assert!(
            !filtered
                .iter()
                .any(|r| matches!(r, Recommendation::Launcher { launcher_type, .. } if launcher_type == "bob")),
            "bob only supports Mcp and should be filtered out for agent-model"
        );

        // vision-mcp only needs the Mcp binding, which bob does support.
        let mcp_caps: HashSet<String> = ["vision-mcp".to_string()].into_iter().collect();
        let mcp_filtered = Revaluator::for_launchers(&discovery, &mcp_caps);
        assert!(
            mcp_filtered
                .iter()
                .any(|r| matches!(r, Recommendation::Launcher { launcher_type, .. } if launcher_type == "bob")),
            "bob supports Mcp and should be included for vision-mcp"
        );
    }

    #[tokio::test]
    async fn revaluator_for_capabilities_filters_by_selected_launchers() {
        let ctx = test_ctx();
        let discovery = run_discovery(&ctx).await;

        // bob supports both the Mcp and SubAgent bindings (the latter via its
        // in-process `pi`-backed delegate MCP server), so both vision-mcp
        // (Mcp) and the sub-agent family (SubAgent) should show -- but
        // nothing needing a binding bob doesn't support (e.g. a bare
        // AgentModel-only capability, if one existed).
        let bob_only: HashSet<String> = ["bob".to_string()].into_iter().collect();
        let filtered = Revaluator::for_capabilities(&discovery, &bob_only);
        let capability_types: HashSet<&str> = filtered
            .iter()
            .filter_map(|r| match r {
                Recommendation::Capability {
                    capability_type, ..
                } => Some(capability_type.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            capability_types,
            HashSet::from([
                "vision-mcp",
                "sub-agent",
                "sub-agent-code",
                "sub-agent-explore",
                "sub-agent-plan",
            ]),
            "with only bob selected, vision-mcp (Mcp) and the sub-agent family (SubAgent) should be recommended: {capability_types:?}"
        );
    }

    #[tokio::test]
    async fn revaluator_for_capabilities_with_no_launchers_returns_none() {
        let ctx = test_ctx();
        let discovery = run_discovery(&ctx).await;
        let filtered = Revaluator::for_capabilities(&discovery, &HashSet::new());
        assert!(filtered.is_empty());
    }

    // -- Variant selection -------------------------------------------------------

    fn model_recommendation(
        model_id: &str,
        md: &ModelMetadata,
        best_variant: ModelVariant,
    ) -> Recommendation {
        Recommendation::Model {
            model_id: model_id.to_string(),
            family: md.family.clone(),
            version: md.version.clone(),
            size: format_size(md.size),
            model_type: md.model_type.clone(),
            best_variant,
            context_fit: ContextFit::Full,
            can_run_by: vec![],
        }
    }

    #[test]
    fn candidate_variants_excludes_formats_no_selected_provider_supports() {
        let ctx = test_ctx();
        let md = MODEL_REGISTRY
            .get("granite-vision-4.1-4b")
            .expect("fixture model should exist in the catalog");
        let selected: HashSet<String> = ["lm-studio".to_string()].into_iter().collect();

        let candidates = SetupCommands::candidate_variants(&md, &selected, &ctx);

        assert!(
            !candidates.is_empty(),
            "lm-studio should be able to run at least one GGUF variant"
        );
        assert!(
            candidates
                .iter()
                .all(|(v, gb)| v.format.eq_ignore_ascii_case("gguf") && *gb > 0.0),
            "every candidate should be a GGUF variant with a positive VRAM estimate"
        );
        assert!(
            !candidates
                .iter()
                .any(|(v, _)| v.format.eq_ignore_ascii_case("safetensors")),
            "lm-studio cannot run safetensors, so it must not appear as a candidate"
        );
    }

    #[tokio::test]
    async fn select_variants_auto_selects_when_only_one_candidate_without_prompting() {
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext {
            config: Config::default(),
            ui: capture.clone(),
        };

        // granite-docling-258M-mlx has exactly one (safetensors) variant;
        // openai-compatible's default `can_run_model` accepts any format.
        let md = MODEL_REGISTRY
            .get("granite-docling-258M-mlx")
            .expect("fixture model should exist in the catalog");
        assert_eq!(
            md.variants.len(),
            1,
            "fixture assumption: exactly one variant"
        );

        let discovery = DiscoveryResult {
            recommendations: vec![model_recommendation(
                "granite-docling-258M-mlx",
                &md,
                md.variants[0].clone(),
            )],
            all_model_candidates: vec![],
            configured_provider_ids: vec![],
            configured_model_ids: vec![],
            configured_launcher_ids: vec![],
            configured_capability_ids: vec![],
        };
        let selected_models: HashSet<String> = ["granite-docling-258M-mlx".to_string()]
            .into_iter()
            .collect();
        let selected_providers: HashSet<String> =
            ["openai-compatible".to_string()].into_iter().collect();

        let chosen = SetupCommands::select_variants(
            &mut ctx,
            &discovery,
            &selected_models,
            &selected_providers,
        )
        .await
        .unwrap();

        let picked = chosen
            .get("granite-docling-258M-mlx")
            .expect("should have auto-selected the sole candidate");
        assert_eq!(picked.format, md.variants[0].format);
        assert_eq!(picked.precision, md.variants[0].precision);
        assert!(
            capture.select_prompts.borrow().is_empty(),
            "a model with only one compatible variant should not prompt"
        );
    }

    #[tokio::test]
    async fn select_variants_defaults_to_discoverys_best_variant() {
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext {
            config: Config::default(),
            ui: capture.clone(),
        };

        let md = MODEL_REGISTRY
            .get("granite-vision-4.1-4b")
            .expect("fixture model should exist in the catalog");
        let gguf_variants: Vec<ModelVariant> = md
            .variants
            .iter()
            .filter(|v| v.format.eq_ignore_ascii_case("gguf"))
            .cloned()
            .collect();
        assert!(
            gguf_variants.len() > 1,
            "fixture assumption: multiple GGUF variants, so a real choice is offered"
        );
        let recommended = gguf_variants[gguf_variants.len() / 2].clone();

        let discovery = DiscoveryResult {
            recommendations: vec![model_recommendation(
                "granite-vision-4.1-4b",
                &md,
                recommended.clone(),
            )],
            all_model_candidates: vec![],
            configured_provider_ids: vec![],
            configured_model_ids: vec![],
            configured_launcher_ids: vec![],
            configured_capability_ids: vec![],
        };
        let selected_models: HashSet<String> =
            ["granite-vision-4.1-4b".to_string()].into_iter().collect();
        let selected_providers: HashSet<String> = ["lm-studio".to_string()].into_iter().collect();

        // No canned select answer -- CaptureUi::select falls back to
        // whatever `default` it was passed, so this proves that default
        // index actually points at discovery's recommended variant.
        let chosen = SetupCommands::select_variants(
            &mut ctx,
            &discovery,
            &selected_models,
            &selected_providers,
        )
        .await
        .unwrap();

        let picked = chosen
            .get("granite-vision-4.1-4b")
            .expect("should have selected a variant");
        assert_eq!(picked.format, recommended.format);
        assert_eq!(picked.precision, recommended.precision);
        assert_eq!(capture.select_prompts.borrow().len(), 1);
    }

    #[tokio::test]
    async fn select_models_configure_different_models_surfaces_partial_fit_candidates() {
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext {
            config: Config::default(),
            ui: capture.clone(),
        };

        let discovery = run_discovery(&ctx).await;
        let selected_caps: HashSet<String> = ["agent-model".to_string()].into_iter().collect();

        // granite-4.2-30b only partially fits, so it must not be in the
        // default recommendation list, but must be reachable through
        // "choose different models".
        let default_ids: HashSet<&str> =
            Revaluator::for_models(&discovery.recommendations, &selected_caps)
                .into_iter()
                .filter_map(|r| match r {
                    Recommendation::Model { model_id, .. } => Some(model_id.as_str()),
                    _ => None,
                })
                .collect();
        assert!(
            !default_ids.contains("granite-4.2-30b"),
            "granite-4.2-30b only partially fits and must not be a default recommendation"
        );

        let manual_candidates =
            Revaluator::for_models(&discovery.all_model_candidates, &selected_caps);
        let thirty_b_idx = manual_candidates
            .iter()
            .position(|r| matches!(r, Recommendation::Model { model_id, .. } if model_id == "granite-4.2-30b"))
            .expect("granite-4.2-30b should be a manual candidate");

        // First multi_select (the default list): pick only the escape hatch
        // row (its index is the number of default items).
        let default_count = default_ids.len();
        capture
            .multi_select_answers
            .borrow_mut()
            .push_back(vec![default_count]);
        // Second multi_select (the manual list): pick granite-4.2-30b.
        capture
            .multi_select_answers
            .borrow_mut()
            .push_back(vec![thirty_b_idx]);

        let selected_launchers: HashSet<String> = HashSet::new();
        let chosen =
            SetupCommands::select_models(&mut ctx, &discovery, &selected_caps, &selected_launchers)
                .await
                .unwrap();

        assert_eq!(
            chosen,
            HashSet::from(["granite-4.2-30b".to_string()]),
            "should have picked exactly the manually-chosen partial-fit model"
        );
    }

    #[tokio::test]
    async fn select_models_surfaces_a_recommended_smaller_sibling_the_deduped_list_would_hide() {
        // Regression test for issue #129 (wizard path): claude.yaml
        // recommends granite-4.2-3b for sub-agent-explore.
        // `discovery.recommendations` (the deduped, one-per-family list
        // `select_models`'s base list used to be sourced from exclusively)
        // keeps only the single largest fully-fitting size per family --
        // granite-4.2-8b under `test_hardware_profile`, since it fully fits
        // -- so the recommended 3b never had a chance to appear as an
        // option at all, even though `recommended_ids` correctly named it.
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext {
            config: Config::default(),
            ui: capture.clone(),
        };
        let discovery = run_discovery(&ctx).await;
        // vision-mcp is included alongside sub-agent-explore so the
        // top-level list isn't empty even before the fix (its recommended
        // granite-vision-4.1-4b is its own family, so it survives the
        // dedup) -- this reproduces the user's exact report ("only
        // granite-4.2-8b and granite-vision-4.1-4b are shown"): the buggy
        // behavior silently substitutes the wrong sibling rather than
        // falling through to the "choose different models" escape hatch,
        // which a completely-empty list (single-capability case) would
        // mask.
        let selected_caps: HashSet<String> =
            ["sub-agent-explore".to_string(), "vision-mcp".to_string()]
                .into_iter()
                .collect();
        let selected_launchers: HashSet<String> = ["claude".to_string()].into_iter().collect();

        // First pass with no canned answer, just to inspect what's offered.
        let _ =
            SetupCommands::select_models(&mut ctx, &discovery, &selected_caps, &selected_launchers)
                .await
                .unwrap();
        let idx = {
            let prompts = capture.multi_select_prompts.borrow();
            assert_eq!(prompts.len(), 1, "expected exactly one multi_select prompt");
            let (_, items, defaults) = &prompts[0];
            assert!(
                items.iter().any(|i| i.starts_with("granite-vision-4.1-4b")),
                "granite-vision-4.1-4b (recommended for vision-mcp) should be offered: {items:?}"
            );
            let idx = items
                .iter()
                .position(|i| i.starts_with("granite-4.2-3b"))
                .expect(
                    "granite-4.2-3b must be offered even though the deduped default list \
                     would substitute 8b instead",
                );
            assert!(
                defaults[idx],
                "the recommended sub-agent-explore candidate should default to selected"
            );
            assert!(
                !items.iter().any(|i| i.starts_with("granite-4.2-8b")),
                "granite-4.2-8b is not recommended for either selected capability and must not \
                 appear: {items:?}"
            );
            idx
        };

        capture
            .multi_select_answers
            .borrow_mut()
            .push_back(vec![idx]);
        let chosen =
            SetupCommands::select_models(&mut ctx, &discovery, &selected_caps, &selected_launchers)
                .await
                .unwrap();
        assert_eq!(chosen, HashSet::from(["granite-4.2-3b".to_string()]));
    }

    // -- configure_all -----------------------------------------------------

    #[tokio::test]
    async fn configure_all_enables_only_capabilities_a_launcher_supports() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        let discovery = run_discovery(&ctx).await;

        // claude supports the AgentModel binding that "agent-model" uses;
        // bob supports only the Mcp binding, so it must not get it.
        let selected_caps: HashSet<String> = ["agent-model".to_string()].into_iter().collect();
        let selected_launchers: HashSet<String> = ["claude".to_string(), "bob".to_string()]
            .into_iter()
            .collect();
        let selected_providers: HashSet<String> = ["ollama".to_string()].into_iter().collect();
        let selected_models: HashSet<String> = ["granite-3.1-8b-instruct".to_string()]
            .into_iter()
            .collect();

        SetupCommands::configure_all(
            &mut ctx,
            &discovery,
            &selected_caps,
            &selected_launchers,
            &selected_providers,
            &selected_models,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();

        let claude_enabled = &ctx
            .config
            .get_launcher("claude")
            .unwrap()
            .enabled_capabilities;
        assert_eq!(
            claude_enabled,
            &vec!["agent-model".to_string()],
            "claude supports the AgentModel binding, so agent-model should be enabled"
        );

        let bob_enabled = &ctx.config.get_launcher("bob").unwrap().enabled_capabilities;
        assert!(
            bob_enabled.is_empty(),
            "bob only supports the Mcp binding, so agent-model must not be enabled"
        );
    }

    #[tokio::test]
    async fn configure_all_does_not_enable_a_capability_on_a_launcher_that_does_not_recommend_it() {
        // Regression test: a capability recommended for one launcher must
        // not be enabled on a *different* selected launcher just because
        // both happen to support the same binding type. Both "claude" and
        // "goose" support the AgentModel binding, but only "goose" is given
        // as recommending "agent-model" here -- "claude" must not get it,
        // even though the old purely-binding-type-based enable logic would
        // have put it there too.
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        let discovery = run_discovery(&ctx).await;

        let selected_caps: HashSet<String> = ["agent-model".to_string()].into_iter().collect();
        let selected_launchers: HashSet<String> = ["claude".to_string(), "goose".to_string()]
            .into_iter()
            .collect();
        let selected_providers: HashSet<String> = ["ollama".to_string()].into_iter().collect();
        let selected_models: HashSet<String> = ["granite-3.1-8b-instruct".to_string()]
            .into_iter()
            .collect();
        let recommended_capability_types_by_launcher: HashMap<String, HashSet<String>> = [(
            "goose".to_string(),
            ["agent-model".to_string()].into_iter().collect(),
        )]
        .into_iter()
        .collect();

        SetupCommands::configure_all(
            &mut ctx,
            &discovery,
            &selected_caps,
            &selected_launchers,
            &selected_providers,
            &selected_models,
            &HashMap::new(),
            &HashMap::new(),
            &recommended_capability_types_by_launcher,
        )
        .await
        .unwrap();

        let goose_enabled = &ctx
            .config
            .get_launcher("goose")
            .unwrap()
            .enabled_capabilities;
        assert_eq!(
            goose_enabled,
            &vec!["agent-model".to_string()],
            "goose recommends agent-model, so it should be enabled"
        );

        let claude_enabled = &ctx
            .config
            .get_launcher("claude")
            .unwrap()
            .enabled_capabilities;
        assert!(
            claude_enabled.is_empty(),
            "claude does not recommend agent-model (goose does), so it must not be enabled \
             on claude even though claude also supports the AgentModel binding: {claude_enabled:?}"
        );
    }

    // -- SetupCommands ---------------------------------------------------------

    #[tokio::test]
    async fn prompt_pull_actually_invokes_model_commands_pull() {
        // Regression test: `prompt_pull` used to just log "Pulling
        // {model}..." and do nothing -- the pull was never actually
        // triggered. Point the configured provider at a closed local port
        // so the real pull attempt fails fast (no network dependency), and
        // assert on the error message that only `ModelCommands::pull`
        // itself produces, proving it was actually called rather than the
        // old no-op.
        let capture = Arc::new(CaptureUi::default());
        capture.confirm_answers.borrow_mut().push_back(true);
        let mut ctx = crate::AppContext {
            config: Config::default(),
            ui: capture.clone(),
        };

        ctx.config.providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({ "base_url": "http://127.0.0.1:1" }),
            },
        );

        let model_id = "granite-3.1-8b-instruct";
        let md = MODEL_REGISTRY
            .get(model_id)
            .expect("fixture model should exist");
        let variant = md
            .variants
            .iter()
            .find(|v| v.format.eq_ignore_ascii_case("ollama"))
            .expect("fixture model should have an Ollama-format variant");
        ctx.config.models.insert(
            model_id.to_string(),
            ModelConfig {
                model_id: model_id.to_string(),
                model_type: model_id.to_string(),
                config: serde_json::json!({}),
                provider_id: "ollama".to_string(),
                variant: Some(format!("{}/{}", variant.format, variant.precision)),
            },
        );

        let selected_models: HashSet<String> = [model_id.to_string()].into_iter().collect();
        let result = SetupCommands::prompt_pull(&mut ctx, &selected_models).await;

        assert!(
            result.is_ok(),
            "prompt_pull should not propagate a per-model pull failure"
        );
        assert!(
            capture
                .errors
                .borrow()
                .iter()
                .any(|e| e.contains("Failed to pull model")),
            "expected ModelCommands::pull's own failure message, proving it was actually invoked; got: {:?}",
            capture.errors.borrow()
        );
    }

    #[tokio::test]
    async fn run_wizard_with_empty_config_shows_info() {
        let mut ctx = test_ctx();
        let _ = SetupCommands::run(&mut ctx, false, Some(false)).await;
        // Wizard should complete without error even with no recommendations
        // (it will show info messages)
    }

    #[tokio::test]
    async fn run_auto_with_no_recommendations_shows_info() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        let result = SetupCommands::run(&mut ctx, true, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn select_launchers_excludes_launchers_without_a_resolved_binary() {
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext {
            config: Config::default(),
            ui: capture.clone(),
        };

        let discovery = DiscoveryResult {
            recommendations: vec![
                Recommendation::Launcher {
                    launcher_type: "found".to_string(),
                    launcher_name: "Found Launcher".to_string(),
                    binary_path: Some("/usr/bin/found".to_string()),
                },
                Recommendation::Launcher {
                    launcher_type: "missing".to_string(),
                    launcher_name: "Missing Launcher".to_string(),
                    binary_path: None,
                },
            ],
            all_model_candidates: vec![],
            configured_provider_ids: vec![],
            configured_model_ids: vec![],
            configured_launcher_ids: vec![],
            configured_capability_ids: vec![],
        };

        SetupCommands::select_launchers(&mut ctx, &discovery)
            .await
            .unwrap();

        let prompts = capture.multi_select_prompts.borrow();
        let (_, items, _) = &prompts[0];
        assert_eq!(
            items.len(),
            2,
            "the missing binary should have been excluded; found + escape-hatch item"
        );
        assert!(items[0].contains("found"));
        assert!(items[1].contains("Configure a different launcher"));
    }

    #[tokio::test]
    async fn find_compatible_provider_rejects_format_mismatch() {
        let mut ctx = test_ctx();
        ctx.config.providers.insert(
            "lm-studio".to_string(),
            crate::config::ProviderConfig {
                provider_id: "lm-studio".to_string(),
                provider_type: "lm-studio".to_string(),
                config: PROVIDER_REGISTRY
                    .default_config("lm-studio")
                    .unwrap_or_default(),
            },
        );
        let selected: HashSet<String> = ["lm-studio".to_string()].into_iter().collect();

        let gguf_variant = ModelVariant {
            format: "GGUF".to_string(),
            precision: "Q4_K_M".to_string(),
            size_gb: Some(4.0),
            url: "https://example.com/model.gguf".to_string(),
        };
        assert_eq!(
            SetupCommands::find_compatible_provider(&gguf_variant, &selected, &ctx),
            Some("lm-studio".to_string())
        );

        let safetensors_variant = ModelVariant {
            format: "safetensors".to_string(),
            precision: "bfloat16".to_string(),
            size_gb: Some(4.0),
            url: "https://example.com/model".to_string(),
        };
        assert_eq!(
            SetupCommands::find_compatible_provider(&safetensors_variant, &selected, &ctx),
            None,
            "lm-studio cannot serve safetensors and should not be picked"
        );
    }

    // -- matching_catalog_ids ------------------------------------------------

    #[test]
    fn matching_catalog_ids_exact_existing() {
        let m = recommended_config::StringMatch::Exact("granite-3.1-8b-instruct".to_string());
        let ids = matching_catalog_ids(&m);
        assert_eq!(ids, vec!["granite-3.1-8b-instruct"]);
    }

    #[test]
    fn matching_catalog_ids_exact_nonexistent() {
        let m = recommended_config::StringMatch::Exact("nonexistent-model".to_string());
        let ids = matching_catalog_ids(&m);
        assert!(ids.is_empty());
    }

    #[test]
    fn matching_catalog_ids_regex_sorted() {
        let m = recommended_config::StringMatch::Regex {
            regex: "granite-3\\.1".to_string(),
        };
        let ids = matching_catalog_ids(&m);
        // Should contain granite-3.1 models, sorted
        assert!(!ids.is_empty());
        assert!(ids.windows(2).all(|w| w[0] <= w[1]));
        assert!(ids.iter().any(|id| id.contains("3.1")));
    }

    // -- resolve_model_set -----------------------------------------------------

    #[test]
    fn resolve_model_set_ordered_fallback() {
        // Build a hardware profile that can fit granite-4.2-8b but not 4.2-30b
        let hardware = HardwareProfile {
            os: "test".to_string(),
            cpu_cores: 8,
            cpu_arch: "test".to_string(),
            gpu_vendor: None,
            vram_gb: None,
            ram_gb: 32.0,
        };
        let ctx = test_ctx();
        let healthy: Vec<String> = ["ollama".to_string()].into_iter().collect();

        // Create a model set where the first candidate is 4.2-30b (won't fit),
        // second is 4.2-8b (will fit)
        let rec_30b = recommended_config::RecommendedModel {
            model: recommended_config::StringMatch::Exact("granite-4.2-30b".to_string()),
            variant_formats: vec![],
            variant_precisions: vec![],
        };
        let rec_8b = recommended_config::RecommendedModel {
            model: recommended_config::StringMatch::Exact("granite-4.2-8b".to_string()),
            variant_formats: vec![],
            variant_precisions: vec![],
        };
        let set = recommended_config::RecommendedModelSet {
            min_context_length: None,
            models: vec![rec_30b, rec_8b],
        };

        let result = resolve_model_set(&set, &hardware, &healthy, &ctx);
        // Should resolve to the 8b model (first that fits), not 30b
        assert!(
            result.is_some(),
            "should have resolved to 4.2-8b as fallback"
        );
        let (model_id, _) = result.unwrap();
        assert_eq!(model_id, "granite-4.2-8b");
    }

    #[test]
    fn resolve_model_set_variant_precision_filtering() {
        let hardware = HardwareProfile {
            os: "test".to_string(),
            cpu_cores: 8,
            cpu_arch: "test".to_string(),
            gpu_vendor: None,
            vram_gb: None,
            ram_gb: 64.0,
        };
        let ctx = test_ctx();
        let healthy: Vec<String> = ["ollama".to_string()].into_iter().collect();

        // granite-3.1-8b-instruct has many variants; only Q4_K_M/Q5_K_M
        // are in the allow-list
        let rec = recommended_config::RecommendedModel {
            model: recommended_config::StringMatch::Exact("granite-3.1-8b-instruct".to_string()),
            variant_formats: vec![],
            variant_precisions: vec![
                recommended_config::StringMatch::Exact("Q4_K_M".to_string()),
                recommended_config::StringMatch::Exact("Q5_K_M".to_string()),
            ],
        };
        let set = recommended_config::RecommendedModelSet {
            min_context_length: None,
            models: vec![rec],
        };

        let result = resolve_model_set(&set, &hardware, &healthy, &ctx);
        assert!(
            result.is_some(),
            "should resolve with precision-filtered variants"
        );
        let (_, variant) = result.unwrap();
        // The selected variant's precision must be in the allow-list
        assert!(
            variant.precision == "Q4_K_M" || variant.precision == "Q5_K_M",
            "variant precision should be in the allow-list, got '{}'",
            variant.precision
        );
    }

    #[test]
    fn resolve_model_set_min_context_length_gates_effective_context() {
        // A model with partial fit where the effective context is below
        // min_context_length should be rejected.
        // Use a small RAM profile to force partial fit.
        let hardware = HardwareProfile {
            os: "test".to_string(),
            cpu_cores: 8,
            cpu_arch: "test".to_string(),
            gpu_vendor: None,
            vram_gb: None,
            ram_gb: 4.0, // Very tight RAM
        };
        let ctx = test_ctx();
        let healthy: Vec<String> = ["ollama".to_string()].into_iter().collect();

        let rec = recommended_config::RecommendedModel {
            model: recommended_config::StringMatch::Exact("granite-3.1-8b-instruct".to_string()),
            variant_formats: vec![],
            variant_precisions: vec![],
        };
        // min_context_length of 100_000 is very high; with 4GB RAM the
        // partial fit will be well below that.
        let set = recommended_config::RecommendedModelSet {
            min_context_length: Some(100_000),
            models: vec![rec],
        };

        let result = resolve_model_set(&set, &hardware, &healthy, &ctx);
        assert!(
            result.is_none(),
            "should reject model whose effective context is below min"
        );
    }

    // -- resolve_capability ----------------------------------------------------

    #[test]
    fn resolve_capability_required_slot_missing_returns_none() {
        let hardware = test_hardware_profile();
        let ctx = test_ctx();
        let healthy: Vec<String> = vec![];

        // Capability requires model_id slot but no recommendation exists for it
        let mut models = HashMap::new();
        // Only provide a recommendation for "other_slot", not "model_id"
        models.insert(
            "other_slot".to_string(),
            recommended_config::RecommendedModelSet {
                min_context_length: None,
                models: vec![recommended_config::RecommendedModel {
                    model: recommended_config::StringMatch::Exact(
                        "granite-3.1-8b-instruct".to_string(),
                    ),
                    variant_formats: vec![],
                    variant_precisions: vec![],
                }],
            },
        );
        let rec_cap = recommended_config::RecommendedCapability {
            capability: "agent-model".to_string(),
            models,
        };

        let result = resolve_capability("agent-model", &rec_cap, &hardware, &healthy, &ctx);
        assert!(
            result.is_none(),
            "should return None when required slot has no recommendation"
        );
    }

    // -- Regression test: issue #129 bug ---------------------------------------

    #[test]
    fn resolve_model_set_falls_through_when_the_top_ranked_variant_is_unrunnable() {
        // Regression test: granite-4.2-3b's variant list contains a
        // "safetensors"/bfloat16 build that ties GGUF quantized builds on
        // ContextFit (both fully fit tiny models on generous hardware), and
        // the fit/size tie-break in `rank_variants_among` prefers the larger
        // one -- so the *top-ranked* variant among an allow-list spanning
        // both is the safetensors build. Ollama can only run GGUF/Ollama
        // formats, so if `resolve_model_set` only ever tried the single
        // best-ranked variant, this would fail to resolve even though a
        // perfectly good GGUF variant is right there in the same allow-list.
        // This is exactly what caused `sub-agent-explore` to silently not
        // get enabled under `setup --auto`.
        let hardware = test_hardware_profile();
        let ctx = test_ctx();
        let healthy = vec!["ollama".to_string()];

        let rec = recommended_config::RecommendedModel {
            model: recommended_config::StringMatch::Exact("granite-4.2-3b".to_string()),
            variant_formats: vec![],
            variant_precisions: vec![
                recommended_config::StringMatch::Exact("Q4_K_M".to_string()),
                recommended_config::StringMatch::Exact("Q5_K_M".to_string()),
                recommended_config::StringMatch::Exact("Q6_K".to_string()),
                recommended_config::StringMatch::Exact("Q8_0".to_string()),
                recommended_config::StringMatch::Exact("bfloat16".to_string()),
            ],
        };
        let set = recommended_config::RecommendedModelSet {
            min_context_length: Some(65536),
            models: vec![rec],
        };

        let result = resolve_model_set(&set, &hardware, &healthy, &ctx);
        let (model_id, variant) = result.expect(
            "should fall through to a GGUF-runnable variant instead of giving up on the \
             unrunnable safetensors one",
        );
        assert_eq!(model_id, "granite-4.2-3b");
        assert!(
            variant.format.eq_ignore_ascii_case("gguf")
                || variant.format.eq_ignore_ascii_case("ollama"),
            "expected a variant Ollama can actually run, got format '{}'",
            variant.format
        );
    }

    #[test]
    fn resolve_model_set_variant_formats_filters_by_format() {
        // "just use the Ollama-served build" -- a format-only constraint
        // with no precision restriction should admit any Ollama-format
        // variant and reject everything else.
        let hardware = test_hardware_profile();
        let ctx = test_ctx();
        let healthy = vec!["ollama".to_string()];

        let rec = recommended_config::RecommendedModel {
            model: recommended_config::StringMatch::Exact("granite-4.2-3b".to_string()),
            variant_formats: vec![recommended_config::StringMatch::Exact("Ollama".to_string())],
            variant_precisions: vec![],
        };
        let set = recommended_config::RecommendedModelSet {
            min_context_length: None,
            models: vec![rec],
        };

        let (model_id, variant) =
            resolve_model_set(&set, &hardware, &healthy, &ctx).expect("should resolve");
        assert_eq!(model_id, "granite-4.2-3b");
        assert!(
            variant.format.eq_ignore_ascii_case("ollama"),
            "expected the Ollama-format variant, got '{}'",
            variant.format
        );
    }

    #[test]
    fn resolve_model_set_variant_formats_and_precisions_combine_with_and() {
        // GGUF-only, Q4_K_M-or-better: a safetensors variant must not be
        // admitted even though its precision string ("bfloat16") isn't in
        // the precision list either -- and a GGUF variant outside the
        // precision allow-list (e.g. Q2_K) must also be excluded.
        let hardware = test_hardware_profile();
        let ctx = test_ctx();
        let healthy = vec!["ollama".to_string()];

        let rec = recommended_config::RecommendedModel {
            model: recommended_config::StringMatch::Exact("granite-4.2-3b".to_string()),
            variant_formats: vec![recommended_config::StringMatch::Exact("GGUF".to_string())],
            variant_precisions: vec![
                recommended_config::StringMatch::Exact("Q4_K_M".to_string()),
                recommended_config::StringMatch::Exact("Q5_K_M".to_string()),
            ],
        };
        let set = recommended_config::RecommendedModelSet {
            min_context_length: None,
            models: vec![rec],
        };

        let (model_id, variant) =
            resolve_model_set(&set, &hardware, &healthy, &ctx).expect("should resolve");
        assert_eq!(model_id, "granite-4.2-3b");
        assert!(variant.format.eq_ignore_ascii_case("gguf"));
        assert!(variant.precision == "Q4_K_M" || variant.precision == "Q5_K_M");
    }

    #[test]
    fn wildcard_agent_model_resolution_never_picks_a_3b_class_model() {
        // Regression test for issue #129: a launcher was getting wired to a
        // 3B model because the generic `ModelRequirement` only checks
        // Chat+ToolCalling (which a 3B model satisfies) and selection among
        // qualifying models was nondeterministic (`HashSet` iteration
        // order).
        //
        // This exercises the *real*, shipped `resources/recommended_configs/
        // default.yaml` data through the real resolution path
        // (`effective_capabilities` + `resolve_capability`), deliberately
        // bypassing `Discover::run`/`run_auto_with_hardware` -- going
        // through the full pipeline would make this test depend on whether
        // some real launcher binary happens to be on the test runner's PATH
        // and would skip provider health-probing entirely for an
        // already-configured provider, either of which lets the test pass
        // vacuously (nothing resolves, so nothing is ever a 3B model)
        // without actually exercising the fix. Driving `resolve_capability`
        // directly, with an explicit `healthy` provider list, makes the
        // check deterministic and environment-independent while still using
        // the real production data and algorithm.
        //
        // Uses "goose" -- a real `LAUNCHER_REGISTRY` entry that supports
        // `AgentModel` but has no `resources/recommended_configs/goose.yaml`
        // of its own, so its effective config comes entirely from the
        // wildcard (per the "a launcher's own entry is authoritative;
        // otherwise fall back to the wildcard" rule).
        let hardware = test_hardware_profile();
        let ctx = test_ctx();
        let healthy = vec!["ollama".to_string()];

        let rec_caps = recommended_config::effective_capabilities(
            "goose",
            &recommended_config::BUILTIN_RECOMMENDED_CONFIGS,
            &HashMap::new(),
        );
        let agent_model_cap = rec_caps
            .iter()
            .find(|c| c.capability == "agent-model")
            .expect("goose has no config of its own, so it should inherit agent-model from the wildcard");

        let resolved =
            resolve_capability("agent-model", agent_model_cap, &hardware, &healthy, &ctx)
                .expect("agent-model should resolve for goose given a healthy ollama provider");

        let (model_id, _variant) = resolved
            .slots
            .get("model_id")
            .expect("model_id slot should have resolved");

        assert!(
            !model_id.contains("3b") || model_id.contains("30b"),
            "bug #129: agent-model resolved to a 3B-class model: {model_id}"
        );
        // Positive check: it actually is one of the real granite-4.x
        // candidates, not just "something that isn't 3b".
        assert!(
            model_id.starts_with("granite-4."),
            "expected a granite-4.x candidate, got '{model_id}'"
        );
    }

    #[test]
    fn claude_sub_agent_explore_resolves_against_the_real_shipped_config() {
        // End-to-end regression check against the real, shipped
        // resources/recommended_configs/claude.yaml data (not synthetic
        // test fixtures): claude's sub-agent-explore recommendation
        // (granite-4.2-3b, precisions spanning both quantized GGUF and a
        // raw safetensors build) must actually resolve. Before the
        // resolve_model_set fix, it silently failed to enable at all,
        // because the top-ranked variant by fit/size happened to be the
        // unrunnable safetensors build and resolution gave up rather than
        // falling through to a GGUF one.
        let hardware = test_hardware_profile();
        let ctx = test_ctx();
        let healthy = vec!["ollama".to_string()];

        let rec_caps = recommended_config::effective_capabilities(
            "claude",
            &recommended_config::BUILTIN_RECOMMENDED_CONFIGS,
            &HashMap::new(),
        );
        let cap = rec_caps
            .iter()
            .find(|c| c.capability == "sub-agent-explore")
            .expect("claude.yaml defines sub-agent-explore");

        let resolved = resolve_capability("sub-agent-explore", cap, &hardware, &healthy, &ctx)
            .expect("sub-agent-explore should resolve for claude given a healthy ollama provider");
        let (model_id, variant) = resolved
            .slots
            .get("model_id")
            .expect("model_id slot should have resolved");
        assert_eq!(model_id, "granite-4.2-3b");
        assert!(
            variant.format.eq_ignore_ascii_case("gguf")
                || variant.format.eq_ignore_ascii_case("ollama"),
            "expected a variant Ollama can actually run, got format '{}'",
            variant.format
        );
    }

    #[tokio::test]
    async fn select_capabilities_does_not_pre_select_a_capability_whose_model_does_not_resolve() {
        // Regression test for issue #129 (wizard path): claude.yaml
        // recommends both sub-agent-explore (granite-4.2-3b, which fully
        // fits `test_hardware_profile`) and sub-agent-code (granite-4.2-30b,
        // which under the same profile only partially fits, below its
        // required 65536 min_context_length, so it never resolves -- see
        // `probe`-verified assumption shared with
        // `claude_sub_agent_explore_resolves_against_the_real_shipped_config`).
        // Both capabilities should still be *shown* (claude's own config
        // names both), but only the one that actually resolves on this
        // hardware should be pre-selected -- previously *every* capability
        // a launcher's YAML named was pre-selected regardless of whether it
        // could ever resolve.
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext {
            config: Config::default(),
            ui: capture.clone(),
        };
        let hardware = test_hardware_profile();

        // Build a DiscoveryResult by hand (rather than the real
        // `discover_providers` health probe, which would hit the network
        // and could vacuously report every provider unhealthy) with a
        // capability recommendation for every capability claude.yaml names,
        // plus a healthy "ollama" provider so resolution has a real chance
        // to succeed for the ones that fit.
        let discovery = DiscoveryResult {
            recommendations: vec![
                Recommendation::Provider {
                    provider_type: "ollama",
                    provider_name: "Ollama".to_string(),
                    health_healthy: true,
                    health_error: None,
                },
                Recommendation::Capability {
                    capability_type: "sub-agent-explore".to_string(),
                    capability_name: "Sub-Agent: Explore".to_string(),
                },
                Recommendation::Capability {
                    capability_type: "sub-agent-code".to_string(),
                    capability_name: "Sub-Agent: Code".to_string(),
                },
            ],
            all_model_candidates: vec![],
            configured_provider_ids: vec![],
            configured_model_ids: vec![],
            configured_launcher_ids: vec![],
            configured_capability_ids: vec![],
        };
        let selected_launchers: HashSet<String> = ["claude".to_string()].into_iter().collect();

        let _ = SetupCommands::select_capabilities(
            &mut ctx,
            &discovery,
            &selected_launchers,
            &hardware,
        )
        .await
        .unwrap();

        let prompts = capture.multi_select_prompts.borrow();
        assert_eq!(prompts.len(), 1, "expected exactly one multi_select prompt");
        let (_, items, defaults) = &prompts[0];
        let explore_idx = items
            .iter()
            .position(|i| i.starts_with("sub-agent-explore"))
            .expect("sub-agent-explore must be offered as an option");
        let code_idx = items
            .iter()
            .position(|i| i.starts_with("sub-agent-code"))
            .expect(
                "sub-agent-code must still be offered as an option even though it can't resolve",
            );

        assert!(
            defaults[explore_idx],
            "sub-agent-explore resolves on this hardware, so it must be pre-selected"
        );
        assert!(
            !defaults[code_idx],
            "sub-agent-code cannot resolve on this hardware (30b doesn't fit), so it must NOT \
             be pre-selected even though claude.yaml names it"
        );
    }

    #[test]
    fn resolved_launcher_type_fixes_naming_ripple_for_custom_instance_ids() {
        // Regression test for the naming-ripple bug (issue #129): when a launcher
        // instance's id differs from its registry type (e.g., instance id
        // "claude-work" with type "claude"), every place that iterates
        // `selected_launchers` and treats the id as a registry key was picking up
        // the wildcard's `agent-model` instead of the launcher's own curated
        // recommendations. The fix resolves the id to its real type via
        // `resolved_launcher_type` before calling `effective_capabilities`.
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();

        // Configure a launcher instance with a custom id that differs from its type
        ctx.config.launchers.insert(
            "claude-work".to_string(),
            crate::config::LauncherConfig {
                launcher_id: "claude-work".to_string(),
                launcher_type: "claude".to_string(),
                enabled_capabilities: vec![],
                config: serde_json::json!({}),
            },
        );

        // The resolver should return "claude", not the raw instance id
        let resolved = resolved_launcher_type(&ctx, "claude-work");
        assert_eq!(
            resolved, "claude",
            "resolved_launcher_type should return the configured type, not the instance id"
        );

        // Effective capabilities for the raw id "claude-work" (no config entry
        // would match, so it hits the wildcard) includes `agent-model` from
        // default.yaml's wildcard.
        let caps_from_raw_id = recommended_config::effective_capabilities(
            "claude-work",
            &recommended_config::BUILTIN_RECOMMENDED_CONFIGS,
            &HashMap::new(),
        );
        assert!(
            caps_from_raw_id
                .iter()
                .any(|c| c.capability == "agent-model"),
            "raw id 'claude-work' hits the wildcard (which includes agent-model)"
        );

        // But effective capabilities for the RESOLVED type "claude" does NOT
        // include `agent-model` (claude.yaml deliberately omits it).
        let caps_from_resolved = recommended_config::effective_capabilities(
            &resolved,
            &recommended_config::BUILTIN_RECOMMENDED_CONFIGS,
            &HashMap::new(),
        );
        assert!(
            !caps_from_resolved
                .iter()
                .any(|c| c.capability == "agent-model"),
            "claude's effective capabilities should NOT include agent-model (claude.yaml deliberately omits it)"
        );

        // Both should include sub-agent-explore (claude.yaml explicitly recommends it)
        assert!(
            caps_from_resolved
                .iter()
                .any(|c| c.capability == "sub-agent-explore"),
            "claude's effective capabilities should include sub-agent-explore"
        );
        assert!(
            caps_from_resolved
                .iter()
                .any(|c| c.capability == "vision-mcp"),
            "claude's effective capabilities should include vision-mcp"
        );
    }

    #[tokio::test]
    async fn select_capabilities_resolves_custom_launcher_id_to_its_real_type_end_to_end() {
        // End-to-end version of the naming-ripple fix: unlike
        // `resolved_launcher_type_fixes_naming_ripple_for_custom_instance_ids`
        // (which tests the resolver and `effective_capabilities` in
        // isolation), this drives the actual `select_capabilities` call site
        // to prove the wiring itself resolves the id, not just that the
        // helper it depends on works in isolation.
        let _home = crate::config::TestConfigHome::new();
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext {
            config: Config::default(),
            ui: capture.clone(),
        };
        ctx.config.launchers.insert(
            "claude-work".to_string(),
            crate::config::LauncherConfig {
                launcher_id: "claude-work".to_string(),
                launcher_type: "claude".to_string(),
                enabled_capabilities: vec![],
                config: serde_json::json!({}),
            },
        );
        let discovery = run_discovery(&ctx).await;
        let selected_launchers: HashSet<String> = ["claude-work".to_string()].into_iter().collect();

        // Decline the escape hatch so the loop returns immediately.
        capture.multi_select_answers.borrow_mut().push_back(vec![]);

        let _ = SetupCommands::select_capabilities(
            &mut ctx,
            &discovery,
            &selected_launchers,
            &test_hardware_profile(),
        )
        .await
        .unwrap();

        let prompts = capture.multi_select_prompts.borrow();
        assert_eq!(prompts.len(), 1, "expected exactly one multi_select prompt");
        let (_, items, _) = &prompts[0];
        assert!(
            !items.iter().any(|i| i.starts_with("agent-model")),
            "instance id 'claude-work' (type 'claude') must resolve to claude's own \
             recommended config, which deliberately excludes agent-model -- if the \
             naming-ripple fix regresses, this would incorrectly fall back to the \
             wildcard's agent-model recommendation instead: {items:?}"
        );
        assert!(
            items.iter().any(|i| i.starts_with("sub-agent-explore")),
            "claude.yaml recommends sub-agent-explore, so it should be offered: {items:?}"
        );
    }

    // -- select_providers escape hatch -----------------------------------------

    #[tokio::test]
    async fn select_providers_escape_hatch_appears_with_zero_healthy_providers() {
        // Regression test: `select_providers` used to return early with an
        // info message when `filtered` was empty, which was exactly the
        // situation where the escape hatch is most needed. The guard was
        // removed so the loop renders with just the escape-hatch item.
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext {
            config: Config::default(),
            ui: capture.clone(),
        };

        let discovery = DiscoveryResult {
            recommendations: vec![
                // A provider that exists in the registry but is unhealthy.
                Recommendation::Provider {
                    provider_type: "ollama",
                    provider_name: "Ollama".to_string(),
                    health_healthy: false,
                    health_error: Some("not running".to_string()),
                },
            ],
            all_model_candidates: vec![],
            configured_provider_ids: vec![],
            configured_model_ids: vec![],
            configured_launcher_ids: vec![],
            configured_capability_ids: vec![],
        };

        // No canned multi_select answers — when the queue is empty,
        // CaptureUi returns vec![] (empty selection), so the loop returns
        // immediately without entering the escape-hatch branch.
        SetupCommands::select_providers(&mut ctx, &discovery, &HashSet::new())
            .await
            .unwrap();

        let prompts = capture.multi_select_prompts.borrow();
        assert_eq!(prompts.len(), 1, "expected exactly one multi_select prompt");
        let (_, items, defaults) = &prompts[0];
        // The only item should be the escape-hatch label.
        assert_eq!(
            items.len(),
            1,
            "with zero healthy providers, only the escape-hatch item should appear"
        );
        assert!(
            items[0].contains("Configure a different provider"),
            "the escape-hatch label must appear even with zero healthy providers: {items:?}"
        );
        assert!(
            !defaults[0],
            "the escape-hatch item should NOT be pre-selected (user must explicitly choose it)"
        );
    }

    // -- configure_all guards --------------------------------------------------

    #[tokio::test]
    async fn configure_all_does_not_clobber_manually_configured_provider() {
        // Pre-insert a ProviderConfig with a distinctive custom field
        // (base_url), call configure_all, and assert the custom field
        // survives. The guard in configure_all's providers loop must
        // skip already-configured ids.
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        let discovery = run_discovery(&ctx).await;

        let custom_url = "http://my-ollama:11434";
        let provider_id = "my-ollama-instance";
        ctx.config.providers.insert(
            provider_id.to_string(),
            ProviderConfig {
                provider_id: provider_id.to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({"base_url": custom_url}),
            },
        );

        let selected_providers: HashSet<String> = [provider_id.to_string()].into_iter().collect();

        SetupCommands::configure_all(
            &mut ctx,
            &discovery,
            &HashSet::new(),
            &HashSet::new(),
            &selected_providers,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();

        let saved = ctx
            .config
            .get_provider(provider_id)
            .expect("provider must still be in config");
        assert_eq!(
            saved.config["base_url"], custom_url,
            "configure_all must not overwrite a manually configured provider's custom base_url"
        );
    }

    #[tokio::test]
    async fn configure_all_does_not_clobber_manually_configured_launcher() {
        // Pre-insert a LauncherConfig with a distinctive custom `config`
        // field, call configure_all, and assert the custom field survives.
        // NOTE: the later "enable capabilities" loop (lines 2280-2326) will
        // recompute enabled_capabilities for configured launchers regardless
        // of the guard, so we only assert on the `config` field (which the
        // guard alone protects) — enabled_capabilities is tested separately
        // by `configure_all_enables_only_capabilities_a_launcher_supports`.
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        let discovery = run_discovery(&ctx).await;

        let custom_config_val = "my-custom-launcher-config-value";
        let launcher_id = "my-claude-instance";
        ctx.config.launchers.insert(
            launcher_id.to_string(),
            crate::config::LauncherConfig {
                launcher_id: launcher_id.to_string(),
                launcher_type: "claude".to_string(),
                enabled_capabilities: vec!["vision-mcp".to_string()],
                config: serde_json::json!({"custom_field": custom_config_val}),
            },
        );

        let selected_launchers: HashSet<String> = [launcher_id.to_string()].into_iter().collect();

        SetupCommands::configure_all(
            &mut ctx,
            &discovery,
            &HashSet::new(),
            &selected_launchers,
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();

        let saved = ctx
            .config
            .get_launcher(launcher_id)
            .expect("launcher must still be in config");
        assert_eq!(
            saved.config["custom_field"], custom_config_val,
            "configure_all must not overwrite a manually configured launcher's custom config"
        );
    }

    #[tokio::test]
    async fn configure_all_does_not_clobber_manually_configured_capability() {
        // Pre-insert a CapabilityConfig with a distinctive custom `config`
        // field, call configure_all, and assert the custom field survives.
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        let discovery = run_discovery(&ctx).await;

        let custom_config_val = "my-custom-cap-config";
        let cap_id = "my-vision-mcp";
        ctx.config.capabilities.insert(
            cap_id.to_string(),
            crate::config::CapabilityConfig {
                capability_id: cap_id.to_string(),
                capability_type: "vision-mcp".to_string(),
                config: serde_json::json!({"custom_field": custom_config_val}),
            },
        );

        let selected_caps: HashSet<String> = [cap_id.to_string()].into_iter().collect();

        SetupCommands::configure_all(
            &mut ctx,
            &discovery,
            &selected_caps,
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();

        let saved = ctx
            .config
            .get_capability(cap_id)
            .expect("capability must still be in config");
        assert_eq!(
            saved.config["custom_field"], custom_config_val,
            "configure_all must not overwrite a manually configured capability's custom config"
        );
    }

    // -- Wizard model binding --------------------------------------------------

    fn string_set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn recommended_models_for_selection_binds_each_capability_to_its_listed_model() {
        // claude.yaml lists granite-4.2-30b for sub-agent-code and
        // granite-4.2-3b for sub-agent-explore. With both models kept, each
        // capability gets the model listed for it.
        let ctx = test_ctx();
        let caps = string_set(&["sub-agent-code", "sub-agent-explore"]);
        let models = string_set(&["granite-4.2-30b", "granite-4.2-3b"]);

        let resolved =
            recommended_models_for_selection(&ctx, &["claude".to_string()], &caps, &models);

        assert_eq!(resolved["sub-agent-code"]["model_id"], "granite-4.2-30b");
        assert_eq!(resolved["sub-agent-explore"]["model_id"], "granite-4.2-3b");
    }

    #[test]
    fn recommended_models_for_selection_leaves_out_a_slot_with_no_kept_candidate() {
        // claude.yaml lists only granite-4.2-30b for sub-agent-code. With only
        // granite-4.2-3b kept, sub-agent-code gets no entry, so configure_all
        // uses find_model_for_capability for it.
        let ctx = test_ctx();
        let caps = string_set(&["sub-agent-code", "sub-agent-explore"]);
        let models = string_set(&["granite-4.2-3b"]);

        let resolved =
            recommended_models_for_selection(&ctx, &["claude".to_string()], &caps, &models);

        assert!(!resolved.contains_key("sub-agent-code"), "{resolved:?}");
        assert_eq!(resolved["sub-agent-explore"]["model_id"], "granite-4.2-3b");
    }

    #[test]
    fn recommended_models_for_selection_uses_the_first_kept_candidate_in_config_order() {
        // goose has no config of its own, so default.yaml applies. Its
        // agent-model entry lists granite-4.2-30b before granite-4.2-8b and
        // does not list granite-4.2-3b. Issue #129 reported agent-model bound
        // to a 3b model.
        let ctx = test_ctx();
        let caps = string_set(&["agent-model"]);
        let models = string_set(&["granite-4.2-3b", "granite-4.2-8b", "granite-4.2-30b"]);

        let resolved =
            recommended_models_for_selection(&ctx, &["goose".to_string()], &caps, &models);

        assert_eq!(resolved["agent-model"]["model_id"], "granite-4.2-30b");
    }

    #[test]
    fn recommended_models_for_selection_keeps_the_first_launchers_model_for_a_shared_capability() {
        // claude.yaml lists granite-4.2-30b for sub-agent-code; a user config
        // for opencode lists granite-4.2-8b. The launchers are read in the
        // order given, so claude's model is kept.
        let mut ctx = test_ctx();
        ctx.config.recommended_configs.insert(
            "opencode".to_string(),
            recommended_config::RecommendedConfiguration {
                launcher: "opencode".to_string(),
                capabilities: vec![recommended_config::RecommendedCapability {
                    capability: "sub-agent-code".to_string(),
                    models: HashMap::from([(
                        "model_id".to_string(),
                        recommended_config::RecommendedModelSet {
                            min_context_length: None,
                            models: vec![recommended_config::RecommendedModel {
                                model: recommended_config::StringMatch::Exact(
                                    "granite-4.2-8b".to_string(),
                                ),
                                variant_formats: vec![],
                                variant_precisions: vec![],
                            }],
                        },
                    )]),
                }],
            },
        );
        let caps = string_set(&["sub-agent-code"]);
        let models = string_set(&["granite-4.2-8b", "granite-4.2-30b"]);

        let resolved = recommended_models_for_selection(
            &ctx,
            &["claude".to_string(), "opencode".to_string()],
            &caps,
            &models,
        );

        assert_eq!(resolved["sub-agent-code"]["model_id"], "granite-4.2-30b");
    }

    #[tokio::test]
    async fn configure_all_binds_wizard_capabilities_to_their_recommended_models() {
        // run_wizard passes recommended_models_for_selection's result to
        // configure_all. With an empty map, both capabilities got the same
        // model, whichever find_model_for_capability returned.
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        let discovery = run_discovery(&ctx).await;
        let launchers = string_set(&["claude"]);
        let caps = string_set(&["sub-agent-code", "sub-agent-explore"]);
        let models = string_set(&["granite-4.2-30b", "granite-4.2-3b"]);
        let providers = string_set(&["ollama"]);
        let resolved =
            recommended_models_for_selection(&ctx, &["claude".to_string()], &caps, &models);

        SetupCommands::configure_all(
            &mut ctx,
            &discovery,
            &caps,
            &launchers,
            &providers,
            &models,
            &HashMap::new(),
            &resolved,
            &HashMap::new(),
        )
        .await
        .unwrap();

        let model_of = |id: &str| {
            ctx.config
                .get_capability(id)
                .unwrap_or_else(|| panic!("{id} should be configured"))
                .config["model_id"]
                .clone()
        };
        assert_eq!(model_of("sub-agent-code"), "granite-4.2-30b");
        assert_eq!(model_of("sub-agent-explore"), "granite-4.2-3b");
    }

    // -- auto_selection --------------------------------------------------------

    #[test]
    fn auto_selection_skips_capabilities_the_launcher_cannot_bind() {
        // bob has no config of its own, so default.yaml applies: agent-model
        // and vision-mcp. bob supports only BindingType::Mcp, so agent-model
        // (BindingType::AgentModel) is not selected and its model is not
        // configured. vision-mcp (BindingType::Mcp) still is.
        let ctx = test_ctx();

        let selection = SetupCommands::auto_selection(
            &ctx,
            &string_set(&["bob"]),
            &["ollama".to_string()],
            &test_hardware_profile(),
        );

        assert_eq!(selection.capabilities, string_set(&["vision-mcp"]));
        assert_eq!(selection.models, string_set(&["granite-vision-4.1-4b"]));
        assert!(
            !selection
                .resolved_capability_models
                .contains_key("agent-model")
        );
        // Still recorded as recommended for bob, so configure_all keeps
        // treating agent-model as a capability with a recommendation.
        assert!(selection.recommended_capability_types_by_launcher["bob"].contains("agent-model"));
    }

    #[test]
    fn auto_selection_skips_vision_mcp_for_a_launcher_without_mcp() {
        // pi supports only BindingType::AgentModel. default.yaml's vision-mcp
        // binds as BindingType::Mcp, so only agent-model is selected.
        let ctx = test_ctx();

        let selection = SetupCommands::auto_selection(
            &ctx,
            &string_set(&["pi"]),
            &["ollama".to_string()],
            &test_hardware_profile(),
        );

        assert_eq!(selection.capabilities, string_set(&["agent-model"]));
        assert!(
            !selection.models.contains("granite-vision-4.1-4b"),
            "{:?}",
            selection.models
        );
    }

    #[test]
    fn auto_selection_resolves_through_a_configured_provider() {
        // Discovery does not health-check configured providers, so on a
        // second `setup --auto` run healthy_provider_types can be empty while
        // an Ollama provider is configured. claude's sub-agent-explore still
        // resolves, through the configured provider.
        let ctx = ctx_with_provider(
            "my-ollama",
            "ollama",
            PROVIDER_REGISTRY.default_config("ollama").unwrap(),
        );

        let selection = SetupCommands::auto_selection(
            &ctx,
            &string_set(&["claude"]),
            &[],
            &test_hardware_profile(),
        );

        assert_eq!(
            selection.resolved_capability_models["sub-agent-explore"]["model_id"],
            "granite-4.2-3b"
        );
        assert!(
            selection.providers.contains("my-ollama"),
            "{:?}",
            selection.providers
        );
    }

    #[test]
    fn auto_selection_skips_a_capability_that_is_already_configured() {
        // sub-agent-explore is configured with another model. configure_all
        // keeps that entry, so granite-4.2-3b is not selected for it.
        let mut ctx = test_ctx();
        ctx.config.capabilities.insert(
            "sub-agent-explore".to_string(),
            crate::config::CapabilityConfig {
                capability_id: "sub-agent-explore".to_string(),
                capability_type: "sub-agent-explore".to_string(),
                config: serde_json::json!({"model_id": "my-model"}),
            },
        );

        let selection = SetupCommands::auto_selection(
            &ctx,
            &string_set(&["claude"]),
            &["ollama".to_string()],
            &test_hardware_profile(),
        );

        assert!(!selection.capabilities.contains("sub-agent-explore"));
        assert!(
            !selection.models.contains("granite-4.2-3b"),
            "{:?}",
            selection.models
        );
    }

    #[tokio::test]
    async fn configure_all_does_not_clobber_a_configured_model() {
        // setup --auto can select a model that is already configured.
        // configure_all keeps its provider and variant.
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_model("granite-4.2-3b", Some("my-ollama"));
        ctx.config.models.get_mut("granite-4.2-3b").unwrap().variant =
            Some("GGUF/Q4_K_M".to_string());
        let discovery = run_discovery(&ctx).await;

        SetupCommands::configure_all(
            &mut ctx,
            &discovery,
            &HashSet::new(),
            &HashSet::new(),
            &string_set(&["ollama"]),
            &string_set(&["granite-4.2-3b"]),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();

        let saved = ctx
            .config
            .get_model("granite-4.2-3b")
            .expect("model must still be configured");
        assert_eq!(saved.provider_id, "my-ollama");
        assert_eq!(saved.variant.as_deref(), Some("GGUF/Q4_K_M"));
    }

    // -- capability_model_ids --------------------------------------------------

    fn model_dependency(config_key: &str, required: bool) -> Dependency {
        Dependency::Model {
            config_key: config_key.to_string(),
            requirement: ModelRequirement::default(),
            resolved_id: None,
            required,
        }
    }

    #[test]
    fn capability_model_ids_is_none_when_one_of_two_required_slots_is_empty() {
        // No registered capability has two model slots, so this uses a
        // capability type that is not registered: find_model_for_capability
        // returns None for it, and only `model_id` has a model.
        let dependencies = [
            model_dependency("model_id", true),
            model_dependency("draft_model_id", true),
        ];
        let resolved: HashMap<String, HashMap<String, String>> = HashMap::from([(
            "two-slot-test".to_string(),
            HashMap::from([("model_id".to_string(), "granite-4.2-8b".to_string())]),
        )]);

        let cap_cfg = crate::config::CapabilityConfig {
            capability_id: "two-slot-test".to_string(),
            capability_type: "two-slot-test".to_string(),
            config: serde_json::json!({}),
        };
        let model_ids = SetupCommands::capability_model_ids(
            "two-slot-test",
            &dependencies,
            &cap_cfg,
            &resolved,
            &HashSet::new(),
        );

        assert_eq!(model_ids, None);
    }

    #[test]
    fn capability_model_ids_leaves_out_an_empty_optional_slot() {
        let dependencies = [
            model_dependency("model_id", true),
            model_dependency("draft_model_id", false),
        ];
        let resolved: HashMap<String, HashMap<String, String>> = HashMap::from([(
            "two-slot-test".to_string(),
            HashMap::from([("model_id".to_string(), "granite-4.2-8b".to_string())]),
        )]);
        let cap_cfg = crate::config::CapabilityConfig {
            capability_id: "two-slot-test".to_string(),
            capability_type: "two-slot-test".to_string(),
            config: serde_json::json!({}),
        };

        let model_ids = SetupCommands::capability_model_ids(
            "two-slot-test",
            &dependencies,
            &cap_cfg,
            &resolved,
            &HashSet::new(),
        );

        assert_eq!(
            model_ids,
            Some(HashMap::from([(
                "model_id".to_string(),
                "granite-4.2-8b".to_string()
            )]))
        );
    }

    #[tokio::test]
    async fn configure_all_warns_once_for_a_capability_without_a_model() {
        // agent-model has one required slot and no model is selected. The
        // capability is skipped with one warning; before, the same warning
        // was printed twice.
        let _home = crate::config::TestConfigHome::new();
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext {
            config: Config::default(),
            ui: capture.clone(),
        };
        let discovery = DiscoveryResult {
            recommendations: vec![],
            all_model_candidates: vec![],
            configured_provider_ids: vec![],
            configured_model_ids: vec![],
            configured_launcher_ids: vec![],
            configured_capability_ids: vec![],
        };

        SetupCommands::configure_all(
            &mut ctx,
            &discovery,
            &string_set(&["agent-model"]),
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            *capture.warns.borrow(),
            vec!["Skipping 'agent-model': no compatible model available.".to_string()]
        );
        assert!(ctx.config.get_capability("agent-model").is_none());
    }

    #[test]
    fn find_model_for_capability_returns_the_first_match_in_sorted_order() {
        // Each iteration builds a new HashSet, and each HashSet iterates in a
        // different order. The result must not change.
        for _ in 0..20 {
            let models = string_set(&["granite-4.2-8b", "granite-4.2-3b", "granite-4.2-30b"]);
            assert_eq!(
                SetupCommands::find_model_for_capability("sub-agent-code", &models),
                Some("granite-4.2-30b".to_string())
            );
        }
    }
}
