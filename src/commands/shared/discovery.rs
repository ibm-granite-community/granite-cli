//! Provider model discovery sync: ask a provider what it serves, diff that
//! against the models granite-cli created from it, and apply the result in one
//! batch.
//!
//! This is the only module that combines the network call, the pure plan in
//! [`crate::models::discovery`], and configuration writes. Callers decide
//! *when* to sync; this module only knows *how*.

// Standard
use std::collections::HashMap;

// Local
use crate::config::validation::{RefKind, dependents};
use crate::models::discovery;
use crate::providers::{Provider, ProviderError, ProviderModel};

/*-- public --------------------------------------------------------------------*/

/// How one provider's sync attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SyncStatus {
    /// A successful listing was applied.
    Synced,
    /// The provider type cannot list models, or its `discover_models` is off.
    Skipped,
    /// The provider could not be asked; nothing was written.
    Failed(String),
}

impl std::fmt::Display for SyncStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncStatus::Synced => write!(f, "synced"),
            SyncStatus::Skipped => write!(f, "off"),
            SyncStatus::Failed(error) => write!(f, "failed: {error}"),
        }
    }
}

/// What one provider's sync did, for the caller to render.
#[derive(Debug, Clone)]
pub(crate) struct SyncReport {
    pub(crate) provider_id: String,
    pub(crate) status: SyncStatus,
    /// Configured instance ids added.
    pub(crate) added: Vec<String>,
    /// Configured instance ids removed.
    pub(crate) removed: Vec<String>,
    /// Managed entries the provider still reports.
    pub(crate) present: usize,
    /// Instance ids that had to be namespaced because the natural id was
    /// taken by a different model.
    pub(crate) conflicts: Vec<String>,
    /// Warnings to show, including dependents of removed models and a failed
    /// save.
    pub(crate) warnings: Vec<String>,
}

impl SyncReport {
    /// One-line summary for commands that only want a hint.
    pub(crate) fn summary(&self) -> String {
        match &self.status {
            SyncStatus::Synced => {
                let removed =
                    if self.removed.is_empty() || self.removed.len() > MAX_SUMMARY_REMOVALS {
                        String::new()
                    } else {
                        format!(" ({})", self.removed.join(", "))
                    };
                format!(
                    "{}: {} added, {} removed{}, {} unchanged{}",
                    self.provider_id,
                    self.added.len(),
                    self.removed.len(),
                    removed,
                    self.present,
                    if self.conflicts.is_empty() {
                        String::new()
                    } else {
                        format!(", {} renamed to avoid collisions", self.conflicts.len())
                    }
                )
            }
            SyncStatus::Skipped => format!("{}: discovery off", self.provider_id),
            SyncStatus::Failed(error) => format!("{}: {error}", self.provider_id),
        }
    }
}

/// Syncs the named providers in sorted order and returns one report per
/// provider that attempted a listing. Providers with discovery off or an
/// unsupported type are returned as [`SyncStatus::Skipped`].
pub(crate) async fn sync_providers(
    ctx: &mut crate::AppContext,
    provider_ids: &[String],
) -> Vec<SyncReport> {
    let mut ids: Vec<String> = provider_ids.to_vec();
    ids.sort();
    ids.dedup();

    let mut reports = Vec::new();
    for provider_id in ids {
        let prepared = prepare_one(ctx, &provider_id).await;
        reports.push(apply_prepared(ctx, prepared));
    }
    reports
}

/// The providers whose models the launcher's enabled capabilities name.
/// Sorted and deduped. Used to keep launch-time sync bounded to what the
/// launch will actually use.
pub(crate) fn providers_used_by_launcher(
    ctx: &crate::AppContext,
    launcher_id: &str,
) -> Vec<String> {
    let Some(launcher) = ctx.config().get_launcher(launcher_id) else {
        return Vec::new();
    };

    let mut providers = Vec::new();
    for capability_id in &launcher.enabled_capabilities {
        let Some(capability) = ctx.config().get_capability(capability_id) else {
            continue;
        };
        let Some(metadata) =
            crate::capabilities::CAPABILITY_REGISTRY.get(&capability.capability_type)
        else {
            continue;
        };
        for dependency in &metadata.dependencies {
            let crate::capabilities::Dependency::Model { config_key, .. } = dependency else {
                continue;
            };
            let Some(model_id) = capability.config.get(config_key).and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(model) = ctx.config().get_model(model_id) else {
                continue;
            };
            providers.push(model.provider_id.clone());
        }
    }
    providers.sort();
    providers.dedup();
    providers
}

