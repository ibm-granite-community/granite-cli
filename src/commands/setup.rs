// Standard
use std::collections::{HashMap, HashSet};
use std::future::Future;

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use anyhow::Result;

// Local
use crate::capabilities::{BindingType, CAPABILITY_REGISTRY, Dependency, ModelRequirement};
use crate::commands::capability::CapabilityCommands;
use crate::commands::launcher::LauncherCommands;
use crate::commands::model::ModelCommands;
use crate::config::recommended_config;
use crate::dependency::{Configured, Requirement};
use crate::launchers::LAUNCHER_REGISTRY;
use crate::models::{ContextFit, MODEL_REGISTRY, ModelFunction, ModelMetadata, ModelVariant};
use crate::providers::{HealthStatus, PROVIDER_REGISTRY, Provider};
use crate::utils::hardware::HardwareProfile;

use_channel!("SETUP");

/*-- public --*/

/// A single recommendation produced during discovery.
enum Recommendation {
    Provider {
        provider_type: &'static str,
        /// The id the provider is saved under: its type for one found on
        /// `local` or a hosted service, `<type>-<host>` on another host, or
        /// the configured id.
        provider_id: String,
        provider_name: String,
        health_healthy: bool,
        health_error: Option<String>,
        discovered_base_url: Option<String>,
        /// The models the provider serves at `discovered_base_url`, as its
        /// listing reports them. Empty when it was not asked, cannot list
        /// models, or the listing failed.
        served_models: Vec<crate::providers::ProviderModel>,
    },
    Model {
        model_id: String,
        family: String,
        version: String,
        size: String,
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
struct DiscoveryResult {
    pub(crate) recommendations: Vec<Recommendation>,
    /// Every unconfigured model with at least a partial hardware fit, one
    /// entry per catalog model_id (not deduplicated by family/version like
    /// `recommendations` is). Used by the "choose different models" escape
    /// hatch so a user can see and pick models that don't fully fit.
    all_model_candidates: Vec<Recommendation>,
    // What was configured before the run. The wizard no longer stops when
    // there is nothing new, since it always offers hosts; tests read some.
    #[allow(dead_code)]
    configured_provider_ids: Vec<String>,
    #[cfg_attr(not(test), allow(dead_code))]
    configured_model_ids: Vec<String>,
    #[cfg_attr(not(test), allow(dead_code))]
    configured_launcher_ids: Vec<String>,
    #[allow(dead_code)]
    configured_capability_ids: Vec<String>,
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
        let providers = Self::discover_providers(ctx, None).await;
        Self::run_with_providers(ctx, profile, providers).await
    }

    /// The rest of discovery, given the provider recommendations and the
    /// configured provider ids from `discover_providers`. Split out so tests
    /// can state which providers are healthy: the health checks reach the
    /// network, and which providers answer decides the models recommended.
    async fn run_with_providers(
        ctx: &crate::AppContext,
        profile: &HardwareProfile,
        (provider_recs, configured_providers): (Vec<Recommendation>, Vec<String>),
    ) -> DiscoveryResult {
        // Models are judged by the providers setup can use: those configured
        // and those discovery found healthy, each on its own hardware.
        let mut model_providers = configured_providers.clone();
        for rec in &provider_recs {
            if let Recommendation::Provider {
                provider_id,
                health_healthy: true,
                ..
            } = rec
                && !model_providers.contains(provider_id)
            {
                model_providers.push(provider_id.clone());
            }
        }
        let model_recs = Self::discover_models(ctx, &model_providers, profile);
        let all_model_candidates =
            Self::discover_all_model_candidates(ctx, &model_providers, profile);
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
            configured_model_ids: ctx.config().models.keys().cloned().collect(),
            configured_launcher_ids: configured_launchers,
            configured_capability_ids: configured_capabilities,
        }
    }

    // -- Provider discovery --------------------------------------------------

    /// Health-checks every provider type that is not configured, at its
    /// default base URLs, or, for a type the user runs, at those URLs with
    /// the host replaced by `host` when one is given. Hosted types are
    /// checked at their own URLs. Returns a recommendation per type, with
    /// the first URL that answered, and the configured provider ids.
    async fn discover_providers(
        ctx: &crate::AppContext,
        host: Option<&str>,
    ) -> (Vec<Recommendation>, Vec<String>) {
        let configured_ids: HashSet<&str> =
            ctx.config().providers.keys().map(|s| s.as_str()).collect();
        let mut configured: Vec<String> = Vec::new();
        let mut recommendations: Vec<Recommendation> = Vec::new();

        for (provider_type, metadata) in PROVIDER_REGISTRY.entries() {
            if configured_ids.contains(provider_type) {
                configured.push(provider_type.to_string());
                continue;
            }

            let default_config = PROVIDER_REGISTRY.default_config(provider_type);
            let runs_on_hardware = crate::config::host::HostLink::default_for(provider_type)
                == crate::config::host::HostLink::Local;
            let default_base_urls = ctx.default_base_urls(provider_type);
            let base_urls = match host {
                Some(host) if runs_on_hardware => base_urls_at(&default_base_urls, host),
                _ => default_base_urls,
            };
            let (discovered_base_url, health_error) =
                first_healthy_base_url(&base_urls, |base_url| {
                    let mut config = default_config.clone();
                    async move {
                        if !config.is_object() {
                            return Err("Default provider config is not an object".to_string());
                        }
                        config["base_url"] = serde_json::Value::String(base_url);
                        let provider = PROVIDER_REGISTRY
                            .construct(provider_type, provider_type, &config)
                            .map_err(|e| format!("Could not construct provider: {e}"))?;
                        Self::run_health_check(&*provider)
                            .await
                            .map_err(|e| format!("Health check failed: {e}"))
                    }
                })
                .await;

            recommendations.push(Recommendation::Provider {
                provider_type,
                provider_id: provider_type.to_string(),
                provider_name: metadata.name.clone(),
                health_healthy: discovered_base_url.is_some(),
                health_error,
                discovered_base_url,
                served_models: Vec::new(),
            });
        }

        configured.sort();
        recommendations.sort_by_key(display_name);
        (recommendations, configured)
    }

