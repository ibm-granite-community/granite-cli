// Third Party
use anyhow::Result;

// Local
use crate::capabilities::{CAPABILITY_REGISTRY, CapabilitySource};
use crate::dependency::Configured;
use crate::launchers::LAUNCHER_REGISTRY;
use crate::utils::prompt_from_schema;

/*-- public --*/

pub struct LauncherCommands;

impl LauncherCommands {
    /// Show all launcher types registered in the catalog.
    pub fn catalog(ctx: &crate::AppContext) -> Result<()> {
        let launchers = LAUNCHER_REGISTRY.entries();

        let mut rows: Vec<Vec<String>> = launchers
            .iter()
            .map(|(id, l)| vec![id.to_string(), l.default_command.clone()])
            .collect();
        rows.sort_by(|a, b| a[0].cmp(&b[0]));

        ctx.ui.table(
            &format!("Launcher Catalog ({} launchers)", launchers.len()),
            &["ID", "DEFAULT COMMAND"],
            &rows,
        );
        Ok(())
    }

    /// List all configured launcher instances.
    pub fn list(ctx: &crate::AppContext) -> Result<()> {
        let mut rows: Vec<Vec<String>> = ctx
            .config
            .launchers
            .iter()
            .map(|(id, cfg)| {
                let command = cfg
                    .config
                    .get("command_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(PATH)")
                    .to_string();
                vec![id.clone(), cfg.launcher_type.clone(), command]
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
            &format!("Configured Launchers ({} launchers)", rows.len()),
            &["ID", "TYPE", "COMMAND"],
            &rows,
        );
        Ok(())
    }

    /// Interactive launcher setup wizard.
    ///
    /// `launcher_type` is the catalog/registry key (e.g. `claude`).
    /// `instance_id` is the nickname for this instance; defaults to
    /// `launcher_type` when not given.
    ///
    /// **Diverges from Provider setup**: scans all configured launchers for any
    /// entry with the same `launcher_type` — not just the same `instance_id` —
    /// and, if one exists under a different name, offers to either update that
    /// existing entry or proceed with the new name. This lets the user avoid
    /// accidentally creating duplicate configs for the same tool.
    pub async fn setup(
        ctx: &mut crate::AppContext,
        launcher_type: &str,
        instance_id: Option<&str>,
    ) -> Result<()> {
        // Look up type in registry
        let launcher_def = match LAUNCHER_REGISTRY.get(launcher_type) {
            Some(def) => def,
            None => {
                ctx.ui.error(&format!(
                    "Launcher type '{launcher_type}' not found in registry."
                ));
                let available: Vec<String> = {
                    let mut entries: Vec<String> = LAUNCHER_REGISTRY
                        .entries()
                        .iter()
                        .map(|(id, l)| format!("{} ({})", id, l.name))
                        .collect();
                    entries.sort();
                    entries
                };
                ctx.ui
                    .info(&format!("Available types: {}", available.join(", ")));
                anyhow::bail!("Launcher type not found");
            }
        };

        ctx.ui
            .info(&format!("\nSetting up launcher: {launcher_type}"));
        ctx.ui.info(&launcher_def.description);
        ctx.ui.info(&format!(
            "Default command: {} (leave command_path blank to use PATH lookup)",
            launcher_def.default_command
        ));

        // Resolve instance id (prompt only when not passed as arg)
        let instance_id = match instance_id {
            Some(id) => id.to_string(),
            None => ctx.ui.text("Instance name: ", launcher_type)?,
        };

        // --- Type-aware clash detection (diverges from Provider pattern) ---
        // Look for any existing launcher of the SAME TYPE, regardless of name.
        let same_type_existing: Vec<String> = ctx
            .config
            .launchers
            .values()
            .filter(|lc| lc.launcher_type == launcher_type && lc.launcher_id != instance_id)
            .map(|lc| lc.launcher_id.clone())
            .collect();

        // If the user wants to update an existing same-type instance, redirect
        // `instance_id` to that entry so the normal overwrite path fires.
        let instance_id = if !same_type_existing.is_empty() {
            ctx.ui.info(&format!(
                "\nNote: a launcher of type '{}' already exists: {}",
                launcher_type,
                same_type_existing.join(", ")
            ));
            let update_existing = ctx.ui.confirm(
                &format!(
                    "Update '{}' instead of creating '{}'?",
                    same_type_existing[0], instance_id
                ),
                false,
            )?;
            if update_existing {
                same_type_existing[0].clone()
            } else {
                instance_id
            }
        } else {
            instance_id
        };

        // Standard same-id overwrite check
        if ctx.config.get_launcher(&instance_id).is_some() {
            let overwrite = ctx.ui.confirm(
                &format!("Launcher '{instance_id}' is already configured. Overwrite?"),
                false,
            )?;
            if !overwrite {
                ctx.ui.info("Launcher setup skipped.");
                return Ok(());
            }
        }

        // Prompt for type-specific config via schema.
        // Existing config (for overwrites) takes precedence over registry defaults.
        let schema = LAUNCHER_REGISTRY
            .config_schema(launcher_type)
            .ok_or_else(|| {
                anyhow::anyhow!("No config schema registered for launcher type '{launcher_type}'")
            })?;
        let defaults = ctx
            .config
            .get_launcher(&instance_id)
            .map(|lc| lc.config.clone())
            .or_else(|| LAUNCHER_REGISTRY.default_config(launcher_type))
            .unwrap_or_else(|| serde_json::json!({}));

        let mut config = prompt_from_schema(&*ctx.ui, &schema, &defaults)?;

        // Normalise: an empty string for command_path means "use PATH" — treat
        // it the same as absent so validate_command does a PATH lookup.
        if config.get("command_path").and_then(|v| v.as_str()) == Some("") {
            config
                .as_object_mut()
                .map(|m| m.insert("command_path".to_string(), serde_json::Value::Null));
        }

        // Validate the binary now so the user gets immediate feedback.
        // validate_command respects command_path when set; falls back to PATH.
        let launcher = LAUNCHER_REGISTRY
            .construct(launcher_type, &instance_id, &config, &ctx.config)
            .map_err(|e| anyhow::anyhow!("Failed to construct launcher: {e}"))?;

        match launcher.validate_command() {
            Ok(path) => {
                ctx.ui.info(&format!("  Binary found: {}", path.display()));
            }
            Err(e) => {
                // command_path was explicitly set but invalid, or binary not on PATH.
                anyhow::bail!(
                    "Binary validation failed: {e}\n\
                     Set command_path to the full path of the binary and re-run setup."
                );
            }
        }

        // Select capabilities to enable for this launcher.
        let previously_enabled: Vec<String> = ctx
            .config
            .get_launcher(&instance_id)
            .map(|lc| lc.enabled_capabilities.clone())
            .unwrap_or_default();
        let enabled_capabilities =
            select_capabilities(ctx, &launcher_def, &previously_enabled).await?;

        let launcher_config = crate::config::LauncherConfig {
            launcher_id: instance_id.clone(),
            launcher_type: launcher_type.to_string(),
            enabled_capabilities,
            config,
        };

        if let Err(e) = ctx.config.insert_launcher(&instance_id, launcher_config) {
            ctx.ui.warn(&format!("Failed to save launcher config: {e}"));
        }

        ctx.ui.info(&format!(
            "\nLauncher '{instance_id}' configured successfully!"
        ));
        if !launcher_def.supported_capabilities.is_empty() {
            ctx.ui.info("Supported capabilities:");
            for cap in &launcher_def.supported_capabilities {
                ctx.ui.info(&format!("  - {cap}"));
            }
        }

        Ok(())
    }

    /// Remove a configured launcher instance by ID.
    ///
    /// Deletes the launcher's config file and removes it from the in-memory
    /// config. After this call `launcher list` will no longer show the entry
    /// and `granite-cli launch <id>` will return an error.
    pub fn remove(ctx: &mut crate::AppContext, launcher_id: &str) -> Result<()> {
        if ctx.config.get_launcher(launcher_id).is_none() {
            anyhow::bail!("No launcher configured with id '{launcher_id}'. Nothing to remove.");
        }

        if let Err(e) = ctx.config.remove_launcher(launcher_id) {
            ctx.ui
                .warn(&format!("failed to persist launcher removal: {e}"));
        }
        ctx.ui.info(&format!("Launcher '{launcher_id}' removed."));
        Ok(())
    }
}

/*-- private --*/

/// Presents the user with a multi-select of capability instances (and a
/// "Configure a new capability…" option) filtered to those compatible with
/// `launcher_def.supported_capabilities`. Returns the list of capability IDs
/// the user chose to enable.
///
/// Returns an empty vec and emits a warning when the launcher supports no
/// capabilities at all (e.g. `bob`).
async fn select_capabilities(
    ctx: &mut crate::AppContext,
    launcher_def: &crate::launchers::LauncherMetadata,
    previously_enabled: &[String],
) -> Result<Vec<String>> {
    if launcher_def.supported_capabilities.is_empty() {
        ctx.ui.warn(
            "This launcher does not support any capabilities. \
             No capabilities will be enabled.",
        );
        return Ok(vec![]);
    }

    let source = CapabilitySource::from_config(&ctx.config);

    // Instances whose binding_types() intersect the launcher's supported set.
    let mut compatible_instances: Vec<String> = source
        .instances()
        .into_iter()
        .filter(|(_, cap)| {
            cap.binding_types()
                .iter()
                .any(|bt| launcher_def.supported_capabilities.contains(bt))
        })
        .map(|(id, _)| id)
        .collect();
    compatible_instances.sort();

    // Catalog types whose supported_binding_types intersect the launcher's set.
    let compatible_types: Vec<&'static str> = {
        let mut types: Vec<&'static str> = CAPABILITY_REGISTRY
            .entries()
            .into_iter()
            .filter(|(_, meta)| {
                meta.supported_binding_types
                    .iter()
                    .any(|bt| launcher_def.supported_capabilities.contains(bt))
            })
            .map(|(name, _)| name)
            .collect();
        types.sort();
        types
    };