/*-- private -------------------------------------------------------------------*/

/// How many removed ids a one-line summary lists before falling back to the
/// count alone. A local catalog dropping hundreds of models should not
/// produce an unreadable line.
const MAX_SUMMARY_REMOVALS: usize = 5;

/// How long one provider's launch-time listing may take before the launch
/// gives up on it (and proceeds with the configured models).
const LAUNCH_SYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Providers listed at once during a launch-time sync.
const MAX_CONCURRENT_SYNCS: usize = 4;

/// A provider listing waiting to be diffed against the configuration. Only
/// the network read and the provider-specific coverage calculation happen
/// during preparation; the diff is applied after earlier providers' plans so
/// cross-provider id collisions resolve deterministically.
struct PreparedSync {
    report: SyncReport,
    plan_input: Option<PlanInput>,
}

struct PlanInput {
    provider_type: String,
    reported: Vec<ProviderModel>,
    covered: HashMap<String, String>,
}

async fn prepare_one(ctx: &crate::AppContext, provider_id: &str) -> PreparedSync {
    let mut report = SyncReport {
        provider_id: provider_id.to_string(),
        status: SyncStatus::Skipped,
        added: Vec::new(),
        removed: Vec::new(),
        present: 0,
        conflicts: Vec::new(),
        warnings: Vec::new(),
    };

    if ctx.config().get_provider(provider_id).is_none() {
        report.status = SyncStatus::Failed("provider is not configured".to_string());
        return PreparedSync {
            report,
            plan_input: None,
        };
    }

    // Read the effective flag from configuration before constructing the
    // provider: constructing a hosted provider builds a TLS client, which is
    // pointless when discovery is off and awkward in environments without a
    // verifier.
    let Some(provider_config) = ctx.config().get_provider(provider_id) else {
        report.status = SyncStatus::Failed("provider is not configured".to_string());
        return PreparedSync {
            report,
            plan_input: None,
        };
    };
    let discover_on = crate::providers::discovery_enabled(
        &provider_config.provider_type,
        &provider_config.config,
    );
    if !discover_on {
        return PreparedSync {
            report,
            plan_input: None,
        };
    }

    let provider = match ctx.sources().providers().upstream(provider_id) {
        Ok(provider) => provider,
        Err(error) => {
            report.status = SyncStatus::Failed(error.to_string());
            return PreparedSync {
                report,
                plan_input: None,
            };
        }
    };
    if !provider.discover_models() {
        return PreparedSync {
            report,
            plan_input: None,
        };
    }

    let reported = match provider.list_models().await {
        Ok(models) => models,
        Err(ProviderError::Unsupported(_)) => {
            return PreparedSync {
                report,
                plan_input: None,
            };
        }
        Err(error) => {
            report.status = SyncStatus::Failed(error.to_string());
            return PreparedSync {
                report,
                plan_input: None,
            };
        }
    };

    let provider_type = ctx
        .config()
        .get_provider(provider_id)
        .map(|config| config.provider_type.clone())
        .unwrap_or_default();
    let covered = covered_wire_names(ctx, provider_id, provider.as_ref());
    report.status = SyncStatus::Synced;

    PreparedSync {
        report,
        plan_input: Some(PlanInput {
            provider_type,
            reported,
            covered,
        }),
    }
}

