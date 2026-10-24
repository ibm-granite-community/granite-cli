// Third Party
use anyhow::Result;

// Local
use crate::config::validation::RefKind;
use crate::providers::{HealthStatus, PROVIDER_REGISTRY};
use crate::utils::prompt_from_schema;

pub struct ProviderCommands;

impl ProviderCommands {
    pub fn catalog(ctx: &crate::AppContext, wide: bool) -> Result<()> {
        let providers = PROVIDER_REGISTRY.entries();

        let mut rows: Vec<Vec<String>> = providers
            .iter()
            .map(|(id, p)| {
                let api_types = p
                    .supported_api_types
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                let formats = p
                    .supported_formats
                    .iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut row = vec![
                    id.to_string(),
                    api_types,
                    formats,
                    p.default_endpoint.clone(),
                ];
                if wide {
                    let endpoints = p
                        .default_function_endpoints
                        .iter()
                        .map(|(func, eps)| {
                            let ep_strs = eps
                                .iter()
                                .map(|ep| format!("{} ({})", ep.api_type(), ep.path()))
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("{func}: {ep_strs}")
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    row.push(p.description.clone());
                    row.push(endpoints);
                }
                row
            })
            .collect();
        rows.sort_by(|a, b| a[0].cmp(&b[0]));

        let headers: &[&str] = if wide {
            &[
                "ID",
                "API TYPES",
                "FORMATS",
                "DEFAULT URL",
                "DESCRIPTION",
                "ENDPOINTS",
            ]
        } else {
            &["ID", "API TYPES", "FORMATS", "DEFAULT URL"]
        };

        ctx.ui.table(
            &format!("Provider Catalog ({} providers)", providers.len()),
            headers,
            &rows,
        );
        Ok(())
    }

    pub fn list(ctx: &crate::AppContext) -> Result<()> {
        let notes = crate::commands::shared::remediation::dangling_notes(ctx, RefKind::Provider);
        let mut rows: Vec<Vec<String>> = ctx
            .config()
            .providers
            .iter()
            .map(|(id, cfg)| {
                let base_url = cfg
                    .config
                    .get("base_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("-")
                    .to_string();
                vec![
                    id.clone(),
                    cfg.provider_type.clone(),
                    base_url,
                    notes.get(id).cloned().unwrap_or_default(),
                ]
            })
            .collect();
        rows.sort_by(|a, b| {
            let type_cmp = a[1].cmp(&b[1]);
            if type_cmp != std::cmp::Ordering::Equal {
                return type_cmp;
            }
            a[0].cmp(&b[0])
        });

        ctx.ui.table(
            &format!("Configured Providers ({} providers)", rows.len()),
            &["ID", "TYPE", "BASE URL", "NOTES"],
            &rows,
        );
        Ok(())
    }

    pub fn info(ctx: &crate::AppContext, id: &str) -> Result<()> {
        let configured = ctx.config().get_provider(id);

        let metadata = configured
            .and_then(|p| PROVIDER_REGISTRY.get(&p.provider_type))
            .or_else(|| PROVIDER_REGISTRY.get(id));

        match metadata {
            Some(md) => {
                let mut type_fields: Vec<(&str, String)> = vec![
                    ("Name", md.name.clone()),
                    ("Description", md.description.clone()),
                    ("Type", md.provider_type.to_string()),
                    ("Default URL", md.default_endpoint.clone()),
                ];

                let api_types = md
                    .supported_api_types
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                if !api_types.is_empty() {
                    type_fields.push(("API Types", api_types));
                }

                let formats = md
                    .supported_formats
                    .iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                if !formats.is_empty() {
                    type_fields.push(("Formats", formats));
                }

                if !md.tags.is_empty() {
                    type_fields.push(("Tags", md.tags.join(", ")));
                }

                ctx.ui.detail("Type Metadata", &type_fields);

                // Only for a catalog type: a configured instance may point at
                // its program through settings the PATH lookup does not read.
                if configured.is_none() {
                    crate::commands::shared::install::show_install_detail(ctx, &md.install);
                }

                if let Some(cfg) = configured {
                    let mut instance_fields: Vec<(&str, String)> = Vec::new();

                    instance_fields.push(("Config: Type", cfg.provider_type.clone()));
                    let config_fields = crate::commands::shared::config_fields(&cfg.config);
                    instance_fields
                        .extend(config_fields.iter().map(|(k, v)| (k.as_str(), v.clone())));

                    ctx.ui.detail(id, &instance_fields);
                }

                Ok(())
            }
            None => {
                if configured.is_some() {
                    let fields: Vec<(&str, String)> = vec![(
                        "Note",
                        "Configured but its type is not found in the bundled registry".to_string(),
                    )];
                    ctx.ui.detail(id, &fields);
                    Ok(())
                } else {
                    ctx.ui
                        .info(&format!("Provider '{id}' not found in registry."));

                    let available: Vec<_> = crate::providers::PROVIDER_REGISTRY
                        .entries()
                        .keys()
                        .map(|k| k.to_string())
                        .collect();
                    ctx.ui
                        .info(&format!("Available providers: {}", available.join(", ")));

                    anyhow::bail!("Provider not found");
                }
            }
        }
    }

    /// Interactive provider setup wizard.
    ///
    /// `provider_type` is the catalog/registry key (e.g. `openai-compatible`).
    /// `instance_id` is this instance's nickname, distinct from its type --
    /// defaults to `provider_type` when not given, but a caller may pass a
    /// different value to configure multiple named instances of one type
    /// (e.g. `openai-compatible` backing `llama-cpp`, `ollama`, `lm-studio`).
    /// `force_overwrite` skips the "already configured, overwrite?" prompt,
    /// for callers that have already obtained confirmation (e.g. remediation).
    pub async fn setup(
        ctx: &mut crate::AppContext,
        provider_type: &str,
        instance_id: Option<&str>,
        force_overwrite: bool,
    ) -> Result<()> {
        let provider_def = match PROVIDER_REGISTRY.get(provider_type) {
            Some(def) => def,
            None => {
                ctx.ui.error(&format!(
                    "Provider type '{provider_type}' not found in registry."
                ));
                let available: Vec<_> = PROVIDER_REGISTRY
                    .entries()
                    .iter()
                    .map(|(p_id, p)| format!("{} ({})", p_id, p.name))
                    .collect();
                ctx.ui.info(&format!(
                    "Available provider types: {}",
                    available.join(", ")
                ));
                anyhow::bail!("Provider type not found");
            }
        };

        ctx.ui
            .info(&format!("\nSetting up provider instance: {provider_type}"));
        ctx.ui.info(&provider_def.description);
        ctx.ui.info("");
        ctx.ui
            .info(&format!("Type: {}", provider_def.provider_type));

        // Get a name for this instance
        let instance_id = match instance_id {
            Some(instance_id_arg) => instance_id_arg.to_string(),
            _ => ctx.ui.text("Instance name: ", provider_type)?,
        };

        if !provider_def.authentication.is_empty() {
            let auths = provider_def
                .authentication
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            ctx.ui.info(&format!("Authentication: {auths}"));
        }

        // Check if this instance is already configured
        let existing_config = ctx.config().get_provider(&instance_id);
        if existing_config.is_some() && !force_overwrite {
            let overwrite = ctx.ui.confirm(
                &format!("Provider instance '{instance_id}' is already configured. Overwrite?"),
                false,
            )?;
            if !overwrite {
                ctx.ui.info("Provider setup skipped.");
                return Ok(());
            }
        }

        let schema = PROVIDER_REGISTRY
            .config_schema(provider_type)
            .ok_or_else(|| {
                anyhow::anyhow!("No config schema registered for provider type '{provider_type}'")
            })?;
        let defaults = existing_config
            .map(|c| c.config.clone())
            .or_else(|| PROVIDER_REGISTRY.default_config(provider_type))
            .unwrap_or_else(|| serde_json::json!({}));

        let config = prompt_from_schema(&*ctx.ui, &schema, &defaults)?;

        let saved = config.clone();
        let provider_config = crate::config::ProviderConfig {
            provider_id: instance_id.clone(),
            provider_type: provider_type.to_string(),
            config,
        };

        ctx.config_mut()
            .insert_provider(&instance_id, provider_config)
            .map_err(|e| anyhow::anyhow!("failed to save provider config: {e}"))?;

        // Health check, through the install environment so that what
        // follows a failure can be tested without a server.
        ctx.ui.info("\nRunning health check...");
        match ctx
            .install_env()
            .check_provider(provider_type, &saved)
            .await
        {
            Ok(status) if status.healthy => {
                ctx.ui
                    .info(&format!("Provider '{instance_id}' is healthy!"));
            }
            Ok(status) => {
                ctx.ui.warn(&format!(
                    "Provider '{instance_id}' health check failed: {}",
                    status.error.unwrap_or_else(|| "no answer".to_string())
                ));
                if after_failed_health_check(ctx, provider_type, &provider_def, &saved).await? {
                    ctx.ui
                        .info(&format!("Provider '{instance_id}' is healthy!"));
                }
            }
            Err(e) => {
                ctx.ui.warn(&format!("Could not run health check: {e}"));
            }
        }

        ctx.ui.info(&format!(
            "\nProvider instance '{instance_id}' configured successfully!"
        ));
        ctx.ui.info("Supported APIs:");
        for (func, endpoints) in &provider_def.default_function_endpoints {
            let endpoint_strs: Vec<String> = endpoints
                .iter()
                .map(|ep| format!("{} ({})", ep.api_type(), ep.path()))
                .collect();
            ctx.ui
                .info(&format!("  - {} -> {}", func, endpoint_strs.join(", ")));
        }

        Ok(())
    }

    /// Check health of a provider or all configured providers.
    pub async fn health(ctx: &mut crate::AppContext, provider_id: Option<&str>) -> Result<()> {
        let providers_to_check: Vec<String> = match provider_id {
            Some(id) => vec![id.to_string()],
            None => ctx.config().providers.keys().cloned().collect(),
        };

        if providers_to_check.is_empty() {
            ctx.ui.info("No configured providers to check.");
            return Ok(());
        }

        for id in &providers_to_check {
            match Self::check_provider_health(ctx, id).await {
                Ok(status) => {
                    let detail = if let Some(ref e) = status.error {
                        format!("{} — {}", status.latency.as_millis(), e)
                    } else {
                        format!("{}ms", status.latency.as_millis())
                    };
                    ctx.ui.status(id, status.healthy, &detail);
                }
                Err(e) => {
                    ctx.ui.status(id, false, &e.to_string());
                }
            }
        }

        Ok(())
    }

    /// Remove a configured provider instance by ID.
    ///
    /// Deletes the provider's config file and removes it from the in-memory
    /// config. After this call `provider list` will no longer show the entry.
    ///
    /// `policy` controls what happens to dependents: `Cascade` removes them
    /// too, `Refuse` aborts, `Leave` strands them. `None` asks interactively
    /// or leaves in non-interactive sessions.
    pub fn remove(
        ctx: &mut crate::AppContext,
        provider_id: &str,
        policy: Option<crate::commands::shared::remediation::DependentsPolicy>,
    ) -> Result<()> {
        if ctx.config().get_provider(provider_id).is_none() {
            anyhow::bail!("No provider configured with id '{provider_id}'. Nothing to remove.");
        }

        // Anything pointing at it would be stranded by this removal.
        match crate::commands::shared::remediation::confirm_removal(
            ctx,
            RefKind::Provider,
            provider_id,
            policy,
        )? {
            crate::commands::shared::remediation::Removal::Cancel => {
                ctx.ui.info(&format!("Keeping provider '{provider_id}'."));
                return Ok(());
            }
            crate::commands::shared::remediation::Removal::Proceed { with } => {
                crate::commands::shared::remediation::remove_all(ctx, &with, policy)?;
            }
        }

        if let Err(e) = ctx.config_mut().remove_provider(provider_id) {
            ctx.ui
                .warn(&format!("failed to persist provider removal: {e}"));
        }
        ctx.ui.info(&format!("Provider '{provider_id}' removed."));
        Ok(())
    }

    async fn check_provider_health(
        ctx: &crate::AppContext,
        provider_id: &str,
    ) -> Result<HealthStatus> {
        let provider_config = ctx.config().get_provider(provider_id).ok_or_else(|| {
            anyhow::anyhow!("Provider '{provider_id}' not found in configuration")
        })?;

        let provider = PROVIDER_REGISTRY
            .construct(
                &provider_config.provider_type,
                &provider_config.provider_id,
                &provider_config.config,
            )
            .map_err(|e| anyhow::anyhow!("Failed to create provider: {e}"))?;

        let status = provider.health_check().await?;

        Ok(status)
    }
}

/*-- private --*/

/// What `provider setup` does after a failed health check, following
/// decision 5 of spec 0028. Returns whether the provider ended healthy.
///
/// A hosted type gets its note. A local type at a localhost or loopback
/// endpoint is looked up on PATH: when its program is missing, the user is
/// told it may be installed elsewhere and is offered the install;
/// when it is present, granite-cli says where it found it. Either way,
/// granite-cli then says the server is not answering, shows how to start it
/// and offers to check again. A local type at any other
/// endpoint gets the warning `provider setup` has always given.
async fn after_failed_health_check(
    ctx: &crate::AppContext,
    provider_type: &str,
    md: &crate::providers::ProviderMetadata,
    config: &serde_json::Value,
) -> Result<bool> {
    let info = &md.install;
    if md.provider_type == crate::providers::ProviderType::Hosted {
        match &info.note {
            Some(note) => ctx.ui.info(&format!("  {note}")),
            None => ctx.ui.warn(NOT_ANSWERING),
        }
        return Ok(false);
    }

    let endpoint = config
        .get("base_url")
        .and_then(|v| v.as_str())
        .unwrap_or(&md.default_endpoint)
        .to_string();
    let (Some(program), true) = (&info.program, is_local_endpoint(&endpoint)) else {
        ctx.ui.warn(NOT_ANSWERING);
        return Ok(false);
    };

    use crate::commands::shared::install::{InstallOutcome, ServerProgram};
    let found = ctx.install_env().find(program);
    let installed = match &found {
        Some(_) => false,
        None => {
            ctx.ui.info(&format!(
                "  '{program}' was not found on PATH. {} may be installed elsewhere.",
                md.name
            ));
            matches!(
                crate::commands::shared::install::offer_install(ctx, &md.name, info).await?,
                InstallOutcome::Installed { .. }
            )
        }
    };
    let state = match (&found, installed) {
        (Some(path), _) => ServerProgram::FoundAt(path),
        (None, true) => ServerProgram::Installed,
        (None, false) => ServerProgram::Unknown,
    };
    crate::commands::shared::install::wait_for_provider(
        ctx,
        &md.name,
        state,
        provider_type,
        config,
        &endpoint,
        info,
    )
    .await
}

const NOT_ANSWERING: &str = "It may need to be started or configured differently.";

/// Whether `endpoint`'s host is `localhost`, a loopback address, or the
/// unspecified address (`0.0.0.0`, `::`), which a client reaches locally.
fn is_local_endpoint(endpoint: &str) -> bool {
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unspecified(),
        None => false,
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ProviderConfig};
    use crate::utils::ui::base::tests::CaptureUi;
    use std::sync::Arc;

    fn test_ctx() -> crate::AppContext {
        crate::AppContext::new(Config::default(), Arc::new(CaptureUi::default()))
    }

    fn ctx_with_provider(id: &str, url: &str) -> crate::AppContext {
        let mut ctx = test_ctx();
        ctx.config_mut().providers.insert(
            id.to_string(),
            ProviderConfig {
                provider_id: id.to_string(),
                provider_type: "openai-compatible".to_string(),
                config: serde_json::json!({ "base_url": url }),
            },
        );
        ctx
    }

    macro_rules! tables {
        ($ctx:expr) => {
            (&*($ctx.ui) as &dyn std::any::Any)
                .downcast_ref::<CaptureUi>()
                .unwrap()
                .tables
                .borrow()
        };
    }

    macro_rules! details {
        ($ctx:expr) => {
            (&*($ctx.ui) as &dyn std::any::Any)
                .downcast_ref::<CaptureUi>()
                .unwrap()
                .details
                .borrow()
        };
    }

    macro_rules! infos {
        ($ctx:expr) => {
            (&*($ctx.ui) as &dyn std::any::Any)
                .downcast_ref::<CaptureUi>()
                .unwrap()
                .infos
                .borrow()
        };
    }

    macro_rules! statuses {
        ($ctx:expr) => {
            (&*($ctx.ui) as &dyn std::any::Any)
                .downcast_ref::<CaptureUi>()
                .unwrap()
                .statuses
                .borrow()
        };
    }

    // -- catalog --------------------------------------------------------------

    #[test]
    fn catalog_table_has_expected_columns() {
        let ctx = test_ctx();
        ProviderCommands::catalog(&ctx, false).unwrap();
        let tables = tables!(ctx);
        assert_eq!(tables.len(), 1);
        let (_, headers, _) = &tables[0];
        assert!(headers.contains(&"ID".to_string()));
        assert!(headers.contains(&"DEFAULT URL".to_string()));
        assert!(headers.contains(&"API TYPES".to_string()));
        assert!(headers.contains(&"FORMATS".to_string()));
        assert!(!headers.contains(&"DESCRIPTION".to_string()));
        assert!(!headers.contains(&"ENDPOINTS".to_string()));
    }

    #[test]
    fn catalog_wide_includes_description_and_endpoints() {
        let ctx = test_ctx();
        ProviderCommands::catalog(&ctx, true).unwrap();
        let tables = tables!(ctx);
        let (_, headers, _) = &tables[0];
        assert!(headers.contains(&"DESCRIPTION".to_string()));
        assert!(headers.contains(&"ENDPOINTS".to_string()));
    }

    #[test]
    fn catalog_wide_rows_have_six_columns() {
        let ctx = test_ctx();
        ProviderCommands::catalog(&ctx, true).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert!(!rows.is_empty());
        for row in rows.iter() {
            assert_eq!(
                row.len(),
                6,
                "expected 6 columns in wide mode, got {}",
                row.len()
            );
        }
    }

    #[test]
    fn catalog_contains_openai_compatible_entry() {
        let ctx = test_ctx();
        ProviderCommands::catalog(&ctx, false).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert!(rows.iter().any(|r| r[0] == "openai-compatible"));
    }

    // -- list -----------------------------------------------------------------

    #[test]
    fn list_empty_config_has_zero_rows() {
        let ctx = test_ctx();
        ProviderCommands::list(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn list_configured_provider_shows_base_url() {
        let ctx = ctx_with_provider("my-ollama", "http://localhost:11434");
        ProviderCommands::list(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert_eq!(rows.len(), 1);
        assert!(rows[0].iter().any(|c| c.contains("11434")));
    }

    #[test]
    fn list_sorted_by_type_then_id() {
        let mut ctx = test_ctx();
        ctx.config_mut().providers.insert(
            "prod-openai".to_string(),
            ProviderConfig {
                provider_id: "prod-openai".to_string(),
                provider_type: "openai-compatible".to_string(),
                config: serde_json::json!({ "base_url": "http://prod" }),
            },
        );
        ctx.config_mut().providers.insert(
            "local-ollama".to_string(),
            ProviderConfig {
                provider_id: "local-ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({ "base_url": "http://localhost:11434" }),
            },
        );
        ctx.config_mut().providers.insert(
            "dev-openai".to_string(),
            ProviderConfig {
                provider_id: "dev-openai".to_string(),
                provider_type: "openai-compatible".to_string(),
                config: serde_json::json!({ "base_url": "http://dev" }),
            },
        );
        ProviderCommands::list(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][1], "ollama");
        assert_eq!(rows[1][1], "openai-compatible");
        assert_eq!(rows[2][1], "openai-compatible");
        assert_eq!(rows[1][0], "dev-openai");
        assert_eq!(rows[2][0], "prod-openai");
    }

    // -- info -----------------------------------------------------------------

    #[test]
    fn info_unknown_provider_returns_err() {
        let ctx = test_ctx();
        let result = ProviderCommands::info(&ctx, "does-not-exist");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Provider not found")
        );
    }

    #[test]
    fn info_catalog_provider_renders_detail() {
        let ctx = test_ctx();
        let result = ProviderCommands::info(&ctx, "openai-compatible");
        assert!(result.is_ok());

        let details = details!(ctx);
        assert_eq!(details.len(), 2);
        let (id, fields) = &details[0];
        assert_eq!(id, "Type Metadata");
        assert!(fields.iter().any(|(k, _)| *k == "Name"));
        assert!(!fields.iter().any(|(k, _)| k.starts_with("Config")));
        assert_eq!(details[1].0, "Install");
    }

    #[test]
    fn info_configured_provider_renders_detail_with_config() {
        let ctx = ctx_with_provider("my-provider", "http://localhost:11434");
        let result = ProviderCommands::info(&ctx, "my-provider");
        assert!(result.is_ok());

        let details = details!(ctx);
        assert_eq!(details.len(), 2);

        let (id1, fields1) = &details[0];
        assert_eq!(id1, "Type Metadata");
        assert!(fields1.iter().any(|(k, _)| *k == "Name"));

        let (id2, fields2) = &details[1];
        assert_eq!(id2, "my-provider");

        assert!(
            fields2
                .iter()
                .any(|(k, v)| *k == "Config: Type" && v == "openai-compatible")
        );
        assert!(
            fields2
                .iter()
                .any(|(k, v)| *k == "Config: base_url" && v == "http://localhost:11434")
        );
    }

    #[test]
    fn info_in_json_keeps_every_config_value() {
        let json = Arc::new(crate::utils::ui::backends::json::JsonOutput::with_capture());
        let mut ctx = crate::AppContext::new(Config::default(), json.clone());
        ctx.config_mut().providers.insert(
            "my-provider".to_string(),
            ProviderConfig {
                provider_id: "my-provider".to_string(),
                provider_type: "openai-compatible".to_string(),
                config: serde_json::json!({
                    "base_url": "http://localhost:11434",
                    "timeout_secs": 10,
                }),
            },
        );

        ProviderCommands::info(&ctx, "my-provider").unwrap();

        let records = json.captured();
        let instance = records
            .iter()
            .find(|r| r["title"] == "my-provider")
            .expect("an instance detail record");
        assert_eq!(
            instance["fields"]["Config: base_url"],
            "http://localhost:11434"
        );
        assert_eq!(instance["fields"]["Config: timeout_secs"], "10");
    }

    #[test]
    fn info_configured_unknown_type_renders_note() {
        let mut ctx = test_ctx();
        ctx.config_mut().providers.insert(
            "custom-provider".to_string(),
            ProviderConfig {
                provider_id: "custom-provider".to_string(),
                provider_type: "not-a-real-type".to_string(),
                config: serde_json::json!({}),
            },
        );
        let result = ProviderCommands::info(&ctx, "custom-provider");
        assert!(result.is_ok());

        let details = details!(ctx);
        assert_eq!(details.len(), 1);
        let (id, fields) = &details[0];
        assert_eq!(id, "custom-provider");
        assert!(
            fields
                .iter()
                .any(|(k, v)| *k == "Note" && v.contains("not found in the bundled registry"))
        );
    }

    fn ctx_on(env: crate::utils::test_support::FakeInstallEnv) -> crate::AppContext {
        let mut ctx = test_ctx();
        ctx.set_install_env(Arc::new(env));
        ctx
    }

    fn install_detail(ctx: &crate::AppContext) -> Vec<(String, String)> {
        details!(ctx)
            .iter()
            .find(|(title, _)| title == "Install")
            .map(|(_, fields)| fields.clone())
            .unwrap_or_default()
    }

    #[test]
    fn info_shows_install_detail_on_a_mac_with_homebrew() {
        use crate::utils::install::Os;
        let ctx = ctx_on(
            crate::utils::test_support::FakeInstallEnv::new(Os::Mac).with_programs(&["brew"]),
        );
        ProviderCommands::info(&ctx, "ollama").unwrap();

        let fields = install_detail(&ctx);
        let get = |key: &str| -> Vec<String> {
            fields
                .iter()
                .filter(|(k, _)| k == key || k.starts_with(&format!("{key} ")))
                .map(|(_, v)| v.clone())
                .collect()
        };
        assert_eq!(get("Program"), vec!["ollama (not found on PATH)"]);
        assert_eq!(
            get("Install"),
            vec!["brew install ollama  [✓ runs on this machine]"]
        );
        assert_eq!(get("Run"), vec!["ollama serve"]);
        assert_eq!(
            get("Start"),
            vec!["Homebrew can run it as a service: brew services start ollama"]
        );
        assert_eq!(get("Page"), vec!["https://ollama.com/download"]);
    }

    #[test]
    fn info_shows_why_no_method_runs_on_a_mac_without_homebrew() {
        use crate::utils::install::Os;
        let ctx = ctx_on(
            crate::utils::test_support::FakeInstallEnv::new(Os::Mac).with_programs(&["curl"]),
        );
        ProviderCommands::info(&ctx, "llama-cpp").unwrap();

        let fields = install_detail(&ctx);
        assert!(fields.contains(&(
            "Install".to_string(),
            "brew install llama.cpp  [Homebrew not found]".to_string()
        )));
        assert!(fields.contains(&("Run".to_string(), "llama-server".to_string())));
    }

    #[test]
    fn every_provider_type_declares_a_page_or_a_note() {
        for (id, md) in PROVIDER_REGISTRY.entries() {
            assert!(
                md.install.page.is_some() || md.install.note.is_some(),
                "{id} declares neither a page nor a note"
            );
        }
    }

    #[test]
    fn local_provider_types_declare_a_program_and_a_run_command() {
        for (id, md) in PROVIDER_REGISTRY.entries() {
            if md.provider_type == crate::providers::ProviderType::Local {
                assert!(md.install.program.is_some(), "{id} declares no program");
                assert!(
                    md.install.run_command.is_some(),
                    "{id} declares no run command"
                );
            }
        }
    }

    #[test]
    fn hosted_provider_types_declare_no_methods() {
        for (id, md) in PROVIDER_REGISTRY.entries() {
            if md.provider_type == crate::providers::ProviderType::Hosted {
                assert!(
                    md.install.methods.is_empty(),
                    "{id} declares install methods"
                );
            }
        }
    }

    /// A vendor script on macOS or Linux often installs outside PATH, so it
    /// names where. The Windows installers put themselves on PATH.
    #[test]
    fn every_unix_script_method_declares_a_directory() {
        use crate::utils::install::{MethodKind, Os};
        for (id, md) in PROVIDER_REGISTRY.entries() {
            for m in &md.install.methods {
                if m.kind == MethodKind::Script && !m.os.contains(&Os::Windows) {
                    assert!(
                        !m.bin_dirs.is_empty(),
                        "{id}: {} declares no directory",
                        m.command
                    );
                }
            }
        }
    }

    #[test]
    fn every_method_that_pipes_to_bash_needs_bash() {
        for (id, md) in PROVIDER_REGISTRY.entries() {
            for m in &md.install.methods {
                if m.command.contains("bash") {
                    assert!(m.needs.iter().any(|n| n == "bash"), "{id}: {}", m.command);
                }
            }
        }
    }

    // -- setup after a failed health check -------------------------------------

    fn capture(ctx: &crate::AppContext) -> &CaptureUi {
        (&*ctx.ui as &dyn std::any::Any)
            .downcast_ref::<CaptureUi>()
            .expect("test contexts are built with a CaptureUi")
    }

    fn texts(ui: &CaptureUi) -> String {
        let mut lines = ui.infos.borrow().clone();
        lines.extend(ui.warns.borrow().iter().cloned());
        lines.join("\n")
    }

    #[tokio::test]
    async fn setup_offers_the_install_then_shows_how_to_start_when_the_program_is_missing() {
        use crate::utils::install::Os;
        use crate::utils::test_support::{FakeInstallEnv, FakeRun};
        let _home = crate::config::TestConfigHome::new();
        let env = Arc::new(
            FakeInstallEnv::new(Os::Mac)
                .with_programs(&["brew"])
                .with_run(
                    "brew install ollama",
                    FakeRun::ok().installs("ollama", "/opt/homebrew/bin/ollama"),
                ),
        );
        let mut ctx = test_ctx();
        ctx.set_install_env(env.clone());
        // Install with brew, then "Continue without it".
        capture(&ctx).select_answers.borrow_mut().extend([0, 1]);

        ProviderCommands::setup(&mut ctx, "ollama", Some("ollama"), false)
            .await
            .unwrap();

        assert_eq!(env.runs(), vec!["brew install ollama".to_string()]);
        let ui = capture(&ctx);
        let text = texts(ui);
        assert!(
            text.contains("'ollama' was not found on PATH. Ollama may be installed elsewhere.")
        );
        assert!(text.contains("Ollama is installed but not answering at http://localhost:11434."));
        assert!(text.contains("  You can start it in another terminal with:"));
        assert!(text.contains("      ollama serve"));
        assert!(text.contains("brew services start ollama"));
        let prompts: Vec<String> = ui
            .select_prompts
            .borrow()
            .iter()
            .map(|p| p.0.clone())
            .collect();
        assert_eq!(prompts, vec!["Install Ollama now?", "Would you like to"]);
        // "Continue without it" leaves the provider configured.
        assert!(ctx.config().get_provider("ollama").is_some());
        assert!(!text.contains("is healthy"));
    }

    #[tokio::test]
    async fn setup_shows_how_to_start_when_the_program_is_present() {
        use crate::utils::install::Os;
        use crate::utils::test_support::FakeInstallEnv;
        let _home = crate::config::TestConfigHome::new();
        let env = Arc::new(
            FakeInstallEnv::new(Os::Linux)
                .with_programs(&["ollama", "curl"])
                .with_health(&[false, true]),
        );
        let mut ctx = test_ctx();
        ctx.set_install_env(env.clone());
        capture(&ctx).select_answers.borrow_mut().push_back(0); // Check again

        ProviderCommands::setup(&mut ctx, "ollama", Some("ollama"), false)
            .await
            .unwrap();

        assert!(env.runs().is_empty());
        let ui = capture(&ctx);
        assert_eq!(ui.select_prompts.borrow().len(), 1);
        let text = texts(ui);
        assert!(text.contains(
            "Ollama was found at /usr/bin/ollama but is not answering at http://localhost:11434."
        ));
        assert!(text.contains("sudo systemctl start ollama"));
        assert!(text.contains("Provider 'ollama' is healthy!"));
        assert_eq!(env.health_checks(), vec!["ollama", "ollama"]);
    }

    #[tokio::test]
    async fn setup_offers_neither_for_a_provider_on_another_host() {
        use crate::utils::install::Os;
        use crate::utils::test_support::FakeInstallEnv;
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        ctx.set_install_env(Arc::new(
            FakeInstallEnv::new(Os::Mac).with_programs(&["brew"]),
        ));
        capture(&ctx)
            .text_answers
            .borrow_mut()
            .push_back("http://gpu-box:11434".to_string());

        ProviderCommands::setup(&mut ctx, "ollama", Some("ollama"), false)
            .await
            .unwrap();

        let ui = capture(&ctx);
        assert!(ui.select_prompts.borrow().is_empty());
        assert!(
            ui.warns
                .borrow()
                .iter()
                .any(|w| w == "It may need to be started or configured differently.")
        );
        assert_eq!(
            ctx.config().get_provider("ollama").unwrap().config["base_url"],
            "http://gpu-box:11434"
        );
    }

    #[tokio::test]
    async fn setup_of_a_hosted_type_shows_its_note() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();

        ProviderCommands::setup(&mut ctx, "openrouter", Some("openrouter"), false)
            .await
            .unwrap();

        let ui = capture(&ctx);
        assert!(ui.select_prompts.borrow().is_empty());
        assert!(
            ui.infos
                .borrow()
                .iter()
                .any(|i| i.contains("OpenRouter is a hosted service."))
        );
        assert!(!ui.warns.borrow().iter().any(|w| w == NOT_ANSWERING));
    }

    #[tokio::test]
    async fn setup_shows_how_to_start_after_a_declined_install() {
        use crate::utils::install::Os;
        use crate::utils::test_support::FakeInstallEnv;
        let _home = crate::config::TestConfigHome::new();
        let env = Arc::new(FakeInstallEnv::new(Os::Mac).with_programs(&["brew"]));
        let mut ctx = test_ctx();
        ctx.set_install_env(env.clone());
        // Don't install, then the wait prompt's default.
        capture(&ctx).select_answers.borrow_mut().push_back(1);

        ProviderCommands::setup(&mut ctx, "ollama", Some("ollama"), false)
            .await
            .unwrap();

        assert!(env.runs().is_empty());
        let ui = capture(&ctx);
        assert!(texts(ui).contains("Ollama is not answering at http://localhost:11434."));
        assert_eq!(ui.select_prompts.borrow().len(), 2);
    }

    #[test]
    fn local_endpoints_are_localhost_and_loopback() {
        assert!(is_local_endpoint("http://localhost:11434"));
        assert!(is_local_endpoint("http://127.0.0.1:8080/v1"));
        assert!(is_local_endpoint("http://[::1]:1234"));
        assert!(is_local_endpoint("http://0.0.0.0:8080"));
        assert!(!is_local_endpoint("http://gpu-box:11434"));
        assert!(!is_local_endpoint("https://openrouter.ai/api"));
        assert!(!is_local_endpoint("not a url"));
    }

    // -- setup -----------------------------------------------------------------

    /// A provider that could not be saved must fail the setup, not report
    /// success over configuration that never reached disk.
    #[cfg(unix)]
    #[tokio::test]
    async fn setup_fails_when_config_cannot_be_saved() {
        let home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();

        home.make_unwritable();
        let result = ProviderCommands::setup(&mut ctx, "ollama", Some("test-ollama"), false).await;
        home.make_writable();

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("failed to save provider config")
        );
        let infos = infos!(ctx);
        assert!(
            !infos.iter().any(|m| m.contains("configured successfully")),
            "{infos:?}"
        );
    }

    // -- health ----------------------------------------------------------------

    #[tokio::test]
    async fn health_no_providers_emits_info_message() {
        let mut ctx = test_ctx();
        ProviderCommands::health(&mut ctx, None).await.unwrap();
        assert!(!infos!(ctx).is_empty());
        assert!(statuses!(ctx).is_empty());
    }

    // -- remove -----------------------------------------------------------------

    #[test]
    fn remove_existing_provider_succeeds_and_disappears_from_list() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_provider("my-ollama", "http://localhost:11434");
        assert!(ctx.config().get_provider("my-ollama").is_some());

        ProviderCommands::remove(&mut ctx, "my-ollama", None).unwrap();

        assert!(ctx.config().get_provider("my-ollama").is_none());
        let infos = infos!(ctx);
        assert!(
            infos
                .iter()
                .any(|m| m.contains("my-ollama") && m.contains("removed"))
        );
    }

    #[test]
    fn remove_nonexistent_provider_returns_err() {
        let mut ctx = test_ctx();
        let result = ProviderCommands::remove(&mut ctx, "doesnt-exist", None);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Nothing to remove")
        );
    }

    #[test]
    fn list_does_not_show_removed_provider() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_provider("my-ollama", "http://localhost:11434");
        ProviderCommands::remove(&mut ctx, "my-ollama", None).unwrap();
        ProviderCommands::list(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert!(rows.is_empty());
    }
}