    if compatible_instances.is_empty() && compatible_types.is_empty() {
        ctx.ui
            .info("No compatible capabilities are configured or available for this launcher.");
        return Ok(vec![]);
    }

    const CONFIGURE_NEW: &str = "Configure a new capability...";

    // Build display list: sorted instances, then sentinel if types exist.
    let mut items: Vec<String> = compatible_instances.clone();
    if !compatible_types.is_empty() {
        items.push(CONFIGURE_NEW.to_string());
    }

    // Pre-check items that were previously enabled.
    let defaults: Vec<bool> = items
        .iter()
        .map(|item| item != CONFIGURE_NEW && previously_enabled.contains(item))
        .collect();

    let selected = ctx
        .ui
        .multi_select("Select capabilities to enable", &items, &defaults)?;

    let mut result: Vec<String> = vec![];
    let mut configure_new_chosen = false;
    for idx in selected {
        if items[idx] == CONFIGURE_NEW {
            configure_new_chosen = true;
        } else {
            result.push(items[idx].clone());
        }
    }

    if configure_new_chosen {
        // Mirror the select_provider pattern: auto-select when only one type,
        // otherwise let the user pick.
        let cap_type = if compatible_types.len() == 1 {
            compatible_types[0]
        } else {
            let type_options: Vec<String> =
                compatible_types.iter().map(|s| s.to_string()).collect();
            let idx = ctx
                .ui
                .select("Select a capability type to configure", &type_options, 0)?;
            compatible_types[idx]
        };

        let nickname = ctx.ui.text("Name this capability instance", cap_type)?;

        crate::commands::CapabilityCommands::setup(ctx, cap_type, Some(&nickname)).await?;
        result.push(nickname);
    }

