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
            .config
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
        let configured = ctx.config.get_provider(id);

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

                if let Some(cfg) = configured {
                    let mut instance_fields: Vec<(&str, String)> = Vec::new();

                    instance_fields.push(("Config: Type", cfg.provider_type.clone()));
                    if let Some(obj) = cfg.config.as_object() {
                        for (k, v) in obj {
                            instance_fields.push(("Config", format!("{k} = {v}")))
                        }
                    }

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
    pub async fn setup(
        ctx: &mut crate::AppContext,
        provider_type: &str,
        instance_id: Option<&str>,
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
        let existing_config = ctx.config.get_provider(&instance_id);
        if existing_config.is_some() {
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

        let provider_config = crate::config::ProviderConfig {
            provider_id: instance_id.clone(),
            provider_type: provider_type.to_string(),
            config,
        };

        ctx.config
            .insert_provider(&instance_id, provider_config)
            .map_err(|e| anyhow::anyhow!("failed to save provider config: {e}"))?;

        // Health check
        ctx.ui.info("\nRunning health check...");
        match Self::check_provider_health(ctx, &instance_id).await {
            Ok(status) => {
                if status.healthy {
                    ctx.ui
                        .info(&format!("Provider '{instance_id}' is healthy!"));
                } else {
                    ctx.ui.warn(&format!("Provider '{instance_id}' health check failed. It may need to be started or configured differently."));
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
            None => ctx.config.providers.keys().cloned().collect(),
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
    pub fn remove(ctx: &mut crate::AppContext, provider_id: &str) -> Result<()> {
        if ctx.config.get_provider(provider_id).is_none() {
            anyhow::bail!("No provider configured with id '{provider_id}'. Nothing to remove.");
        }

        // Anything pointing at it would be stranded by this removal.
        match crate::commands::shared::remediation::confirm_removal(
            ctx,
            RefKind::Provider,
            provider_id,
        )? {
            crate::commands::shared::remediation::Removal::Cancel => {
                ctx.ui.info(&format!("Keeping provider '{provider_id}'."));
                return Ok(());
            }
            crate::commands::shared::remediation::Removal::Proceed { with } => {
                crate::commands::shared::remediation::remove_all(ctx, &with)?;
            }
        }

        if let Err(e) = ctx.config.remove_provider(provider_id) {
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
        let provider_config = ctx.config.get_provider(provider_id).ok_or_else(|| {
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

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ProviderConfig};
    use crate::utils::ui::base::tests::CaptureUi;
    use std::sync::Arc;

    fn test_ctx() -> crate::AppContext {
        crate::AppContext {
            config: Config::default(),
            ui: Arc::new(CaptureUi::default()),
        }
    }

    fn ctx_with_provider(id: &str, url: &str) -> crate::AppContext {
        let mut ctx = test_ctx();
        ctx.config.providers.insert(
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
        ctx.config.providers.insert(
            "prod-openai".to_string(),
            ProviderConfig {
                provider_id: "prod-openai".to_string(),
                provider_type: "openai-compatible".to_string(),
                config: serde_json::json!({ "base_url": "http://prod" }),
            },
        );
        ctx.config.providers.insert(
            "local-ollama".to_string(),
            ProviderConfig {
                provider_id: "local-ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({ "base_url": "http://localhost:11434" }),
            },
        );
        ctx.config.providers.insert(
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
        assert_eq!(details.len(), 1);
        let (id, fields) = &details[0];
        assert_eq!(id, "Type Metadata");
        assert!(fields.iter().any(|(k, _)| *k == "Name"));
        assert!(!fields.iter().any(|(k, _)| k.starts_with("Config")));
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
                .any(|(k, v)| *k == "Config" && v.contains("http://localhost:11434"))
        );
    }

    #[test]
    fn info_configured_unknown_type_renders_note() {
        let mut ctx = test_ctx();
        ctx.config.providers.insert(
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

    // -- setup -----------------------------------------------------------------

    /// A provider that could not be saved must fail the setup, not report
    /// success over configuration that never reached disk.
    #[cfg(unix)]
    #[tokio::test]
    async fn setup_fails_when_config_cannot_be_saved() {
        let home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();

        home.make_unwritable();
        let result = ProviderCommands::setup(&mut ctx, "ollama", Some("test-ollama")).await;
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
        assert!(ctx.config.get_provider("my-ollama").is_some());

        ProviderCommands::remove(&mut ctx, "my-ollama").unwrap();

        assert!(ctx.config.get_provider("my-ollama").is_none());
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
        let result = ProviderCommands::remove(&mut ctx, "doesnt-exist");
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
        ProviderCommands::remove(&mut ctx, "my-ollama").unwrap();
        ProviderCommands::list(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert!(rows.is_empty());
    }
}
