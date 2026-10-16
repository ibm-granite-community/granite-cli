// Third Party
use anyhow::Result;

// Local
use crate::config::validation::RefKind;
use crate::providers::{HealthStatus, PROVIDER_REGISTRY};
use crate::utils::prompt_from_schema;

pub(crate) struct ProviderCommands;

impl ProviderCommands {
    pub(crate) fn catalog(ctx: &crate::AppContext, wide: bool) -> Result<()> {
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
                    p.formatted_default_base_urls(),
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
                "DEFAULT URLS",
                "DESCRIPTION",
                "ENDPOINTS",
            ]
        } else {
            &["ID", "API TYPES", "FORMATS", "DEFAULT URLS"]
        };

        ctx.ui.table(
            &format!("Provider Catalog ({} providers)", providers.len()),
            headers,
            &rows,
        );
        Ok(())
    }

    pub(crate) fn list(ctx: &crate::AppContext) -> Result<()> {
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
                    hardware_label(ctx, cfg),
                    notes
                        .get(id)
                        .cloned()
                        .into_iter()
                        .chain(legacy_hardware_note(cfg))
                        .collect::<Vec<_>>()
                        .join("; "),
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
            &["ID", "TYPE", "BASE URL", "HARDWARE", "NOTES"],
            &rows,
        );
        Ok(())
    }

    pub(crate) async fn info(ctx: &mut crate::AppContext, id: &str) -> Result<()> {
        // A configured provider's hardware link can be broken; an id that
        // names a catalog type is being browsed, not diagnosed. A run that
        // cannot ask prints the problem.
        if ctx.config().get_provider(id).is_some() {
            let can_prompt = crate::commands::HardwareCommands::can_prompt(ctx);
            crate::commands::shared::remediation::remediate(
                ctx,
                RefKind::Provider,
                id,
                crate::commands::shared::remediation::OnDecline::Skip,
                can_prompt,
            )
            .await?;

            // Removing it is one of the choices offered above, and leaves
            // nothing to show.
            if ctx.config().get_provider(id).is_none() {
                return Ok(());
            }
        }
        Self::info_detail(ctx, id)
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
    pub(crate) async fn setup(
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
            .unwrap_or_else(|| PROVIDER_REGISTRY.default_config(provider_type));

        let current_link = existing_config.and_then(|c| c.hardware.clone());

        let config = prompt_from_schema(&*ctx.ui, &schema, &defaults)?;
        let hardware =
            crate::commands::HardwareCommands::choose_link(ctx, provider_type, current_link)?;

        let hardware_note = hardware.profile_id().unwrap_or("not specified").to_string();
        let provider_config = crate::config::ProviderConfig {
            provider_id: instance_id.clone(),
            provider_type: provider_type.to_string(),
            config,
            hardware: Some(hardware),
        };

        ctx.config_mut()
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
            "\nProvider instance '{instance_id}' configured successfully (hardware: \
             {hardware_note})."
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
    pub(crate) async fn health(
        ctx: &mut crate::AppContext,
        provider_id: Option<&str>,
    ) -> Result<()> {
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
    pub(crate) fn remove(
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

impl ProviderCommands {
    fn info_detail(ctx: &crate::AppContext, id: &str) -> Result<()> {
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
                    ("Default URLs", md.formatted_default_base_urls()),
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
                    let config_fields = crate::commands::shared::config_fields(&cfg.config);
                    instance_fields
                        .extend(config_fields.iter().map(|(k, v)| (k.as_str(), v.clone())));
                    instance_fields.push(("Hardware", hardware_label(ctx, cfg)));
                    if let Some(note) = legacy_hardware_note(cfg) {
                        instance_fields.push(("Note", note));
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
}

/// The HARDWARE column of `provider list`: the hardware profile id, "hosted"
/// for a hosted provider with none, and a warning for any other provider
/// with none, since fit cannot be estimated for it. A provider saved before
/// links existed is estimated against `local`, which nobody chose, so it is
/// shown with a warning.
fn hardware_label(ctx: &crate::AppContext, cfg: &crate::config::ProviderConfig) -> String {
    use crate::config::hardware::HardwareLink;
    match &cfg.hardware {
        None => ctx.ui.warn_mark("⚠ local (not set)"),
        Some(HardwareLink::Unspecified)
            if HardwareLink::default_for(&cfg.provider_type) == HardwareLink::Unspecified =>
        {
            "hosted".to_string()
        }
        Some(HardwareLink::Unspecified) => ctx.ui.warn_mark("⚠ not specified"),
        Some(link) => link.profile_id().unwrap_or_default().to_string(),
    }
}

/// For a provider saved before links existed, the command that sets its
/// hardware link.
fn legacy_hardware_note(cfg: &crate::config::ProviderConfig) -> Option<String> {
    if cfg.hardware.is_some() {
        return None;
    }
    let id = if cfg.provider_id == cfg.provider_type {
        String::new()
    } else {
        format!(" --id {}", cfg.provider_id)
    };
    Some(format!(
        "legacy config: run `granite-cli provider setup {}{id}` to set",
        cfg.provider_type
    ))
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

    fn non_interactive(ctx: &crate::AppContext) {
        *(&*(ctx.ui) as &dyn std::any::Any)
            .downcast_ref::<CaptureUi>()
            .unwrap()
            .interactive
            .borrow_mut() = Some(false);
    }

    fn ctx_with_provider(id: &str, url: &str) -> crate::AppContext {
        let mut ctx = test_ctx();
        ctx.config_mut().providers.insert(
            id.to_string(),
            ProviderConfig {
                hardware: Default::default(),
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
        assert!(headers.contains(&"DEFAULT URLS".to_string()));
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

    #[test]
    fn catalog_displays_all_default_urls_in_order() {
        let ctx = test_ctx();
        ProviderCommands::catalog(&ctx, false).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        let llama_cpp = rows.iter().find(|row| row[0] == "llama-cpp").unwrap();
        let openai = rows
            .iter()
            .find(|row| row[0] == "openai-compatible")
            .unwrap();

        assert_eq!(llama_cpp[3], "http://localhost:9931, http://localhost:8080");
        assert_eq!(openai[3], "-");
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
                hardware: Default::default(),
                provider_id: "prod-openai".to_string(),
                provider_type: "openai-compatible".to_string(),
                config: serde_json::json!({ "base_url": "http://prod" }),
            },
        );
        ctx.config_mut().providers.insert(
            "local-ollama".to_string(),
            ProviderConfig {
                hardware: Default::default(),
                provider_id: "local-ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({ "base_url": "http://localhost:11434" }),
            },
        );
        ctx.config_mut().providers.insert(
            "dev-openai".to_string(),
            ProviderConfig {
                hardware: Default::default(),
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

    #[tokio::test]
    async fn info_unknown_provider_returns_err() {
        let mut ctx = test_ctx();
        let result = ProviderCommands::info(&mut ctx, "does-not-exist").await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Provider not found")
        );
    }

    #[tokio::test]
    async fn info_catalog_provider_renders_detail() {
        let mut ctx = test_ctx();
        let result = ProviderCommands::info(&mut ctx, "openai-compatible").await;
        assert!(result.is_ok());

        let details = details!(ctx);
        assert_eq!(details.len(), 1);
        let (id, fields) = &details[0];
        assert_eq!(id, "Type Metadata");
        assert!(fields.iter().any(|(k, _)| *k == "Name"));
        assert!(!fields.iter().any(|(k, _)| k.starts_with("Config")));
    }

    #[tokio::test]
    async fn info_configured_provider_renders_detail_with_config() {
        let mut ctx = ctx_with_provider("my-provider", "http://localhost:11434");
        let result = ProviderCommands::info(&mut ctx, "my-provider").await;
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

    #[tokio::test]
    async fn info_in_json_keeps_every_config_value() {
        let json = Arc::new(crate::utils::ui::backends::json::JsonOutput::with_capture());
        let mut ctx = crate::AppContext::new(Config::default(), json.clone());
        ctx.config_mut().providers.insert(
            "my-provider".to_string(),
            ProviderConfig {
                provider_id: "my-provider".to_string(),
                provider_type: "openai-compatible".to_string(),
                hardware: Default::default(),
                config: serde_json::json!({
                    "base_url": "http://localhost:11434",
                    "timeout_secs": 10,
                }),
            },
        );

        ProviderCommands::info(&mut ctx, "my-provider")
            .await
            .unwrap();

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

    #[tokio::test]
    async fn info_warns_about_a_broken_hardware_link_and_still_shows_the_provider() {
        let ui = Arc::new(CaptureUi::default());
        *ui.interactive.borrow_mut() = Some(false);
        let mut ctx = crate::AppContext::new(Config::default(), ui.clone());
        ctx.config_mut().providers.insert(
            "llamacpp-spark".to_string(),
            ProviderConfig {
                hardware: Some(crate::config::hardware::HardwareLink::Profile(
                    "gone".to_string(),
                )),
                provider_id: "llamacpp-spark".to_string(),
                provider_type: "llama-cpp".to_string(),
                config: serde_json::json!({ "base_url": "http://spark:8080" }),
            },
        );

        ProviderCommands::info(&mut ctx, "llamacpp-spark")
            .await
            .unwrap();

        let warns = ui.warns.borrow();
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("'gone'"), "{}", warns[0]);
        assert!(ui.select_prompts.borrow().is_empty());
        assert_eq!(ui.details.borrow().len(), 2);
    }

    #[tokio::test]
    async fn info_configured_unknown_type_renders_note() {
        let mut ctx = test_ctx();
        ctx.config_mut().providers.insert(
            "custom-provider".to_string(),
            ProviderConfig {
                hardware: Default::default(),
                provider_id: "custom-provider".to_string(),
                provider_type: "not-a-real-type".to_string(),
                config: serde_json::json!({}),
            },
        );
        let result = ProviderCommands::info(&mut ctx, "custom-provider").await;
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
        // A run that cannot ask: the hardware link takes the current or
        // default value, and `local` is not detected and written.
        non_interactive(&ctx);

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

    #[tokio::test]
    async fn setup_saves_the_hardware_link_and_keeps_an_existing_one() {
        use crate::config::hardware::HardwareLink;
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        // A run that cannot ask: the hardware link takes the current or
        // default value, and `local` is not detected and written.
        non_interactive(&ctx);

        // openai-compatible usually fronts a server the user runs: a new one
        // runs on `local`.
        ProviderCommands::setup(&mut ctx, "openai-compatible", Some("vllm-lab"), false)
            .await
            .unwrap();
        let link = || {
            ctx.config()
                .get_provider("vllm-lab")
                .unwrap()
                .hardware
                .clone()
        };
        assert_eq!(link(), Some(HardwareLink::Local));
        assert!(
            infos!(ctx)
                .iter()
                .any(|m| m.contains("'vllm-lab' configured successfully (hardware: local)")),
            "{:?}",
            infos!(ctx)
        );

        ctx.config_mut()
            .update_provider("vllm-lab", |p| {
                p.hardware = Some(HardwareLink::Profile("spark".to_string()))
            })
            .unwrap();
        ProviderCommands::setup(&mut ctx, "openai-compatible", Some("vllm-lab"), true)
            .await
            .unwrap();

        assert_eq!(
            ctx.config().get_provider("vllm-lab").unwrap().hardware,
            Some(HardwareLink::Profile("spark".to_string()))
        );
    }

    #[tokio::test]
    async fn setup_of_a_legacy_provider_offers_the_default_for_its_type() {
        use crate::config::hardware::HardwareLink;
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        // A run that cannot ask: the hardware link takes the current or
        // default value, and `local` is not detected and written.
        non_interactive(&ctx);
        ProviderCommands::setup(&mut ctx, "openai-compatible", Some("vllm-lab"), false)
            .await
            .unwrap();
        ctx.config_mut()
            .update_provider("vllm-lab", |p| p.hardware = None)
            .unwrap();

        ProviderCommands::setup(&mut ctx, "openai-compatible", Some("vllm-lab"), true)
            .await
            .unwrap();

        assert_eq!(
            ctx.config().get_provider("vllm-lab").unwrap().hardware,
            Some(HardwareLink::Local)
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

    #[test]
    fn hardware_column_shows_the_link_hosted_or_a_warning() {
        use crate::config::hardware::HardwareLink;
        let ctx = crate::AppContext::new(
            crate::config::Config::default(),
            std::sync::Arc::new(crate::utils::ui::base::tests::CaptureUi::default()),
        );
        let provider =
            |provider_type: &str, hardware: HardwareLink| crate::config::ProviderConfig {
                provider_id: "p".to_string(),
                provider_type: provider_type.to_string(),
                config: serde_json::json!({}),
                hardware: Some(hardware),
            };

        assert_eq!(
            hardware_label(&ctx, &provider("ollama", HardwareLink::Local)),
            "local"
        );
        assert_eq!(
            hardware_label(
                &ctx,
                &provider("llama-cpp", HardwareLink::Profile("spark".to_string()))
            ),
            "spark"
        );
        assert_eq!(
            hardware_label(&ctx, &provider("openrouter", HardwareLink::Unspecified)),
            "hosted"
        );
        assert_eq!(
            hardware_label(&ctx, &provider("llama-cpp", HardwareLink::Unspecified)),
            "⚠ not specified"
        );
        // openai-compatible usually fronts a server the user runs, so "not
        // specified" is a warning for it, as for any provider the user runs.
        assert_eq!(
            hardware_label(
                &ctx,
                &provider("openai-compatible", HardwareLink::Unspecified)
            ),
            "⚠ not specified"
        );
        assert_eq!(
            hardware_label(&ctx, &provider("openai-compatible", HardwareLink::Local)),
            "local"
        );
    }

    #[test]
    fn a_legacy_provider_is_marked_with_the_command_to_set_it() {
        let _home = crate::config::TestConfigHome::new();
        let mut config = crate::config::Config::new().unwrap();
        for (id, provider_type) in [("ollama", "ollama"), ("far", "ollama")] {
            config.providers.insert(
                id.to_string(),
                crate::config::ProviderConfig {
                    provider_id: id.to_string(),
                    provider_type: provider_type.to_string(),
                    config: serde_json::json!({}),
                    hardware: None,
                },
            );
        }
        let ctx = crate::AppContext::new(config, std::sync::Arc::new(CaptureUi::default()));

        ProviderCommands::list(&ctx).unwrap();

        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        let row = |id: &str| rows.iter().find(|r| r[0] == id).unwrap().clone();
        assert_eq!(row("ollama")[3], "⚠ local (not set)");
        assert_eq!(
            row("ollama")[4],
            "legacy config: run `granite-cli provider setup ollama` to set"
        );
        assert_eq!(
            row("far")[4],
            "legacy config: run `granite-cli provider setup ollama --id far` to set"
        );
    }
}