/// Applies one prepared listing: computes its plan against the configuration
/// as it now stands, then writes. A save failure keeps the in-memory mutation
/// and is reported as a warning, matching the rest of the configuration
/// writes.
fn apply_prepared(ctx: &mut crate::AppContext, prepared: PreparedSync) -> SyncReport {
    let PreparedSync {
        mut report,
        plan_input,
    } = prepared;
    let Some(input) = plan_input else {
        return report;
    };

    let provider_id = report.provider_id.clone();
    let plan = discovery::plan(
        &provider_id,
        &input.provider_type,
        &input.reported,
        &ctx.config().models,
        &input.covered,
    );

    let duplicates = discovery::duplicates(&input.reported);
    if !duplicates.is_empty() {
        report.warnings.push(format!(
            "provider reported duplicate model ids: {}",
            duplicates.join(", ")
        ));
    }
    for (_, addition) in &plan.additions {
        report.added.push(addition.model_id.clone());
    }
    for removed in &plan.removals {
        let refs = dependents(RefKind::Model, removed, ctx.config());
        if !refs.is_empty() {
            let names = refs
                .iter()
                .map(|(kind, id)| format!("{kind} '{id}'"))
                .collect::<Vec<_>>()
                .join(", ");
            report
                .warnings
                .push(format!("removed '{removed}', still referenced by {names}"));
        }
    }
    report.removed = plan.removals.clone();
    report.present = plan.present;
    report.conflicts = plan.conflicts.clone();

    if plan.is_empty() {
        return report;
    }
    if let Err(error) = ctx
        .config_mut()
        .apply_model_changes(plan.additions, &plan.removals)
    {
        report
            .warnings
            .push(format!("failed to save discovered models: {error}"));
    }
    report
}

/// Syncs up to [`MAX_CONCURRENT_SYNCS`] providers at once, applying each
/// batch in the original order. A provider that exceeds `timeout` is reported
/// failed and changes nothing.
async fn sync_providers_bounded(
    ctx: &mut crate::AppContext,
    provider_ids: &[String],
    timeout: std::time::Duration,
) -> Vec<SyncReport> {
    let mut ids: Vec<String> = provider_ids.to_vec();
    ids.sort();
    ids.dedup();

    let mut reports = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(MAX_CONCURRENT_SYNCS) {
        let prepared = {
            let futures = chunk
                .iter()
                .map(|provider_id| tokio::time::timeout(timeout, prepare_one(ctx, provider_id)));
            futures_util::future::join_all(futures).await
        };
        for (provider_id, result) in chunk.iter().zip(prepared) {
            let prepared = result.unwrap_or_else(|_| PreparedSync {
                report: timeout_report(provider_id, timeout),
                plan_input: None,
            });
            reports.push(apply_prepared(ctx, prepared));
        }
    }
    reports
}

fn timeout_report(provider_id: &str, timeout: std::time::Duration) -> SyncReport {
    SyncReport {
        provider_id: provider_id.to_string(),
        status: SyncStatus::Failed(format!("timed out after {}ms", timeout.as_millis())),
        added: Vec::new(),
        removed: Vec::new(),
        present: 0,
        conflicts: Vec::new(),
        warnings: Vec::new(),
    }
}

/// Every wire name a configured model already answers to for `provider_id`.
/// The wire name is the one requests use: a discovered entry's stored
/// provider name, otherwise the provider alias for the pinned variant, and
/// the instance id only when neither applies. Adding the instance id as well
/// would make an unrelated model reported under that name look covered.
fn covered_wire_names(
    ctx: &crate::AppContext,
    provider_id: &str,
    provider: &dyn Provider,
) -> HashMap<String, String> {
    let mut covered = HashMap::new();
    let models_source = ctx.sources().models();

    for (instance_id, config) in &ctx.config().models {
        if config.provider_id != provider_id {
            continue;
        }
        let wire_name = config
            .discovered
            .as_ref()
            .map(|discovered| discovered.provider_model_id.clone())
            .or_else(|| {
                let variant_str = config.variant.as_deref()?;
                let model = models_source.get(instance_id).ok()?;
                let variant = crate::models::find_variant(model.variants(), Some(variant_str))?;
                provider.model_alias(instance_id.clone(), Some(variant))
            })
            .unwrap_or_else(|| instance_id.clone());
        covered.insert(wire_name, instance_id.clone());
    }

    covered
}

/// Convenience for the launch path: sync only the providers the launcher
/// uses, concurrently and with a short cap, and never fail the launch because
/// discovery did.
pub(crate) async fn sync_for_launcher(
    ctx: &mut crate::AppContext,
    launcher_id: &str,
) -> Vec<SyncReport> {
    let provider_ids = providers_used_by_launcher(ctx, launcher_id);
    sync_providers_bounded(ctx, &provider_ids, LAUNCH_SYNC_TIMEOUT).await
}