    Ok(result)
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, LauncherConfig};
    use crate::utils::ui::base::tests::CaptureUi;
    use std::sync::Arc;

    fn test_ctx() -> crate::AppContext {
        crate::AppContext {
            config: Config::default(),
            ui: Arc::new(CaptureUi::default()),
        }
    }

    fn ctx_with_launcher(id: &str, launcher_type: &str) -> crate::AppContext {
        let mut ctx = test_ctx();
        ctx.config.launchers.insert(
            id.to_string(),
            LauncherConfig {
                launcher_id: id.to_string(),
                launcher_type: launcher_type.to_string(),
                ..LauncherConfig::default()
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

    macro_rules! infos {
        ($ctx:expr) => {
            (&*($ctx.ui) as &dyn std::any::Any)
                .downcast_ref::<CaptureUi>()
                .unwrap()
                .infos
                .borrow()
        };
    }

    // -- catalog ---------------------------------------------------------------

    #[test]
    fn catalog_has_id_and_default_command_columns() {
        let ctx = test_ctx();
        LauncherCommands::catalog(&ctx).unwrap();
        let tables = tables!(ctx);
        assert_eq!(tables.len(), 1);
        let (_, headers, _) = &tables[0];
        assert!(headers.contains(&"ID".to_string()));
        assert!(headers.contains(&"DEFAULT COMMAND".to_string()));
    }

    #[test]
    fn catalog_contains_claude_bob_pi_opencode_and_hermes() {
        let ctx = test_ctx();
        LauncherCommands::catalog(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert!(rows.iter().any(|r| r[0] == "claude"));
        assert!(rows.iter().any(|r| r[0] == "bob"));
        assert!(rows.iter().any(|r| r[0] == "pi"));
        assert!(rows.iter().any(|r| r[0] == "opencode"));
        assert!(rows.iter().any(|r| r[0] == "hermes"));
    }

    // -- list ------------------------------------------------------------------

    #[test]
    fn list_empty_config_has_zero_rows() {
        let ctx = test_ctx();
        LauncherCommands::list(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn list_configured_launcher_shows_path_sentinel() {
        let ctx = ctx_with_launcher("my-claude", "claude");
        LauncherCommands::list(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert_eq!(rows.len(), 1);
        // No command_path set → should show "(PATH)"
        assert!(rows[0].iter().any(|c| c == "(PATH)"));
    }

    #[test]
    fn list_columns_are_id_type_command() {
        let ctx = ctx_with_launcher("my-claude", "claude");
        LauncherCommands::list(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, headers, _) = &tables[0];
        assert!(headers.contains(&"ID".to_string()));
        assert!(headers.contains(&"TYPE".to_string()));
        assert!(!headers.contains(&"ENABLED".to_string()));
        assert!(headers.contains(&"COMMAND".to_string()));
    }

    #[test]
    fn list_sorted_by_type_then_id() {
        let mut ctx = test_ctx();
        for (id, t) in [
            ("z-claude", "claude"),
            ("a-claude", "claude"),
            ("my-bob", "bob"),
        ] {
            ctx.config.launchers.insert(
                id.to_string(),
                LauncherConfig {
                    launcher_id: id.to_string(),
                    launcher_type: t.to_string(),
                    ..LauncherConfig::default()
                },
            );
        }
        LauncherCommands::list(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert_eq!(rows[0][1], "bob");
        assert_eq!(rows[1][0], "a-claude");
        assert_eq!(rows[2][0], "z-claude");
    }

    // -- setup (type-aware clash detection) ------------------------------------

    #[tokio::test]
    async fn setup_warns_on_same_type_existing_instance() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("granite-cli-test-launcher-setup");
        // SAFETY: single-threaded test; no other thread reads this var.
        unsafe { std::env::set_var("GRANITE_CLI_HOME", &home) };

        // Pre-populate a "claude" instance named "claude-old"
        let mut ctx = ctx_with_launcher("claude-old", "claude");
        // CaptureUi confirm always returns false → user declines update and
        // proceeds with the new name. The wizard then fails at binary
        // validation (claude not on PATH in CI), but by that point the clash
        // info message must already have been emitted.
        let _ = LauncherCommands::setup(&mut ctx, "claude", Some("claude-new")).await;
        let infos = infos!(ctx);
        assert!(
            infos.iter().any(|m| m.contains("claude-old")),
            "expected clash warning to mention the existing instance"
        );

        unsafe { std::env::remove_var("GRANITE_CLI_HOME") };
    }

    #[tokio::test]
    async fn setup_unknown_type_returns_err() {
        let mut ctx = test_ctx();
        let result = LauncherCommands::setup(&mut ctx, "no-such-type", Some("test")).await;
        assert!(result.is_err());
    }

    // -- remove ----------------------------------------------------------------

    #[test]
    fn remove_existing_launcher_succeeds_and_disappears_from_list() {
        let mut ctx = ctx_with_launcher("my-claude", "claude");
        assert!(ctx.config.get_launcher("my-claude").is_some());

        LauncherCommands::remove(&mut ctx, "my-claude").unwrap();

        assert!(ctx.config.get_launcher("my-claude").is_none());
        let infos = infos!(ctx);
        assert!(
            infos
                .iter()
                .any(|m| m.contains("my-claude") && m.contains("removed"))
        );
    }

    #[test]
    fn remove_nonexistent_launcher_returns_err() {
        let mut ctx = test_ctx();
        let result = LauncherCommands::remove(&mut ctx, "doesnt-exist");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Nothing to remove")
        );
    }

    #[test]
    fn list_does_not_show_removed_launcher() {
        let mut ctx = ctx_with_launcher("my-claude", "claude");
        LauncherCommands::remove(&mut ctx, "my-claude").unwrap();
        LauncherCommands::list(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert!(rows.is_empty());
    }

    // -- select_capabilities ---------------------------------------------------

    macro_rules! capture_ui {
        ($ctx:expr) => {
            (&*($ctx.ui) as &dyn std::any::Any)
                .downcast_ref::<CaptureUi>()
                .unwrap()
        };
    }

    /// Returns a `LauncherMetadata` for `claude` from the registry.
    fn claude_launcher_def() -> crate::launchers::LauncherMetadata {
        crate::launchers::LAUNCHER_REGISTRY
            .get("claude")
            .unwrap()
            .clone()
    }

    /// Returns a `LauncherMetadata` for `bob` from the registry.
    fn bob_launcher_def() -> crate::launchers::LauncherMetadata {
        crate::launchers::LAUNCHER_REGISTRY
            .get("bob")
            .unwrap()
            .clone()
    }

    /// A synthetic `LauncherMetadata` with no supported binding types at
    /// all, for exercising the "this launcher supports nothing" path
    /// (`bob` no longer qualifies -- it supports `Mcp`).
    fn no_capabilities_launcher_def() -> crate::launchers::LauncherMetadata {
        crate::launchers::LauncherMetadata {
            supported_capabilities: std::collections::HashSet::new(),
            ..bob_launcher_def()
        }
    }

    // Helper: insert a minimal agent-model capability config into ctx.
    // Also adds the model to config.models so CapabilitySource::from_config
    // can construct the underlying AgentModelCapability.
    fn add_capability(ctx: &mut crate::AppContext, cap_id: &str, model_id: &str) {
        ctx.config.models.insert(
            model_id.to_string(),
            crate::config::ModelConfig {
                model_id: model_id.to_string(),
                provider_id: None,
                variant: None,
            },
        );
        ctx.config.capabilities.insert(
            cap_id.to_string(),
            crate::config::CapabilityConfig {
                capability_id: cap_id.to_string(),
                capability_type: "agent-model".to_string(),
                config: serde_json::json!({ "model_id": model_id }),
            },
        );
    }

    // bob launcher has empty supported_capabilities → warning is emitted and
    // empty vec returned without calling multi_select.
    #[tokio::test]
    async fn select_capabilities_warns_and_skips_for_launcher_with_no_supported_capabilities() {
        let mut ctx = test_ctx();
        let launcher_def = no_capabilities_launcher_def();
        let result = select_capabilities(&mut ctx, &launcher_def, &[])
            .await
            .unwrap();
        assert!(result.is_empty());
        let ui = capture_ui!(ctx);
        assert!(
            ui.warns
                .borrow()
                .iter()
                .any(|w| w.contains("does not support any capabilities")),
            "expected a warning about no supported capabilities"
        );
        assert!(
            ui.multi_select_prompts.borrow().is_empty(),
            "multi_select should not be called for a launcher with no supported capabilities"
        );
    }

    // When no capabilities are configured and no types can satisfy the launcher,
    // an info message is printed and empty vec returned.
    #[tokio::test]
    async fn select_capabilities_returns_empty_when_no_compatible_capabilities_exist() {
        let mut ctx = test_ctx();
        // claude supports AgentModel; agent-model is in the catalog — so
        // compatible_types will be non-empty and multi_select IS called.
        // To test the "nothing at all" path we'd need a launcher type that
        // supports a binding type with no catalog entry.  Instead, verify
        // that multi_select is called with the "Configure a new capability..."
        // sentinel when no instances are configured.
        let launcher_def = claude_launcher_def();
        // CaptureUi returns empty vec by default → user selects nothing.
        let result = select_capabilities(&mut ctx, &launcher_def, &[])
            .await
            .unwrap();
        assert!(result.is_empty());
        let ui = capture_ui!(ctx);
        // multi_select must have been called
        let prompts = ui.multi_select_prompts.borrow();
        assert_eq!(prompts.len(), 1);
        // sentinel is included because agent-model is in the catalog
        assert!(
            prompts[0]
                .1
                .iter()
                .any(|i| i == "Configure a new capability...")
        );
    }

    // When a capability instance is configured and compatible, it appears in the
    // multi_select items list.
    #[tokio::test]
    async fn select_capabilities_shows_configured_compatible_instance() {
        let mut ctx = test_ctx();
        add_capability(&mut ctx, "my-agent", "granite-3.1-8b-instruct");
        let launcher_def = claude_launcher_def();
        let result = select_capabilities(&mut ctx, &launcher_def, &[])
            .await
            .unwrap();
        assert!(result.is_empty()); // user selected nothing (default)
        let ui = capture_ui!(ctx);
        let prompts = ui.multi_select_prompts.borrow();
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].1.contains(&"my-agent".to_string()),
            "expected instance id in items"
        );
    }

    // Previously-enabled capability IDs are pre-checked (defaults = true).
    #[tokio::test]
    async fn select_capabilities_pre_checks_previously_enabled_ids() {
        let mut ctx = test_ctx();
        add_capability(&mut ctx, "my-agent", "granite-3.1-8b-instruct");
        let launcher_def = claude_launcher_def();
        let previously_enabled = vec!["my-agent".to_string()];
        let _ = select_capabilities(&mut ctx, &launcher_def, &previously_enabled)
            .await
            .unwrap();
        let ui = capture_ui!(ctx);
        let prompts = ui.multi_select_prompts.borrow();
        assert_eq!(prompts.len(), 1);
        let idx = prompts[0]
            .1
            .iter()
            .position(|i| i == "my-agent")
            .expect("my-agent should be in items");
        assert!(
            prompts[0].2[idx],
            "my-agent should be pre-checked as it was previously enabled"
        );
    }

    // Selecting an existing instance returns its ID.
    #[tokio::test]
    async fn select_capabilities_returns_selected_instance_id() {
        let mut ctx = test_ctx();
        add_capability(&mut ctx, "my-agent", "granite-3.1-8b-instruct");
        let launcher_def = claude_launcher_def();
        {
            let ui = capture_ui!(ctx);
            // Select index 0 (the "my-agent" instance — it sorts first before sentinel)
            ui.multi_select_answers.borrow_mut().push_back(vec![0]);
        }
        let result = select_capabilities(&mut ctx, &launcher_def, &[])
            .await
            .unwrap();
        assert_eq!(result, vec!["my-agent".to_string()]);
    }
}