    /// Asks each provider in `recommendations` that answered its health
    /// check which models it serves, at the address it answered on, and
    /// records them in its `served_models`. A provider whose type cannot
    /// list models, or whose listing fails, keeps an empty list.
    async fn list_served_models(recommendations: &mut [Recommendation]) {
        for rec in recommendations {
            let Recommendation::Provider {
                provider_type,
                discovered_base_url: Some(base_url),
                served_models,
                ..
            } = rec
            else {
                continue;
            };
            let mut config = PROVIDER_REGISTRY.default_config(provider_type);
            if !config.is_object() {
                continue;
            }
            config["base_url"] = base_url.clone().into();
            let Ok(provider) = PROVIDER_REGISTRY.construct(provider_type, provider_type, &config)
            else {
                continue;
            };
            if !provider.discover_models() {
                continue;
            }
            match provider.list_models().await {
                Ok(models) => *served_models = models,
                Err(error) => alog_channel!(
                    MessageLevel::Debug,
                    "Listing the models '{provider_type}' serves failed: {error}"
                ),
            }
        }
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
        let configured_ids: HashSet<&str> =
            ctx.config().models.keys().map(|s| s.as_str()).collect();

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
        let configured_ids: HashSet<&str> =
            ctx.config().models.keys().map(|s| s.as_str()).collect();

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
        let (variant, fit) = best_variant(md, profile, configured_provider_ids, ctx)?;
        if require_full_fit && fit != ContextFit::Full {
            return None;
        }
        let can_run_by = Self::find_can_run_providers(&variant, configured_provider_ids, ctx);
        Some(Recommendation::Model {
            model_id: model_id.to_string(),
            family: md.family.clone(),
            version: md.version.clone(),
            size: format_size(md.size),
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
            .filter(|pid| {
                construct_provider(pid, ctx)
                    .is_some_and(|p| p.can_run_model(&variant.format, &variant.precision))
            })
            .cloned()
            .collect()
    }

    // -- Launcher discovery --------------------------------------------------

    async fn discover_launchers(ctx: &crate::AppContext) -> (Vec<Recommendation>, Vec<String>) {
        let configured_ids: HashSet<&str> =
            ctx.config().launchers.keys().map(|s| s.as_str()).collect();
        let mut configured: Vec<String> = Vec::new();
        let mut recommendations: Vec<Recommendation> = Vec::new();

        for (launcher_type, metadata) in LAUNCHER_REGISTRY.entries() {
            if configured_ids.contains(launcher_type) {
                configured.push(launcher_type.to_string());
                continue;
            }

            // Construct a transient instance with default config
            let default_config = LAUNCHER_REGISTRY.default_config(launcher_type);
            match LAUNCHER_REGISTRY.construct(launcher_type, launcher_type, &default_config) {
                Ok(launcher) => recommendations.push(Recommendation::Launcher {
                    launcher_type: launcher_type.to_string(),
                    launcher_name: metadata.name.clone(),
                    binary_path: ctx.launcher_path(launcher_type, || {
                        launcher
                            .validate_command()
                            .ok()
                            .map(|path| path.to_string_lossy().to_string())
                    }),
                }),
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
        let configured_ids: Vec<&str> = ctx
            .config()
            .capabilities
            .keys()
            .map(|s| s.as_str())
            .collect();
        let configured: Vec<String> = configured_ids.iter().copied().map(String::from).collect();

        let mut recommendations: Vec<Recommendation> = Vec::new();

        for (capability_type, metadata) in CAPABILITY_REGISTRY.entries() {
            if configured_ids.contains(&capability_type) || !metadata.auto_configurable {
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
                if let Recommendation::Provider { provider_id, .. } = r {
                    providers_that_can_run.contains(provider_id)
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
        let all_requirements = model_requirements(selected_cap_types);

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

/// The best variant of `model` for setup: ranked across `provider_ids`,
/// each provider's variants estimated on the hardware it runs on, as
/// `setup --auto` ranks them. With no provider to judge by, as on a machine
/// where discovery found none, the best fit on `profile`, this machine.
fn best_variant(
    model: &ModelMetadata,
    profile: &crate::utils::hardware::HardwareProfile,
    provider_ids: &[String],
    ctx: &crate::AppContext,
) -> Option<(ModelVariant, ContextFit)> {
    if provider_ids.is_empty() {
        return best_variant_among(
            model.variants.iter(),
            model.context_length,
            &model.architecture,
            &model.native_dtype,
            profile,
        );
    }
    rank_variants_for_providers(model, model.variants.iter(), profile, provider_ids, ctx)
        .into_iter()
        .next()
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

/// The hosts other than `local` that `variant` would run on, through the
/// selected providers that can run it, as " (host: spark)". Empty when they
/// all run on `local`, which is what the list shows without a note.
fn variant_host_note(
    variant: &ModelVariant,
    selected_providers: &HashSet<String>,
    ctx: &crate::AppContext,
) -> String {
    use crate::config::host::HostLink;
    let mut hosts: Vec<String> = selected_providers
        .iter()
        .filter(|id| provider_can_run(id, variant, ctx))
        .filter_map(|id| match provider_link(id, ctx) {
            HostLink::Host(host) => Some(host),
            HostLink::Unspecified => Some("not specified".to_string()),
            HostLink::Local => None,
        })
        .collect();
    hosts.sort();
    hosts.dedup();
    if hosts.is_empty() {
        String::new()
    } else {
        format!(" (host: {})", hosts.join(", "))
    }
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

/// The host link of provider `provider_id`: its saved link when it is
/// configured, the host the wizard found it on, or otherwise the default for
/// the type it is named after: `local`, or "not specified" for a hosted
/// type.
fn provider_link(provider_id: &str, ctx: &crate::AppContext) -> crate::config::host::HostLink {
    use crate::config::host::HostLink;
    if let Some(pc) = ctx.config().get_provider(provider_id) {
        return pc.host_link();
    }
    if let Some(found) = ctx.found_provider(provider_id) {
        return found.host_link();
    }
    HostLink::default_for(provider_id)
}

/// Whether provider `provider_id`, configured or about to be added under
/// its type's name, is of a hosted type: one whose hardware is not
/// specified by default. An openai-compatible provider is not, since it
/// usually points at a server the user runs.
fn provider_is_hosted(provider_id: &str, ctx: &crate::AppContext) -> bool {
    let provider_type = ctx
        .config()
        .get_provider(provider_id)
        .map_or(provider_id, |pc| pc.provider_type.as_str());
    crate::config::host::HostLink::default_for(provider_type)
        == crate::config::host::HostLink::Unspecified
}

/// Whether the provider `provider_id` can run `variant`. A provider in
/// `ctx.config().providers` is constructed from its own config. Any other id is
/// taken as a provider type found by discovery and constructed with that
/// type's default config. Same lookup as `candidate_variants`.
fn provider_can_run(provider_id: &str, variant: &ModelVariant, ctx: &crate::AppContext) -> bool {
    construct_provider(provider_id, ctx)
        .is_some_and(|p| p.can_run_model(&variant.format, &variant.precision))
}

/// The provider `provider_id`: built from its config when it is configured,
/// otherwise as a provider of that type with the type's default config.
fn construct_provider(
    provider_id: &str,
    ctx: &crate::AppContext,
) -> Option<Box<dyn crate::providers::Provider>> {
    match ctx
        .config()
        .get_provider(provider_id)
        .or_else(|| ctx.found_provider(provider_id))
    {
        Some(pc) => PROVIDER_REGISTRY.construct(&pc.provider_type, &pc.provider_id, &pc.config),
        None => {
            let default_config = PROVIDER_REGISTRY.default_config(provider_id);
            PROVIDER_REGISTRY.construct(provider_id, provider_id, &default_config)
        }
    }
    .ok()
}

/// A model a provider found by the wizard serves, which is no catalog model,
/// picked by the user: the provider and the name it serves the model under.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServedPick {
    provider_id: String,
    wire_id: String,
}

/// What the providers found by the wizard serve, split by whether each
/// listed model is a catalog model.
#[derive(Default)]
struct ServedModels<'a> {
    /// Catalog model id -> each provider serving it and the variant it
    /// serves, in provider id order.
    catalog: HashMap<String, Vec<(String, ModelVariant)>>,
    /// The listed models that are no catalog model, as (provider id, model),
    /// in provider id then listing order.
    others: Vec<(String, &'a crate::providers::ProviderModel)>,
}

impl ServedModels<'_> {
    /// The variant of catalog model `model_id` one of `providers` serves,
    /// with that provider, the first by provider id.
    fn served_variant(
        &self,
        model_id: &str,
        providers: &HashSet<String>,
    ) -> Option<&(String, ModelVariant)> {
        self.catalog
            .get(model_id)?
            .iter()
            .find(|(provider_id, _)| providers.contains(provider_id))
    }
}

/// The models the providers in `discovery` serve. A listed model is catalog
/// model M in variant V when the provider can run V and serves it under
/// `wire_name` for M and V, the name sync matches a configured model by. So
/// a catalog model written in that variant is not added again by sync.
fn served_models(discovery: &DiscoveryResult) -> ServedModels<'_> {
    let mut listings: Vec<(&str, &str, &Vec<crate::providers::ProviderModel>)> = discovery
        .recommendations
        .iter()
        .filter_map(|rec| match rec {
            Recommendation::Provider {
                provider_type,
                provider_id,
                served_models,
                ..
            } if !served_models.is_empty() => {
                Some((provider_id.as_str(), *provider_type, served_models))
            }
            _ => None,
        })
        .collect();
    listings.sort_by_key(|(provider_id, _, _)| *provider_id);

    // In id order, so a name two catalog models share goes to the same one
    // on every run.
    let mut catalog_entries: Vec<(&str, ModelMetadata)> =
        MODEL_REGISTRY.entries().into_iter().collect();
    catalog_entries.sort_by_key(|(model_id, _)| *model_id);
    let mut served = ServedModels::default();
    for (provider_id, provider_type, listed) in listings {
        let default_config = PROVIDER_REGISTRY.default_config(provider_type);
        let Ok(provider) = PROVIDER_REGISTRY.construct(provider_type, provider_id, &default_config)
        else {
            continue;
        };
        // Wire name -> (catalog model id, variant), for every variant the
        // provider can run.
        let mut catalog_names: HashMap<String, (&str, &ModelVariant)> = HashMap::new();
        for (model_id, md) in &catalog_entries {
            for variant in &md.variants {
                if provider.can_run_model(&variant.format, &variant.precision) {
                    catalog_names
                        .entry(wire_name(&*provider, model_id, variant))
                        .or_insert((*model_id, variant));
                }
            }
        }
        for model in listed {
            match catalog_names.get(&model.id) {
                Some((model_id, variant)) => served
                    .catalog
                    .entry(model_id.to_string())
                    .or_default()
                    .push((provider_id.to_string(), (*variant).clone())),
                None => served.others.push((provider_id.to_string(), model)),
            }
        }
    }
    served
}

/// The name `provider` serves catalog model `model_id` under in `variant`:
/// its alias for that variant, otherwise the catalog id. Sync matches a
/// configured model by the same name.
fn wire_name(provider: &dyn Provider, model_id: &str, variant: &ModelVariant) -> String {
    provider
        .model_alias(model_id.to_string(), Some(variant))
        .unwrap_or_else(|| model_id.to_string())
}

/// The requirement of every model slot of each capability type in
/// `capability_types` that a model is chosen for.
fn model_requirements(capability_types: &HashSet<String>) -> Vec<ModelRequirement> {
    capability_types
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
        .collect()
}

/// Whether `model` meets `req`, leaving out a model with a multimodal
/// function `req` does not ask for, as `admits_for_recommendation` does for
/// catalog models.
fn admits_served(req: &ModelRequirement, model: &(dyn crate::models::Model + 'static)) -> bool {
    if !req.admits_instance(model) {
        return false;
    }
    let req_wants_multimodal = req
        .supported_functions
        .iter()
        .any(|f| MULTIMODAL_FUNCTIONS.contains(f));
    req_wants_multimodal
        || !model
            .supported_functions()
            .iter()
            .any(|f| MULTIMODAL_FUNCTIONS.contains(f))
}

/// The model sync would write for `model`, served by `provider_id`, built
/// as an instance so a capability's requirement can be checked against it.
fn served_instance(
    provider_id: &str,
    model: &crate::providers::ProviderModel,
) -> Option<Box<dyn crate::models::Model>> {
    // The wizard adds a provider it found under its type's name.
    let config = crate::models::discovery::discovered_model_config(
        provider_id,
        provider_id,
        model,
        model.id.clone(),
    );
    MODEL_REGISTRY
        .construct(&config.model_type, &config.model_id, &config.config)
        .ok()
}

/// The id of the configured model `pick` is: the entry discovery manages
/// for that name on that provider.
fn served_instance_id(ctx: &crate::AppContext, pick: &ServedPick) -> Option<String> {
    ctx.config()
        .models
        .iter()
        .find(|(_, mc)| {
            mc.discovered.as_ref().is_some_and(|discovered| {
                discovered.provider_id == pick.provider_id
                    && discovered.provider_model_id == pick.wire_id
            })
        })
        .map(|(id, _)| id.clone())
}

/// One capability's resolution against its `RecommendedCapability`: which
/// model+variant fills each of the capability's `Dependency::Model` config_key
/// slots. Only present when every *required* slot resolved.
struct ResolvedCapability {
    capability_type: String,
    /// config_key -> (model_id, variant)
    slots: HashMap<String, (String, ModelVariant)>,
}

/// How the providers question labels a provider added at an address that
/// did not answer.
const ADDED_ANYWAY: &str = "added, not answering; models not listed";

/// The provider types offered after probing a host: the types the user
/// runs, or, for the hosted services (`hosted`), the hosted types and
/// openai-compatible. Sorted.
fn offered_types(hosted: bool) -> Vec<&'static str> {
    use crate::config::host::HostLink;
    let mut types: Vec<&'static str> = PROVIDER_REGISTRY
        .entries()
        .keys()
        .copied()
        .filter(|t| {
            let runs = HostLink::default_for(t) == HostLink::Local;
            if hosted {
                !runs || *t == "openai-compatible"
            } else {
                runs
            }
        })
        .collect();
    types.sort();
    types
}

/// A host the wizard looks for providers on in this run.
struct HostTarget {
    id: String,
    address: String,
    link: crate::config::host::HostLink,
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

async fn first_healthy_base_url<F, Fut>(
    base_urls: &[String],
    mut probe: F,
) -> (Option<String>, Option<String>)
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = std::result::Result<HealthStatus, String>>,
{
    if base_urls.is_empty() {
        return (
            None,
            Some("No default base URLs are available for discovery".to_string()),
        );
    }

    let mut failures = Vec::new();
    for base_url in base_urls {
        match probe(base_url.clone()).await {
            Ok(status) if status.healthy => {
                return (Some(base_url.clone()), status.error);
            }
            Ok(status) => failures.push(format!(
                "{base_url}: {}",
                status.error.unwrap_or_else(|| "unhealthy".to_string())
            )),
            Err(error) => failures.push(format!("{base_url}: {error}")),
        }
    }

    (None, Some(failures.join("; ")))
}

/// `base_urls` with the host of each replaced by `host`, keeping scheme,
/// port and path, in order and without repeats. A URL that does not parse,
/// or whose host cannot be `host`, is left out.
fn base_urls_at(base_urls: &[String], host: &str) -> Vec<String> {
    let mut at: Vec<String> = Vec::new();
    for moved in base_urls
        .iter()
        .filter_map(|url| crate::config::host::url_at(url, host))
    {
        if !at.contains(&moved) {
            at.push(moved);
        }
    }
    at
}

/// The providers setup resolves models against: the healthy ones discovery
/// found, then the configured ones, sorted. Discovery health-checks only
/// providers that are not configured, so configured providers are added
/// without a health check.
fn setup_provider_ids(healthy_provider_types: &[String], ctx: &crate::AppContext) -> Vec<String> {
    let mut provider_ids: Vec<String> = healthy_provider_types.to_vec();
    let mut configured: Vec<String> = ctx.config().providers.keys().cloned().collect();
    configured.sort();
    for id in configured {
        if !provider_ids.contains(&id) {
            provider_ids.push(id);
        }
    }
    provider_ids
}

/// Healthy provider types found among a discovery pass's recommendations.
/// Discovery skips configured providers, so none of them is in the result;
/// `setup_provider_ids` adds them.
fn healthy_provider_types(discovery: &DiscoveryResult) -> Vec<String> {
    discovery
        .recommendations
        .iter()
        .filter_map(|r| match r {
            Recommendation::Provider {
                provider_id,
                health_healthy: true,
                ..
            } => Some(provider_id.clone()),
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
    ctx.config()
        .get_launcher(launcher_id)
        .map(|l| l.launcher_type.clone())
        .unwrap_or_else(|| launcher_id.to_string())
}

/// The id `LauncherCommands::setup` just wrote to or updated in
/// `ctx.config().launchers`, found by diffing against a `before` snapshot
/// taken just before the call -- necessary because that wizard lets the
/// user free-type an instance name, so the resulting id isn't known ahead
/// of time. `None` when nothing changed (the user declined an overwrite
/// confirmation).
fn changed_launcher_id(
    before: &std::collections::HashMap<String, crate::config::LauncherConfig>,
    ctx: &crate::AppContext,
) -> Option<String> {
    ctx.config()
        .launchers
        .iter()
        .find_map(|(id, cfg)| match before.get(id) {
            None => Some(id.clone()),
            Some(prev) => (serde_json::to_value(prev).ok() != serde_json::to_value(cfg).ok())
                .then(|| id.clone()),
        })
}

/// The id `CapabilityCommands::setup` just wrote to or updated in
/// `ctx.config().capabilities`, found by diffing against a `before` snapshot
/// taken just before the call -- necessary because that wizard lets the
/// user free-type an instance name, so the resulting id isn't known ahead
/// of time. `None` when nothing changed (the user declined an overwrite
/// confirmation).
fn changed_capability_id(
    before: &std::collections::HashMap<String, crate::config::CapabilityConfig>,
    ctx: &crate::AppContext,
) -> Option<String> {
    ctx.config()
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
            // then rank every one that fits on the hardware of a provider
            // that can run it, best first.
            let ranked = rank_variants_for_providers(
                &md,
                md.variants.iter().filter(|v| admits_variant(rec_model, v)),
                hardware,
                provider_ids,
                ctx,
            );

            for (variant, fit) in ranked {
                // Compute effective context length from the fit
                let effective_context = match fit {
                    ContextFit::Full => md.context_length,
                    ContextFit::Partial(n) => n,
                    ContextFit::None => continue, // Already excluded by the ranking
                };

                // Check min_context_length gate (against the context that
                // fits on that provider's hardware, not the native one)
                if effective_context < set.min_context_length.unwrap_or(0) {
                    continue;
                }

                return Some((model_id, variant));
            }
        }
    }

    None
}

/// Whether `variant` passes `rec_model`'s format and precision allow-lists:
/// AND between the two fields, OR within each, empty meaning any.
fn admits_variant(
    rec_model: &recommended_config::RecommendedModel,
    variant: &ModelVariant,
) -> bool {
    (rec_model.variant_formats.is_empty()
        || rec_model
            .variant_formats
            .iter()
            .any(|f| f.matches(&variant.format)))
        && (rec_model.variant_precisions.is_empty()
            || rec_model
                .variant_precisions
                .iter()
                .any(|p| p.matches(&variant.precision)))
}

/// The figures fit is estimated on for `provider_id`: its host's devices,
/// as the provider uses them (see `AppContext::host_fit_profile`), through
/// its link when it is configured, otherwise the link setup gives it. `local` is the
/// profile the run already read. An error, with the reason, when the
/// provider has no hardware profile or its figures cannot be looked up, so
/// fit cannot be estimated for it.
fn provider_hardware(
    provider_id: &str,
    local: &HardwareProfile,
    ctx: &crate::AppContext,
) -> std::result::Result<HardwareProfile, String> {
    use crate::config::host::{HostLink, LOCAL_HOST_ID};
    let using = match ctx
        .config()
        .get_provider(provider_id)
        .or_else(|| ctx.found_provider(provider_id))
    {
        Some(pc) => crate::providers::DeviceUse::of(&pc.provider_type, &pc.config),
        None => crate::providers::DeviceUse::of(provider_id, &serde_json::Value::Null),
    };
    match provider_link(provider_id, ctx) {
        // With one device, `local` is the figures this run read, which may
        // be a detection the user kept for this run only. With several, fit
        // uses the saved figures of every device.
        HostLink::Local if ctx.config().host_hardware(LOCAL_HOST_ID).len() > 1 => {
            ctx.host_fit_profile(LOCAL_HOST_ID, using)
        }
        HostLink::Local => Ok(local.clone()),
        HostLink::Host(id) => ctx.host_fit_profile(&id, using),
        HostLink::Unspecified => Err(format!("no host for '{provider_id}'")),
    }
}

/// Rank `variants` of `md` best first. Each variant is estimated once for
/// every provider in `provider_ids` that can run it, against that provider's
/// hardware profile, and keeps its best fit. A variant no provider can run,
/// or that fits on none of their hardware, is left out. Ties in fit go to the
/// larger variant. When every provider runs on `local`, this ranks variants
/// by their fit on this machine, as setup did before providers were linked.
fn rank_variants_for_providers<'a>(
    md: &ModelMetadata,
    variants: impl Iterator<Item = &'a ModelVariant>,
    local: &HardwareProfile,
    provider_ids: &[String],
    ctx: &crate::AppContext,
) -> Vec<(ModelVariant, ContextFit)> {
    let fit_rank = |fit: &ContextFit| fit_order(fit).0;
    // Each provider is built once and kept for every variant.
    let hardware: Vec<(Box<dyn crate::providers::Provider>, Option<HardwareProfile>)> =
        provider_ids
            .iter()
            .filter_map(|id| {
                construct_provider(id, ctx).map(|p| (p, provider_hardware(id, local, ctx).ok()))
            })
            .collect();

    let mut ranked: Vec<(ModelVariant, ContextFit)> = variants
        .filter_map(|v| {
            hardware
                .iter()
                .filter(|(p, _)| p.can_run_model(&v.format, &v.precision))
                .filter_map(|(_, profile)| profile.as_ref())
                .map(|profile| {
                    crate::models::context_fit::estimate(
                        md.context_length,
                        &md.architecture,
                        &md.native_dtype,
                        v,
                        profile,
                    )
                })
                .max_by_key(fit_order)
                .filter(|fit| *fit != ContextFit::None)
                .map(|fit| (v.clone(), fit))
        })
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

/// How good a fit is, as an ordering key: Full above Partial above None,
/// and a larger fitting context above a smaller one.
fn fit_order(fit: &ContextFit) -> (u8, u64) {
    match fit {
        ContextFit::Full => (2, 0),
        ContextFit::Partial(context) => (1, *context),
        ContextFit::None => (0, 0),
    }
}

/// The variants in `set` that a provider in `provider_ids` can run but whose
/// fit cannot be estimated, because the variant states no size (always so
/// on a hosted provider) or the provider has no hardware profile. Setup
/// never picks these on its own; `setup --auto` lists them so the user can
/// choose one by hand. There is one entry for each model, provider and
/// reason, sorted, naming the variants: "<model> on <provider> (<variant>,
/// ...): <reason>".
fn fit_unavailable_candidates(
    set: &recommended_config::RecommendedModelSet,
    local: &HardwareProfile,
    provider_ids: &[String],
    ctx: &crate::AppContext,
) -> Vec<String> {
    let mut found: std::collections::BTreeMap<(String, String, String), Vec<String>> =
        std::collections::BTreeMap::new();
    for rec_model in &set.models {
        for model_id in matching_catalog_ids(&rec_model.model) {
            let Some(md) = MODEL_REGISTRY.get(&model_id) else {
                continue;
            };
            for v in md.variants.iter().filter(|v| admits_variant(rec_model, v)) {
                for id in provider_ids
                    .iter()
                    .filter(|id| provider_can_run(id, v, ctx))
                {
                    let reason = match provider_hardware(id, local, ctx) {
                        _ if v.size_gb.is_none() && provider_is_hosted(id, ctx) => {
                            "hosted provider doesn't specify size".to_string()
                        }
                        Err(reason) => reason,
                        Ok(_) if v.size_gb.is_none() => "no size".to_string(),
                        Ok(_) => continue,
                    };
                    let variant = if v.precision.is_empty() {
                        v.format.clone()
                    } else {
                        format!("{} / {}", v.format, v.precision)
                    };
                    let variants = found
                        .entry((model_id.clone(), id.clone(), reason))
                        .or_default();
                    if !variants.contains(&variant) {
                        variants.push(variant);
                    }
                }
            }
        }
    }
    found
        .into_iter()
        .map(|((model_id, id, reason), variants)| {
            format!("{model_id} on {id} ({}): {reason}", variants.join(", "))
        })
        .collect()
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
            &ctx.config().recommended_configs,
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

pub(crate) struct SetupCommands;

impl SetupCommands {
    /// Entry point for `granite-cli setup`.
    /// `pull`: `Some(true)` => pull without prompting, `Some(false)` => skip
    /// pull, `None` => prompt in interactive mode / no pull in auto mode.
    pub(crate) async fn run(
        ctx: &mut crate::AppContext,
        auto: bool,
        pull: Option<bool>,
    ) -> Result<()> {
        if auto {
            Self::run_auto(ctx, pull).await
        } else {
            Self::run_wizard(ctx, pull).await
        }
    }

    /// Run the interactive wizard.
    async fn run_wizard(ctx: &mut crate::AppContext, pull: Option<bool>) -> Result<()> {
        Self::run_wizard_with(ctx, pull, crate::utils::hardware::detect_hardware).await
    }

    /// The wizard, with `detect` reading this machine's first device, so
    /// tests never read the machine they run on.
    ///
    /// It collects every input first (local checks and repairs, launchers,
    /// hosts, the providers on them and the models they serve), then builds
    /// the plan (capabilities, models, providers, variants), and writes it
    /// at the end. Nothing other than confirmed repairs is written before
    /// then, so quitting leaves the configuration as it was.
    async fn run_wizard_with(
        ctx: &mut crate::AppContext,
        pull: Option<bool>,
        detect: impl FnOnce() -> HardwareProfile,
    ) -> Result<()> {
        ctx.ui.info("=== granite-cli Setup Wizard ===\n");

        // Read once and threaded through the selection phases (not just
        // discovery) so `select_capabilities` resolves recommended
        // capabilities against the same hardware profile discovery used.
        let can_prompt = crate::commands::HardwareCommands::can_prompt(ctx);
        crate::commands::HardwareCommands::check_local(ctx, detect, can_prompt)?;
        let hardware = ctx
            .hardware_profile(crate::config::hardware::LOCAL_HARDWARE_ID)
            .map_err(|e| anyhow::anyhow!("hardware profile 'local': {e}"))?;
        ctx.ensure_local_host(|| hardware.clone())?;
        // Discovery ranks models over the configured providers too, on the
        // hardware each is linked to.
        Self::repair_provider_links(ctx, can_prompt).await?;

        // From here, the run keeps what it configures in memory until the
        // write phase.
        ctx.config_mut().defer_writes();

        let ui = std::sync::Arc::clone(&ctx.ui);
        ui.info("Discovering available components...\n");
        // Providers are probed once the user has chosen the hosts, below;
        // this pass finds the launchers.
        let mut configured_providers: Vec<String> =
            ctx.config().providers.keys().cloned().collect();
        configured_providers.sort();
        let discovery =
            Discover::run_with_providers(ctx, &hardware, (Vec::new(), configured_providers)).await;

        // Phase 1: Launchers selection (show all detected launchers)
        let selected_launchers = Self::select_launchers(ctx, &discovery).await?;

        // Phase 2: The hosts to run models on. Providers configured there are
        // listed, providers of the types the user runs are probed at each
        // host's address, and models are ranked on each provider's host.
        let (targets, hosted) = Self::select_hosts(ctx)?;
        let (mut providers, configured_here) = Self::probe_hosts(ctx, &targets, hosted).await?;
        // Configured providers on hosts left out of this run take no part.
        providers.retain(|rec| match rec {
            Recommendation::Provider { provider_id, .. } => {
                ctx.config().get_provider(provider_id).is_none()
                    || configured_here.contains(provider_id)
            }
            _ => true,
        });
        let discovery =
            Discover::run_with_providers(ctx, &hardware, (providers, configured_here)).await;

        // Phase 3: Capabilities selection (filtered by what the selected
        // launchers can actually bind, pre-selected only where a
        // recommendation actually resolves on that hardware)
        let selected_caps =
            Self::select_capabilities(ctx, &discovery, &selected_launchers, &hardware).await?;

        // Phase 4: Models selection (filtered by capability requirements):
        // catalog models, and models the providers found serve that are not
        // in the catalog.
        let (selected_models, served_picks) =
            Self::select_models(ctx, &discovery, &selected_caps, &selected_launchers).await?;

        // Phase 5: Providers selection (those found, filtered by model
        // compatibility, with the ones serving a selected model checked).
        // Each new provider is saved with the host and the address it was
        // found at.
        let selected_providers =
            Self::select_providers(ctx, &discovery, &selected_models, &served_picks).await?;
        Self::configure_new_providers(ctx, &discovery, &selected_providers);

        // Phase 4.5: Variant selection (limited to formats the selected
        // providers can actually run, with a VRAM estimate at full context)
        let selected_variants =
            Self::select_variants(ctx, &discovery, &selected_models, &selected_providers).await?;

        // Phase 5: Configuration. Same per-launcher recommended-capability
        // tracking as `run_auto_with_hardware`, so a capability recommended
        // for one selected launcher doesn't also land on another selected
        // launcher just because both happen to support its binding type.
        // Keys are the RESOLVED type (matching what configure_all's enable-loop
        // looks up via `ctx.config().get_launcher(launcher_id).map(|l| l.launcher_type)`).
        let recommended_capability_types_by_launcher: HashMap<String, HashSet<String>> =
            selected_launchers
                .iter()
                .map(|launcher_id| {
                    let launcher_type = resolved_launcher_type(ctx, launcher_id);
                    let types = recommended_config::effective_capabilities(
                        &launcher_type,
                        &recommended_config::BUILTIN_RECOMMENDED_CONFIGS,
                        &ctx.config().recommended_configs,
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
            &served_picks,
            &selected_variants,
            &resolved_capability_models,
            &recommended_capability_types_by_launcher,
        )
        .await?;

        // The write phase: everything the run configured, at once.
        ctx.config_mut().write_deferred()?;

        Self::sync_discovered(ctx, &selected_providers).await;

        // Phase 6: Pull (optional), of the models not on their provider yet.
        let to_pull = Self::not_yet_served(ctx, &discovery, &selected_models);
        match pull {
            Some(false) => {}
            Some(true) => Self::do_pull(ctx, &to_pull).await?,
            None => Self::prompt_pull(ctx, &to_pull).await?,
        }

        // Phase 7: Summary
        let mut configured_models = selected_models.clone();
        configured_models.extend(
            served_picks
                .iter()
                .filter_map(|pick| served_instance_id(ctx, pick)),
        );
        Self::print_summary(
            ctx,
            &selected_caps,
            &selected_launchers,
            &selected_providers,
            &configured_models,
        );

        Ok(())
    }

    /// The models in `selected_models` their configured provider does not
    /// serve yet in their configured variant, by what `discovery` found it
    /// serving. The rest need no pull.
    fn not_yet_served(
        ctx: &crate::AppContext,
        discovery: &DiscoveryResult,
        selected_models: &HashSet<String>,
    ) -> HashSet<String> {
        let served = served_models(discovery);
        selected_models
            .iter()
            .filter(|model_id| {
                let Some(mc) = ctx.config().get_model(model_id) else {
                    return true;
                };
                let Some(on_servers) = served.catalog.get(model_id.as_str()) else {
                    return true;
                };
                !on_servers.iter().any(|(provider_id, variant)| {
                    *provider_id == mc.provider_id
                        && mc.variant.as_deref()
                            == Some(format!("{}/{}", variant.format, variant.precision).as_str())
                })
            })
            .cloned()
            .collect()
    }

    /// Run auto mode — detect, configure everything with defaults.
    async fn run_auto(ctx: &mut crate::AppContext, pull: Option<bool>) -> Result<()> {
        // `setup --auto` never prompts, so a hardware difference is a warning.
        let hardware = crate::commands::HardwareCommands::local_for_fit(ctx, false)?;
        Self::run_auto_with_hardware(ctx, &hardware, pull).await
    }

    /// Hardware-aware variant of `run_auto` for testability.
    async fn run_auto_with_hardware(
        ctx: &mut crate::AppContext,
        hardware: &crate::utils::hardware::HardwareProfile,
        pull: Option<bool>,
    ) -> Result<()> {
        // New providers are linked to `local` or "not specified" by type.
        ctx.ensure_local_host(|| hardware.clone())?;
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
            &[],
            &selection.variants,
            &selection.resolved_capability_models,
            &selection.recommended_capability_types_by_launcher,
        )
        .await?;

        Self::sync_discovered(ctx, &selection.providers).await;

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
    /// and the providers already in `ctx.config().providers`. A capability in a
    /// launcher's config is resolved only when the launcher supports one of
    /// the capability's binding types, and only when `ctx.config().capabilities`
    /// has no entry with the capability type as its id.
    fn auto_selection(
        ctx: &crate::AppContext,
        launchers: &HashSet<String>,
        healthy_provider_types: &[String],
        hardware: &HardwareProfile,
    ) -> AutoSelection {
        let ui = &*ctx.ui;
        let mut selection = AutoSelection::default();

        let provider_ids = setup_provider_ids(healthy_provider_types, ctx);
        // The candidates whose fit is not available, for each capability that
        // did not resolve.
        let mut unavailable: std::collections::BTreeMap<
            String,
            std::collections::BTreeSet<String>,
        > = std::collections::BTreeMap::new();

        // Iterate launchers in sorted order for determinism, resolving
        // capabilities via the recommended config system.
        let mut sorted_launchers: Vec<String> = launchers.iter().cloned().collect();
        sorted_launchers.sort();

        for launcher_type in &sorted_launchers {
            let effective_caps = recommended_config::effective_capabilities(
                launcher_type,
                &recommended_config::BUILTIN_RECOMMENDED_CONFIGS,
                &ctx.config().recommended_configs,
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
                if ctx.config().get_capability(&rec_cap.capability).is_some() {
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
                } else {
                    // Nothing resolved on its own. A candidate whose fit cannot
                    // be estimated is never picked here; it is named below so
                    // the user can choose it by hand.
                    let notes = unavailable.entry(rec_cap.capability.clone()).or_default();
                    for set in rec_cap.models.values() {
                        notes.extend(fit_unavailable_candidates(
                            set,
                            hardware,
                            &provider_ids,
                            ctx,
                        ));
                    }
                }
            }
        }

        // Once for each capability that no launcher could resolve, though
        // several launchers recommend it.
        unavailable.retain(|cap, notes| !selection.capabilities.contains(cap) && !notes.is_empty());
        for (cap, notes) in &unavailable {
            ui.info(&format!(
                "Capability '{cap}' not selected; fit not available for:"
            ));
            for note in notes {
                ui.info(&format!("  {note}"));
            }
        }
        if !unavailable.is_empty() {
            ui.info("Choose a model for these by hand with `granite-cli model setup`.");
        }

        // Select every healthy provider type and every configured provider.
        // After writing the new providers to config, `configure_all` uses
        // `find_compatible_provider` to pick, for each model, a provider that
        // can run its variant. It does not write configured providers again.
        selection.providers = provider_ids.into_iter().collect();

        selection
    }

    /*-- Selection phases ----------------------------------------------------*/

    const ADD_HOST_LABEL: &'static str = "→ Add a host…";

    /// The hosts step: lists `local`, each configured host and the hosted
    /// services, all checked, and "Add a host…", which runs the `host setup`
    /// steps and returns here with the new host checked. Returns the hosts
    /// checked, `local` first, and whether the hosted services are.
    /// Unchecking a host leaves it out of this run.
    fn select_hosts(ctx: &mut crate::AppContext) -> Result<(Vec<HostTarget>, bool)> {
        use crate::config::host::{HostLink, LOCAL_HOST_ID};
        let mut unchecked: HashSet<String> = HashSet::new();
        let hosted_label = "hosted services";
        loop {
            let mut ids: Vec<String> = ctx
                .config()
                .hosts
                .keys()
                .filter(|id| id.as_str() != LOCAL_HOST_ID)
                .cloned()
                .collect();
            ids.sort();
            ids.insert(0, LOCAL_HOST_ID.to_string());
            let targets: Vec<HostTarget> = ids
                .iter()
                .map(|id| HostTarget {
                    id: id.clone(),
                    address: ctx.config().get_host(id).map_or_else(
                        || crate::config::host::LOCAL_ADDRESS.to_string(),
                        |h| h.address.clone(),
                    ),
                    link: if id == LOCAL_HOST_ID {
                        HostLink::Local
                    } else {
                        HostLink::Host(id.clone())
                    },
                })
                .collect();
            let mut items: Vec<String> = targets
                .iter()
                .map(|t| {
                    format!(
                        "{} — {}, {}",
                        t.id,
                        t.address,
                        crate::commands::HardwareCommands::devices_summary(ctx, &t.id)
                    )
                })
                .collect();
            let mut hosted_types: Vec<&str> = PROVIDER_REGISTRY
                .entries()
                .keys()
                .copied()
                .filter(|t| HostLink::default_for(t) == HostLink::Unspecified)
                .collect();
            hosted_types.sort();
            items.push(format!("{hosted_label} — {}", hosted_types.join(", ")));
            items.push(Self::ADD_HOST_LABEL.to_string());
            let mut defaults: Vec<bool> =
                targets.iter().map(|t| !unchecked.contains(&t.id)).collect();
            defaults.push(!unchecked.contains(hosted_label));
            defaults.push(false);
            let picked = ctx
                .ui
                .multi_select("Hosts to run models on", &items, &defaults)?;
            let add = items.len() - 1;
            let hosted_index = items.len() - 2;
            if picked.contains(&add) {
                unchecked = (0..targets.len())
                    .filter(|i| !picked.contains(i))
                    .map(|i| targets[i].id.clone())
                    .collect();
                if !picked.contains(&hosted_index) {
                    unchecked.insert(hosted_label.to_string());
                }
                // The new host is checked: it is not in `unchecked`. Keeping
                // a configured host as it was returns to the list.
                if crate::commands::HostCommands::add(ctx)?.is_none() {
                    ctx.ui.info("No host added.");
                }
                continue;
            }
            let chosen: Vec<HostTarget> = targets
                .into_iter()
                .enumerate()
                .filter(|(i, _)| picked.contains(i))
                .map(|(_, t)| t)
                .collect();
            let hosted = picked.contains(&hosted_index);
            if chosen.is_empty() && !hosted {
                ctx.ui
                    .warn("No host is checked: no provider will be looked for.");
            }
            return Ok((chosen, hosted));
        }
    }

    /// The providers on `targets`, and on the hosted services when `hosted`:
    /// for each host, the configured providers there, listed and not probed,
    /// and each type the user runs probed at its default URLs with the host
    /// part replaced by the host's address, leaving out a URL a configured
    /// provider uses. What each answering or configured provider serves is
    /// listed. A provider found on `local` keeps its type as its id; one
    /// found on another host gets `<type>-<host id>`, and the user is asked
    /// for an id when that one belongs to a provider at another address.
    /// Each found provider is recorded in `ctx` with its address and host.
    /// Returns a recommendation per provider, configured ones included, and
    /// the ids of the configured providers on `targets`.
    async fn probe_hosts(
        ctx: &mut crate::AppContext,
        targets: &[HostTarget],
        hosted: bool,
    ) -> Result<(Vec<Recommendation>, Vec<String>)> {
        use crate::config::host::HostLink;
        let ui = std::sync::Arc::clone(&ctx.ui);
        let normalize = |url: &str| url.trim_end_matches('/').to_ascii_lowercase();
        let used_urls: HashSet<String> = ctx
            .config()
            .providers
            .values()
            .filter_map(|p| p.config.get("base_url")?.as_str().map(normalize))
            .collect();
        let mut recommendations: Vec<Recommendation> = Vec::new();
        let mut configured_ids: Vec<String> = Vec::new();
        let mut types: Vec<(&'static str, String)> = PROVIDER_REGISTRY
            .entries()
            .iter()
            .map(|(t, m)| (*t, m.name.clone()))
            .collect();
        types.sort();

        let mut groups: Vec<(String, HostLink, Option<String>)> = targets
            .iter()
            .map(|t| (t.address.clone(), t.link.clone(), Some(t.id.clone())))
            .collect();
        if hosted {
            groups.push(("hosted services".to_string(), HostLink::Unspecified, None));
        }
        for (address, link, host_id) in groups {
            ui.info(&format!("\nLooking for providers on {address}..."));
            let mut found_here: Vec<Recommendation> = Vec::new();

            // Configured providers on this host: listed, not probed.
            let mut configured: Vec<crate::config::ProviderConfig> = ctx
                .config()
                .providers
                .values()
                .filter(|p| p.host_link() == link)
                .cloned()
                .collect();
            configured.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
            let mut configured_here: Vec<Recommendation> = Vec::new();
            for pc in configured {
                let Some((provider_type, name)) =
                    types.iter().find(|(t, _)| *t == pc.provider_type).cloned()
                else {
                    continue;
                };
                configured_ids.push(pc.provider_id.clone());
                // A provider the user runs is listed whether or not it mirrors
                // its models into the configuration, so the model list can
                // say what it has. A hosted one is listed only when it
                // mirrors them: its listing is a whole catalog, behind its
                // API key.
                let served_models = match PROVIDER_REGISTRY.construct(
                    &pc.provider_type,
                    &pc.provider_id,
                    &pc.config,
                ) {
                    Ok(provider) if link != HostLink::Unspecified || provider.discover_models() => {
                        match provider.list_models().await {
                            Ok(models) => models,
                            Err(e) => {
                                ui.warn(&format!(
                                    "Could not list what '{}' serves: {e}",
                                    pc.provider_id
                                ));
                                Vec::new()
                            }
                        }
                    }
                    _ => Vec::new(),
                };
                configured_here.push(Recommendation::Provider {
                    provider_type,
                    provider_id: pc.provider_id.clone(),
                    provider_name: name,
                    health_healthy: true,
                    health_error: None,
                    discovered_base_url: pc
                        .config
                        .get("base_url")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    served_models,
                });
            }

            for (provider_type, name) in &types {
                let runs_here = HostLink::default_for(provider_type) == link
                    || (link != HostLink::Unspecified
                        && HostLink::default_for(provider_type) == HostLink::Local);
                if !runs_here {
                    continue;
                }
                let defaults = ctx.default_base_urls(provider_type);
                let urls: Vec<String> = if link == HostLink::Unspecified {
                    defaults
                } else {
                    base_urls_at(&defaults, &address)
                }
                .into_iter()
                .filter(|u| !used_urls.contains(&normalize(u)))
                .collect();
                if urls.is_empty() {
                    continue;
                }
                let default_config = PROVIDER_REGISTRY.default_config(provider_type);
                let (base_url, _) = first_healthy_base_url(&urls, |base_url| {
                    let mut config = default_config.clone();
                    async move {
                        if !config.is_object() {
                            return Err("Default provider config is not an object".to_string());
                        }
                        config["base_url"] = serde_json::Value::String(base_url);
                        let provider = PROVIDER_REGISTRY
                            .construct(provider_type, provider_type, &config)
                            .map_err(|e| format!("Could not construct provider: {e}"))?;
                        Discover::run_health_check(&*provider)
                            .await
                            .map_err(|e| format!("Health check failed: {e}"))
                    }
                })
                .await;
                let Some(base_url) = base_url else {
                    continue;
                };
                let id = match &host_id {
                    Some(host) if link != HostLink::Local => format!("{provider_type}-{host}"),
                    _ => provider_type.to_string(),
                };
                let id = Self::free_provider_id(ctx, id, &base_url)?;
                let mut config = default_config.clone();
                config["base_url"] = base_url.clone().into();
                ctx.add_found_provider(crate::config::ProviderConfig {
                    provider_id: id.clone(),
                    provider_type: provider_type.to_string(),
                    config,
                    host: Some(link.clone()),
                });
                found_here.push(Recommendation::Provider {
                    provider_type,
                    provider_id: id,
                    provider_name: name.clone(),
                    health_healthy: true,
                    health_error: None,
                    discovered_base_url: Some(base_url),
                    served_models: Vec::new(),
                });
            }

            Discover::list_served_models(&mut found_here).await;
            found_here.splice(0..0, configured_here);
            if found_here.is_empty() {
                ui.info("  nothing answered on the default ports");
            }
            for rec in &found_here {
                if let Recommendation::Provider {
                    provider_id,
                    discovered_base_url,
                    served_models,
                    ..
                } = rec
                {
                    let configured = ctx.config().get_provider(provider_id).is_some();
                    let mut notes: Vec<String> = Vec::new();
                    if configured {
                        notes.push("configured".to_string());
                    }
                    if !served_models.is_empty() {
                        notes.push(format!(
                            "serves {} {}",
                            served_models.len(),
                            if served_models.len() == 1 {
                                "model"
                            } else {
                                "models"
                            }
                        ));
                    }
                    ui.info(&format!(
                        "  {provider_id:<16} {:<28} {}",
                        discovered_base_url.as_deref().unwrap_or("-"),
                        notes.join(", ")
                    ));
                }
            }
            let added = Self::add_at_other_addresses(
                ctx,
                &address,
                &link,
                host_id.as_deref(),
                !found_here.is_empty(),
            )
            .await?;
            recommendations.extend(found_here);
            recommendations.extend(added);
        }
        recommendations.sort_by_key(display_name);
        Ok((recommendations, configured_ids))
    }

    const NO_CONTINUE_LABEL: &'static str = "No, continue";

    /// After probing a host, asks whether to add a provider at another
    /// address on it, until the user says no. The question starts on "No,
    /// continue" when something answered there, and on the first type
    /// otherwise. On a host the user runs, it offers the types the user runs
    /// and takes an address on that host, starting on the type's default URL
    /// there; for the hosted services, the hosted types and
    /// openai-compatible, with an address the user types and the link "not
    /// specified". The type's other settings follow. An address that does
    /// not answer can be tried again, added anyway (its models are not
    /// listed) or skipped. Each provider added is recorded like one found.
    async fn add_at_other_addresses(
        ctx: &mut crate::AppContext,
        address: &str,
        link: &crate::config::host::HostLink,
        host_id: Option<&str>,
        something_answered: bool,
    ) -> Result<Vec<Recommendation>> {
        use crate::config::host::HostLink;
        let hosted = *link == HostLink::Unspecified;
        let types = offered_types(hosted);
        let mut items: Vec<String> = types.iter().map(|t| t.to_string()).collect();
        items.push(Self::NO_CONTINUE_LABEL.to_string());
        let question = if hosted {
            "Add a hosted provider at another address?".to_string()
        } else {
            format!("Add a provider on {address} at another address?")
        };
        let mut added: Vec<Recommendation> = Vec::new();
        let mut default = if something_answered { types.len() } else { 0 };
        loop {
            let picked = ctx.ui.select(&question, &items, default)?;
            default = types.len();
            let Some(provider_type) = types.get(picked).copied() else {
                return Ok(added);
            };
            if let Some(rec) =
                Self::add_one_at_other_address(ctx, provider_type, address, link, host_id).await?
            {
                added.push(rec);
            }
        }
    }

    /// The id of a provider of `provider_type` at `base_url`, configured or
    /// found in this run, if there is one.
    fn provider_at(ctx: &crate::AppContext, provider_type: &str, base_url: &str) -> Option<String> {
        let normalize = |u: &str| u.trim_end_matches('/').to_ascii_lowercase();
        let wanted = normalize(base_url);
        let at = |p: &crate::config::ProviderConfig| {
            p.provider_type == provider_type
                && p.config
                    .get("base_url")
                    .and_then(|v| v.as_str())
                    .is_some_and(|u| normalize(u) == wanted)
        };
        ctx.config()
            .providers
            .values()
            .chain(ctx.found_providers())
            .find(|p| at(p))
            .map(|p| p.provider_id.clone())
    }

    /// Asks the settings of `provider_type` other than its address, starting
    /// on `config`, and returns them with `base_url` as the address.
    fn other_settings(
        ctx: &crate::AppContext,
        provider_type: &str,
        base_url: &str,
        config: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let Some(mut schema) = PROVIDER_REGISTRY.config_schema(provider_type) else {
            return Ok(config.clone());
        };
        if let Some(fields) = schema.as_object_mut() {
            if let Some(properties) = fields.get_mut("properties").and_then(|p| p.as_object_mut()) {
                properties.remove("base_url");
            }
            if let Some(required) = fields.get_mut("required").and_then(|r| r.as_array_mut()) {
                required.retain(|r| r != "base_url");
            }
        }
        let mut asked = crate::utils::prompt_from_schema(&*ctx.ui, &schema, config)?;
        asked["base_url"] = base_url.into();
        Ok(asked)
    }

    /// One provider of `provider_type` at an address the user types on the
    /// host at `address` (any address for the hosted services). `None` when
    /// the user skips it.
    async fn add_one_at_other_address(
        ctx: &mut crate::AppContext,
        provider_type: &'static str,
        address: &str,
        link: &crate::config::host::HostLink,
        host_id: Option<&str>,
    ) -> Result<Option<Recommendation>> {
        use crate::config::host::{HostLink, same_address, url_at, url_host};
        let hosted = *link == HostLink::Unspecified;
        let start = ctx
            .default_base_urls(provider_type)
            .first()
            .and_then(|url| {
                if hosted {
                    Some(url.clone())
                } else {
                    url_at(url, address)
                }
            })
            .unwrap_or_else(|| {
                if hosted {
                    "https://api.example.com/v1".to_string()
                } else {
                    url_at("http://localhost:8000/v1", address).unwrap_or_default()
                }
            });
        let mut current = start;
        // A hosted service may need its API key to answer, so its settings
        // are asked once, before its address is checked; a server the user
        // runs is checked first, and its settings are asked once it is
        // added.
        let mut config = PROVIDER_REGISTRY.default_config(provider_type);
        if hosted {
            config = Self::other_settings(ctx, provider_type, &current, &config)?;
        }
        let base_url = loop {
            let answer = ctx
                .ui
                .text(&format!("Address of the {provider_type} server"), &current)?;
            let answer = answer.trim().trim_end_matches('/').to_string();
            let Some(host) = url_host(&answer) else {
                ctx.ui
                    .warn(&format!("'{answer}' is not a URL, such as {current}."));
                continue;
            };
            if !hosted && !same_address(&host, address) {
                ctx.ui.warn(&format!(
                    "{answer} is not on {address}; add its machine with \"Add a host…\"."
                ));
                continue;
            }
            current = answer.clone();
            if let Some(existing) = Self::provider_at(ctx, provider_type, &answer) {
                ctx.ui.warn(&format!(
                    "{provider_type} at {answer} is already in this run as '{existing}'."
                ));
                continue;
            }
            config["base_url"] = answer.clone().into();
            let healthy = match PROVIDER_REGISTRY.construct(provider_type, provider_type, &config) {
                Ok(provider) => Discover::run_health_check(&*provider)
                    .await
                    .is_ok_and(|h| h.healthy),
                Err(_) => false,
            };
            let keep = if healthy {
                true
            } else {
                ctx.ui
                    .warn(&format!("{provider_type} is not answering at {answer}."));
                let choices = [
                    "Try another address".to_string(),
                    "Add it anyway (models not listed)".to_string(),
                    "Skip".to_string(),
                ];
                match ctx.ui.select("What would you like to do?", &choices, 0)? {
                    0 => continue,
                    1 => false,
                    _ => return Ok(None),
                }
            };
            if !hosted {
                config = Self::other_settings(ctx, provider_type, &answer, &config)?;
            }
            break (answer, config.clone(), keep && healthy);
        };
        let (base_url, config, healthy) = base_url;
        let id = match host_id {
            Some(host) if !hosted && *link != HostLink::Local => format!("{provider_type}-{host}"),
            _ => provider_type.to_string(),
        };
        let id = Self::free_provider_id(ctx, id, &base_url)?;
        ctx.add_found_provider(crate::config::ProviderConfig {
            provider_id: id.clone(),
            provider_type: provider_type.to_string(),
            config,
            host: Some(link.clone()),
        });
        let name = PROVIDER_REGISTRY
            .get(provider_type)
            .map(|m| m.name.clone())
            .unwrap_or_default();
        let mut recs = vec![Recommendation::Provider {
            provider_type,
            provider_id: id.clone(),
            provider_name: name,
            health_healthy: true,
            health_error: (!healthy).then(|| ADDED_ANYWAY.to_string()),
            discovered_base_url: Some(base_url.clone()),
            served_models: Vec::new(),
        }];
        if healthy {
            Discover::list_served_models(&mut recs).await;
        }
        if let Some(Recommendation::Provider { served_models, .. }) = recs.first() {
            let serves = if healthy {
                format!("serves {} models", served_models.len())
            } else {
                "added, models not listed".to_string()
            };
            ctx.ui.info(&format!("  {id:<16} {base_url:<28} {serves}"));
        }
        Ok(recs.pop())
    }

    /// `id`, unless a configured provider or one found earlier in this run
    /// has it at another address than `base_url`; then the id the user types.
    fn free_provider_id(ctx: &crate::AppContext, id: String, base_url: &str) -> Result<String> {
        let taken = |candidate: &str| {
            ctx.config()
                .get_provider(candidate)
                .or_else(|| ctx.found_provider(candidate))
                .is_some_and(|p| {
                    p.config.get("base_url").and_then(|v| v.as_str()) != Some(base_url)
                })
        };
        if !taken(&id) {
            return Ok(id);
        }
        let default = (2..)
            .map(|n| format!("{id}-{n}"))
            .find(|c| !taken(c))
            .expect("an unbounded range has a free id");
        loop {
            let answer = ctx.ui.text(
                &format!("'{id}' is a provider at another address. Id for {base_url}"),
                &default,
            )?;
            let answer = answer.trim().to_string();
            if answer.is_empty() || taken(&answer) {
                ctx.ui
                    .warn(&format!("'{answer}' is taken; choose another id."));
                continue;
            }
            return Ok(answer);
        }
    }

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
                        &ctx.config().recommended_configs,
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
        // The providers in this run: those found and those configured on
        // the hosts it covers.
        let mut provider_ids = healthy_provider_types(discovery);
        for id in &discovery.configured_provider_ids {
            if !provider_ids.contains(id) {
                provider_ids.push(id.clone());
            }
        }
        let resolvable_cap_types: HashSet<String> = effective_caps_by_launcher
            .iter()
            .filter(|rec_cap| {
                resolve_capability(&rec_cap.capability, rec_cap, hardware, &provider_ids, ctx)
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
                ctx.config().capabilities.clone();
            CapabilityCommands::setup(ctx, &chosen_type, None, false).await?;
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
                ctx.config().launchers.clone();
            LauncherCommands::setup(ctx, &chosen_type, None, false).await?;
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
        served_picks: &[ServedPick],
    ) -> Result<HashSet<String>> {
        let ui = ctx.ui.clone(); // owned Arc<dyn Ui>: doesn't borrow `ctx`, so it stays usable across the later `&mut ctx` calls into ProviderCommands::setup

        // Providers serving a selected model: the provider of each server
        // model picked, and each provider that already has a selected
        // catalog model. They are offered checked, whether or not they can
        // run the other selected models.
        let served = served_models(discovery);
        let mut serving: HashSet<String> = served_picks
            .iter()
            .map(|pick| pick.provider_id.clone())
            .collect();
        for model_id in selected_models {
            if let Some(on_servers) = served.catalog.get(model_id) {
                serving.extend(
                    on_servers
                        .iter()
                        .map(|(provider_id, _)| provider_id.clone()),
                );
            }
        }
        let mut offered = Revaluator::for_providers(discovery, selected_models, ctx);
        for rec in &discovery.recommendations {
            if let Recommendation::Provider { provider_id, .. } = rec
                && serving.contains(provider_id)
                && !offered.iter().any(|r| std::ptr::eq(*r, rec))
            {
                offered.push(rec);
            }
        }
        offered.sort_by_key(|r| display_name(r));

        let filtered: Vec<_> = offered
            .into_iter()
            .filter_map(|r| match r {
                Recommendation::Provider {
                    provider_id,
                    provider_name,
                    health_healthy,
                    health_error,
                    ..
                } if *health_healthy => {
                    Some((provider_id.clone(), provider_name, health_error.clone()))
                }
                _ => None,
            })
            .collect();
        // NOTE: no early return here when filtered.is_empty() -- the escape hatch
        // below is exactly what a user needs when no providers are healthy.

        // Providers configured before this run, on the hosts it covers.
        // Models can be assigned to them, so they are offered pre-checked.
        let mut configured: Vec<String> = discovery
            .configured_provider_ids
            .iter()
            .filter(|id| ctx.config().get_provider(id).is_some())
            .filter(|id| !filtered.iter().any(|(f, ..)| f == *id))
            .cloned()
            .collect();
        configured.sort();

        loop {
            let mut items: Vec<String> = filtered
                .iter()
                .map(|(id, name, error)| {
                    let status = match error.as_deref() {
                        None | Some("") => "healthy".to_string(),
                        Some(e) if e.starts_with(ADDED_ANYWAY) => e.to_string(),
                        Some(e) => format!("healthy ({e})"),
                    };
                    format!("{id} — {name} ({status})")
                })
                .collect();
            let mut defaults: Vec<bool> = filtered
                .iter()
                .map(|(id, ..)| serving.contains(id))
                .collect();
            for id in &configured {
                items.push(format!("{id} — configured"));
                defaults.push(true);
            }
            if items.is_empty() {
                // Providers are added in the hosts step, at another address
                // when nothing answered on the default ports.
                ui.info(
                    "No providers on the hosts of this run. Run `granite-cli setup` again and \
                     add one at its address in the hosts step.",
                );
                return Ok(HashSet::new());
            }

            let selected = ui.multi_select("Select providers to configure", &items, &defaults)?;
            let result: HashSet<String> = selected
                .iter()
                .map(|&i| {
                    if i < filtered.len() {
                        filtered[i].0.clone()
                    } else {
                        configured[i - filtered.len()].clone()
                    }
                })
                .collect();
            // Every selected model needs a provider to run on.
            if result.is_empty() && !(selected_models.is_empty() && served_picks.is_empty()) {
                ui.warn("Select at least one provider to run the selected models.");
                continue;
            }
            return Ok(result);
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
        let served = served_models(discovery);

        for model_id in selected_models {
            let Some(md) = MODEL_REGISTRY.get(model_id) else {
                continue;
            };

            // A selected provider already serves it: use that variant, which
            // needs no pull.
            if let Some((provider_id, variant)) =
                served.served_variant(model_id, selected_providers)
            {
                ctx.ui.info(&format!(
                    "'{model_id}' is already on {provider_id} as {} / {}; using that variant.",
                    variant.format, variant.precision
                ));
                chosen.insert(model_id.clone(), variant.clone());
                continue;
            }

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

            // The default is the variant that fits best across the selected
            // providers, each on the hardware it is now linked to; without
            // the `local` figures, the one discovery recommended.
            let provider_ids: Vec<String> = selected_providers.iter().cloned().collect();
            let ranked = ctx
                .hardware_profile(crate::config::hardware::LOCAL_HARDWARE_ID)
                .ok()
                .and_then(|local| {
                    rank_variants_for_providers(
                        &md,
                        candidates.iter().map(|(v, _)| v),
                        &local,
                        &provider_ids,
                        ctx,
                    )
                    .into_iter()
                    .next()
                    .map(|(v, _)| v)
                });
            let recommended = ranked.or_else(|| {
                discovery.recommendations.iter().find_map(|r| match r {
                    Recommendation::Model {
                        model_id: rec_id,
                        best_variant,
                        ..
                    } if rec_id == model_id => Some(best_variant.clone()),
                    _ => None,
                })
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
                .map(|(v, gb)| {
                    format!(
                        "{}{}",
                        Self::format_variant_option(v, *gb, md.context_length),
                        variant_host_note(v, selected_providers, ctx)
                    )
                })
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
                if let Some(pc) = ctx.config().get_provider(pid) {
                    PROVIDER_REGISTRY
                        .construct(&pc.provider_type, &pc.provider_id, &pc.config)
                        .ok()
                } else {
                    let default_config = PROVIDER_REGISTRY.default_config(pid);
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

    /// One line of the model picker. `providers` are the providers already
    /// configured before this run that can run the model's best variant. It
    /// is empty on a first setup, whose providers are chosen after the
    /// models, so the line names providers only when there are some.
    fn format_model_option(id: &str, size: &str, fit: ContextFit, providers: &[String]) -> String {
        if providers.is_empty() {
            format!("{id} — {size} — Fit: {fit}")
        } else {
            format!(
                "{id} — {size} — Fit: {fit} — runs on: {}",
                providers.join(", ")
            )
        }
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
    ) -> Result<(HashSet<String>, Vec<ServedPick>)> {
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
                &ctx.config().recommended_configs,
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

        // Models the providers found serve: catalog models are marked with
        // the providers that have them, and the others are offered after
        // the catalog models, unchecked.
        let served = served_models(discovery);
        let others = Self::served_model_options(&served, selected_caps);

        let mut chosen: HashSet<String> = HashSet::new();
        let mut picks: Vec<ServedPick> = Vec::new();
        let mut configure_different = filtered.is_empty() && others.is_empty();

        if filtered.is_empty() {
            ui.info("No models fully fit your hardware for the selected capabilities.");
        }
        if !configure_different {
            let escape_hatch_idx = filtered.len() + others.len();
            let mut items: Vec<String> = filtered
                .iter()
                .map(|(id, size, fit, providers)| {
                    let line = Self::format_model_option(id, size, *fit, providers);
                    match served.catalog.get(id) {
                        Some(on_servers) => {
                            let names: Vec<&str> = on_servers
                                .iter()
                                .map(|(provider_id, _)| provider_id.as_str())
                                .collect();
                            format!("{line} — already on: {}", names.join(", "))
                        }
                        None => line,
                    }
                })
                .collect();
            items.extend(others.iter().map(|(_, line)| line.clone()));
            items.push(Self::CONFIGURE_DIFFERENT_MODELS_LABEL.to_string());

            let mut defaults = vec![true; filtered.len()];
            defaults.extend(vec![false; others.len()]);
            defaults.push(false);

            let selected = ui.multi_select("Select models to configure", &items, &defaults)?;

            configure_different = selected.contains(&escape_hatch_idx);
            for i in selected {
                if i < filtered.len() {
                    chosen.insert(filtered[i].0.clone());
                } else if i < escape_hatch_idx {
                    picks.push(others[i - filtered.len()].0.clone());
                }
            }
        }

        if configure_different {
            let manual = Self::select_models_manually(ctx, discovery, selected_caps).await?;
            chosen.extend(manual);
        }

        Ok((chosen, picks))
    }

    /// The models in `served.others` that meet the requirement of a model
    /// slot of a capability in `selected_caps`, or all of them when no
    /// selected capability asks for a model, each with its line in the
    /// model picker. An id with a control character is left out, as sync
    /// leaves it out.
    fn served_model_options(
        served: &ServedModels,
        selected_caps: &HashSet<String>,
    ) -> Vec<(ServedPick, String)> {
        let requirements = model_requirements(selected_caps);
        served
            .others
            .iter()
            .filter(|(_, model)| !model.id.chars().any(char::is_control))
            .filter_map(|(provider_id, model)| {
                let instance = served_instance(provider_id, model)?;
                let admitted = requirements.is_empty()
                    || requirements
                        .iter()
                        .any(|req| admits_served(req, &*instance));
                if !admitted {
                    return None;
                }
                let name = instance.family();
                let line = if name.is_empty() || name == model.id {
                    format!("{} — on {provider_id}, not in the catalog", model.id)
                } else {
                    format!(
                        "{} — {name} — on {provider_id}, not in the catalog",
                        model.id
                    )
                };
                let pick = ServedPick {
                    provider_id: provider_id.clone(),
                    wire_id: model.id.clone(),
                };
                Some((pick, line))
            })
            .collect()
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

    /// Offers to repair each configured provider whose host link is
    /// broken, in id order, before setup estimates fit on that provider's
    /// hardware: the remediation prompt when `can_prompt`, a warning
    /// otherwise. Providers whose references resolve are not mentioned.
    async fn repair_provider_links(ctx: &mut crate::AppContext, can_prompt: bool) -> Result<()> {
        use crate::commands::shared::remediation::{OnDecline, remediate};
        use crate::config::validation::RefKind;
        let mut ids: Vec<String> = ctx.config().providers.keys().cloned().collect();
        ids.sort();
        for id in ids {
            // An earlier repair may have removed it.
            if ctx.config().get_provider(&id).is_some() {
                remediate(ctx, RefKind::Provider, &id, OnDecline::Skip, can_prompt).await?;
            }
        }
        Ok(())
    }

    /// Saves each provider in `selected_providers` that is not configured
    /// yet: as the wizard found it, with its address and the host it was
    /// found on, or otherwise with the default config for its type, the
    /// address discovery found it at and the default link for its type. In
    /// id order. An id already configured, by a
    /// previous session or an escape-hatch wizard earlier in this run, is
    /// left alone.
    fn configure_new_providers(
        ctx: &mut crate::AppContext,
        discovery: &DiscoveryResult,
        selected_providers: &HashSet<String>,
    ) {
        let ui = std::sync::Arc::clone(&ctx.ui);
        let mut provider_ids: Vec<&String> = selected_providers.iter().collect();
        provider_ids.sort();
        for provider_id in provider_ids {
            if ctx.config().get_provider(provider_id).is_some() {
                continue;
            }
            ui.info(&format!("\nConfiguring provider: {provider_id}..."));
            let provider_config = match ctx.found_provider(provider_id) {
                Some(found) => found.clone(),
                None => {
                    let provider_type = discovery
                        .recommendations
                        .iter()
                        .find_map(|rec| match rec {
                            Recommendation::Provider {
                                provider_type,
                                provider_id: id,
                                ..
                            } if id == provider_id => Some(provider_type.to_string()),
                            _ => None,
                        })
                        .unwrap_or_else(|| provider_id.to_string());
                    let mut default_config = PROVIDER_REGISTRY.default_config(&provider_type);
                    if let Some(base_url) =
                        discovery.recommendations.iter().find_map(|rec| match rec {
                            Recommendation::Provider {
                                provider_id: id,
                                discovered_base_url: Some(base_url),
                                ..
                            } if id == provider_id => Some(base_url),
                            _ => None,
                        })
                    {
                        default_config["base_url"] = base_url.clone().into();
                    }
                    crate::config::ProviderConfig {
                        provider_id: provider_id.clone(),
                        host: Some(crate::config::host::HostLink::default_for(&provider_type)),
                        provider_type,
                        config: default_config,
                    }
                }
            };

            if ctx
                .config_mut()
                .insert_provider(provider_id, provider_config)
                .is_err()
            {
                ui.warn(&format!(
                    "Failed to save provider config for '{provider_id}'"
                ));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn configure_all(
        ctx: &mut crate::AppContext,
        discovery: &DiscoveryResult,
        selected_caps: &HashSet<String>,
        selected_launchers: &HashSet<String>,
        selected_providers: &HashSet<String>,
        selected_models: &HashSet<String>,
        served_picks: &[ServedPick],
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
        // A handle, not a borrow of `ctx`: writing configuration below takes
        // `ctx` mutably, and a borrow of its `ui` field would still be live.
        let ui = std::sync::Arc::clone(&ctx.ui);

        // Configure providers first. The wizard has already saved the ones
        // it selected, so this adds only what is left.
        Self::configure_new_providers(ctx, discovery, selected_providers);

        // Configure launchers
        for launcher_id in selected_launchers {
            // An id already configured (e.g. via a previous session or an
            // escape-hatch wizard invoked earlier in this same run) must be
            // left alone rather than clobbered with a freshly-built default.
            if ctx.config().get_launcher(launcher_id).is_some() {
                continue;
            }
            ui.info(&format!("\nConfiguring launcher: {launcher_id}..."));
            let default_config = LAUNCHER_REGISTRY.default_config(launcher_id);

            let launcher_config = crate::config::LauncherConfig {
                launcher_id: launcher_id.to_string(),
                launcher_type: launcher_id.to_string(),
                enabled_capabilities: Vec::new(),
                config: default_config,
            };

            if ctx
                .config_mut()
                .insert_launcher(launcher_id, launcher_config)
                .is_err()
            {
                ui.warn(&format!(
                    "Failed to save launcher config for '{launcher_id}'"
                ));
            }
        }

        let served = served_models(discovery);

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
            if ctx.config().get_model(model_id).is_some() {
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

            // A selected provider that already serves the model in that
            // variant runs it.
            let serving = |v: &ModelVariant| {
                served
                    .served_variant(model_id, selected_providers)
                    .filter(|(_, sv)| {
                        sv.format.eq_ignore_ascii_case(&v.format)
                            && sv.precision.eq_ignore_ascii_case(&v.precision)
                    })
                    .map(|(provider_id, _)| provider_id.clone())
            };
            let (provider_id, variant) = match &chosen_variant {
                Some(v) => (
                    serving(v)
                        .or_else(|| {
                            Self::find_compatible_provider(model_id, v, selected_providers, ctx)
                        })
                        .or_else(|| selected_providers.iter().next().cloned()),
                    Some(format!("{}/{}", v.format, v.precision)),
                ),
                None => (selected_providers.iter().next().cloned(), None),
            };
            if provider_id.is_none() {
                let for_variant = variant
                    .as_deref()
                    .map(|v| format!(" (variant {v})"))
                    .unwrap_or_default();
                anyhow::bail!(
                    "No provider was selected to run model '{model_id}'{for_variant}. \
                     Run setup again and select a provider that can run it."
                );
            }

            let model_config = crate::config::ModelConfig {
                discovered: None,
                model_id: model_id.clone(),
                model_type: model_id.clone(),
                config: serde_json::json!({}),
                provider_id: provider_id.unwrap(),
                variant,
            };

            if ctx
                .config_mut()
                .insert_model(model_id, model_config)
                .is_err()
            {
                ui.warn(&format!("Failed to save model config for '{model_id}'"));
            }
        }

        // The server models the user picked, written as sync writes them,
        // so the capabilities below can name them. Sync finds them present.
        let served_instances =
            Self::configure_served_models(ctx, &served, served_picks, selected_providers);

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
                config: CAPABILITY_REGISTRY.default_config(cap_type),
            };
            let Some(cap_model_ids) = Self::capability_model_ids(
                cap_type,
                &dependencies,
                &cap_cfg,
                resolved_capability_models,
                selected_models,
                &served_instances,
            ) else {
                ui.warn(&format!(
                    "Skipping '{cap_type}': no compatible model available."
                ));
                continue;
            };

            // An id already configured (e.g. via a previous session or an
            // escape-hatch wizard invoked earlier in this same run) must be
            // left alone rather than clobbered with a freshly-built default.
            if ctx.config().get_capability(cap_type).is_some() {
                continue;
            }
            ui.info(&format!("\nConfiguring capability: {cap_type}..."));

            let mut config = CAPABILITY_REGISTRY.default_config(cap_type);

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
                .config_mut()
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
        let capability_source = ctx.sources().capabilities();
        for launcher_id in selected_launchers {
            let Some(launcher_type) = ctx
                .config()
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
                .config_mut()
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

    /// Picks the selected provider to bind `model_id` at `variant` to: of
    /// those that can run the variant, the one on whose hardware it fits
    /// best, which is where setup ranked it. Ties go to a provider on
    /// `local`, then to the provider id that sorts first, so a model that
    /// fits on this machine stays on a provider here when one on other
    /// hardware is added. Without the model's metadata or the `local`
    /// figures, the first provider that can run it in that order. Assumes
    /// providers have already been written to `ctx.config` (as
    /// `configure_all` does, before configuring models).
    fn find_compatible_provider(
        model_id: &str,
        variant: &ModelVariant,
        selected_providers: &HashSet<String>,
        ctx: &crate::AppContext,
    ) -> Option<String> {
        use crate::config::host::HostLink;
        let mut candidates: Vec<&String> = selected_providers
            .iter()
            .filter(|pid| {
                ctx.config()
                    .get_provider(pid)
                    .and_then(|pc| {
                        PROVIDER_REGISTRY
                            .construct(&pc.provider_type, &pc.provider_id, &pc.config)
                            .ok()
                    })
                    .is_some_and(|p| p.can_run_model(&variant.format, &variant.precision))
            })
            .collect();
        let on_local = |id: &str| provider_link(id, ctx) == HostLink::Local;
        candidates.sort_by_key(|id| (!on_local(id), id.as_str()));
        let md = MODEL_REGISTRY.get(model_id);
        let local = ctx
            .hardware_profile(crate::config::hardware::LOCAL_HARDWARE_ID)
            .ok();
        let (Some(md), Some(local)) = (md, local) else {
            return candidates.first().map(|id| id.to_string());
        };
        let fit = |id: &String| {
            provider_hardware(id, &local, ctx).ok().map(|hardware| {
                fit_order(&crate::models::context_fit::estimate(
                    md.context_length,
                    &md.architecture,
                    &md.native_dtype,
                    variant,
                    &hardware,
                ))
            })
        };
        let best = candidates.iter().map(|id| fit(id)).max()?;
        candidates.into_iter().find(|id| fit(id) == best).cloned()
    }

    /// The model id for each model slot in `dependencies`, keyed by the
    /// slot's `config_key`. A slot gets the model `resolved_capability_models`
    /// lists for `cap_type` and that `config_key`, otherwise the result of
    /// `find_model_for_capability`, otherwise the first of `served_instances`
    /// that meets the slot's requirement. An optional slot with no model is
    /// left out. Returns `None` when any required slot has no model.
    fn capability_model_ids(
        cap_type: &str,
        dependencies: &[Dependency],
        cap_cfg: &crate::config::CapabilityConfig,
        resolved_capability_models: &HashMap<String, HashMap<String, String>>,
        selected_models: &HashSet<String>,
        served_instances: &[(String, Box<dyn crate::models::Model>)],
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
                requirement,
                ..
            } = dep
            else {
                continue;
            };
            let model_id = resolved_capability_models
                .get(cap_type)
                .and_then(|slots| slots.get(config_key.as_str()).cloned())
                .or_else(|| Self::find_model_for_capability(cap_type, selected_models))
                .or_else(|| {
                    served_instances
                        .iter()
                        .find(|(_, model)| admits_served(requirement, &**model))
                        .map(|(id, _)| id.clone())
                });
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

    /// Writes each model in `served_picks` as sync writes a model a provider
    /// lists, from the listing in `served`, and returns them as instances by
    /// configured id, in pick order. A pick already configured is returned
    /// as it is. A pick whose provider was not selected is skipped with a
    /// warning.
    fn configure_served_models(
        ctx: &mut crate::AppContext,
        served: &ServedModels,
        served_picks: &[ServedPick],
        selected_providers: &HashSet<String>,
    ) -> Vec<(String, Box<dyn crate::models::Model>)> {
        let ui = std::sync::Arc::clone(&ctx.ui);
        let mut instances = Vec::new();
        for pick in served_picks {
            let provider_type = ctx
                .config()
                .get_provider(&pick.provider_id)
                .map(|pc| pc.provider_type.clone());
            let (true, Some(provider_type)) = (
                selected_providers.contains(&pick.provider_id),
                provider_type,
            ) else {
                ui.warn(&format!(
                    "Skipping '{}': provider '{}' was not selected.",
                    pick.wire_id, pick.provider_id
                ));
                continue;
            };
            let Some((_, model)) = served.others.iter().find(|(provider_id, model)| {
                *provider_id == pick.provider_id && model.id == pick.wire_id
            }) else {
                continue;
            };
            if served_instance_id(ctx, pick).is_none() {
                ui.info(&format!("\nConfiguring model: {}...", pick.wire_id));
                let plan = crate::models::discovery::plan(
                    &pick.provider_id,
                    &provider_type,
                    std::slice::from_ref(*model),
                    &ctx.config().models,
                    &HashMap::new(),
                );
                // The additions only: the plan is for a listing of this one
                // model, so its removals are every other model discovery
                // manages for the provider.
                if let Err(error) = ctx.config_mut().apply_model_changes(plan.additions, &[]) {
                    ui.warn(&format!(
                        "Failed to save model config for '{}': {error}",
                        pick.wire_id
                    ));
                    continue;
                }
            }
            let Some(instance_id) = served_instance_id(ctx, pick) else {
                continue;
            };
            let instance = ctx.config().get_model(&instance_id).and_then(|mc| {
                MODEL_REGISTRY
                    .construct(&mc.model_type, &instance_id, &mc.config)
                    .ok()
            });
            if let Some(instance) = instance {
                instances.push((instance_id, instance));
            }
        }
        instances
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
                ctx.config().get_model(model_id).and_then(|mc| {
                    ctx.config()
                        .get_provider(mc.provider_id.as_str())
                        .map(|pc| {
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

    /// Reconciles the models of the providers the wizard just configured.
    /// Discovery is part of each provider type's defaults, so a local
    /// provider set up here already has the models it reports available for
    /// the next run. Failures are warnings; setup has already succeeded.
    async fn sync_discovered(ctx: &mut crate::AppContext, providers: &HashSet<String>) {
        let ids: Vec<String> = providers.iter().cloned().collect();
        let reports = crate::commands::shared::discovery::sync_providers(ctx, &ids).await;
        for report in reports {
            if matches!(
                report.status,
                crate::commands::shared::discovery::SyncStatus::Synced
            ) {
                ctx.ui.info(&format!("Discovery: {}", report.summary()));
            }
            if let crate::commands::shared::discovery::SyncStatus::Failed(error) = &report.status {
                ctx.ui.warn(&format!(
                    "Model discovery for '{}' failed: {error}",
                    report.provider_id
                ));
            }
            for warning in &report.warnings {
                ctx.ui.warn(warning);
            }
        }
    }

    fn print_summary(
        ctx: &crate::AppContext,
        selected_caps: &HashSet<String>,
        selected_launchers: &HashSet<String>,
        selected_providers: &HashSet<String>,
        selected_models: &HashSet<String>,
    ) {
        let ui = &*ctx.ui;

        ui.info("\n=== Setup Complete ===");
        let hosts = summary_hosts(ctx, selected_providers);
        if !hosts.is_empty() {
            ui.info(&format!("Hosts: {}", hosts.join(", ")));
        }
        ui.info(&format!("Providers: {}", summary_list(selected_providers)));
        ui.info(&format!("Models: {}", summary_list(selected_models)));
        ui.info(&format!("Launchers: {}", summary_list(selected_launchers)));
        ui.info(&format!("Capabilities: {}", summary_list(selected_caps)));
        for line in launchers_without_capability(ctx, selected_launchers) {
            ui.warn(&line);
        }
        for host in &hosts {
            if let Some(line) = host_memory_note(ctx, host) {
                ui.info(&line);
            }
        }

        ui.info("\nRun `granite-cli launcher list` to see configured launchers.");
        if let Some(first_launcher) = selected_launchers.iter().min() {
            ui.info(&format!(
                "Run `granite-cli launch {first_launcher}` to launch with Granite overlay.",
            ));
        }
    }
}

/// The hosts the providers in `providers` run on, `local` first, then by
/// id. Hosted providers have none.
fn summary_hosts(ctx: &crate::AppContext, providers: &HashSet<String>) -> Vec<String> {
    let mut hosts: Vec<String> = providers
        .iter()
        .filter_map(|id| provider_link(id, ctx).host_id().map(str::to_string))
        .collect();
    hosts.sort_by_key(|h| (h != crate::config::host::LOCAL_HOST_ID, h.clone()));
    hosts.dedup();
    hosts
}

/// One line for each launcher in `launchers` that has no configured
/// capability enabled: the capability its recommended configuration asks
/// for, that it did not resolve, and the command that binds one.
fn launchers_without_capability(
    ctx: &crate::AppContext,
    launchers: &HashSet<String>,
) -> Vec<String> {
    let mut ids: Vec<&String> = launchers.iter().collect();
    ids.sort();
    let mut lines = Vec::new();
    for id in ids {
        let enabled = ctx.config().get_launcher(id).is_some_and(|l| {
            l.enabled_capabilities
                .iter()
                .any(|c| ctx.config().get_capability(c).is_some())
        });
        if enabled {
            continue;
        }
        let launcher_type = resolved_launcher_type(ctx, id);
        let supported = LAUNCHER_REGISTRY
            .get(&launcher_type)
            .map(|m| m.supported_capabilities.clone())
            .unwrap_or_default();
        let recommended = recommended_config::effective_capabilities(
            &launcher_type,
            &recommended_config::BUILTIN_RECOMMENDED_CONFIGS,
            &ctx.config().recommended_configs,
        )
        .into_iter()
        .map(|c| c.capability)
        .find(|cap| {
            CAPABILITY_REGISTRY.get(cap).is_some_and(|m| {
                m.supported_binding_types
                    .iter()
                    .any(|b| supported.contains(b))
            })
        });
        let why = match recommended {
            Some(cap) => format!("its recommended '{cap}' did not resolve to a selected model"),
            None => "no capability is recommended for it".to_string(),
        };
        lines.push(format!(
            "Launcher '{id}' has no capability: {why}. Run `granite-cli capability setup` to \
             bind one."
        ));
    }
    lines
}

/// When the models bound to capabilities on the providers of host `host`
/// need more memory together than the host's devices have, says so. A model
/// whose variant states no size is left out of the total, and the line says
/// so. `None` for a host whose hardware is not known, or under its memory.
///
/// The memory is the devices' combined usable memory, less the default
/// overhead for each device after the first, whichever providers the models
/// are on: the models are counted as if loaded together, and the fit of
/// each model on its own provider is checked when it is ranked.
fn host_memory_note(ctx: &crate::AppContext, host: &str) -> Option<String> {
    let usable = ctx
        .host_fit_profile(
            host,
            crate::providers::DeviceUse {
                splits: true,
                overhead_gb: crate::providers::DEFAULT_DEVICE_OVERHEAD_GB,
            },
        )
        .ok()?
        .usable_memory_gb();
    let mut bound: Vec<String> = Vec::new();
    for cap in ctx.config().capabilities.values() {
        let Some(metadata) = CAPABILITY_REGISTRY.get(&cap.capability_type) else {
            continue;
        };
        for model_id in
            crate::utils::capability_model_ids(cap, &metadata.dependencies).into_values()
        {
            if !bound.contains(&model_id) {
                bound.push(model_id);
            }
        }
    }
    let mut total = 0.0;
    let mut unknown = 0;
    let mut counted = 0;
    for model_id in &bound {
        let Some(mc) = ctx.config().get_model(model_id) else {
            continue;
        };
        if provider_link(&mc.provider_id, ctx).host_id() != Some(host) {
            continue;
        }
        let size = MODEL_REGISTRY.get(&mc.model_type).and_then(|md| {
            let (format, precision) = mc.variant.as_deref()?.split_once('/')?;
            md.variants
                .iter()
                .find(|v| v.format == format && v.precision == precision)?
                .size_gb
        });
        match size {
            Some(gb) => {
                total += gb;
                counted += 1;
            }
            None => unknown += 1,
        }
    }
    if counted == 0 || total <= usable {
        return None;
    }
    let mut line = format!(
        "{host}: models bound here total {total:.1} GB, usable memory is {usable:.1} GB.\n  \
         They cannot all be loaded at once; switching between them reloads weights."
    );
    if unknown > 0 {
        line.push_str(&format!(
            "\n  {unknown} model(s) of unknown size are not counted."
        ));
    }
    Some(line)
}

/// The ids in `ids` as one line of the setup summary, sorted so that the same
/// selection reads the same on every run, or `none` when there are none.
fn summary_list(ids: &HashSet<String>) -> String {
    if ids.is_empty() {
        return "none".to_string();
    }
    let mut sorted: Vec<&str> = ids.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.join(", ")
}

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ModelConfig, ProviderConfig};
    use crate::models::ModelType;
    use crate::utils::test_support::{FakeServer, closed_base_url};
    use crate::utils::ui::base::tests::CaptureUi;
    use std::sync::{Arc, Mutex};

    fn test_ctx() -> crate::AppContext {
        crate::AppContext::new(Config::default(), Arc::new(CaptureUi::default()))
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
        HardwareProfile::with_memory(crate::utils::hardware::Memory::System { ram_gb: 64.0 })
    }

    /// Discovery with a fixed hardware profile and fixed provider health:
    /// only Ollama answers, at its first default URL. What a test sees then
    /// does not depend on which providers the machine running it can reach.
    async fn run_discovery(ctx: &crate::AppContext) -> DiscoveryResult {
        let providers = providers_with_healthy(ctx, &["ollama"]);
        Discover::run_with_providers(ctx, &test_hardware_profile(), providers).await
    }

    /// What `discover_providers` returns when the provider types in
    /// `healthy` answer their health check and no other type does.
    fn providers_with_healthy(
        ctx: &crate::AppContext,
        healthy: &[&str],
    ) -> (Vec<Recommendation>, Vec<String>) {
        let mut configured: Vec<String> = Vec::new();
        let mut recommendations: Vec<Recommendation> = Vec::new();
        for (provider_type, metadata) in PROVIDER_REGISTRY.entries() {
            if ctx.config().providers.contains_key(provider_type) {
                configured.push(provider_type.to_string());
                continue;
            }
            let is_healthy = healthy.contains(&provider_type);
            recommendations.push(Recommendation::Provider {
                provider_type,
                provider_id: provider_type.to_string(),
                provider_name: metadata.name.clone(),
                health_healthy: is_healthy,
                health_error: (!is_healthy).then(|| "not reachable in tests".to_string()),
                discovered_base_url: is_healthy
                    .then(|| metadata.default_base_urls.first().cloned())
                    .flatten(),
                served_models: Vec::new(),
            });
        }
        configured.sort();
        recommendations.sort_by_key(display_name);
        (recommendations, configured)
    }

    fn ctx_with_provider(
        id: &str,
        provider_type: &str,
        config: serde_json::Value,
    ) -> crate::AppContext {
        let mut ctx = test_ctx();
        ctx.config_mut().providers.insert(
            id.to_string(),
            ProviderConfig {
                host: Default::default(),
                provider_id: id.to_string(),
                provider_type: provider_type.to_string(),
                config,
            },
        );
        ctx
    }

    fn ctx_with_model(id: &str, provider_id: Option<&str>) -> crate::AppContext {
        let mut ctx = test_ctx();
        ctx.config_mut().models.insert(
            id.to_string(),
            ModelConfig {
                discovered: None,
                model_id: id.to_string(),
                model_type: id.to_string(),
                config: serde_json::json!({}),
                provider_id: provider_id.unwrap_or("ollama").to_string(),
                variant: None,
            },
        );
        ctx
    }

    /// Points the default URLs of every provider type at a closed loopback
    /// port, except the types in `servers`, whose default URL is the given
    /// server's. Probing then reaches only the test's own servers, and no
    /// hosted service or server on the machine running the test.
    async fn probe_only(ctx: &mut crate::AppContext, servers: &[(&str, &FakeServer)]) {
        let closed = closed_base_url().await;
        for (provider_type, _) in PROVIDER_REGISTRY.entries() {
            let url = servers
                .iter()
                .find(|(t, _)| *t == provider_type)
                .map_or_else(
                    || closed.clone(),
                    |(_, server)| server.base_url().to_string(),
                );
            ctx.set_default_base_urls(provider_type, vec![url]);
        }
    }

    // -- discover_providers ----------------------------------------------------

    #[tokio::test]
    async fn discovery_probes_the_default_url_a_test_has_set() {
        let server = FakeServer::start("llama-cpp", &[]).await;
        let mut ctx = test_ctx();
        probe_only(&mut ctx, &[("llama-cpp", &server)]).await;

        let (recommendations, _) = Discover::discover_providers(&ctx, None).await;

        let healthy: Vec<(&str, Option<&str>)> = recommendations
            .iter()
            .filter_map(|rec| match rec {
                Recommendation::Provider {
                    provider_type,
                    health_healthy: true,
                    discovered_base_url,
                    ..
                } => Some((*provider_type, discovered_base_url.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(healthy, vec![("llama-cpp", Some(server.base_url()))]);
        assert_eq!(server.requests(), 1);
    }

    #[tokio::test]
    async fn provider_discovery_uses_the_first_healthy_base_url() {
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&attempts);
        let base_urls = vec![
            "http://localhost:8080".to_string(),
            "http://localhost:9931".to_string(),
            "http://localhost:10000".to_string(),
        ];

        let (base_url, error) = first_healthy_base_url(&base_urls, move |base_url| {
            let observed = Arc::clone(&observed);
            async move {
                observed.lock().unwrap().push(base_url.clone());
                Ok(HealthStatus {
                    healthy: base_url.ends_with(":9931"),
                    latency: std::time::Duration::ZERO,
                    error: (!base_url.ends_with(":9931")).then(|| "not running".to_string()),
                })
            }
        })
        .await;

        assert_eq!(base_url.as_deref(), Some("http://localhost:9931"));
        assert_eq!(error, None);
        assert_eq!(
            *attempts.lock().unwrap(),
            vec!["http://localhost:8080", "http://localhost:9931"]
        );
    }

    #[tokio::test]
    async fn discover_providers_skips_configured() {
        let ctx = ctx_with_provider("ollama", "ollama", serde_json::json!({}));
        let (_, configured) = Discover::discover_providers(&ctx, None).await;
        assert!(configured.contains(&"ollama".to_string()));
    }

    #[tokio::test]
    async fn discover_providers_recommends_unconfigured() {
        let ctx = test_ctx();
        let (recommendations, _) = Discover::discover_providers(&ctx, None).await;
        let provider_recs: Vec<_> = recommendations
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
        ctx.config_mut().launchers.insert(
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
        const MODEL_ID: &str = "granite-guardian-hap-38m";
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());

        // granite-guardian-hap-38m has exactly one (safetensors) variant;
        // openai-compatible's default `can_run_model` accepts any format.
        let md = MODEL_REGISTRY
            .get(MODEL_ID)
            .expect("fixture model should exist in the catalog");
        assert_eq!(
            md.variants.len(),
            1,
            "fixture assumption: exactly one variant"
        );

        let discovery = DiscoveryResult {
            recommendations: vec![model_recommendation(MODEL_ID, &md, md.variants[0].clone())],
            all_model_candidates: vec![],
            configured_provider_ids: vec![],
            configured_model_ids: vec![],
            configured_launcher_ids: vec![],
            configured_capability_ids: vec![],
        };
        let selected_models: HashSet<String> = [MODEL_ID.to_string()].into_iter().collect();
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
            .get(MODEL_ID)
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
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());

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
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());

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
                .unwrap()
                .0;

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
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());
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
                .unwrap()
                .0;
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
                .unwrap()
                .0;
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
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();

        let claude_enabled = &ctx
            .config()
            .get_launcher("claude")
            .unwrap()
            .enabled_capabilities;
        assert_eq!(
            claude_enabled,
            &vec!["agent-model".to_string()],
            "claude supports the AgentModel binding, so agent-model should be enabled"
        );

        let bob_enabled = ctx
            .config()
            .get_launcher("bob")
            .unwrap()
            .enabled_capabilities
            .clone();
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
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &recommended_capability_types_by_launcher,
        )
        .await
        .unwrap();

        let goose_enabled = &ctx
            .config()
            .get_launcher("goose")
            .unwrap()
            .enabled_capabilities;
        assert_eq!(
            goose_enabled,
            &vec!["agent-model".to_string()],
            "goose recommends agent-model, so it should be enabled"
        );

        let claude_enabled = &ctx
            .config()
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
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());

        ctx.config_mut().providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                host: Default::default(),
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
        ctx.config_mut().models.insert(
            model_id.to_string(),
            ModelConfig {
                discovered: None,
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
        // The wizard saves the local hardware profile.
        let _home = crate::config::TestConfigHome::new();
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
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());

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
        ctx.config_mut().providers.insert(
            "lm-studio".to_string(),
            crate::config::ProviderConfig {
                host: Default::default(),
                provider_id: "lm-studio".to_string(),
                provider_type: "lm-studio".to_string(),
                config: PROVIDER_REGISTRY.default_config("lm-studio"),
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
            SetupCommands::find_compatible_provider(
                "granite-4.2-8b",
                &gguf_variant,
                &selected,
                &ctx
            ),
            Some("lm-studio".to_string())
        );

        let safetensors_variant = ModelVariant {
            format: "safetensors".to_string(),
            precision: "bfloat16".to_string(),
            size_gb: Some(4.0),
            url: "https://example.com/model".to_string(),
        };
        assert_eq!(
            SetupCommands::find_compatible_provider(
                "granite-4.2-8b",
                &safetensors_variant,
                &selected,
                &ctx
            ),
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
        let hardware =
            HardwareProfile::with_memory(crate::utils::hardware::Memory::System { ram_gb: 32.0 });
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
        let hardware =
            HardwareProfile::with_memory(crate::utils::hardware::Memory::System { ram_gb: 64.0 });
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
        // Very tight RAM.
        let hardware =
            HardwareProfile::with_memory(crate::utils::hardware::Memory::System { ram_gb: 4.0 });
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
        // the fit/size tie-break in `rank_variants_for_providers` prefers the larger
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
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());
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
                    provider_id: "ollama".to_string(),
                    provider_name: "Ollama".to_string(),
                    health_healthy: true,
                    health_error: None,
                    discovered_base_url: Some("http://localhost:11434".to_string()),
                    served_models: Vec::new(),
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
        ctx.config_mut().launchers.insert(
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
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());
        ctx.config_mut().launchers.insert(
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
    async fn select_providers_says_how_to_add_one_when_none_is_offered() {
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());
        let discovery = DiscoveryResult {
            recommendations: vec![Recommendation::Provider {
                provider_type: "ollama",
                provider_id: "ollama".to_string(),
                provider_name: "Ollama".to_string(),
                health_healthy: false,
                health_error: Some("not running".to_string()),
                discovered_base_url: None,
                served_models: Vec::new(),
            }],
            ..no_discovery()
        };

        let selected = SetupCommands::select_providers(&mut ctx, &discovery, &HashSet::new(), &[])
            .await
            .unwrap();

        assert!(selected.is_empty());
        assert!(capture.multi_select_prompts.borrow().is_empty());
        assert!(
            capture.infos.borrow()[0].starts_with("No providers on the hosts of this run."),
            "{:?}",
            capture.infos.borrow()
        );
    }

    fn no_discovery() -> DiscoveryResult {
        DiscoveryResult {
            recommendations: vec![],
            all_model_candidates: vec![],
            configured_provider_ids: vec![],
            configured_model_ids: vec![],
            configured_launcher_ids: vec![],
            configured_capability_ids: vec![],
        }
    }

    #[tokio::test]
    async fn select_providers_offers_configured_providers_pre_checked() {
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());
        ctx.config_mut().providers.insert(
            "ollama".to_string(),
            crate::config::ProviderConfig {
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({}),
                host: Default::default(),
            },
        );
        capture.multi_select_answers.borrow_mut().push_back(vec![0]);

        let discovery = DiscoveryResult {
            configured_provider_ids: vec!["ollama".to_string()],
            ..no_discovery()
        };
        let selected = SetupCommands::select_providers(&mut ctx, &discovery, &HashSet::new(), &[])
            .await
            .unwrap();

        assert_eq!(selected, HashSet::from(["ollama".to_string()]));
        let prompts = capture.multi_select_prompts.borrow();
        let (_, items, defaults) = &prompts[0];
        assert_eq!(items[0], "ollama — configured");
        assert!(defaults[0]);
        assert!(
            capture.infos.borrow().is_empty(),
            "no 'no healthy providers' notice"
        );
    }

    #[tokio::test]
    async fn select_providers_asks_again_when_models_are_selected_and_no_provider_is() {
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());
        ctx.config_mut().providers.insert(
            "ollama".to_string(),
            crate::config::ProviderConfig {
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({}),
                host: Default::default(),
            },
        );
        capture
            .multi_select_answers
            .borrow_mut()
            .extend([vec![], vec![0]]);
        let models = HashSet::from(["granite-4.2-8b".to_string()]);
        let discovery = DiscoveryResult {
            configured_provider_ids: vec!["ollama".to_string()],
            ..no_discovery()
        };

        let selected = SetupCommands::select_providers(&mut ctx, &discovery, &models, &[])
            .await
            .unwrap();

        assert_eq!(selected, HashSet::from(["ollama".to_string()]));
        assert_eq!(capture.multi_select_prompts.borrow().len(), 2);
        assert_eq!(
            *capture.warns.borrow(),
            vec!["Select at least one provider to run the selected models.".to_string()]
        );
    }

    #[test]
    fn a_model_option_names_providers_only_when_some_can_run_it() {
        assert_eq!(
            SetupCommands::format_model_option("granite-4.2-8b", "8B", ContextFit::Full, &[]),
            "granite-4.2-8b — 8B — Fit: Full"
        );
        assert_eq!(
            SetupCommands::format_model_option(
                "granite-4.2-8b",
                "8B",
                ContextFit::Partial(32 * 1024),
                &["ollama".to_string(), "lm-studio".to_string()],
            ),
            "granite-4.2-8b — 8B — Fit: Partial (32K) — runs on: ollama, lm-studio"
        );
    }

    #[tokio::test]
    async fn configure_all_names_the_model_and_variant_when_no_provider_was_selected() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        let discovery = DiscoveryResult {
            recommendations: vec![],
            all_model_candidates: vec![],
            configured_provider_ids: vec![],
            configured_model_ids: vec![],
            configured_launcher_ids: vec![],
            configured_capability_ids: vec![],
        };
        let model_id = "granite-4.2-8b".to_string();
        let selected_models: HashSet<String> = [model_id.clone()].into_iter().collect();
        let variant = ModelVariant {
            format: "GGUF".to_string(),
            precision: "Q8_0".to_string(),
            size_gb: None,
            url: String::new(),
        };
        let selected_variants: HashMap<String, ModelVariant> =
            [(model_id.clone(), variant)].into_iter().collect();

        let err = SetupCommands::configure_all(
            &mut ctx,
            &discovery,
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            &selected_models,
            &[],
            &selected_variants,
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "No provider was selected to run model 'granite-4.2-8b' (variant GGUF/Q8_0). \
             Run setup again and select a provider that can run it."
        );
    }

    #[test]
    fn the_summary_lists_ids_in_sorted_order() {
        let capture = Arc::new(CaptureUi::default());
        let ctx = crate::AppContext::new(Config::default(), capture.clone());
        let set = |ids: &[&str]| -> HashSet<String> { ids.iter().map(|s| s.to_string()).collect() };

        SetupCommands::print_summary(
            &ctx,
            &set(&["vision-mcp", "agent-model", "sub-agent-explore"]),
            &set(&["pi", "claude", "opencode", "bob"]),
            &set(&["openrouter", "ollama"]),
            &HashSet::new(),
        );

        let infos = capture.infos.borrow().clone();
        for expected in [
            "Providers: ollama, openrouter",
            "Models: none",
            "Launchers: bob, claude, opencode, pi",
            "Capabilities: agent-model, sub-agent-explore, vision-mcp",
            "Run `granite-cli launch bob` to launch with Granite overlay.",
        ] {
            assert!(infos.iter().any(|i| i == expected), "{expected}: {infos:?}");
        }
    }

    // -- configure_all guards --------------------------------------------------

    #[tokio::test]
    async fn configure_all_persists_the_discovered_provider_base_url() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        let discovered_base_url = "http://localhost:9931";
        let discovery = DiscoveryResult {
            recommendations: vec![Recommendation::Provider {
                provider_type: "llama-cpp",
                provider_id: "llama-cpp".to_string(),
                provider_name: "llama.cpp".to_string(),
                health_healthy: true,
                health_error: None,
                discovered_base_url: Some(discovered_base_url.to_string()),
                served_models: Vec::new(),
            }],
            all_model_candidates: vec![],
            configured_provider_ids: vec![],
            configured_model_ids: vec![],
            configured_launcher_ids: vec![],
            configured_capability_ids: vec![],
        };

        SetupCommands::configure_all(
            &mut ctx,
            &discovery,
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::from(["llama-cpp".to_string()]),
            &HashSet::new(),
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            ctx.config().get_provider("llama-cpp").unwrap().config["base_url"],
            discovered_base_url
        );
    }

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
        ctx.config_mut().providers.insert(
            provider_id.to_string(),
            ProviderConfig {
                host: Default::default(),
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
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();

        let saved = ctx
            .config()
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
        ctx.config_mut().launchers.insert(
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
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();

        let saved = ctx
            .config()
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
        ctx.config_mut().capabilities.insert(
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
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();

        let saved = ctx
            .config()
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
        ctx.config_mut().recommended_configs.insert(
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
            &[],
            &HashMap::new(),
            &resolved,
            &HashMap::new(),
        )
        .await
        .unwrap();

        let model_of = |id: &str| {
            ctx.config()
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
            PROVIDER_REGISTRY.default_config("ollama"),
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
        ctx.config_mut().capabilities.insert(
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
        ctx.config_mut()
            .models
            .get_mut("granite-4.2-3b")
            .unwrap()
            .variant = Some("GGUF/Q4_K_M".to_string());
        let discovery = run_discovery(&ctx).await;

        SetupCommands::configure_all(
            &mut ctx,
            &discovery,
            &HashSet::new(),
            &HashSet::new(),
            &string_set(&["ollama"]),
            &string_set(&["granite-4.2-3b"]),
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();

        let saved = ctx
            .config()
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
            &[],
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
            &[],
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
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());
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
            &[],
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
        assert!(ctx.config().get_capability("agent-model").is_none());
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

    // -- fit per provider hardware ----------------------------------------------

    /// A context with providers `(id, type, link)`, the `spark` catalog
    /// profile and the `spark` host listing it, built in memory.
    fn ctx_with_linked_providers(
        providers: &[(&str, &str, crate::config::host::HostLink)],
    ) -> crate::AppContext {
        let mut config = Config::default();
        config.hardware.insert(
            "spark".to_string(),
            crate::config::HardwareConfig::catalog("spark", "dgx-spark", 128.0),
        );
        config.hosts.insert(
            "spark".to_string(),
            crate::config::HostConfig {
                host_id: "spark".to_string(),
                address: "spark.lan".to_string(),
                hardware: vec!["spark".to_string()],
            },
        );
        for (id, provider_type, link) in providers {
            config.providers.insert(
                id.to_string(),
                crate::config::ProviderConfig {
                    provider_id: id.to_string(),
                    provider_type: provider_type.to_string(),
                    config: crate::providers::PROVIDER_REGISTRY.default_config(provider_type),
                    host: Some(link.clone()),
                },
            );
        }
        crate::AppContext::new(config, Arc::new(CaptureUi::default()))
    }

    /// A context with `local` and the `spark` profile and host (at
    /// `spark.lan`) configured, and a discovery that
    /// found llama-cpp running at `http://localhost:8080`.
    fn wizard_with_llama_cpp_found() -> (crate::AppContext, Arc<CaptureUi>, DiscoveryResult) {
        let mut config = Config::default();
        config.hardware.insert(
            crate::config::hardware::LOCAL_HARDWARE_ID.to_string(),
            crate::config::HardwareConfig::local_from_detection(small_local()),
        );
        config.hardware.insert(
            "spark".to_string(),
            crate::config::HardwareConfig::catalog("spark", "dgx-spark", 128.0),
        );
        config.hosts.insert(
            "spark".to_string(),
            crate::config::HostConfig {
                host_id: "spark".to_string(),
                address: "spark.lan".to_string(),
                hardware: vec!["spark".to_string()],
            },
        );
        let capture = Arc::new(CaptureUi::default());
        let ctx = crate::AppContext::new(config, capture.clone());
        let discovery = DiscoveryResult {
            recommendations: vec![Recommendation::Provider {
                provider_type: "llama-cpp",
                provider_id: "llama-cpp".to_string(),
                provider_name: "llama.cpp".to_string(),
                health_healthy: true,
                health_error: None,
                discovered_base_url: Some("http://localhost:8080".to_string()),
                served_models: Vec::new(),
            }],
            all_model_candidates: vec![],
            configured_provider_ids: vec![],
            configured_model_ids: vec![],
            configured_launcher_ids: vec![],
            configured_capability_ids: vec![],
        };
        (ctx, capture, discovery)
    }

    #[test]
    fn the_wizard_saves_a_found_provider_on_the_host_it_was_found_on_with_its_address() {
        use crate::config::host::HostLink;
        let _home = crate::config::TestConfigHome::new();
        let (mut ctx, capture, mut discovery) = wizard_with_llama_cpp_found();
        if let Recommendation::Provider {
            discovered_base_url,
            ..
        } = &mut discovery.recommendations[0]
        {
            *discovered_base_url = Some("http://spark:8080".to_string());
        }
        ctx.add_found_provider(crate::config::ProviderConfig {
            provider_id: "llama-cpp".to_string(),
            provider_type: "llama-cpp".to_string(),
            config: serde_json::json!({ "base_url": "http://spark:8080" }),
            host: Some(HostLink::Host("spark".to_string())),
        });
        let selected: HashSet<String> = ["llama-cpp".to_string()].into();

        SetupCommands::configure_new_providers(&mut ctx, &discovery, &selected);

        let provider = ctx.config().get_provider("llama-cpp").unwrap();
        assert_eq!(provider.host, Some(HostLink::Host("spark".to_string())));
        assert_eq!(provider.config["base_url"], "http://spark:8080");
        assert!(capture.select_prompts.borrow().is_empty());
        assert!(capture.text_prompts.borrow().is_empty());
    }

    #[test]
    fn auto_setup_saves_a_found_provider_on_its_default_link() {
        use crate::config::host::HostLink;
        let _home = crate::config::TestConfigHome::new();
        let (mut ctx, _capture, discovery) = wizard_with_llama_cpp_found();
        let selected: HashSet<String> = ["llama-cpp".to_string()].into();

        SetupCommands::configure_new_providers(&mut ctx, &discovery, &selected);

        let provider = ctx.config().get_provider("llama-cpp").unwrap();
        assert_eq!(provider.host, Some(HostLink::Local));
        assert_eq!(provider.config["base_url"], "http://localhost:8080");
    }

    #[test]
    fn a_found_provider_is_linked_to_the_host_it_was_found_on() {
        use crate::config::host::HostLink;
        let spark = HostLink::Host("spark".to_string());
        let mut ctx = ctx_with_linked_providers(&[("lab", "llama-cpp", HostLink::Local)]);
        assert_eq!(provider_link("ollama", &ctx), HostLink::Local);

        ctx.add_found_provider(crate::config::ProviderConfig {
            provider_id: "ollama-spark".to_string(),
            provider_type: "ollama".to_string(),
            config: serde_json::json!({ "base_url": "http://spark.lan:11434" }),
            host: Some(spark.clone()),
        });

        assert_eq!(provider_link("ollama-spark", &ctx), spark);
        assert_eq!(provider_link("ollama", &ctx), HostLink::Local);
        assert_eq!(provider_link("openrouter", &ctx), HostLink::Unspecified);
        assert_eq!(provider_link("lab", &ctx), HostLink::Local);
    }

    #[test]
    fn default_base_urls_are_moved_to_the_host_named() {
        let urls = vec![
            "http://localhost:8080".to_string(),
            "http://127.0.0.1:8080".to_string(),
            "http://localhost:1234/v1".to_string(),
        ];

        assert_eq!(
            base_urls_at(&urls, "spark.lan"),
            vec![
                "http://spark.lan:8080".to_string(),
                "http://spark.lan:1234/v1".to_string(),
            ]
        );
        assert_eq!(
            base_urls_at(&urls, "192.168.1.20"),
            vec![
                "http://192.168.1.20:8080".to_string(),
                "http://192.168.1.20:1234/v1".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn the_wizard_ranks_models_on_the_host_a_provider_was_found_on() {
        use crate::config::host::HostLink;
        let recommends_30b = |d: &DiscoveryResult| {
            d.recommendations.iter().any(|r| {
                matches!(r, Recommendation::Model { model_id, .. } if model_id == "granite-4.2-30b")
            })
        };
        let (mut ctx, _, _) = wizard_with_llama_cpp_found();
        let providers = providers_with_healthy(&ctx, &["ollama"]);
        let on_local = Discover::run_with_providers(&ctx, &small_local(), providers).await;
        assert!(!recommends_30b(&on_local));

        ctx.add_found_provider(crate::config::ProviderConfig {
            provider_id: "ollama-spark".to_string(),
            provider_type: "ollama".to_string(),
            config: serde_json::json!({ "base_url": "http://spark.lan:11434" }),
            host: Some(HostLink::Host("spark".to_string())),
        });
        let (mut recs, configured) = providers_with_healthy(&ctx, &["ollama"]);
        for rec in &mut recs {
            if let Recommendation::Provider {
                provider_id,
                health_healthy: true,
                ..
            } = rec
            {
                *provider_id = "ollama-spark".to_string();
            }
        }
        let on_spark = Discover::run_with_providers(&ctx, &small_local(), (recs, configured)).await;

        assert!(recommends_30b(&on_spark));
    }

    /// A machine too small for granite-4.2-30b.
    fn small_local() -> HardwareProfile {
        HardwareProfile::with_memory(crate::utils::hardware::Memory::System { ram_gb: 16.0 })
    }

    /// A model with an OpenRouter variant. It is looked up, since catalog
    /// updates change which models have one.
    fn hosted_model_id() -> String {
        MODEL_REGISTRY
            .entries()
            .iter()
            .filter(|(_, m)| m.variants.iter().any(|v| v.format == "OpenRouter"))
            .map(|(id, _)| id.to_string())
            .min()
            .expect("a model with an OpenRouter variant")
    }

    fn set_for(model: &str, format: &str) -> recommended_config::RecommendedModelSet {
        recommended_config::RecommendedModelSet {
            min_context_length: None,
            models: vec![recommended_config::RecommendedModel {
                model: recommended_config::StringMatch::Exact(model.to_string()),
                variant_formats: vec![recommended_config::StringMatch::Exact(format.to_string())],
                variant_precisions: vec![],
            }],
        }
    }

    #[test]
    fn a_model_rejected_for_a_local_provider_is_accepted_on_larger_hardware() {
        use crate::config::host::HostLink;
        let set = set_for("granite-4.2-30b", "GGUF");

        let local_only = ctx_with_linked_providers(&[("ollama", "ollama", HostLink::Local)]);
        let on_spark =
            ctx_with_linked_providers(&[("ollama", "ollama", HostLink::Host("spark".to_string()))]);
        let ids = vec!["ollama".to_string()];

        assert!(resolve_model_set(&set, &small_local(), &ids, &local_only).is_none());
        let (model_id, _) = resolve_model_set(&set, &small_local(), &ids, &on_spark)
            .expect("granite-4.2-30b fits on the Spark");
        assert_eq!(model_id, "granite-4.2-30b");
    }

    #[test]
    fn on_a_local_host_with_two_cards_a_provider_that_splits_uses_both() {
        use crate::config::host::HostLink;
        let _home = crate::config::TestConfigHome::new();
        let card = || {
            HardwareProfile::with_memory(crate::utils::hardware::Memory::Discrete {
                vram_gb: 24.0,
                ram_gb: Some(64.0),
            })
        };
        let mut ctx = ctx_with_linked_providers(&[
            ("llama-cpp", "llama-cpp", HostLink::Local),
            ("vllm", "openai-compatible", HostLink::Local),
        ]);
        ctx.set_other_gpus(vec![card()]);
        ctx.ensure_local_host(card).unwrap();
        let local = ctx.hardware_profile("local").unwrap();

        let usable = |id: &str| {
            provider_hardware(id, &local, &ctx)
                .unwrap()
                .usable_memory_gb()
        };

        assert_eq!(usable("llama-cpp"), 2.0 * 16.0 - 1.0);
        assert_eq!(usable("vllm"), 16.0);
        // A type found in this run, not configured yet, gets its defaults.
        assert_eq!(usable("ollama"), 2.0 * 16.0 - 1.0);
    }

    #[test]
    fn fit_through_a_host_is_the_fit_on_the_profile_it_lists() {
        use crate::config::host::HostLink;
        let ctx =
            ctx_with_linked_providers(&[("ollama", "ollama", HostLink::Host("spark".to_string()))]);
        let spark = ctx.hardware_profile("spark").unwrap();
        let through_host = provider_hardware("ollama", &small_local(), &ctx).unwrap();

        for model_id in ["granite-4.2-3b", "granite-4.2-30b"] {
            let md = MODEL_REGISTRY.get(model_id).unwrap();
            for variant in &md.variants {
                let fit = |hardware: &HardwareProfile| {
                    crate::models::context_fit::estimate(
                        md.context_length,
                        &md.architecture,
                        &md.native_dtype,
                        variant,
                        hardware,
                    )
                };
                assert_eq!(fit(&through_host), fit(&spark), "{model_id} {variant:?}");
            }
        }
    }

    #[test]
    fn a_provider_without_a_hardware_profile_is_listed_and_not_selected() {
        use crate::config::host::HostLink;
        let set = set_for("granite-4.2-3b", "GGUF");
        let ctx = ctx_with_linked_providers(&[("ollama", "ollama", HostLink::Unspecified)]);
        let ids = vec!["ollama".to_string()];

        assert!(resolve_model_set(&set, &test_hardware_profile(), &ids, &ctx).is_none());
        let notes = fit_unavailable_candidates(&set, &test_hardware_profile(), &ids, &ctx);
        assert!(!notes.is_empty());
        assert!(
            notes.len() == 1
                && notes[0].starts_with("granite-4.2-3b on ollama (GGUF / ")
                && notes[0].ends_with("): no host for 'ollama'"),
            "{notes:?}"
        );
    }

    #[test]
    fn a_hosted_variant_is_listed_and_not_selected() {
        use crate::config::host::HostLink;
        let model = hosted_model_id();
        let set = set_for(&model, "OpenRouter");
        // The default link for a hosted type. The reason names the missing
        // size, not the missing hardware profile.
        let ctx = ctx_with_linked_providers(&[("openrouter", "openrouter", HostLink::Unspecified)]);
        let ids = vec!["openrouter".to_string()];

        assert!(resolve_model_set(&set, &test_hardware_profile(), &ids, &ctx).is_none());
        assert_eq!(
            fit_unavailable_candidates(&set, &test_hardware_profile(), &ids, &ctx),
            vec![format!(
                "{model} on openrouter (OpenRouter): hosted provider doesn't specify size"
            )]
        );
    }

    #[test]
    fn a_size_less_variant_on_a_provider_the_user_runs_names_the_missing_size() {
        use crate::config::host::HostLink;
        let model = hosted_model_id();
        let set = set_for(&model, "OpenRouter");
        // openai-compatible can serve the variant and is not hosted, so the
        // reason is the size the variant does not state.
        let ctx = ctx_with_linked_providers(&[(
            "openai-compatible",
            "openai-compatible",
            HostLink::Local,
        )]);
        let ids = vec!["openai-compatible".to_string()];

        assert!(resolve_model_set(&set, &test_hardware_profile(), &ids, &ctx).is_none());
        assert_eq!(
            fit_unavailable_candidates(&set, &test_hardware_profile(), &ids, &ctx),
            vec![format!(
                "{model} on openai-compatible (OpenRouter): no size"
            )]
        );
    }

    #[tokio::test]
    async fn the_variant_default_follows_the_hardware_a_provider_is_linked_to() {
        use crate::config::host::HostLink;
        let mut ctx = ctx_with_linked_providers(&[(
            "llama-cpp",
            "llama-cpp",
            HostLink::Host("spark".to_string()),
        )]);
        ctx.config_mut().hardware.insert(
            crate::config::hardware::LOCAL_HARDWARE_ID.to_string(),
            crate::config::HardwareConfig::local_from_detection(small_local()),
        );
        let md = MODEL_REGISTRY.get("granite-4.2-30b").unwrap();
        // Discovery, ranked for the default link, recommended the smallest.
        let gguf: Vec<ModelVariant> = md
            .variants
            .iter()
            .filter(|v| v.format == "GGUF")
            .cloned()
            .collect();
        let smallest = gguf
            .iter()
            .min_by(|a, b| a.size_gb.partial_cmp(&b.size_gb).unwrap())
            .unwrap()
            .clone();
        let smallest_size = smallest.size_gb;
        let discovery = DiscoveryResult {
            recommendations: vec![model_recommendation("granite-4.2-30b", &md, smallest)],
            all_model_candidates: vec![],
            configured_provider_ids: vec![],
            configured_model_ids: vec![],
            configured_launcher_ids: vec![],
            configured_capability_ids: vec![],
        };
        let models: HashSet<String> = ["granite-4.2-30b".to_string()].into_iter().collect();
        let providers: HashSet<String> = ["llama-cpp".to_string()].into_iter().collect();

        let chosen = SetupCommands::select_variants(&mut ctx, &discovery, &models, &providers)
            .await
            .unwrap();

        // The default is no longer discovery's pick: it is a larger variant
        // that fits fully on the Spark the provider is linked to.
        let picked = chosen.get("granite-4.2-30b").unwrap();
        assert!(picked.size_gb > smallest_size, "{picked:?}");
        let spark = ctx.hardware_profile("spark").unwrap();
        assert_eq!(
            crate::models::context_fit::estimate(
                md.context_length,
                &md.architecture,
                &md.native_dtype,
                picked,
                &spark,
            ),
            ContextFit::Full
        );
    }

    #[test]
    fn the_variant_list_names_hardware_other_than_local() {
        use crate::config::host::HostLink;
        let ctx = ctx_with_linked_providers(&[
            ("ollama", "ollama", HostLink::Local),
            (
                "llama-cpp",
                "llama-cpp",
                HostLink::Host("spark".to_string()),
            ),
        ]);
        let md = MODEL_REGISTRY.get("granite-4.2-30b").unwrap();
        let gguf = md.variants.iter().find(|v| v.format == "GGUF").unwrap();

        let local_only: HashSet<String> = ["ollama".to_string()].into();
        let both: HashSet<String> = ["ollama".to_string(), "llama-cpp".to_string()].into();

        assert_eq!(variant_host_note(gguf, &local_only, &ctx), "");
        assert_eq!(variant_host_note(gguf, &both, &ctx), " (host: spark)");
    }

    #[test]
    fn min_context_length_is_checked_against_the_providers_hardware() {
        use crate::config::host::HostLink;
        // 64 GB of system RAM gives granite-4.2-30b only part of its context.
        let local =
            HardwareProfile::with_memory(crate::utils::hardware::Memory::System { ram_gb: 64.0 });
        let mut set = set_for("granite-4.2-30b", "GGUF");
        let ids = vec!["ollama".to_string()];
        let on_local = ctx_with_linked_providers(&[("ollama", "ollama", HostLink::Local)]);
        let on_spark =
            ctx_with_linked_providers(&[("ollama", "ollama", HostLink::Host("spark".to_string()))]);

        assert!(resolve_model_set(&set, &local, &ids, &on_local).is_some());
        set.min_context_length = Some(131_072);
        assert!(resolve_model_set(&set, &local, &ids, &on_local).is_none());
        assert!(resolve_model_set(&set, &local, &ids, &on_spark).is_some());
    }

    #[test]
    fn a_variant_keeps_its_best_fit_across_providers() {
        use crate::config::host::HostLink;
        let ctx = ctx_with_linked_providers(&[
            ("ollama", "ollama", HostLink::Local),
            (
                "llama-cpp",
                "llama-cpp",
                HostLink::Host("spark".to_string()),
            ),
        ]);
        let md = MODEL_REGISTRY.get("granite-4.2-30b").unwrap();
        let ids = vec!["ollama".to_string(), "llama-cpp".to_string()];

        let ranked = rank_variants_for_providers(
            &md,
            md.variants.iter().filter(|v| v.format == "GGUF"),
            &small_local(),
            &ids,
            &ctx,
        );

        // Nothing fits the small local machine, so every fit here is the
        // Spark's, and the largest variant leads with a full fit.
        assert!(!ranked.is_empty());
        assert_eq!(ranked[0].1, ContextFit::Full);
    }

    #[test]
    fn the_variant_note_names_the_default_link_of_a_discovered_provider() {
        // openrouter is found by discovery but not configured yet: setup will
        // add it with its default link, "not specified".
        let ctx = test_ctx();
        let md = MODEL_REGISTRY.get(&hosted_model_id()).unwrap();
        let hosted = md
            .variants
            .iter()
            .find(|v| v.format == "OpenRouter")
            .expect("an OpenRouter variant");
        let selected: HashSet<String> = ["openrouter".to_string()].into_iter().collect();

        assert_eq!(
            variant_host_note(hosted, &selected, &ctx),
            " (host: not specified)"
        );
    }

    #[test]
    fn a_model_is_bound_to_the_provider_where_its_variant_fits() {
        use crate::config::host::HostLink;
        // Named so that the id sorting first, `ollama`, is the one on the
        // small machine: only the fit can pick `z-llama`.
        let mut ctx = ctx_with_linked_providers(&[
            ("ollama", "ollama", HostLink::Local),
            ("z-llama", "llama-cpp", HostLink::Host("spark".to_string())),
        ]);
        ctx.config_mut().hardware.insert(
            crate::config::hardware::LOCAL_HARDWARE_ID.to_string(),
            crate::config::HardwareConfig::local_from_detection(small_local()),
        );
        let md = MODEL_REGISTRY.get("granite-4.2-30b").unwrap();
        let largest = md
            .variants
            .iter()
            .filter(|v| v.format == "GGUF")
            .max_by(|a, b| a.size_gb.partial_cmp(&b.size_gb).unwrap())
            .unwrap();
        let selected: HashSet<String> = ["ollama".to_string(), "z-llama".to_string()]
            .into_iter()
            .collect();

        let provider =
            SetupCommands::find_compatible_provider("granite-4.2-30b", largest, &selected, &ctx);

        assert_eq!(provider.as_deref(), Some("z-llama"));
    }

    #[test]
    fn a_model_that_fits_on_both_stays_on_the_provider_on_local() {
        use crate::config::host::HostLink;
        // `llamacpp-spark` sorts before `ollama`, so only the link can pick
        // `ollama`.
        let mut ctx = ctx_with_linked_providers(&[
            ("ollama", "ollama", HostLink::Local),
            (
                "llamacpp-spark",
                "llama-cpp",
                HostLink::Host("spark".to_string()),
            ),
        ]);
        // This machine has the same figures as the Spark, so every fit ties.
        let spark = ctx.hardware_profile("spark").unwrap();
        ctx.config_mut().hardware.insert(
            crate::config::hardware::LOCAL_HARDWARE_ID.to_string(),
            crate::config::HardwareConfig::local_from_detection(spark),
        );
        let md = MODEL_REGISTRY.get("granite-4.2-3b").unwrap();
        let smallest = md
            .variants
            .iter()
            .filter(|v| v.format == "GGUF")
            .min_by(|a, b| a.size_gb.partial_cmp(&b.size_gb).unwrap())
            .unwrap();
        let selected: HashSet<String> = ["ollama".to_string(), "llamacpp-spark".to_string()]
            .into_iter()
            .collect();
        let fit_on = |profile: &crate::utils::hardware::HardwareProfile| {
            crate::models::context_fit::estimate(
                md.context_length,
                &md.architecture,
                &md.native_dtype,
                smallest,
                profile,
            )
        };
        let local = ctx
            .hardware_profile(crate::config::hardware::LOCAL_HARDWARE_ID)
            .unwrap();
        let spark = ctx.hardware_profile("spark").unwrap();
        assert_eq!(fit_on(&local), fit_on(&spark), "the test needs a tie");

        let provider =
            SetupCommands::find_compatible_provider("granite-4.2-3b", smallest, &selected, &ctx);

        assert_eq!(provider.as_deref(), Some("ollama"));
    }

    #[tokio::test]
    async fn the_wizard_offers_to_repair_a_configured_provider_with_a_broken_link() {
        use crate::config::host::HostLink;
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_linked_providers(&[
            ("ollama", "ollama", HostLink::Local),
            ("llama-cpp", "llama-cpp", HostLink::Host("gone".to_string())),
        ]);
        let capture = Arc::new(CaptureUi::default());
        ctx.ui = capture.clone();

        // Declined (the default): the provider stays, still broken.
        SetupCommands::repair_provider_links(&mut ctx, true)
            .await
            .unwrap();
        assert!(ctx.config().get_provider("llama-cpp").is_some());
        let prompts = capture.select_prompts.borrow().clone();
        assert_eq!(prompts.len(), 1, "only the broken provider is asked about");
        let remove = prompts[0]
            .1
            .iter()
            .position(|item| item.starts_with("Remove"))
            .expect("a remove choice");

        capture.select_answers.borrow_mut().push_back(remove);
        SetupCommands::repair_provider_links(&mut ctx, true)
            .await
            .unwrap();

        assert!(ctx.config().get_provider("llama-cpp").is_none());
        assert!(ctx.config().get_provider("ollama").is_some());
    }

    #[tokio::test]
    async fn a_run_that_cannot_ask_warns_about_a_broken_link() {
        use crate::config::host::HostLink;
        let mut ctx = ctx_with_linked_providers(&[(
            "llama-cpp",
            "llama-cpp",
            HostLink::Host("gone".to_string()),
        )]);
        let capture = Arc::new(CaptureUi::default());
        ctx.ui = capture.clone();

        SetupCommands::repair_provider_links(&mut ctx, false)
            .await
            .unwrap();

        assert!(capture.select_prompts.borrow().is_empty());
        let warns = capture.warns.borrow();
        assert!(warns.iter().any(|w| w.contains("'gone'")), "{warns:?}");
        assert!(ctx.config().get_provider("llama-cpp").is_some());
    }

    #[test]
    fn auto_setup_names_a_candidate_whose_fit_is_not_available() {
        let model = hosted_model_id();
        use crate::config::host::HostLink;
        let mut ctx = ctx_with_linked_providers(&[("openrouter", "openrouter", HostLink::Local)]);
        let capture = Arc::new(CaptureUi::default());
        ctx.ui = capture.clone();
        ctx.config_mut().recommended_configs.insert(
            "claude".to_string(),
            recommended_config::RecommendedConfiguration {
                launcher: "claude".to_string(),
                capabilities: vec![recommended_config::RecommendedCapability {
                    capability: "agent-model".to_string(),
                    models: [("model_id".to_string(), set_for(&model, "OpenRouter"))].into(),
                }],
            },
        );
        let launchers: HashSet<String> = ["claude".to_string()].into();

        let selection =
            SetupCommands::auto_selection(&ctx, &launchers, &[], &test_hardware_profile());

        assert!(!selection.capabilities.contains("agent-model"));
        assert_eq!(
            *capture.infos.borrow(),
            vec![
                "Capability 'agent-model' not selected; fit not available for:".to_string(),
                format!(
                    "  {model} on openrouter (OpenRouter): hosted provider doesn't specify size"
                ),
                "Choose a model for these by hand with `granite-cli model setup`.".to_string(),
            ]
        );
    }

    #[test]
    fn auto_setup_names_a_capability_once_for_every_launcher_that_recommends_it() {
        let model = hosted_model_id();
        use crate::config::host::HostLink;
        let mut ctx = ctx_with_linked_providers(&[("openrouter", "openrouter", HostLink::Local)]);
        let capture = Arc::new(CaptureUi::default());
        ctx.ui = capture.clone();
        for launcher in ["claude", "opencode"] {
            ctx.config_mut().recommended_configs.insert(
                launcher.to_string(),
                recommended_config::RecommendedConfiguration {
                    launcher: launcher.to_string(),
                    capabilities: vec![recommended_config::RecommendedCapability {
                        capability: "agent-model".to_string(),
                        models: [("model_id".to_string(), set_for(&model, "OpenRouter"))].into(),
                    }],
                },
            );
        }
        let launchers: HashSet<String> = ["claude".to_string(), "opencode".to_string()].into();

        SetupCommands::auto_selection(&ctx, &launchers, &[], &test_hardware_profile());

        let infos = capture.infos.borrow();
        let headers = infos
            .iter()
            .filter(|m| m.starts_with("Capability 'agent-model'"));
        assert_eq!(headers.count(), 1, "{infos:?}");
        assert_eq!(infos.len(), 3, "{infos:?}");
    }

    // -- models the providers found serve --------------------------------------

    /// A model as a provider's listing reports it.
    fn listed(
        id: &str,
        model_type: ModelType,
        functions: &[ModelFunction],
    ) -> crate::providers::ProviderModel {
        crate::providers::ProviderModel {
            id: id.to_string(),
            display_name: None,
            context_length: Some(8192),
            parameter_count: None,
            disk_size_bytes: None,
            model_type: Some(model_type),
            supported_functions: functions.to_vec(),
        }
    }

    /// A listed chat model with tool calling.
    fn chat(id: &str) -> crate::providers::ProviderModel {
        listed(
            id,
            ModelType::Text,
            &[ModelFunction::Chat, ModelFunction::ToolCalling],
        )
    }

    /// The `nth` variant of catalog model `model_id` a provider of
    /// `provider_type` can run, with the name it serves the model under in
    /// that variant.
    fn served_name(provider_type: &str, model_id: &str, nth: usize) -> (String, ModelVariant) {
        let provider = PROVIDER_REGISTRY
            .construct(
                provider_type,
                provider_type,
                &PROVIDER_REGISTRY.default_config(provider_type),
            )
            .unwrap();
        let md = MODEL_REGISTRY.get(model_id).unwrap();
        let variant = md
            .variants
            .iter()
            .filter(|v| provider.can_run_model(&v.format, &v.precision))
            .nth(nth)
            .unwrap_or_else(|| panic!("{provider_type} runs no variant {nth} of {model_id}"))
            .clone();
        (wire_name(&*provider, model_id, &variant), variant)
    }

    /// `discovery`, with the healthy provider `provider_type` listing
    /// `models`.
    fn serving(
        mut discovery: DiscoveryResult,
        provider_type: &str,
        models: Vec<crate::providers::ProviderModel>,
    ) -> DiscoveryResult {
        for rec in &mut discovery.recommendations {
            if let Recommendation::Provider {
                provider_type: found,
                health_healthy: true,
                served_models,
                ..
            } = rec
                && *found == provider_type
            {
                *served_models = models;
                return discovery;
            }
        }
        panic!("{provider_type} is not a healthy provider in this discovery");
    }

    fn same_variant(a: &ModelVariant, b: &ModelVariant) -> bool {
        a.format == b.format && a.precision == b.precision
    }

    fn pick(provider_id: &str, wire_id: &str) -> ServedPick {
        ServedPick {
            provider_id: provider_id.to_string(),
            wire_id: wire_id.to_string(),
        }
    }

    #[tokio::test]
    async fn the_wizard_records_what_each_provider_found_serves() {
        let server = FakeServer::start("ollama", &["llama3:8b"]).await;
        let base_url = server.base_url().to_string();
        let mut recommendations = vec![
            Recommendation::Provider {
                provider_type: "ollama",
                provider_id: "ollama".to_string(),
                provider_name: "Ollama".to_string(),
                health_healthy: true,
                health_error: None,
                discovered_base_url: Some(base_url),
                served_models: Vec::new(),
            },
            Recommendation::Provider {
                provider_type: "llama-cpp",
                provider_id: "llama-cpp".to_string(),
                provider_name: "llama.cpp".to_string(),
                health_healthy: false,
                health_error: Some("not running".to_string()),
                discovered_base_url: None,
                served_models: Vec::new(),
            },
        ];

        Discover::list_served_models(&mut recommendations).await;

        let served: Vec<Vec<String>> = recommendations
            .iter()
            .map(|rec| match rec {
                Recommendation::Provider { served_models, .. } => {
                    served_models.iter().map(|m| m.id.clone()).collect()
                }
                _ => Vec::new(),
            })
            .collect();
        assert_eq!(served, vec![vec!["llama3:8b".to_string()], Vec::new()]);
    }

    #[tokio::test]
    async fn a_listed_model_is_a_catalog_model_when_served_under_its_name() {
        let ctx = test_ctx();
        let (name, variant) = served_name("ollama", "granite-4.2-8b", 0);
        let discovery = serving(
            run_discovery(&ctx).await,
            "ollama",
            vec![chat(&name), chat("llama3:8b")],
        );

        let served = served_models(&discovery);

        let on_servers = &served.catalog["granite-4.2-8b"];
        assert_eq!(on_servers.len(), 1);
        assert_eq!(on_servers[0].0, "ollama");
        assert!(same_variant(&on_servers[0].1, &variant));
        let others: Vec<(&str, &str)> = served
            .others
            .iter()
            .map(|(provider_id, model)| (provider_id.as_str(), model.id.as_str()))
            .collect();
        assert_eq!(others, vec![("ollama", "llama3:8b")]);
    }

    #[tokio::test]
    async fn only_server_models_a_selected_capability_can_use_are_offered() {
        let ctx = test_ctx();
        let discovery = serving(
            run_discovery(&ctx).await,
            "ollama",
            vec![
                chat("llama3:8b"),
                listed(
                    "nomic-embed-text:latest",
                    ModelType::Embedding,
                    &[ModelFunction::Embeddings],
                ),
            ],
        );
        let served = served_models(&discovery);
        let lines = |caps: &[&str]| -> Vec<String> {
            SetupCommands::served_model_options(&served, &string_set(caps))
                .into_iter()
                .map(|(_, line)| line)
                .collect()
        };

        assert_eq!(
            lines(&["agent-model"]),
            vec!["llama3:8b — on ollama, not in the catalog".to_string()]
        );
        assert_eq!(lines(&[]).len(), 2, "no capability asks for a model");
    }

    #[tokio::test]
    async fn the_model_list_marks_catalog_models_a_server_has_and_offers_the_others() {
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());
        let discovery = run_discovery(&ctx).await;
        let Some(Recommendation::Model { model_id, .. }) = discovery
            .recommendations
            .iter()
            .find(|r| matches!(r, Recommendation::Model { .. }))
        else {
            panic!("discovery recommends no model");
        };
        let model_id = model_id.clone();
        let (name, _) = served_name("ollama", &model_id, 0);
        let discovery = serving(discovery, "ollama", vec![chat(&name), chat("llama3:8b")]);
        let server_only = "llama3:8b — on ollama, not in the catalog";
        let index = {
            let served = served_models(&discovery);
            let options = SetupCommands::model_options(Revaluator::for_models(
                &discovery.recommendations,
                &HashSet::new(),
            ));
            assert!(SetupCommands::served_model_options(&served, &HashSet::new()).len() == 1);
            options.len()
        };
        capture
            .multi_select_answers
            .borrow_mut()
            .push_back(vec![index]);

        let (models, picks) =
            SetupCommands::select_models(&mut ctx, &discovery, &HashSet::new(), &HashSet::new())
                .await
                .unwrap();

        let prompts = capture.multi_select_prompts.borrow();
        let (_, items, defaults) = &prompts[0];
        let catalog_line = items
            .iter()
            .find(|item| item.starts_with(&format!("{model_id} — ")))
            .unwrap();
        assert!(
            catalog_line.ends_with("— already on: ollama"),
            "{catalog_line}"
        );
        assert_eq!(items[index], server_only);
        assert!(!defaults[index], "a server model is offered unchecked");
        assert!(models.is_empty());
        assert_eq!(picks, vec![pick("ollama", "llama3:8b")]);
    }

    #[tokio::test]
    async fn the_provider_serving_a_picked_model_is_offered_checked() {
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());
        let (_, variant) = served_name("llama-cpp", "granite-4.2-8b", 0);
        // The selected catalog model can run only on llama-cpp, which leaves
        // ollama out of the providers offered for it.
        let discovery = DiscoveryResult {
            recommendations: vec![
                Recommendation::Provider {
                    provider_type: "llama-cpp",
                    provider_id: "llama-cpp".to_string(),
                    provider_name: "llama.cpp".to_string(),
                    health_healthy: true,
                    health_error: None,
                    discovered_base_url: Some("http://localhost:8080".to_string()),
                    served_models: Vec::new(),
                },
                Recommendation::Provider {
                    provider_type: "ollama",
                    provider_id: "ollama".to_string(),
                    provider_name: "Ollama".to_string(),
                    health_healthy: true,
                    health_error: None,
                    discovered_base_url: Some("http://localhost:11434".to_string()),
                    served_models: vec![chat("llama3:8b")],
                },
                Recommendation::Model {
                    model_id: "granite-4.2-8b".to_string(),
                    family: "Granite".to_string(),
                    version: "4.2".to_string(),
                    size: "8B".to_string(),
                    best_variant: variant,
                    context_fit: ContextFit::Full,
                    can_run_by: vec!["llama-cpp".to_string()],
                },
            ],
            ..no_discovery()
        };
        capture
            .multi_select_answers
            .borrow_mut()
            .push_back(vec![0, 1]);

        SetupCommands::select_providers(
            &mut ctx,
            &discovery,
            &string_set(&["granite-4.2-8b"]),
            &[pick("ollama", "llama3:8b")],
        )
        .await
        .unwrap();

        let prompts = capture.multi_select_prompts.borrow();
        let (_, items, defaults) = &prompts[0];
        let checked = |id: &str| {
            let i = items
                .iter()
                .position(|item| item.starts_with(&format!("{id} — ")))
                .unwrap_or_else(|| panic!("{id} is not offered: {items:?}"));
            defaults[i]
        };
        assert!(checked("ollama"));
        assert!(!checked("llama-cpp"));
    }

    #[tokio::test]
    async fn a_model_a_selected_provider_serves_keeps_the_variant_it_has() {
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());
        // Not the first variant, which the ranking would likely pick.
        let (name, variant) = served_name("ollama", "granite-4.2-8b", 1);
        let discovery = serving(run_discovery(&ctx).await, "ollama", vec![chat(&name)]);

        let chosen = SetupCommands::select_variants(
            &mut ctx,
            &discovery,
            &string_set(&["granite-4.2-8b"]),
            &string_set(&["ollama"]),
        )
        .await
        .unwrap();

        assert!(same_variant(&chosen["granite-4.2-8b"], &variant));
        assert!(capture.select_prompts.borrow().is_empty());
        assert!(
            capture
                .infos
                .borrow()
                .iter()
                .any(|info| info.starts_with("'granite-4.2-8b' is already on ollama as ")),
            "{:?}",
            capture.infos.borrow()
        );
    }

    #[tokio::test]
    async fn a_picked_server_model_is_written_and_bound_to_a_capability() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        let discovery = serving(run_discovery(&ctx).await, "ollama", vec![chat("llama3:8b")]);

        SetupCommands::configure_all(
            &mut ctx,
            &discovery,
            &string_set(&["agent-model"]),
            &HashSet::new(),
            &string_set(&["ollama"]),
            &HashSet::new(),
            &[pick("ollama", "llama3:8b")],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();

        let model = ctx.config().get_model("llama3:8b").expect("written");
        let discovered = model.discovered.as_ref().expect("managed by sync");
        assert_eq!(discovered.provider_id, "ollama");
        assert_eq!(discovered.provider_model_id, "llama3:8b");
        assert_eq!(
            ctx.config().get_capability("agent-model").unwrap().config["model_id"],
            "llama3:8b"
        );
    }

    #[tokio::test]
    async fn a_picked_server_model_of_a_provider_not_selected_is_skipped() {
        let _home = crate::config::TestConfigHome::new();
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());
        let discovery = serving(run_discovery(&ctx).await, "ollama", vec![chat("llama3:8b")]);

        SetupCommands::configure_all(
            &mut ctx,
            &discovery,
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
            &[pick("ollama", "llama3:8b")],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();

        assert!(ctx.config().get_model("llama3:8b").is_none());
        assert_eq!(
            *capture.warns.borrow(),
            vec!["Skipping 'llama3:8b': provider 'ollama' was not selected.".to_string()]
        );
    }

    #[tokio::test]
    async fn a_catalog_model_runs_on_the_selected_provider_that_serves_it() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        let (name, variant) = served_name("ollama", "granite-4.2-8b", 0);
        let providers = providers_with_healthy(&ctx, &["llama-cpp", "ollama"]);
        let discovery =
            Discover::run_with_providers(&ctx, &test_hardware_profile(), providers).await;
        let discovery = serving(discovery, "ollama", vec![chat(&name)]);
        let selected_providers = string_set(&["llama-cpp", "ollama"]);

        SetupCommands::configure_all(
            &mut ctx,
            &discovery,
            &HashSet::new(),
            &HashSet::new(),
            &selected_providers,
            &string_set(&["granite-4.2-8b"]),
            &[],
            &HashMap::from([("granite-4.2-8b".to_string(), variant.clone())]),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await
        .unwrap();

        // Without the listing, llama-cpp would run it.
        assert_eq!(
            SetupCommands::find_compatible_provider(
                "granite-4.2-8b",
                &variant,
                &selected_providers,
                &ctx
            )
            .as_deref(),
            Some("llama-cpp")
        );
        assert_eq!(
            ctx.config()
                .get_model("granite-4.2-8b")
                .unwrap()
                .provider_id,
            "ollama"
        );
    }

    #[tokio::test]
    async fn a_model_already_on_its_provider_is_not_offered_for_pull() {
        let mut ctx = test_ctx();
        let (name, variant) = served_name("ollama", "granite-4.2-8b", 0);
        let discovery = serving(run_discovery(&ctx).await, "ollama", vec![chat(&name)]);
        for (model_id, variant) in [
            ("granite-4.2-8b", variant.clone()),
            (
                "granite-4.2-3b",
                served_name("ollama", "granite-4.2-3b", 0).1,
            ),
        ] {
            ctx.config_mut().models.insert(
                model_id.to_string(),
                ModelConfig {
                    discovered: None,
                    model_id: model_id.to_string(),
                    model_type: model_id.to_string(),
                    config: serde_json::json!({}),
                    provider_id: "ollama".to_string(),
                    variant: Some(format!("{}/{}", variant.format, variant.precision)),
                },
            );
        }

        let to_pull = SetupCommands::not_yet_served(
            &ctx,
            &discovery,
            &string_set(&["granite-4.2-8b", "granite-4.2-3b"]),
        );

        assert_eq!(to_pull, string_set(&["granite-4.2-3b"]));
    }

    // -- the wizard, from the first question to the summary ---------------------

    /// What a wizard test drives: the context and its capture UI.
    struct Wizard {
        ctx: crate::AppContext,
        ui: Arc<CaptureUi>,
        local: HardwareProfile,
    }

    /// A wizard over the temporary configuration home with `local` saved in
    /// manual mode with the figures `local`, so neither the check nor
    /// ranking reads the machine, and every provider type's default URL
    /// closed except the fake servers given.
    async fn wizard(local: HardwareProfile, servers: &[(&str, &FakeServer)]) -> Wizard {
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext::new(Config::new().unwrap(), ui.clone());
        ctx.config_mut()
            .insert_hardware(
                crate::config::hardware::LOCAL_HARDWARE_ID,
                crate::config::HardwareConfig::local(crate::config::hardware::LocalHardware {
                    mode: crate::config::hardware::LocalMode::Manual,
                    profile: local.clone(),
                    detected: None,
                }),
            )
            .unwrap();
        let figures = local.clone();
        ctx.ensure_local_host(move || figures).unwrap();
        probe_only(&mut ctx, servers).await;
        // No launcher on PATH, unless a test says so.
        ctx.set_launcher_paths(&[]);
        Wizard { ctx, ui, local }
    }

    fn unified(gb: f64) -> HardwareProfile {
        HardwareProfile::with_memory(crate::utils::hardware::Memory::Unified {
            vram_gb: gb,
            ram_gb: Some(gb),
        })
    }

    impl Wizard {
        async fn run(&mut self) -> Result<()> {
            let local = self.local.clone();
            SetupCommands::run_wizard_with(&mut self.ctx, Some(false), move || local).await
        }

        /// Queues the answers of the next multi-selects, in order.
        fn answer(&self, answers: &[&[usize]]) {
            for a in answers {
                self.ui
                    .multi_select_answers
                    .borrow_mut()
                    .push_back(a.to_vec());
            }
        }

        fn text(&self, answers: &[&str]) {
            for a in answers {
                self.ui.text_answers.borrow_mut().push_back(a.to_string());
            }
        }

        /// The items of the multi-select whose prompt starts with `prompt`.
        fn items(&self, prompt: &str) -> Vec<String> {
            self.ui
                .multi_select_prompts
                .borrow()
                .iter()
                .find(|(p, _, _)| p.starts_with(prompt))
                .map(|(_, items, _)| items.clone())
                .unwrap_or_else(|| panic!("no '{prompt}' question"))
        }

        fn infos(&self) -> Vec<String> {
            self.ui.infos.borrow().clone()
        }

        fn add_host(&mut self, id: &str, address: &str, hardware: &[&str]) {
            self.ctx
                .config_mut()
                .insert_host(
                    id,
                    crate::config::HostConfig {
                        host_id: id.to_string(),
                        address: address.to_string(),
                        hardware: hardware.iter().map(|h| h.to_string()).collect(),
                    },
                )
                .unwrap();
        }
    }

    /// The index of "No, continue" in the question that follows probing a
    /// host the user runs, or the hosted services when `hosted`.
    fn no_more(hosted: bool) -> usize {
        offered_types(hosted).len()
    }

    impl Wizard {
        /// Queues the answers of the next selects, in order.
        fn select(&self, answers: &[usize]) {
            self.ui.select_answers.borrow_mut().extend(answers);
        }
    }

    // The answers the tests give, in order: launchers (none, so the run does
    // not depend on what is on PATH), hosts, capabilities, models, providers.
    // After each host is probed, "No, continue" (`no_more`).
    const NO_LAUNCHER: &[usize] = &[];
    const NO_CAPABILITY: &[usize] = &[];
    const NO_MODEL: &[usize] = &[];

    #[tokio::test]
    async fn wizard_local_host_only() {
        let _home = crate::config::TestConfigHome::new();
        let server = FakeServer::start("ollama", &["llama3:8b"]).await;
        let mut w = wizard(unified(96.0), &[("ollama", &server)]).await;
        // local only; ollama.
        w.answer(&[NO_LAUNCHER, &[0], NO_CAPABILITY, NO_MODEL, &[0]]);
        w.select(&[no_more(false)]);

        w.run().await.unwrap();

        let selects = w.ui.select_prompts.borrow().clone();
        assert_eq!(
            selects[0].0,
            "Add a provider on localhost at another address?"
        );
        assert_eq!(
            selects[0].2,
            no_more(false),
            "ollama answered: No, continue"
        );
        let hosts = w.items("Hosts to run models on");
        assert_eq!(hosts[0], "local — localhost, local (unified 96.0 GB)");
        assert!(
            w.infos()
                .contains(&"\nLooking for providers on localhost...".to_string())
        );
        let provider = Config::new()
            .unwrap()
            .get_provider("ollama")
            .cloned()
            .unwrap();
        assert_eq!(provider.host, Some(crate::config::host::HostLink::Local));
        assert_eq!(provider.config["base_url"], server.base_url_at("localhost"));
        assert!(Config::new().unwrap().get_host("local").is_some());
    }

    #[tokio::test]
    async fn wizard_add_host_probes_its_address() {
        let _home = crate::config::TestConfigHome::new();
        let server = FakeServer::start("ollama", &["llama3:8b"]).await;
        let mut w = wizard(unified(96.0), &[("ollama", &server)]).await;
        // Hosts: "Add a host…" (local, hosted services, add); then local and
        // the new host. Its address, and "unknown" of create and unknown;
        // the id takes its default.
        w.answer(&[NO_LAUNCHER, &[2], &[1]]);
        w.select(&[1, no_more(false)]);
        w.text(&["127.0.0.1"]);
        w.answer(&[NO_CAPABILITY, NO_MODEL, &[0]]);

        w.run().await.unwrap();

        let second = &w.ui.multi_select_prompts.borrow()[2];
        assert_eq!(second.1[1], "host — 127.0.0.1, unknown");
        assert!(second.2[1], "the new host is checked");
        assert!(
            w.infos()
                .contains(&"\nLooking for providers on 127.0.0.1...".to_string())
        );
        let config = Config::new().unwrap();
        assert_eq!(config.get_host("host").unwrap().address, "127.0.0.1");
        let provider = config.get_provider("ollama-host").unwrap();
        assert_eq!(
            provider.host,
            Some(crate::config::host::HostLink::Host("host".to_string()))
        );
        assert_eq!(provider.config["base_url"], server.base_url());
    }

    #[tokio::test]
    async fn wizard_same_type_on_two_hosts_two_ids() {
        let _home = crate::config::TestConfigHome::new();
        let server = FakeServer::start("ollama", &[]).await;
        let mut w = wizard(unified(96.0), &[("ollama", &server)]).await;
        w.add_host("box", "127.0.0.1", &[]);
        // local and box; both providers.
        w.answer(&[NO_LAUNCHER, &[0, 1], NO_CAPABILITY, NO_MODEL, &[0, 1]]);
        w.select(&[no_more(false), no_more(false)]);

        w.run().await.unwrap();

        let config = Config::new().unwrap();
        let url = |id: &str| config.get_provider(id).unwrap().config["base_url"].clone();
        assert_eq!(url("ollama"), server.base_url_at("localhost"));
        assert_eq!(url("ollama-box"), server.base_url());
        assert_eq!(
            config.get_provider("ollama-box").unwrap().host,
            Some(crate::config::host::HostLink::Host("box".to_string()))
        );
    }

    #[tokio::test]
    async fn wizard_unknown_hardware_offers_served_models() {
        let _home = crate::config::TestConfigHome::new();
        let server = FakeServer::start("ollama", &["llama3:8b"]).await;
        let mut w = wizard(unified(96.0), &[("ollama", &server)]).await;
        w.add_host("lab", "127.0.0.1", &[]);
        // lab only (local, lab, hosted, add).
        w.answer(&[NO_LAUNCHER, &[1], NO_CAPABILITY, NO_MODEL, NO_MODEL]);
        w.select(&[no_more(false)]);

        w.run().await.unwrap();

        let models = w.items("Select models to configure");
        assert!(
            models.contains(&"llama3:8b — on ollama-lab, not in the catalog".to_string()),
            "{models:?}"
        );
        assert!(
            !models.iter().any(|m| m.contains("Fit:")),
            "no model is ranked on a host whose hardware is not known: {models:?}"
        );
    }

    #[tokio::test]
    async fn wizard_marks_models_on_configured_provider() {
        async fn run(served: &[&str]) -> Vec<String> {
            let _home = crate::config::TestConfigHome::new();
            let server = FakeServer::start("ollama", served).await;
            let mut w = wizard(unified(96.0), &[]).await;
            w.ctx
                .config_mut()
                .insert_provider(
                    "ollama",
                    crate::config::ProviderConfig {
                        provider_id: "ollama".to_string(),
                        provider_type: "ollama".to_string(),
                        config: serde_json::json!({ "base_url": server.base_url() }),
                        host: Some(crate::config::host::HostLink::Local),
                    },
                )
                .unwrap();
            w.answer(&[NO_LAUNCHER, &[0], NO_CAPABILITY, NO_MODEL, NO_MODEL]);
            w.select(&[no_more(false)]);
            w.run().await.unwrap();
            w.items("Select models to configure")
        }
        // A catalog model the wizard offers on this machine, from a run
        // where the server has none.
        let offered = run(&[]).await;
        let model_id = offered
            .iter()
            .find(|m| m.contains(" — Fit: "))
            .and_then(|m| m.split(" — ").next())
            .expect("a catalog model is offered")
            .to_string();
        let (name, _) = served_name("ollama", &model_id, 0);

        let models = run(&[&name, "llama3:8b"]).await;

        assert!(
            models
                .iter()
                .any(|m| m.starts_with(&format!("{model_id} — "))
                    && m.ends_with("already on: ollama")),
            "{models:?}"
        );
        assert!(models.contains(&"llama3:8b — on ollama, not in the catalog".to_string()));
    }

    #[tokio::test]
    async fn wizard_binds_model_where_it_fits_best() {
        let model_id = "granite-4.2-30b";
        // The index of the model in the list, from a run that picks none.
        let index = {
            let _home = crate::config::TestConfigHome::new();
            let server = FakeServer::start("llama-cpp", &[]).await;
            let mut w = wizard(unified(12.0), &[("llama-cpp", &server)]).await;
            w.ctx
                .config_mut()
                .insert_hardware(
                    "spark",
                    crate::config::HardwareConfig::catalog("spark", "dgx-spark", 128.0),
                )
                .unwrap();
            w.add_host("big", "127.0.0.1", &["spark"]);
            w.answer(&[NO_LAUNCHER, &[0, 1], NO_CAPABILITY, NO_MODEL, NO_MODEL]);
            w.select(&[no_more(false), no_more(false)]);
            w.run().await.unwrap();
            w.items("Select models to configure")
                .iter()
                .position(|m| m.starts_with(&format!("{model_id} — ")))
                .unwrap_or_else(|| panic!("{model_id} is not offered"))
        };
        let _home = crate::config::TestConfigHome::new();
        let server = FakeServer::start("llama-cpp", &[]).await;
        let mut w = wizard(unified(12.0), &[("llama-cpp", &server)]).await;
        w.ctx
            .config_mut()
            .insert_hardware(
                "spark",
                crate::config::HardwareConfig::catalog("spark", "dgx-spark", 128.0),
            )
            .unwrap();
        w.add_host("big", "127.0.0.1", &["spark"]);
        // local and big; the model; both providers.
        w.answer(&[NO_LAUNCHER, &[0, 1], NO_CAPABILITY, &[index], &[0, 1]]);
        w.select(&[no_more(false), no_more(false)]);

        w.run().await.unwrap();

        let config = Config::new().unwrap();
        assert!(config.get_provider("llama-cpp").is_some());
        assert_eq!(
            config.get_model(model_id).unwrap().provider_id,
            "llama-cpp-big",
            "the 30B model fits on the Spark, not on this 12 GB machine"
        );
    }

    #[tokio::test]
    async fn wizard_quit_writes_nothing() {
        let _home = crate::config::TestConfigHome::new();
        let server = FakeServer::start("ollama", &["llama3:8b"]).await;
        let mut w = wizard(unified(96.0), &[("ollama", &server)]).await;
        let home = std::path::PathBuf::from(std::env::var("GRANITE_CLI_HOME").unwrap());
        let before = tree(&home);
        // Add a host, then quit at the providers question.
        w.answer(&[NO_LAUNCHER, &[2], &[0, 1], NO_CAPABILITY, NO_MODEL]);
        w.select(&[1, no_more(false), no_more(false)]);
        w.text(&["127.0.0.1"]);
        *w.ui.quit_at.borrow_mut() = Some("Select providers".to_string());

        let result = w.run().await;

        assert!(result.is_err());
        assert_eq!(tree(&home), before);
    }

    /// Every file under `dir` with its content, sorted by path.
    fn tree(dir: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        let mut files = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(d) = pending.pop() {
            for entry in std::fs::read_dir(&d).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    files.push((path.clone(), std::fs::read(&path).unwrap()));
                }
            }
        }
        files.sort();
        files
    }

    #[tokio::test]
    async fn wizard_offers_configured_hosts_and_skips_their_urls() {
        let _home = crate::config::TestConfigHome::new();
        let configured = FakeServer::start("llama-cpp", &[]).await;
        let mut w = wizard(unified(96.0), &[("llama-cpp", &configured)]).await;
        w.add_host("box", "127.0.0.1", &[]);
        w.ctx
            .config_mut()
            .insert_provider(
                "llama-cpp-box",
                crate::config::ProviderConfig {
                    provider_id: "llama-cpp-box".to_string(),
                    provider_type: "llama-cpp".to_string(),
                    config: serde_json::json!({
                        "base_url": configured.base_url(),
                        "discover_models": false,
                    }),
                    host: Some(crate::config::host::HostLink::Host("box".to_string())),
                },
            )
            .unwrap();
        // box only: the type's default URL at 127.0.0.1 is the configured
        // provider's, so nothing is probed there.
        w.answer(&[NO_LAUNCHER, &[1], NO_CAPABILITY, NO_MODEL, NO_MODEL]);
        w.select(&[no_more(false)]);

        w.run().await.unwrap();

        let prompts = w.ui.multi_select_prompts.borrow();
        let (_, items, defaults) = &prompts[1];
        assert_eq!(items[1], "box — 127.0.0.1, unknown");
        assert!(
            defaults.iter().take(3).all(|d| *d),
            "hosts and hosted services checked"
        );
        // One request: the listing of what it serves, no health check.
        assert_eq!(
            configured.requests(),
            1,
            "a configured provider is not probed"
        );
        assert!(
            w.infos()
                .iter()
                .any(|i| i.starts_with("  llama-cpp-box") && i.ends_with("configured")),
            "{:?}",
            w.infos()
        );
    }

    #[tokio::test]
    async fn unchecking_the_hosted_services_skips_their_probe() {
        for (hosts, probed) in [(&[0usize][..], false), (&[0, 1][..], true)] {
            let _home = crate::config::TestConfigHome::new();
            // A server standing in for OpenRouter's default URL.
            let hosted = FakeServer::start("ollama", &[]).await;
            let mut w = wizard(unified(96.0), &[("openrouter", &hosted)]).await;
            w.answer(&[NO_LAUNCHER, hosts, NO_CAPABILITY, NO_MODEL, NO_MODEL]);
            w.select(&[no_more(false), no_more(true)]);

            w.run().await.unwrap();

            assert_eq!(hosted.requests() > 0, probed, "hosts {hosts:?}");
        }
    }

    #[tokio::test]
    async fn a_type_configured_on_local_is_probed_on_another_host() {
        let _home = crate::config::TestConfigHome::new();
        let on_local = FakeServer::start("ollama", &[]).await;
        let on_box = FakeServer::start("ollama", &[]).await;
        let mut w = wizard(unified(96.0), &[("ollama", &on_box)]).await;
        w.ctx
            .config_mut()
            .insert_provider(
                "ollama",
                crate::config::ProviderConfig {
                    provider_id: "ollama".to_string(),
                    provider_type: "ollama".to_string(),
                    config: serde_json::json!({ "base_url": on_local.base_url_at("localhost") }),
                    host: Some(crate::config::host::HostLink::Local),
                },
            )
            .unwrap();
        w.add_host("box", "127.0.0.1", &[]);
        // box only. Nothing is ranked there and nothing is served, so there
        // is no models question; the provider found.
        w.answer(&[NO_LAUNCHER, &[1], NO_CAPABILITY, &[0]]);
        w.select(&[no_more(false)]);

        w.run().await.unwrap();

        let providers = w.items("Select providers to configure");
        assert_eq!(
            providers,
            vec!["ollama-box — Ollama (healthy)"],
            "ollama on local, which is not in this run, is not offered"
        );
        let config = Config::new().unwrap();
        assert_eq!(
            config.get_provider("ollama-box").unwrap().config["base_url"],
            on_box.base_url()
        );
        assert_eq!(
            config.get_provider("ollama").unwrap().config["base_url"],
            on_local.base_url_at("localhost")
        );
    }

    #[tokio::test]
    async fn setup_auto_writes_local_and_links_new_providers_to_it() {
        let _home = crate::config::TestConfigHome::new();
        let server = FakeServer::start("ollama", &[]).await;
        let mut ctx =
            crate::AppContext::new(Config::new().unwrap(), Arc::new(CaptureUi::default()));
        probe_only(&mut ctx, &[("ollama", &server)]).await;
        ctx.config_mut()
            .insert_hardware(
                crate::config::hardware::LOCAL_HARDWARE_ID,
                crate::config::HardwareConfig::local_from_detection(unified(96.0)),
            )
            .unwrap();

        SetupCommands::run_auto_with_hardware(&mut ctx, &unified(96.0), Some(false))
            .await
            .unwrap();

        let config = Config::new().unwrap();
        assert_eq!(
            config.get_host("local"),
            Some(&crate::config::HostConfig::local())
        );
        assert_eq!(
            config.get_provider("ollama").unwrap().host,
            Some(crate::config::host::HostLink::Local)
        );
    }

    /// The index of capability type `id` in the escape hatch's type list.
    fn capability_type_index(id: &str) -> usize {
        let mut ids: Vec<&str> = CAPABILITY_REGISTRY.entries().keys().copied().collect();
        ids.sort();
        ids.iter().position(|t| *t == id).unwrap()
    }

    #[tokio::test]
    async fn a_capability_typed_by_hand_is_written_at_the_end_or_not_at_all() {
        for quit in [false, true] {
            let _home = crate::config::TestConfigHome::new();
            let server = FakeServer::start("ollama", &["llama3:8b"]).await;
            let mut w = wizard(unified(96.0), &[("ollama", &server)]).await;
            let home = std::path::PathBuf::from(std::env::var("GRANITE_CLI_HOME").unwrap());
            let before = tree(&home);
            // Capabilities: the escape hatch (its only item), then
            // `model-provider` by hand, then keep it checked.
            w.answer(&[NO_LAUNCHER, &[0], &[0], &[0], NO_MODEL, &[0]]);
            w.select(&[no_more(false), capability_type_index("model-provider")]);
            if quit {
                *w.ui.quit_at.borrow_mut() = Some("Select providers".to_string());
            }

            let result = w.run().await;

            let written = Config::new()
                .unwrap()
                .get_capability("model-provider")
                .is_some();
            if quit {
                assert!(result.is_err());
                assert!(!written, "quitting writes nothing");
                assert_eq!(tree(&home), before);
            } else {
                result.unwrap();
                assert!(written, "{:?}", w.ui.multi_select_prompts.borrow());
            }
        }
    }

    #[tokio::test]
    async fn add_a_host_refuses_a_taken_address_and_id_and_returns_to_the_list() {
        let _home = crate::config::TestConfigHome::new();
        let mut w = wizard(unified(96.0), &[]).await;
        w.add_host("box", "box.lan", &[]);
        // "Add a host…" (local, box, hosted, add); then local only.
        w.answer(&[NO_LAUNCHER, &[3], &[0], NO_CAPABILITY, NO_MODEL, NO_MODEL]);
        // The address of box, refused; another; box's id, refused; then the
        // default id.
        w.text(&["box.lan", "10.0.0.5", "box"]);
        w.select(&[1, no_more(false)]); // unknown

        w.run().await.unwrap();

        assert_eq!(
            *w.ui.warns.borrow(),
            vec![
                "box.lan is the address of host 'box'".to_string(),
                "host 'box' is already configured".to_string(),
            ]
        );
        assert_eq!(
            Config::new().unwrap().get_host("host").unwrap().address,
            "10.0.0.5"
        );
        assert_eq!(
            w.ui.multi_select_prompts
                .borrow()
                .iter()
                .filter(|(p, _, _)| p == "Hosts to run models on")
                .count(),
            2
        );
    }

    /// The index of `provider_type` in the question that follows probing a
    /// host the user runs.
    fn type_index(provider_type: &str) -> usize {
        offered_types(false)
            .iter()
            .position(|t| *t == provider_type)
            .unwrap()
    }

    #[tokio::test]
    async fn wizard_provider_at_other_address_is_ranked_and_bound() {
        /// Nothing answers on the default ports; the user adds llama-cpp at
        /// its address. opencode is the one launcher on PATH.
        async fn run(answers: &[&[usize]]) -> (Wizard, String) {
            let server = FakeServer::start("llama-cpp", &["llama3:8b"]).await;
            let url = server.base_url_at("localhost");
            let mut w = wizard(unified(96.0), &[]).await;
            w.ctx
                .set_launcher_paths(&[("opencode", "/usr/local/bin/opencode")]);
            w.answer(answers);
            // llama-cpp at its address; then "No, continue" by default.
            w.select(&[type_index("llama-cpp")]);
            w.text(&[&url]);
            w.run().await.unwrap();
            (w, url)
        }
        // The capability's index, from a run that picks opencode and nothing
        // else, and then the model's, from one that also picks the
        // capability, which decides the models offered.
        let capability = {
            let _home = crate::config::TestConfigHome::new();
            let (w, _) = run(&[&[0], &[0], NO_CAPABILITY, NO_MODEL, &[0]]).await;
            let models = w.items("Select models to configure");
            assert!(
                models
                    .iter()
                    .any(|m| m.contains(" — Fit: ") && m.contains("runs on: llama-cpp")),
                "catalog models are ranked for the provider added: {models:?}"
            );
            let capabilities = w.items("Select capabilities to configure");
            capabilities
                .iter()
                .position(|c| c.starts_with("agent-model — "))
                .unwrap_or_else(|| panic!("{capabilities:?}"))
        };
        let model = {
            let _home = crate::config::TestConfigHome::new();
            let _user_home = TempHome::new();
            let (w, _) = run(&[&[0], &[0], &[capability], NO_MODEL, &[0]]).await;
            let models = w.items("Select models to configure");
            models
                .iter()
                .position(|m| m == "llama3:8b — on llama-cpp, not in the catalog")
                .unwrap_or_else(|| panic!("the model it serves is offered: {models:?}"))
        };
        let _home = crate::config::TestConfigHome::new();
        let _user_home = TempHome::new();

        let (w, url) = run(&[&[0], &[0], &[capability], &[model], &[0]]).await;

        let prompts = w.ui.select_prompts.borrow();
        assert_eq!(
            prompts[0].0,
            "Add a provider on localhost at another address?"
        );
        assert_eq!(prompts[0].2, 0, "nothing answered: the first type");
        assert_eq!(
            prompts[1].2,
            no_more(false),
            "after one is added: No, continue"
        );
        let config = Config::new().unwrap();
        let provider = config.get_provider("llama-cpp").unwrap();
        assert_eq!(provider.config["base_url"], url);
        assert_eq!(provider.host, Some(crate::config::host::HostLink::Local));
        let model = config
            .get_model("llama3:8b")
            .expect("the served model is written");
        assert_eq!(model.provider_id, "llama-cpp");
        assert_eq!(
            config.get_capability("agent-model").unwrap().config["model_id"],
            "llama3:8b",
            "bound to the capability"
        );
        assert_eq!(
            config
                .get_launcher("opencode")
                .unwrap()
                .enabled_capabilities,
            vec!["agent-model".to_string()],
            "the launcher ends with a capability"
        );
    }

    #[tokio::test]
    async fn an_address_elsewhere_is_refused_and_one_that_does_not_answer_offers_three_choices() {
        let _home = crate::config::TestConfigHome::new();
        let closed = closed_base_url().await.replace("127.0.0.1", "localhost");
        let mut w = wizard(unified(96.0), &[]).await;
        w.answer(&[NO_LAUNCHER, &[0], NO_CAPABILITY, NO_MODEL, &[0]]);
        // llama-cpp: an address on another machine, then a closed port,
        // "Try another address", the closed port again, "Add it anyway".
        // ollama: the closed port, "Skip". Then "No, continue".
        // The settings asked once llama-cpp is added, at their defaults.
        let settings = Arc::new(CaptureUi::default());
        let mut config = PROVIDER_REGISTRY.default_config("llama-cpp");
        config["base_url"] = closed.clone().into();
        let probe = crate::AppContext::new(Config::default(), settings.clone());
        SetupCommands::other_settings(&probe, "llama-cpp", &closed, &config).unwrap();
        let texts: Vec<String> = settings
            .text_prompts
            .borrow()
            .iter()
            .map(|(_, d)| d.clone())
            .collect();
        let selects: Vec<usize> = settings
            .select_prompts
            .borrow()
            .iter()
            .map(|(_, _, d)| *d)
            .collect();
        w.ui.confirm_answers
            .borrow_mut()
            .extend(settings.confirm_prompts.borrow().iter().map(|(_, d)| *d));
        let mut select = vec![type_index("llama-cpp"), 0, 1];
        select.extend(selects);
        select.extend([type_index("ollama"), 2, no_more(false)]);
        w.select(&select);
        let mut text = vec![
            "http://10.9.9.9:8080".to_string(),
            closed.clone(),
            closed.clone(),
        ];
        text.extend(texts);
        text.push(closed.clone());
        w.text(&text.iter().map(String::as_str).collect::<Vec<_>>());

        w.run().await.unwrap();

        let warns = w.ui.warns.borrow();
        assert!(
            warns.contains(
                &"http://10.9.9.9:8080 is not on localhost; add its machine with \"Add a host…\"."
                    .to_string()
            ),
            "{warns:?}"
        );
        assert_eq!(
            warns
                .iter()
                .filter(|m| m.ends_with(&format!("is not answering at {closed}.")))
                .count(),
            3
        );
        let config = Config::new().unwrap();
        assert_eq!(
            config.get_provider("llama-cpp").unwrap().config["base_url"],
            closed,
            "added anyway"
        );
        assert!(config.get_provider("ollama").is_none(), "skipped");
    }

    fn launcher(id: &str, capabilities: &[&str]) -> crate::config::LauncherConfig {
        serde_json::from_value(serde_json::json!({
            "launcher_id": id,
            "type": id,
            "enabled_capabilities": capabilities,
            "config": {},
        }))
        .unwrap()
    }

    #[test]
    fn wizard_summary_names_launcher_without_capability() {
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());
        ctx.config_mut()
            .launchers
            .insert("opencode".to_string(), launcher("opencode", &[]));

        SetupCommands::print_summary(
            &ctx,
            &HashSet::new(),
            &string_set(&["opencode"]),
            &HashSet::new(),
            &HashSet::new(),
        );

        let warns = capture.warns.borrow();
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert!(
            warns[0].starts_with("Launcher 'opencode' has no capability: its recommended '"),
            "{}",
            warns[0]
        );
        assert!(warns[0].ends_with("Run `granite-cli capability setup` to bind one."));
    }

    /// A context with host `box` listing `hardware`, provider `llama-cpp-box`
    /// on it, and a capability for each model in `models`, each bound to
    /// `llama-cpp-box` in its largest GGUF variant. Returns it and the total
    /// size of those variants.
    fn ctx_with_bound_models(
        hardware: &[&str],
        models: &[&str],
    ) -> (crate::AppContext, Arc<CaptureUi>, f64) {
        let capture = Arc::new(CaptureUi::default());
        let mut ctx = crate::AppContext::new(Config::default(), capture.clone());
        ctx.config_mut().hardware.insert(
            "card".to_string(),
            crate::config::HardwareConfig::catalog("card", "nvidia-rtx-4090", 24.0),
        );
        ctx.config_mut().hosts.insert(
            "box".to_string(),
            crate::config::HostConfig {
                host_id: "box".to_string(),
                address: "box.lan".to_string(),
                hardware: hardware.iter().map(|h| h.to_string()).collect(),
            },
        );
        ctx.config_mut().providers.insert(
            "llama-cpp-box".to_string(),
            ProviderConfig {
                provider_id: "llama-cpp-box".to_string(),
                provider_type: "llama-cpp".to_string(),
                config: serde_json::json!({}),
                host: Some(crate::config::host::HostLink::Host("box".to_string())),
            },
        );
        let mut total = 0.0;
        for (i, model_id) in models.iter().enumerate() {
            let md = MODEL_REGISTRY.get(model_id).unwrap();
            let variant = md
                .variants
                .iter()
                .filter(|v| v.format == "GGUF")
                .max_by(|a, b| {
                    a.size_gb
                        .unwrap_or(0.0)
                        .total_cmp(&b.size_gb.unwrap_or(0.0))
                })
                .unwrap();
            total += variant.size_gb.unwrap_or(0.0);
            ctx.config_mut().models.insert(
                model_id.to_string(),
                ModelConfig {
                    discovered: None,
                    model_id: model_id.to_string(),
                    model_type: model_id.to_string(),
                    config: serde_json::json!({}),
                    provider_id: "llama-cpp-box".to_string(),
                    variant: Some(format!("{}/{}", variant.format, variant.precision)),
                },
            );
            let cap_id = format!("cap-{i}");
            ctx.config_mut().capabilities.insert(
                cap_id.clone(),
                crate::config::CapabilityConfig {
                    capability_id: cap_id,
                    capability_type: "agent-model".to_string(),
                    config: serde_json::json!({ "model_id": model_id }),
                },
            );
        }
        (ctx, capture, total)
    }

    #[test]
    fn wizard_summary_memory_over_host() {
        let models = ["granite-4.2-30b", "granite-4.2-8b"];
        let (ctx, capture, total) = ctx_with_bound_models(&["card"], &models);
        assert!(
            total > 16.0,
            "the two need more than one 24 GB card: {total}"
        );

        SetupCommands::print_summary(
            &ctx,
            &HashSet::new(),
            &HashSet::new(),
            &string_set(&["llama-cpp-box"]),
            &string_set(&models),
        );

        let infos = capture.infos.borrow();
        assert!(infos.contains(&"Hosts: box".to_string()), "{infos:?}");
        let line = infos
            .iter()
            .find(|i| i.starts_with("box: models bound here total "))
            .unwrap_or_else(|| panic!("{infos:?}"));
        assert!(
            line.contains(&format!("total {total:.1} GB, usable memory is 16.0 GB.")),
            "{line}"
        );
    }

    #[test]
    fn no_memory_line_under_the_memory_or_on_unknown_hardware() {
        for (hardware, model) in [
            (&["card"][..], "granite-4.2-3b"),
            (&[][..], "granite-4.2-30b"),
        ] {
            let (ctx, capture, _) = ctx_with_bound_models(hardware, &[model]);

            SetupCommands::print_summary(
                &ctx,
                &HashSet::new(),
                &HashSet::new(),
                &string_set(&["llama-cpp-box"]),
                &string_set(&[model]),
            );

            assert!(
                !capture
                    .infos
                    .borrow()
                    .iter()
                    .any(|i| i.contains("models bound here")),
                "{hardware:?} {model}"
            );
        }
    }

    #[test]
    fn a_model_of_unknown_size_is_noted_and_not_counted() {
        let models = ["granite-4.2-30b", "granite-4.2-8b"];
        let (mut ctx, capture, _) = ctx_with_bound_models(&["card"], &models);
        ctx.config_mut()
            .models
            .get_mut("granite-4.2-8b")
            .unwrap()
            .variant = None;

        SetupCommands::print_summary(
            &ctx,
            &HashSet::new(),
            &HashSet::new(),
            &string_set(&["llama-cpp-box"]),
            &string_set(&models),
        );

        let infos = capture.infos.borrow();
        let line = infos
            .iter()
            .find(|i| i.starts_with("box: models bound here"))
            .unwrap_or_else(|| panic!("{infos:?}"));
        assert!(
            line.ends_with("1 model(s) of unknown size are not counted."),
            "{line}"
        );
    }

    /// Points HOME at a temporary directory while it lives, for runs that
    /// configure a launcher, which may write its settings under HOME.
    struct TempHome {
        previous: Option<std::ffi::OsString>,
        _dir: tempfile::TempDir,
    }

    impl TempHome {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let previous = std::env::var_os("HOME");
            // SAFETY: serialized by the configuration home lock the caller
            // holds.
            unsafe { std::env::set_var("HOME", dir.path()) };
            Self {
                previous,
                _dir: dir,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            // SAFETY: as in `new`.
            unsafe {
                match &self.previous {
                    Some(home) => std::env::set_var("HOME", home),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    #[tokio::test]
    async fn a_hosted_provider_is_added_at_any_address_with_its_settings_first() {
        let _home = crate::config::TestConfigHome::new();
        let closed = closed_base_url().await;
        let mut w = wizard(unified(96.0), &[]).await;
        // The settings openai-compatible asks, at their defaults, before
        // its address.
        let settings = Arc::new(CaptureUi::default());
        let probe = crate::AppContext::new(Config::default(), settings.clone());
        SetupCommands::other_settings(
            &probe,
            "openai-compatible",
            "https://api.example.com/v1",
            &PROVIDER_REGISTRY.default_config("openai-compatible"),
        )
        .unwrap();
        let mut text: Vec<String> = settings
            .text_prompts
            .borrow()
            .iter()
            .map(|(_, d)| d.clone())
            .collect();
        text.push(closed.clone());
        let hosted_index = offered_types(true)
            .iter()
            .position(|t| *t == "openai-compatible")
            .unwrap();
        let mut select = vec![hosted_index];
        select.extend(settings.select_prompts.borrow().iter().map(|(_, _, d)| *d));
        // "Add it anyway", then "No, continue".
        select.extend([1, no_more(true)]);
        w.ui.confirm_answers
            .borrow_mut()
            .extend(settings.confirm_prompts.borrow().iter().map(|(_, d)| *d));
        // The hosted services only (local, hosted services, add).
        w.answer(&[NO_LAUNCHER, &[1], NO_CAPABILITY, &[0]]);
        w.select(&select);
        w.text(&text.iter().map(String::as_str).collect::<Vec<_>>());

        w.run().await.unwrap();

        let prompts = w.ui.select_prompts.borrow();
        assert_eq!(prompts[0].0, "Add a hosted provider at another address?");
        let mut expected: Vec<String> = offered_types(true).iter().map(|t| t.to_string()).collect();
        expected.push("No, continue".to_string());
        assert_eq!(prompts[0].1, expected);
        assert!(expected.contains(&"openai-compatible".to_string()));
        let providers = w.items("Select providers to configure");
        assert_eq!(
            providers,
            vec![format!(
                "openai-compatible — OpenAI Compatible Provider ({ADDED_ANYWAY})"
            )]
        );
        let provider = Config::new()
            .unwrap()
            .get_provider("openai-compatible")
            .cloned()
            .unwrap();
        assert_eq!(
            provider.host,
            Some(crate::config::host::HostLink::Unspecified)
        );
        assert_eq!(provider.config["base_url"], closed);
    }

    #[tokio::test]
    async fn an_address_already_in_the_run_is_refused() {
        let _home = crate::config::TestConfigHome::new();
        let server = FakeServer::start("llama-cpp", &[]).await;
        let url = server.base_url_at("localhost");
        let mut w = wizard(unified(96.0), &[("llama-cpp", &server)]).await;
        w.answer(&[NO_LAUNCHER, &[0], NO_CAPABILITY, NO_MODEL, NO_MODEL]);
        // llama-cpp, found at its default port; adding it again there is
        // refused, then "Skip" of the other address that does not answer.
        let closed = closed_base_url().await.replace("127.0.0.1", "localhost");
        w.select(&[type_index("llama-cpp"), 2, no_more(false)]);
        w.text(&[&url, &closed]);

        w.run().await.unwrap();

        assert!(
            w.ui.warns.borrow().contains(&format!(
                "llama-cpp at {url} is already in this run as 'llama-cpp'."
            )),
            "{:?}",
            w.ui.warns.borrow()
        );
    }
}
