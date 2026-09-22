// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use anyhow::Result;

// Local
use crate::capabilities::{CAPABILITY_REGISTRY, CapabilitySource};
use crate::config::validation::RefKind;
use crate::dependency::Configured;
use crate::launchers::LAUNCHER_REGISTRY;
use crate::utils::prompt_from_schema;

use_channel!("LNCHR");

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
        let notes = crate::commands::shared::remediation::dangling_notes(ctx, RefKind::Launcher);
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
                vec![
                    id.clone(),
                    cfg.launcher_type.clone(),
                    command,
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
            &format!("Configured Launchers ({} launchers)", rows.len()),
            &["ID", "TYPE", "COMMAND", "NOTES"],
            &rows,
        );
        Ok(())
    }

    pub fn info(ctx: &crate::AppContext, id: &str) -> Result<()> {
        let configured = ctx.config.get_launcher(id);

        let metadata = configured
            .and_then(|c| LAUNCHER_REGISTRY.get(&c.launcher_type))
            .or_else(|| LAUNCHER_REGISTRY.get(id));

        match metadata {
            Some(md) => {
                let mut type_fields: Vec<(&str, String)> = vec![
                    ("Name", md.name.clone()),
                    ("Description", md.description.clone()),
                    ("Default Command", md.default_command.clone()),
                ];

                let mut caps: Vec<_> = md
                    .supported_capabilities
                    .iter()
                    .map(|c| c.to_string())
                    .collect();
                if !caps.is_empty() {
                    caps.sort();
                    type_fields.push(("Supported Capabilities", caps.join(", ")));
                }

                if !md.tags.is_empty() {
                    type_fields.push(("Tags", md.tags.join(", ")));
                }

                ctx.ui.detail("Type Metadata", &type_fields);

                if let Some(cfg) = configured {
                    let mut instance_fields: Vec<(&str, String)> = Vec::new();

                    instance_fields.push(("Config: Type", cfg.launcher_type.clone()));

                    if !cfg.enabled_capabilities.is_empty() {
                        instance_fields
                            .push(("Enabled Capabilities", cfg.enabled_capabilities.join(", ")));
                    }

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
                        .info(&format!("Launcher '{id}' not found in registry."));

                    let available: Vec<_> = crate::launchers::LAUNCHER_REGISTRY
                        .entries()
                        .keys()
                        .map(|k| k.to_string())
                        .collect();
                    ctx.ui
                        .info(&format!("Available launchers: {}", available.join(", ")));

                    anyhow::bail!("Launcher not found");
                }
            }
        }
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
        alog_channel!(MessageLevel::Debug3, "Defaults: {:#?}", defaults);

        let config = prompt_from_schema(&*ctx.ui, &schema, &defaults)?;

        // Validate the binary now so the user gets immediate feedback.
        // validate_command respects command_path when set; falls back to PATH.
        let launcher = LAUNCHER_REGISTRY
            .construct(launcher_type, &instance_id, &config)
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

        ctx.config
            .insert_launcher(&instance_id, launcher_config)
            .map_err(|e| anyhow::anyhow!("Failed to save launcher config: {e}"))?;

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

    /// Configuration integrity check for `launch`: validates the launcher,
    /// the capabilities it enables, and what those resolve to, offering a fix
    /// for anything broken.
    ///
    /// Declining aborts the launch rather than skipping, because a capability
    /// that cannot bind would fail later during the launch itself. This runs
    /// before anything about the environment, so a configuration problem is
    /// reported before a missing binary is.
    pub async fn prelaunch(ctx: &mut crate::AppContext, launcher_id: &str) -> Result<()> {
        let outcome = crate::commands::shared::remediation::remediate(
            ctx,
            RefKind::Launcher,
            launcher_id,
            crate::commands::shared::remediation::OnDecline::Abort,
            true,
        )
        .await?;

        if outcome == crate::commands::shared::remediation::Outcome::Unresolved {
            anyhow::bail!(
                "Launch aborted: launcher '{launcher_id}' has a configuration problem \
                 that was not fixed."
            );
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

        // Anything pointing at it would be stranded by this removal.
        match crate::commands::shared::remediation::confirm_removal(
            ctx,
            RefKind::Launcher,
            launcher_id,
        )? {
            crate::commands::shared::remediation::Removal::Cancel => {
                ctx.ui.info(&format!("Keeping launcher '{launcher_id}'."));
                return Ok(());
            }
            crate::commands::shared::remediation::Removal::Proceed { with } => {
                crate::commands::shared::remediation::remove_all(ctx, &with)?;
            }
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

    // `enabled` accumulates the user's final selections across loop iterations.
    let mut enabled: Vec<String> = previously_enabled.to_vec();
    // Newly-configured capability IDs collected across loop iterations.
    let mut result: Vec<String> = vec![];
    // Whether the note about carried-through ids has been shown.
    let mut announced = false;

    loop {
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

        // Ids this launcher already enables that cannot be offered: the
        // instance is gone, its references do not resolve, or it does not
        // bind to anything this launcher supports. They are carried through
        // rather than dropped, since editing a launcher should not disable
        // what it already had. `launch` reports each one and offers a fix.
        let carried: Vec<String> = previously_enabled
            .iter()
            .filter(|id| !compatible_instances.contains(id))
            .cloned()
            .collect();
        if !carried.is_empty() && !announced {
            ctx.ui.warn(&format!(
                "Left enabled but not listed below: {}. Each one is missing or \
                 has a reference that does not resolve.",
                carried.join(", ")
            ));
            announced = true;
        }

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

        // Pre-check items that were previously enabled or added this session.
        let defaults: Vec<bool> = items
            .iter()
            .map(|item| item != CONFIGURE_NEW && enabled.contains(item))
            .collect();

        let selected = ctx
            .ui
            .multi_select("Select capabilities to enable", &items, &defaults)?;

        result.clear();
        let mut configure_new_chosen = false;
        for idx in selected {
            if items[idx] == CONFIGURE_NEW {
                configure_new_chosen = true;
            } else {
                result.push(items[idx].clone());
            }
        }

        if !configure_new_chosen {
            for id in carried {
                if !result.contains(&id) {
                    result.push(id);
                }
            }
            break;
        }

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

        // Pre-select the new capability on the next iteration.
        enabled.push(nickname.clone());
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

    fn capture(ctx: &crate::AppContext) -> &CaptureUi {
        (&*ctx.ui as &dyn std::any::Any)
            .downcast_ref::<CaptureUi>()
            .expect("test contexts are built with a CaptureUi")
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
    fn catalog_contains_claude_bob_pi_and_opencode() {
        let ctx = test_ctx();
        LauncherCommands::catalog(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert!(rows.iter().any(|r| r[0] == "claude"));
        assert!(rows.iter().any(|r| r[0] == "bob"));
        assert!(rows.iter().any(|r| r[0] == "pi"));
        assert!(rows.iter().any(|r| r[0] == "opencode"));
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

    // -- info -----------------------------------------------------------------

    #[test]
    fn info_unknown_launcher_returns_err() {
        let ctx = test_ctx();
        let result = LauncherCommands::info(&ctx, "does-not-exist");

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Launcher not found")
        );
    }

    #[test]
    fn info_catalog_launcher_renders_detail() {
        let ctx = test_ctx();
        let result = LauncherCommands::info(&ctx, "claude");

        assert!(result.is_ok());

        let details = details!(ctx);
        assert_eq!(details.len(), 1);

        let (id, fields) = &details[0];
        assert_eq!(id, "Type Metadata");
        assert!(fields.iter().any(|(k, _)| *k == "Name"));
        // Config fields should not be present for catalog-only lookups
        assert!(!fields.iter().any(|(k, _)| k.starts_with("Config")));
    }

    #[test]
    fn info_configured_launcher_renders_detail_with_config() {
        let mut ctx = ctx_with_launcher("my-claude", "claude");

        if let Some(cfg) = ctx.config.launchers.get_mut("my-claude") {
            cfg.enabled_capabilities = vec!["chat".to_string(), "plan".to_string()];
        }

        let result = LauncherCommands::info(&ctx, "my-claude");

        assert!(result.is_ok());

        let details = details!(ctx);
        assert_eq!(details.len(), 2);

        let (id1, fields1) = &details[0];
        assert_eq!(id1, "Type Metadata");
        assert!(fields1.iter().any(|(k, _)| *k == "Name"));

        let (id2, fields2) = &details[1];
        assert_eq!(id2, "my-claude");

        assert!(
            fields2
                .iter()
                .any(|(k, v)| *k == "Config: Type" && v == "claude")
        );
        assert!(
            fields2
                .iter()
                .any(|(k, v)| *k == "Enabled Capabilities" && v == "chat, plan")
        );
    }

    #[test]
    fn info_configured_unknown_type_renders_note() {
        let mut ctx = test_ctx();
        ctx.config.launchers.insert(
            "custom-launcher".to_string(),
            LauncherConfig {
                launcher_id: "custom-launcher".to_string(),
                launcher_type: "not-a-real-type".to_string(),
                ..LauncherConfig::default()
            },
        );

        let result = LauncherCommands::info(&ctx, "custom-launcher");

        assert!(result.is_ok());

        let details = details!(ctx);
        assert_eq!(details.len(), 1);

        let (id, fields) = &details[0];
        assert_eq!(id, "custom-launcher");
        assert!(
            fields
                .iter()
                .any(|(k, v)| *k == "Note" && v.contains("not found in the bundled registry"))
        );
    }

    // -- setup (type-aware clash detection) ------------------------------------

    #[tokio::test]
    async fn setup_warns_on_same_type_existing_instance() {
        let _home = crate::config::TestConfigHome::new();

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
    }

    #[tokio::test]
    async fn setup_unknown_type_returns_err() {
        let mut ctx = test_ctx();
        let result = LauncherCommands::setup(&mut ctx, "no-such-type", Some("test")).await;
        assert!(result.is_err());
    }

    /// A launcher that could not be saved must fail the setup, not report
    /// success over configuration that never reached disk.
    #[cfg(unix)]
    #[tokio::test]
    async fn setup_fails_when_config_cannot_be_saved() {
        let home = crate::config::TestConfigHome::new();
        let mut ctx = test_ctx();
        // The launcher validates its binary before saving, so point
        // command_path at a file that exists.
        let binary =
            std::path::Path::new(&std::env::var("GRANITE_CLI_HOME").unwrap()).join("fake-claude");
        std::fs::write(&binary, "").unwrap();
        capture(&ctx)
            .text_answers
            .borrow_mut()
            .push_back(binary.to_string_lossy().into_owned());

        home.make_unwritable();
        let result = LauncherCommands::setup(&mut ctx, "claude", Some("test-claude")).await;
        home.make_writable();

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to save launcher config")
        );
        let infos = infos!(ctx);
        assert!(
            !infos.iter().any(|m| m.contains("configured successfully")),
            "{infos:?}"
        );
    }

    // -- remove ----------------------------------------------------------------

    #[test]
    fn remove_existing_launcher_succeeds_and_disappears_from_list() {
        let _home = crate::config::TestConfigHome::new();
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
        let _home = crate::config::TestConfigHome::new();
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
        // The model needs a provider to bind, so a capability is only
        // constructible, and so only listed, with one configured.
        ctx.config.providers.insert(
            "ollama".to_string(),
            crate::config::ProviderConfig {
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({}),
            },
        );
        ctx.config.models.insert(
            model_id.to_string(),
            crate::config::ModelConfig {
                model_id: model_id.to_string(),
                model_type: model_id.to_string(),
                config: serde_json::json!({}),
                provider_id: "ollama".to_string(),
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

    #[tokio::test]
    async fn select_capabilities_carries_through_an_id_it_cannot_offer() {
        let mut ctx = test_ctx();
        add_capability(&mut ctx, "my-agent", "granite-3.1-8b-instruct");
        let launcher_def = claude_launcher_def();
        {
            let ui = capture_ui!(ctx);
            // Select "my-agent", the only instance on offer.
            ui.multi_select_answers.borrow_mut().push_back(vec![0]);
        }

        // `gone` is enabled but not configured, so it cannot be listed.
        let previously_enabled = vec!["my-agent".to_string(), "gone".to_string()];
        let result = select_capabilities(&mut ctx, &launcher_def, &previously_enabled)
            .await
            .unwrap();

        assert!(
            result.contains(&"gone".to_string()),
            "editing a launcher must not disable what it could not offer: {result:?}"
        );
        let ui = capture_ui!(ctx);
        let items = &ui.multi_select_prompts.borrow()[0].1;
        assert!(!items.contains(&"gone".to_string()), "{items:?}");
        assert!(
            ui.warns.borrow().iter().any(|w| w.contains("gone")),
            "the carried id is named: {:?}",
            ui.warns.borrow()
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
