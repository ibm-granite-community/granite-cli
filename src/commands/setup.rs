// Standard
use std::collections::{HashMap, HashSet};

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use anyhow::Result;

// Local
use crate::capabilities::{BindingType, CAPABILITY_REGISTRY, Dependency, ModelRequirement};
use crate::commands::model::ModelCommands;
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
    /// Run the full discovery pipeline against the machine's real hardware.
    pub async fn run(ctx: &crate::AppContext) -> DiscoveryResult {
        Self::run_with_hardware(ctx, &detect_hardware()).await
    }

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
            let result = PROVIDER_REGISTRY.construct(
                provider_type,
                provider_type,
                &default_config,
                &ctx.config,
            );

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
                    .construct(&pc.provider_type, &pc.provider_id, &pc.config, &ctx.config)
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
            match LAUNCHER_REGISTRY.construct(
                launcher_type,
                launcher_type,
                &default_config,
                &ctx.config,
            ) {
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
    let fit_rank = |fit: &ContextFit| match fit {
        ContextFit::Full => 1,
        ContextFit::Partial(_) => 0,
        ContextFit::None => -1,
    };

    model
        .variants
        .iter()
        .map(|v| {
            let fit = crate::models::context_fit::estimate(
                model.context_length,
                &model.architecture,
                &model.native_dtype,
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
    pub async fn run(ctx: &mut crate::AppContext, auto: bool, skip_pull: bool) -> Result<()> {
        if auto {
            Self::run_auto(ctx).await
        } else {
            Self::run_wizard(ctx, skip_pull).await
        }
    }

    /// Run the interactive wizard.
    async fn run_wizard(ctx: &mut crate::AppContext, skip_pull: bool) -> Result<()> {
        let ui = &*ctx.ui;
        ui.info("=== granite-cli Setup Wizard ===\n");
        ui.info("Discovering available components...\n");

        let discovery = Discover::run(ctx).await;

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
        // launchers can actually bind)
        let selected_caps = Self::select_capabilities(ctx, &discovery, &selected_launchers).await?;

        // Phase 3: Models selection (filtered by capability requirements)
        let selected_models = Self::select_models(ctx, &discovery, &selected_caps).await?;

        // Phase 4: Providers selection (only healthy, filtered by model compatibility)
        let selected_providers = Self::select_providers(ctx, &discovery, &selected_models).await?;

        // Phase 4.5: Variant selection (limited to formats the selected
        // providers can actually run, with a VRAM estimate at full context)
        let selected_variants =
            Self::select_variants(ctx, &discovery, &selected_models, &selected_providers).await?;

        // Phase 5: Configuration
        Self::configure_all(
            ctx,
            &discovery,
            &selected_caps,
            &selected_launchers,
            &selected_providers,
            &selected_models,
            &selected_variants,
        )
        .await?;

        // Phase 6: Pull (optional)
        if !skip_pull {
            Self::prompt_pull(ctx, &selected_models).await?;
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
    async fn run_auto(ctx: &mut crate::AppContext) -> Result<()> {
        let ui = &*ctx.ui;
        ui.info("=== granite-cli Auto Setup ===\n");
        ui.info("Auto-detecting and configuring all available components...\n");

        let discovery = Discover::run(ctx).await;

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

        let selected_caps: HashSet<String> =
            Revaluator::for_capabilities(&discovery, &selected_launchers)
                .into_iter()
                .filter_map(|r| match r {
                    Recommendation::Capability {
                        capability_type, ..
                    } => Some(capability_type.clone()),
                    _ => None,
                })
                .collect();

        let selected_models: HashSet<String> =
            Revaluator::for_models(&discovery.recommendations, &selected_caps)
                .into_iter()
                .filter_map(|r| match r {
                    Recommendation::Model { model_id, .. } => Some(model_id.clone()),
                    _ => None,
                })
                .collect();

        let selected_providers: HashSet<String> =
            Revaluator::for_providers(&discovery, &selected_models, ctx)
                .into_iter()
                .filter_map(|r| match r {
                    Recommendation::Provider {
                        provider_type,
                        health_healthy,
                        ..
                    } if *health_healthy => Some(provider_type.to_string()),
                    _ => None,
                })
                .collect();

        // --auto is non-interactive, so there's no prompt for variant
        // selection -- `configure_all` falls back to discovery's
        // hardware-fit `best_variant` for every model.
        Self::configure_all(
            ctx,
            &discovery,
            &selected_caps,
            &selected_launchers,
            &selected_providers,
            &selected_models,
            &HashMap::new(),
        )
        .await?;

        // Never auto-pull in --auto mode
        Self::print_summary(
            ctx,
            &selected_caps,
            &selected_launchers,
            &selected_providers,
            &selected_models,
        );

        Ok(())
    }

    /*-- Selection phases ----------------------------------------------------*/

    async fn select_capabilities(
        ctx: &mut crate::AppContext,
        discovery: &DiscoveryResult,
        selected_launchers: &HashSet<String>,
    ) -> Result<HashSet<String>> {
        let ui = &*ctx.ui;

        let caps: Vec<_> = Revaluator::for_capabilities(discovery, selected_launchers)
            .into_iter()
            .filter_map(|r| match r {
                Recommendation::Capability {
                    capability_type,
                    capability_name,
                } => Some((capability_type.clone(), capability_name.clone())),
                _ => None,
            })
            .collect();

        if caps.is_empty() {
            ui.info("No capabilities available for the selected launchers.");
            return Ok(HashSet::new());
        }

        let items: Vec<String> = caps
            .iter()
            .map(|(id, name)| format!("{id} — {name}"))
            .collect();
        let defaults = vec![true; items.len()];

        let selected = ui.multi_select("Select capabilities to configure", &items, &defaults)?;

        Ok(selected.into_iter().map(|i| caps[i].0.clone()).collect())
    }

    async fn select_launchers(
        ctx: &mut crate::AppContext,
        discovery: &DiscoveryResult,
    ) -> Result<HashSet<String>> {
        let ui = &*ctx.ui;

        let filtered: Vec<_> = Revaluator::for_launchers(discovery, &HashSet::new())
            .into_iter()
            .filter_map(|r| match r {
                Recommendation::Launcher {
                    launcher_type,
                    launcher_name,
                    binary_path: Some(binary_path),
                } => Some((launcher_type.clone(), launcher_name, binary_path.clone())),
                _ => None,
            })
            .collect();

        if filtered.is_empty() {
            ui.info("No launchers detected on this system.");
            return Ok(HashSet::new());
        }

        let items: Vec<String> = filtered
            .iter()
            .map(|(id, name, path)| format!("{id} — {name} ({path})"))
            .collect();
        let defaults = vec![false; items.len()];

        let selected = ui.multi_select("Select launchers to configure", &items, &defaults)?;

        Ok(selected
            .into_iter()
            .map(|i| filtered[i].0.clone())
            .collect())
    }

    async fn select_providers(
        ctx: &mut crate::AppContext,
        discovery: &DiscoveryResult,
        selected_models: &HashSet<String>,
    ) -> Result<HashSet<String>> {
        let ui = &*ctx.ui;

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

        if filtered.is_empty() {
            ui.info("No healthy providers available to configure.");
            return Ok(HashSet::new());
        }

        let items: Vec<String> = filtered
            .iter()
            .map(|(id, name, error)| {
                let status = if error.is_none() || error.as_ref().is_some_and(|e| e.is_empty()) {
                    "healthy".to_string()
                } else if let Some(e) = &error {
                    format!("healthy ({e})")
                } else {
                    "healthy".to_string()
                };
                format!("{id} — {name} ({status})")
            })
            .collect();
        let defaults = vec![false; items.len()];

        let selected = ui.multi_select("Select providers to configure", &items, &defaults)?;

        Ok(selected
            .into_iter()
            .map(|i| filtered[i].0.clone())
            .collect())
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
    /// (i.e. full context). Providers are constructed transiently with
    /// registry defaults, matching how discovery evaluates them, since this
    /// runs before providers are written to config.
    fn candidate_variants(
        md: &ModelMetadata,
        selected_providers: &HashSet<String>,
        ctx: &crate::AppContext,
    ) -> Vec<(ModelVariant, f64)> {
        let providers: Vec<Box<dyn Provider>> = selected_providers
            .iter()
            .filter_map(|pid| {
                let default_config = PROVIDER_REGISTRY.default_config(pid).unwrap_or_default();
                PROVIDER_REGISTRY
                    .construct(pid, pid, &default_config, &ctx.config)
                    .ok()
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

    /// Label for the extra row appended to the default model list that
    /// hands off to `select_models_manually`.
    const CHOOSE_DIFFERENT_MODELS_LABEL: &'static str = "→ Choose different models…";

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
    ) -> Result<HashSet<String>> {
        let ui = &*ctx.ui;

        let filtered = Self::model_options(Revaluator::for_models(
            &discovery.recommendations,
            selected_caps,
        ));

        let mut chosen: HashSet<String> = HashSet::new();
        let mut choose_different = filtered.is_empty();

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
            items.push(Self::CHOOSE_DIFFERENT_MODELS_LABEL.to_string());

            let mut defaults = vec![true; filtered.len()];
            defaults.push(false);

            let selected = ui.multi_select("Select models to configure", &items, &defaults)?;

            choose_different = selected.contains(&escape_hatch_idx);
            chosen = selected
                .into_iter()
                .filter(|&i| i < escape_hatch_idx)
                .map(|i| filtered[i].0.clone())
                .collect();
        }

        if choose_different {
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

    async fn configure_all(
        ctx: &mut crate::AppContext,
        discovery: &DiscoveryResult,
        selected_caps: &HashSet<String>,
        selected_launchers: &HashSet<String>,
        selected_providers: &HashSet<String>,
        selected_models: &HashSet<String>,
        selected_variants: &HashMap<String, ModelVariant>,
    ) -> Result<()> {
        let ui = &*ctx.ui;

        // Configure providers first
        for provider_id in selected_providers {
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

            let model_config = crate::config::ModelConfig {
                model_id: model_id.clone(),
                model_type: model_id.clone(),
                config: serde_json::json!({}),
                provider_id,
                variant,
            };

            if ctx.config.insert_model(model_id, model_config).is_err() {
                ui.warn(&format!("Failed to save model config for '{model_id}'"));
            }
        }

        // Configure capabilities
        for cap_type in selected_caps {
            let model_id = Self::find_model_for_capability(cap_type, selected_models);

            // Skip capabilities that have a required model dependency but no
            // matching model was selected — saving them with model_id="" would
            // panic when CapabilitySource constructs the instance below.
            let needs_model = CAPABILITY_REGISTRY.get(cap_type).is_some_and(|meta| {
                meta.dependencies
                    .iter()
                    .any(|d| matches!(d, Dependency::Model { required: true, .. }))
            });
            if needs_model && model_id.is_none() {
                ui.warn(&format!(
                    "Skipping '{cap_type}': no compatible model available."
                ));
                continue;
            }

            ui.info(&format!("\nConfiguring capability: {cap_type}..."));

            let mut config = CAPABILITY_REGISTRY
                .default_config(cap_type)
                .unwrap_or_default();

            // Set model_id if the capability requires a model
            if let Some(model_id) = model_id {
                config["model_id"] = model_id.into();
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

        // Enable every configured capability on each configured launcher
        // that supports it. Must run after both loops above, since it needs
        // a live `CapabilitySource` built from the now-populated capability
        // configs.
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

            let mut enabled: Vec<String> = capability_source
                .instances()
                .into_iter()
                .filter(|(_, cap)| {
                    cap.binding_types()
                        .iter()
                        .any(|bt| launcher_meta.supported_capabilities.contains(bt))
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
                            .construct(&pc.provider_type, &pc.provider_id, &pc.config, &ctx.config)
                            .ok()
                    })
                    .is_some_and(|p| p.can_run_model(&variant.format, &variant.precision))
            })
            .cloned()
    }

    /// Find a model_id from selected_models that satisfies a capability's model
    /// requirement. Returns the first matching model or None if no match.
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
        // satisfies any requirement.
        selected_models
            .iter()
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

    async fn prompt_pull(
        ctx: &mut crate::AppContext,
        selected_models: &HashSet<String>,
    ) -> Result<()> {
        // Cloned (not `&*ctx.ui`) so it doesn't hold a borrow of `ctx` --
        // `ModelCommands::pull` below needs `&mut ctx` for the whole struct.
        let ui = ctx.ui.clone();

        // Find local provider models that were configured
        let pullable: Vec<_> = selected_models
            .iter()
            .filter_map(|model_id| {
                ctx.config
                    .get_model(model_id)
                    .and_then(|mc| mc.provider_id.clone())
                    .and_then(|provider_id| {
                        ctx.config
                            .get_provider(&provider_id)
                            .map(|pc| (model_id.clone(), provider_id, pc.provider_type.clone()))
                    })
            })
            .collect();

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
                provider_id: provider_id.map(String::from),
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

        // bob only supports the Mcp binding, so only Mcp-binding capabilities
        // (vision-mcp, docling-mcp, etc.) should show.
        let mcp_only_caps: HashSet<&str> = ["vision-mcp", "docling-mcp"].into_iter().collect();
        let bob_only: HashSet<String> = ["bob".to_string()].into_iter().collect();
        let filtered = Revaluator::for_capabilities(&discovery, &bob_only);
        assert!(
            filtered.iter().all(|r| matches!(
                r,
                Recommendation::Capability { capability_type, .. }
                    if mcp_only_caps.contains(capability_type.as_str())
            )),
            "with only bob (Mcp-only) selected, only Mcp-binding capabilities should be recommended"
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
    async fn select_models_choose_different_models_surfaces_partial_fit_candidates() {
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

        let chosen = SetupCommands::select_models(&mut ctx, &discovery, &selected_caps)
            .await
            .unwrap();

        assert_eq!(
            chosen,
            HashSet::from(["granite-4.2-30b".to_string()]),
            "should have picked exactly the manually-chosen partial-fit model"
        );
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
                provider_id: Some("ollama".to_string()),
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
        let _ = SetupCommands::run(&mut ctx, false, true).await;
        // Wizard should complete without error even with no recommendations
        // (it will show info messages)
    }

    #[tokio::test]
    async fn run_auto_with_no_recommendations_shows_info() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        let result = SetupCommands::run(&mut ctx, true, true).await;
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
            1,
            "the missing binary should have been excluded"
        );
        assert!(items[0].contains("found"));
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
}