/*-- tests --------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DiscoveredModel, ModelConfig, ProviderConfig};
    use crate::utils::test_support::FakeProvider;
    use crate::utils::ui::base::tests::CaptureUi;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn providers_used_by_launcher_follows_model_dependencies() {
        let mut config = Config::default();
        config.providers.insert(
            "p1".to_string(),
            ProviderConfig {
                provider_id: "p1".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({}),
            },
        );
        config.models.insert(
            "m1".to_string(),
            ModelConfig {
                model_id: "m1".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                provider_id: "p1".to_string(),
                variant: None,
                discovered: None,
                config: serde_json::json!({}),
            },
        );
        config.capabilities.insert(
            "chat".to_string(),
            crate::config::CapabilityConfig {
                capability_id: "chat".to_string(),
                capability_type: "agent-model".to_string(),
                config: serde_json::json!({ "model_id": "m1" }),
            },
        );
        config.launchers.insert(
            "claude".to_string(),
            crate::config::LauncherConfig {
                launcher_id: "claude".to_string(),
                launcher_type: "claude".to_string(),
                enabled_capabilities: vec!["chat".to_string()],
                config: serde_json::json!({}),
            },
        );

        let ctx = crate::AppContext::new(config, Arc::new(CaptureUi::default()));
        assert_eq!(
            providers_used_by_launcher(&ctx, "claude"),
            vec!["p1".to_string()]
        );
    }

    #[test]
    fn providers_used_by_launcher_is_empty_for_unknown_launcher() {
        let ctx = crate::AppContext::new(Config::default(), Arc::new(CaptureUi::default()));
        assert!(providers_used_by_launcher(&ctx, "nope").is_empty());
    }

    fn fake_provider(alias: Option<&str>) -> FakeProvider {
        FakeProvider {
            instance_id: "p".to_string(),
            base_url: "http://localhost".to_string(),
            api_key: None,
            verify_ssl: true,
            api_types: Vec::new(),
            endpoints: HashMap::new(),
            alias: alias.map(str::to_string),
        }
    }

    #[test]
    fn coverage_uses_the_discovered_wire_name_not_the_instance_id() {
        let mut config = Config::default();
        config.models.insert(
            "instance".to_string(),
            ModelConfig {
                model_id: "instance".to_string(),
                model_type: "custom".to_string(),
                provider_id: "p".to_string(),
                variant: None,
                discovered: Some(DiscoveredModel {
                    provider_id: "p".to_string(),
                    provider_model_id: "wire".to_string(),
                }),
                config: serde_json::json!({}),
            },
        );
        let ctx = crate::AppContext::new(config, Arc::new(CaptureUi::default()));

        let covered = covered_wire_names(&ctx, "p", &fake_provider(None));
        assert_eq!(covered.get("wire"), Some(&"instance".to_string()));
        assert!(!covered.contains_key("instance"));
    }

    #[test]
    fn coverage_uses_the_variant_alias_not_the_instance_id() {
        let mut config = config_with_ollama("p", "http://localhost:11434", false);
        config.models.insert(
            "granite".to_string(),
            ModelConfig {
                model_id: "granite".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                provider_id: "p".to_string(),
                variant: Some("Ollama/Q4_K_M".to_string()),
                discovered: None,
                config: serde_json::json!({}),
            },
        );
        let ctx = crate::AppContext::new(config, Arc::new(CaptureUi::default()));

        let covered = covered_wire_names(&ctx, "p", &fake_provider(Some("granite3.1-dense:8b")));
        assert_eq!(
            covered.get("granite3.1-dense:8b"),
            Some(&"granite".to_string())
        );
        assert!(!covered.contains_key("granite"));
    }

    struct TestState {
        tags: std::sync::Mutex<serde_json::Value>,
        fail: std::sync::atomic::AtomicBool,
        requests: std::sync::atomic::AtomicUsize,
    }

    async fn discovery_server(state: std::sync::Arc<TestState>) -> String {
        use axum::extract::State;
        use axum::response::{IntoResponse, Response};
        use axum::{
            Json, Router,
            routing::{get, post},
        };

        async fn tags(State(state): State<std::sync::Arc<TestState>>) -> Response {
            state
                .requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if state.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
            }
            Json(state.tags.lock().unwrap().clone()).into_response()
        }

        async fn show(axum::Json(body): axum::Json<serde_json::Value>) -> Json<serde_json::Value> {
            let model = body
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            Json(serde_json::json!({
                "capabilities": ["completion", "tools"],
                "model_info": { format!("{model}.context_length"): 8192 }
            }))
        }

        let app = Router::new()
            .route("/api/tags", get(tags))
            .route("/api/show", post(show))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn hanging_discovery_server() -> String {
        use axum::{Json, Router, routing::get};

        async fn tags() -> Json<serde_json::Value> {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Json(serde_json::json!({ "models": [] }))
        }

        let app = Router::new().route("/api/tags", get(tags));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn missing_discovery_server() -> String {
        use axum::{Router, routing::get};

        async fn tags() -> axum::http::StatusCode {
            axum::http::StatusCode::NOT_FOUND
        }

        let app = Router::new().route("/api/tags", get(tags));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn config_with_ollama(id: &str, base_url: &str, discover: bool) -> Config {
        let mut config = Config::default();
        config.providers.insert(
            id.to_string(),
            ProviderConfig {
                provider_id: id.to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({
                    "base_url": base_url,
                    "discover_models": discover
                }),
            },
        );
        config
    }

    #[tokio::test]
    async fn sync_adds_then_removes_models_from_a_successful_listing() {
        let _home = crate::config::TestConfigHome::new();
        let state = std::sync::Arc::new(TestState {
            tags: std::sync::Mutex::new(serde_json::json!({
                "models": [
                    { "name": "granite:8b", "model": "granite:8b", "size": 1u64, "details": { "parameter_size": "8.0B" } },
                    { "name": "llama3:8b", "model": "llama3:8b", "size": 2u64, "details": { "parameter_size": "8.0B" } }
                ]
            })),
            fail: std::sync::atomic::AtomicBool::new(false),
            requests: std::sync::atomic::AtomicUsize::new(0),
        });
        let base_url = discovery_server(state.clone()).await;
        let mut ctx = crate::AppContext::new(
            config_with_ollama("p", &base_url, true),
            Arc::new(CaptureUi::default()),
        );

        let reports = sync_providers(&mut ctx, &["p".to_string()]).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].status, SyncStatus::Synced);
        assert_eq!(reports[0].added.len(), 2);
        assert!(ctx.config().get_model("granite:8b").is_some());
        assert!(ctx.config().get_model("llama3:8b").is_some());

        // The model disappears from the provider: a successful listing is
        // authoritative, so the next sync removes it.
        state.tags.lock().unwrap()["models"]
            .as_array_mut()
            .unwrap()
            .pop();
        let reports = sync_providers(&mut ctx, &["p".to_string()]).await;
        assert_eq!(reports[0].removed, vec!["llama3:8b".to_string()]);
        assert!(ctx.config().get_model("llama3:8b").is_none());

        // The removal reached disk too, so a reload agrees.
        let reloaded = Config::new().unwrap();
        assert!(reloaded.get_model("granite:8b").is_some());
        assert!(reloaded.get_model("llama3:8b").is_none());
    }

    #[tokio::test]
    async fn an_empty_listing_removes_managed_models_and_warns_about_dependents() {
        let _home = crate::config::TestConfigHome::new();
        let state = std::sync::Arc::new(TestState {
            tags: std::sync::Mutex::new(serde_json::json!({ "models": [] })),
            fail: std::sync::atomic::AtomicBool::new(false),
            requests: std::sync::atomic::AtomicUsize::new(0),
        });
        let base_url = discovery_server(state.clone()).await;
        let mut config = config_with_ollama("p", &base_url, true);
        config.models.insert(
            "managed".to_string(),
            ModelConfig {
                model_id: "managed".to_string(),
                model_type: "custom".to_string(),
                provider_id: "p".to_string(),
                variant: None,
                discovered: Some(DiscoveredModel {
                    provider_id: "p".to_string(),
                    provider_model_id: "gone:1b".to_string(),
                }),
                config: serde_json::json!({ "family": "Gone" }),
            },
        );
        config.capabilities.insert(
            "chat".to_string(),
            crate::config::CapabilityConfig {
                capability_id: "chat".to_string(),
                capability_type: "agent-model".to_string(),
                config: serde_json::json!({ "model_id": "managed" }),
            },
        );
        let mut ctx = crate::AppContext::new(config, Arc::new(CaptureUi::default()));

        let reports = sync_providers(&mut ctx, &["p".to_string()]).await;
        assert_eq!(reports[0].removed, vec!["managed".to_string()]);
        assert!(
            reports[0]
                .warnings
                .iter()
                .any(|w| w.contains("still referenced by") && w.contains("chat")),
            "{:?}",
            reports[0].warnings
        );
        assert!(ctx.config().get_model("managed").is_none());
        assert!(ctx.config().get_capability("chat").is_some());

        let reloaded = Config::new().unwrap();
        assert!(reloaded.get_model("managed").is_none());
    }

    #[tokio::test]
    async fn a_missing_listing_endpoint_is_skipped() {
        let _home = crate::config::TestConfigHome::new();
        let base_url = missing_discovery_server().await;
        let mut ctx = crate::AppContext::new(
            config_with_ollama("p", &base_url, true),
            Arc::new(CaptureUi::default()),
        );

        let reports = sync_providers(&mut ctx, &["p".to_string()]).await;
        assert_eq!(reports[0].status, SyncStatus::Skipped);
        assert!(ctx.config().models.is_empty());
    }

    #[tokio::test]
    async fn a_failed_listing_changes_nothing() {
        let _home = crate::config::TestConfigHome::new();
        let state = std::sync::Arc::new(TestState {
            tags: std::sync::Mutex::new(serde_json::json!({
                "models": [{ "name": "granite:8b", "model": "granite:8b" }]
            })),
            fail: std::sync::atomic::AtomicBool::new(false),
            requests: std::sync::atomic::AtomicUsize::new(0),
        });
        let base_url = discovery_server(state.clone()).await;
        let mut ctx = crate::AppContext::new(
            config_with_ollama("p", &base_url, true),
            Arc::new(CaptureUi::default()),
        );
        sync_providers(&mut ctx, &["p".to_string()]).await;
        assert!(ctx.config().get_model("granite:8b").is_some());

        state.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let reports = sync_providers(&mut ctx, &["p".to_string()]).await;
        assert!(matches!(reports[0].status, SyncStatus::Failed(_)));
        assert!(ctx.config().get_model("granite:8b").is_some());
    }

    #[test]
    fn summary_lists_removed_ids_only_when_the_count_is_small() {
        let mut report = SyncReport {
            provider_id: "p".to_string(),
            status: SyncStatus::Synced,
            added: Vec::new(),
            removed: vec!["a".to_string(), "b".to_string()],
            present: 0,
            conflicts: Vec::new(),
            warnings: Vec::new(),
        };
        assert!(report.summary().contains("2 removed (a, b)"));

        report.removed = (0..=MAX_SUMMARY_REMOVALS)
            .map(|i| format!("m{i}"))
            .collect();
        assert!(!report.summary().contains("m0"));
        assert!(
            report
                .summary()
                .contains(&format!("{} removed", MAX_SUMMARY_REMOVALS + 1))
        );
    }

    #[tokio::test]
    async fn discovery_off_makes_no_request() {
        let _home = crate::config::TestConfigHome::new();
        let state = std::sync::Arc::new(TestState {
            tags: std::sync::Mutex::new(serde_json::json!({ "models": [] })),
            fail: std::sync::atomic::AtomicBool::new(false),
            requests: std::sync::atomic::AtomicUsize::new(0),
        });
        let base_url = discovery_server(state.clone()).await;
        let mut ctx = crate::AppContext::new(
            config_with_ollama("p", &base_url, false),
            Arc::new(CaptureUi::default()),
        );
        let reports = sync_providers(&mut ctx, &["p".to_string()]).await;
        assert_eq!(reports[0].status, SyncStatus::Skipped);
        assert_eq!(state.requests.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_timed_out_listing_changes_nothing() {
        let _home = crate::config::TestConfigHome::new();
        let base_url = hanging_discovery_server().await;
        let mut ctx = crate::AppContext::new(
            config_with_ollama("p", &base_url, true),
            Arc::new(CaptureUi::default()),
        );

        let reports =
            sync_providers_bounded(&mut ctx, &["p".to_string()], Duration::from_millis(50)).await;
        assert!(matches!(reports[0].status, SyncStatus::Failed(_)));
        assert!(ctx.config().models.is_empty());
    }

    #[tokio::test]
    async fn bounded_sync_namespaces_the_same_id_from_two_providers_deterministically() {
        let _home = crate::config::TestConfigHome::new();
        let first = std::sync::Arc::new(TestState {
            tags: std::sync::Mutex::new(serde_json::json!({
                "models": [{ "name": "shared", "model": "shared" }]
            })),
            fail: std::sync::atomic::AtomicBool::new(false),
            requests: std::sync::atomic::AtomicUsize::new(0),
        });
        let second = std::sync::Arc::new(TestState {
            tags: std::sync::Mutex::new(serde_json::json!({
                "models": [{ "name": "shared", "model": "shared" }]
            })),
            fail: std::sync::atomic::AtomicBool::new(false),
            requests: std::sync::atomic::AtomicUsize::new(0),
        });
        let first_url = discovery_server(first.clone()).await;
        let second_url = discovery_server(second.clone()).await;
        let mut config = config_with_ollama("a", &first_url, true);
        config.providers.insert(
            "b".to_string(),
            ProviderConfig {
                provider_id: "b".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({
                    "base_url": second_url,
                    "discover_models": true
                }),
            },
        );
        let mut ctx = crate::AppContext::new(config, Arc::new(CaptureUi::default()));

        let reports = sync_providers_bounded(
            &mut ctx,
            &["a".to_string(), "b".to_string()],
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(reports.len(), 2);
        assert_eq!(ctx.config().get_model("shared").unwrap().provider_id, "a");
        assert_eq!(ctx.config().get_model("b/shared").unwrap().provider_id, "b");
        assert!(reports[1].conflicts.contains(&"b/shared".to_string()));
    }

    #[tokio::test]
    async fn launcher_sync_only_asks_the_used_provider() {
        let _home = crate::config::TestConfigHome::new();
        let used_state = std::sync::Arc::new(TestState {
            tags: std::sync::Mutex::new(serde_json::json!({
                "models": [{ "name": "used:8b", "model": "used:8b" }]
            })),
            fail: std::sync::atomic::AtomicBool::new(false),
            requests: std::sync::atomic::AtomicUsize::new(0),
        });
        let unused_state = std::sync::Arc::new(TestState {
            tags: std::sync::Mutex::new(serde_json::json!({
                "models": [{ "name": "unused:8b", "model": "unused:8b" }]
            })),
            fail: std::sync::atomic::AtomicBool::new(false),
            requests: std::sync::atomic::AtomicUsize::new(0),
        });
        let used_url = discovery_server(used_state.clone()).await;
        let unused_url = discovery_server(unused_state.clone()).await;

        let mut config = config_with_ollama("used", &used_url, true);
        config.providers.insert(
            "unused".to_string(),
            ProviderConfig {
                provider_id: "unused".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({
                    "base_url": unused_url,
                    "discover_models": true
                }),
            },
        );
        config.models.insert(
            "m1".to_string(),
            ModelConfig {
                model_id: "m1".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                provider_id: "used".to_string(),
                variant: None,
                discovered: None,
                config: serde_json::json!({}),
            },
        );
        config.capabilities.insert(
            "chat".to_string(),
            crate::config::CapabilityConfig {
                capability_id: "chat".to_string(),
                capability_type: "agent-model".to_string(),
                config: serde_json::json!({ "model_id": "m1" }),
            },
        );
        config.launchers.insert(
            "claude".to_string(),
            crate::config::LauncherConfig {
                launcher_id: "claude".to_string(),
                launcher_type: "claude".to_string(),
                enabled_capabilities: vec!["chat".to_string()],
                config: serde_json::json!({}),
            },
        );
        let mut ctx = crate::AppContext::new(config, Arc::new(CaptureUi::default()));

        let reports = sync_for_launcher(&mut ctx, "claude").await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].provider_id, "used");
        assert!(ctx.config().get_model("used:8b").is_some());
        assert_eq!(
            unused_state
                .requests
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }
}
