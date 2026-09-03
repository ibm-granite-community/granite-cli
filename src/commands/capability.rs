// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use anyhow::Result;

// Local
use super::{ModelCommands, ProviderCommands};
use crate::capabilities::{CAPABILITY_REGISTRY, Dependency, ModelRequirement, ProviderRequirement};
use crate::dependency::{self, Configured};
use crate::utils::prompt_from_schema;

pub struct CapabilityCommands;

use_channel!("CAPBL");

impl CapabilityCommands {
    pub fn catalog(ctx: &crate::AppContext) -> Result<()> {
        let capabilities = CAPABILITY_REGISTRY.entries();

        let mut rows: Vec<Vec<String>> = capabilities
            .iter()
            .map(|(cap_id, cap)| {
                let deps: Vec<_> = cap.dependencies.iter().map(|d| d.to_string()).collect();
                let deps_str = if deps.is_empty() {
                    "None".to_string()
                } else {
                    deps.join(", ")
                };
                vec![cap_id.to_string(), cap.name.clone(), deps_str]
            })
            .collect();
        rows.sort_by(|a, b| a[0].cmp(&b[0]));

        ctx.ui.table(
            &format!("Capability Catalog ({} capabilities)", capabilities.len()),
            &["ID", "NAME", "DEPENDENCIES"],
            &rows,
        );
        Ok(())
    }

    pub fn list(ctx: &crate::AppContext) -> Result<()> {
        let mut rows: Vec<Vec<String>> = ctx
            .config
            .capabilities
            .iter()
            .map(|(id, cfg)| vec![id.clone(), cfg.capability_type.clone()])
            .collect();
        rows.sort_by(|a, b| {
            let type_cmp = a[1].cmp(&b[1]);
            if type_cmp != std::cmp::Ordering::Equal {
                return type_cmp;
            }
            a[0].cmp(&b[0])
        });

        ctx.ui.table(
            &format!("Configured Capabilities ({} capabilities)", rows.len()),
            &["ID", "TYPE"],
            &rows,
        );
        Ok(())
    }

    pub fn info(ctx: &crate::AppContext, capability_id: &str) -> Result<()> {
        let configured = ctx.config.get_capability(capability_id);

        let catalog_entry = configured
            .and_then(|c| CAPABILITY_REGISTRY.get(&c.capability_type))
            .or_else(|| CAPABILITY_REGISTRY.get(capability_id));

        match catalog_entry {
            Some(cap) => {
                let mut fields: Vec<(&str, String)> = vec![
                    ("Name", cap.name.clone()),
                    ("Description", cap.description.clone()),
                ];

                if !cap.tags.is_empty() {
                    fields.push(("Tags", cap.tags.join(", ")));
                }

                if let Some(configured) = configured {
                    fields.push(("Type", configured.capability_type.clone()));
                    if let Some(obj) = configured.config.as_object() {
                        for (k, v) in obj {
                            fields.push(("Config", format!("{k} = {v}")));
                        }
                    }
                }

                ctx.ui.detail(capability_id, &fields);
                Ok(())
            }
            None => {
                if configured.is_some() {
                    let fields: Vec<(&str, String)> = vec![(
                        "Note",
                        "Configured but its type is not found in the bundled registry.".to_string(),
                    )];
                    ctx.ui.detail(capability_id, &fields);
                    Ok(())
                } else {
                    ctx.ui.error(&format!(
                        "Capability '{capability_id}' not found in registry."
                    ));
                    anyhow::bail!("Capability not found");
                }
            }
        }
    }

    /// Interactive capability setup wizard.
    ///
    /// `capability_type` is the catalog/registry key (e.g. `agent-model`).
    /// `instance_id` is the nickname for this instance; defaults to
    /// `capability_type` when not given.
    pub async fn setup(
        ctx: &mut crate::AppContext,
        capability_type: &str,
        instance_id: Option<&str>,
    ) -> Result<()> {
        let cap_def = match CAPABILITY_REGISTRY.get(capability_type) {
            Some(def) => def,
            None => {
                ctx.ui.error(&format!(
                    "Capability type '{capability_type}' not found in registry."
                ));
                let available: Vec<String> = {
                    let mut entries: Vec<String> = CAPABILITY_REGISTRY
                        .entries()
                        .iter()
                        .map(|(id, c)| format!("{} ({})", id, c.name))
                        .collect();
                    entries.sort();
                    entries
                };
                ctx.ui
                    .info(&format!("Available types: {}", available.join(", ")));
                anyhow::bail!("Capability type not found");
            }
        };

        ctx.ui
            .info(&format!("\nSetting up capability: {capability_type}"));
        ctx.ui.info(&cap_def.description);

        let instance_id = match instance_id {
            Some(id) => id.to_string(),
            None => ctx.ui.text("Instance name: ", capability_type)?,
        };

        let existing_config = ctx.config.get_capability(&instance_id);
        if existing_config.is_some() {
            let overwrite = ctx.ui.confirm(
                &format!("Capability '{instance_id}' is already configured. Overwrite?"),
                false,
            )?;
            if !overwrite {
                ctx.ui.info("Capability setup skipped.");
                return Ok(());
            }
        }

        let mut schema = CAPABILITY_REGISTRY
            .config_schema(capability_type)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No config schema registered for capability type '{capability_type}'"
                )
            })?;
        let defaults = existing_config
            .map(|c| c.config.clone())
            .or_else(|| CAPABILITY_REGISTRY.default_config(capability_type))
            .unwrap_or_else(|| serde_json::json!({}));

        // Phase A: prompt for everything except dependency-resolved fields --
        // those are picked from configured instances below, never free-typed.
        let dependency_keys: std::collections::HashSet<&str> = cap_def
            .dependencies
            .iter()
            .filter_map(|d| match d {
                Dependency::Model { config_key, .. } | Dependency::Provider { config_key, .. } => {
                    Some(config_key.as_str())
                }
                Dependency::ExternalTool { .. } => None,
            })
            .collect();
        if let Some(serde_json::Value::Object(props)) = schema.get_mut("properties") {
            props.retain(|k, _| !dependency_keys.contains(k.as_str()));
        }
        if let Some(serde_json::Value::Array(req)) = schema.get_mut("required") {
            req.retain(|v| !v.as_str().is_some_and(|s| dependency_keys.contains(s)));
        }
        let mut config = prompt_from_schema(&*ctx.ui, &schema, &defaults)?;

        // Phase B: resolve the capability's dependencies (model/provider)
        // against currently configured models/providers. Use the capability
        // definition's metadata rather than a preview instance, since some
        // dependency fields (like model_id) may not be set yet.
        for dep in &cap_def.dependencies {
            match dep {
                Dependency::Model {
                    config_key,
                    requirement,
                    required,
                    ..
                } => {
                    if let Some(id) =
                        Self::resolve_model_dependency(ctx, requirement, *required).await?
                    {
                        config
                            // NOTE: Safe since config MUST be an object when registered
                            .as_object_mut()
                            .unwrap()
                            .insert(config_key.clone(), serde_json::Value::String(id));
                    }
                }
                Dependency::Provider {
                    config_key,
                    requirement,
                    required,
                    ..
                } => {
                    if let Some(id) =
                        Self::resolve_provider_dependency(ctx, requirement, *required).await?
                    {
                        config
                            // NOTE: Safe since config MUST be an object when registered
                            .as_object_mut()
                            .unwrap()
                            .insert(config_key.clone(), serde_json::Value::String(id));
                    }
                }
                Dependency::ExternalTool {
                    requirement,
                    required,
                } => {
                    if *required && !requirement.is_satisfied() {
                        anyhow::bail!(
                            "Required external command '{}' is not available.",
                            requirement.command
                        );
                    }
                }
            }
        }

        let capability_config = crate::config::CapabilityConfig {
            capability_id: instance_id.clone(),
            capability_type: capability_type.to_string(),
            config,
        };

        if let Err(e) = ctx
            .config
            .insert_capability(&instance_id, capability_config)
        {
            ctx.ui
                .warn(&format!("failed to save capability config: {e}"));
        }

        ctx.ui.info(&format!(
            "\nCapability '{instance_id}' configured successfully!"
        ));

        Ok(())
    }

    /// Resolve a capability's model dependency against currently configured
    /// models, narrowed to those whose attached provider also supports every
    /// function the requirement asks for. Always offers a "configure a new
    /// model" option alongside any usable existing instances -- a freshly
    /// configured model is re-checked against the same narrowing before
    /// being accepted, since the user could configure one whose provider
    /// doesn't actually satisfy the requirement. Returns the chosen model
    /// id, or `None` if the dependency isn't required and nothing (existing
    /// or configurable) satisfies it.
    async fn resolve_model_dependency(
        ctx: &mut crate::AppContext,
        requirement: &ModelRequirement,
        required: bool,
    ) -> Result<Option<String>> {
        alog_channel!(
            MessageLevel::Debug2,
            "Resolving requirement: {:?}",
            requirement
        );
        let (usable, configurable_types) = Self::model_candidates(ctx, requirement);
        alog_channel!(
            MessageLevel::Debug2,
            "Usable: {:?}, Configurable Types: {:?}",
            usable,
            configurable_types
        );
        if usable.is_empty() && configurable_types.is_empty() {
            if required {
                anyhow::bail!(
                    "No configured model satisfies this capability's requirements yet, and none can be configured. Configure a compatible model and provider first."
                );
            }
            return Ok(None);
        }

        const CONFIGURE_NEW: &str = "Configure a new model...";
        let mut options = usable;
        let mut configure_new_idx: Option<usize> = None;
        if !configurable_types.is_empty() {
            configure_new_idx = Some(options.len());
            options.push(CONFIGURE_NEW.to_string());
        }

        let choice_idx = if options.len() == 1 {
            0
        } else {
            ctx.ui
                .select("Select a model for this capability:", &options, 0)?
        };
        let choice = options[choice_idx].clone();
        if configure_new_idx.is_none_or(|v| v != choice_idx) {
            return Ok(Some(choice));
        }

        let model_type = if configurable_types.len() == 1 {
            configurable_types[0]
        } else {
            let type_options: Vec<String> =
                configurable_types.iter().map(|s| s.to_string()).collect();
            let index = ctx
                .ui
                .select("Select a model type to configure:", &type_options, 0)?;
            configurable_types[index]
        };

        ModelCommands::setup(ctx, model_type, None).await?;

        alog_channel!(
            MessageLevel::Debug3,
            "Getting model candidates for requirements: {:#?}",
            requirement
        );
        let (usable_after, _) = Self::model_candidates(ctx, requirement);
        let new_usable: Vec<_> = usable_after
            .iter()
            .filter(|x| !options.contains(x))
            .collect();
        if new_usable.len() == 1 {
            return Ok(Some(new_usable[0].to_string()));
        }
        if required {
            anyhow::bail!(
                "The newly configured model '{model_type}' does not satisfy this capability's requirements (its provider may not support what's needed). Configure a compatible model/provider combination and try again."
            );
        }
        ctx.ui.warn(&format!(
            "The newly configured model '{model_type}' does not satisfy this capability's requirements; skipping."
        ));
        Ok(None)
    }

    /// Existing configured models that satisfy `requirement` (narrowed to
    /// those whose attached provider also supports every requested
    /// function), and catalog model types that could satisfy it if
    /// configured. Both lists are sorted for deterministic display/tests.
    fn model_candidates(
        ctx: &crate::AppContext,
        requirement: &ModelRequirement,
    ) -> (Vec<String>, Vec<&'static str>) {
        let source = crate::models::ModelSource::from_config(&ctx.config);
        let resolution = dependency::resolve(requirement, &source);
        let instances = source.instances();
        let mut usable: Vec<String> = resolution
            .existing_instances
            .into_iter()
            .filter(|id| {
                instances
                    .iter()
                    .find(|(i, _)| i == id)
                    .is_some_and(|(_, model)| {
                        let model_functions = model.supported_functions();
                        alog_channel!(
                            MessageLevel::Debug4,
                            "Checking model candidate {:#?} with supported functions {:#?}",
                            model.instance_id(),
                            model_functions
                        );
                        let model_ok = requirement
                            .supported_functions
                            .iter()
                            .all(|f| model_functions.contains(f));
                        match model.provider() {
                            Ok(p) => {
                                model_ok
                                    && requirement
                                        .supported_functions
                                        .iter()
                                        .all(|f| p.supports_function(f))
                            }
                            Err(_) => false,
                        }
                    })
            })
            .collect();
        usable.sort();
        let mut configurable_types = resolution.configurable_types;
        configurable_types.sort();
        (usable, configurable_types)
    }

    /// Resolve a capability's provider dependency against currently
    /// configured providers. Always offers a "configure a new provider"
    /// option alongside any satisfying existing instances -- a freshly
    /// configured provider is re-checked before being accepted, since the
    /// user could configure one that doesn't actually satisfy the
    /// requirement. Returns the chosen provider id, or `None` if the
    /// dependency isn't required and nothing (existing or configurable)
    /// satisfies it.
    async fn resolve_provider_dependency(
        ctx: &mut crate::AppContext,
        requirement: &ProviderRequirement,
        required: bool,
    ) -> Result<Option<String>> {
        let (existing, configurable_types) = Self::provider_candidates(ctx, requirement);
        if existing.is_empty() && configurable_types.is_empty() {
            if required {
                anyhow::bail!(
                    "No configured provider satisfies this capability's requirements yet, and none can be configured. Configure a compatible provider first."
                );
            }
            return Ok(None);
        }

        const CONFIGURE_NEW: &str = "Configure a new provider...";
        let mut options = existing;
        let mut configure_new_idx: Option<usize> = None;
        if !configurable_types.is_empty() {
            configure_new_idx = Some(options.len());
            options.push(CONFIGURE_NEW.to_string());
        }

        let choice_idx = if options.len() == 1 {
            0
        } else {
            ctx.ui
                .select("Select a provider for this capability:", &options, 0)?
        };
        let choice = options[choice_idx].clone();
        if configure_new_idx.is_none_or(|v| v != choice_idx) {
            return Ok(Some(choice));
        }

        let provider_type = if configurable_types.len() == 1 {
            configurable_types[0]
        } else {
            let type_options: Vec<String> =
                configurable_types.iter().map(|s| s.to_string()).collect();
            let index = ctx
                .ui
                .select("Select a provider type to configure:", &type_options, 0)?;
            configurable_types[index]
        };

        let nickname = ctx.ui.text("Name this provider instance", provider_type)?;
        ProviderCommands::setup(ctx, provider_type, Some(&nickname)).await?;

        let (existing_after, _) = Self::provider_candidates(ctx, requirement);
        if existing_after.contains(&nickname) {
            return Ok(Some(nickname));
        }
        if required {
            anyhow::bail!(
                "The newly configured provider '{nickname}' does not satisfy this capability's requirements. Configure a different provider and try again."
            );
        }
        ctx.ui.warn(&format!(
            "The newly configured provider '{nickname}' does not satisfy this capability's requirements; skipping."
        ));
        Ok(None)
    }

    /// Existing configured providers that satisfy `requirement`, and
    /// catalog provider types that could satisfy it if configured. Both
    /// lists are sorted for deterministic display/tests.
    fn provider_candidates(
        ctx: &crate::AppContext,
        requirement: &ProviderRequirement,
    ) -> (Vec<String>, Vec<&'static str>) {
        let source = crate::providers::ProviderSource::from_config(&ctx.config);
        let resolution = dependency::resolve(requirement, &source);
        let mut existing = resolution.existing_instances;
        existing.sort();
        let mut configurable_types = resolution.configurable_types;
        configurable_types.sort();
        (existing, configurable_types)
    }

    /// Remove a configured capability instance by ID.
    ///
    /// Deletes the capability's config file and removes it from the
    /// in-memory config. After this call `capability list` will no longer
    /// show the entry.
    pub fn remove(ctx: &mut crate::AppContext, capability_id: &str) -> Result<()> {
        if ctx.config.get_capability(capability_id).is_none() {
            anyhow::bail!("No capability configured with id '{capability_id}'. Nothing to remove.");
        }

        if let Err(e) = ctx.config.remove_capability(capability_id) {
            ctx.ui
                .warn(&format!("failed to persist capability removal: {e}"));
        }
        ctx.ui
            .info(&format!("Capability '{capability_id}' removed."));
        Ok(())
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CapabilityConfig, Config};
    use crate::utils::ui::base::tests::CaptureUi;
    use std::sync::Arc;

    fn test_ctx() -> crate::AppContext {
        crate::AppContext {
            config: Config::default(),
            ui: Arc::new(CaptureUi::default()),
        }
    }

    fn ctx_with_capability(id: &str, capability_type: &str) -> crate::AppContext {
        let mut ctx = test_ctx();
        ctx.config.capabilities.insert(
            id.to_string(),
            CapabilityConfig {
                capability_id: id.to_string(),
                capability_type: capability_type.to_string(),
                config: serde_json::json!({}),
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

    // -- catalog --------------------------------------------------------------

    #[test]
    fn catalog_table_has_id_name_dependencies_columns() {
        let ctx = test_ctx();
        CapabilityCommands::catalog(&ctx).unwrap();
        let tables = tables!(ctx);
        assert_eq!(tables.len(), 1);
        let (_, headers, _) = &tables[0];
        assert!(headers.contains(&"ID".to_string()));
        assert!(headers.contains(&"NAME".to_string()));
        assert!(headers.contains(&"DEPENDENCIES".to_string()));
    }

    #[test]
    fn catalog_contains_agent_model() {
        let ctx = test_ctx();
        CapabilityCommands::catalog(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert!(rows.iter().any(|r| r[0] == "agent-model"));
    }

    // -- list -----------------------------------------------------------------

    #[test]
    fn list_empty_config_has_zero_rows() {
        let ctx = test_ctx();
        CapabilityCommands::list(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn list_configured_capability_shows_row() {
        let ctx = ctx_with_capability("my-cap", "agent-model");
        CapabilityCommands::list(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], "my-cap");
        assert_eq!(rows[0][1], "agent-model");
    }

    // -- info -----------------------------------------------------------------

    #[test]
    fn info_unknown_capability_returns_err() {
        let ctx = test_ctx();
        let result = CapabilityCommands::info(&ctx, "does-not-exist");
        assert!(result.is_err());
    }

    #[test]
    fn info_configured_only_capability_renders_detail_not_err() {
        let ctx = ctx_with_capability("custom-cap", "not-a-real-type");
        let result = CapabilityCommands::info(&ctx, "custom-cap");
        assert!(result.is_ok());
        assert!(!details!(ctx).is_empty());
    }

    #[test]
    fn info_configured_agent_model_resolves_via_catalog_type() {
        let ctx = ctx_with_capability("chat", "agent-model");
        let result = CapabilityCommands::info(&ctx, "chat");
        assert!(result.is_ok());
        assert!(!details!(ctx).is_empty());
    }

    // -- setup ------------------------------------------------------------------

    #[tokio::test]
    async fn setup_unknown_type_returns_err() {
        let mut ctx = test_ctx();
        let result = CapabilityCommands::setup(&mut ctx, "no-such-type", Some("test")).await;
        assert!(result.is_err());
    }

    fn ctx_with_chat_capable_model() -> crate::AppContext {
        use crate::config::{ModelConfig, ProviderConfig};

        let mut ctx = test_ctx();
        ctx.config.providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({}),
            },
        );
        ctx.config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                config: serde_json::json!({}),
                provider_id: Some("ollama".to_string()),
                variant: None,
            },
        );
        ctx
    }

    #[tokio::test]
    async fn setup_agent_model_persists_config() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_chat_capable_model();
        // CaptureUi's text() echoes back the default when prompted; here we
        // pass an explicit instance id so no prompt is needed. Exactly one
        // configured model satisfies the Chat requirement, so it's picked
        // automatically without a select prompt.
        let result = CapabilityCommands::setup(&mut ctx, "agent-model", Some("chat")).await;
        assert!(result.is_ok());
        let configured = ctx.config.get_capability("chat").unwrap();
        assert_eq!(
            configured.config.get("model_id").and_then(|v| v.as_str()),
            Some("granite-3.1-8b-instruct")
        );
        let infos = infos!(ctx);
        assert!(
            infos
                .iter()
                .any(|m| m.contains("chat") && m.contains("configured successfully"))
        );
    }

    // These exercise the pure decision helpers directly rather than driving
    // them through `setup()`: with nothing configured, `configurable_types`
    // is never empty (the catalog always has something), so the "configure
    // a new instance" option would auto-select and recurse into a real,
    // live `ModelCommands::setup`/`ProviderCommands::setup` call against the
    // real registries -- unsafe/nondeterministic for a unit test.

    #[test]
    fn model_candidates_excludes_providerless_model() {
        use crate::config::ModelConfig;
        use crate::models::ModelFunction;

        let mut ctx = test_ctx();
        // Configured model with no provider_id -- Model::provider() errs, so
        // it must not be offered as a usable candidate.
        ctx.config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                config: serde_json::json!({}),
                provider_id: None,
                variant: None,
            },
        );

        let requirement = ModelRequirement {
            supported_functions: vec![ModelFunction::Chat],
            ..Default::default()
        };
        let (usable, _) = CapabilityCommands::model_candidates(&ctx, &requirement);
        assert!(!usable.contains(&"granite-3.1-8b-instruct".to_string()));
    }

    #[test]
    fn model_candidates_offers_configurable_types_when_nothing_configured() {
        let ctx = test_ctx();
        let requirement = ModelRequirement::default();
        let (usable, configurable_types) = CapabilityCommands::model_candidates(&ctx, &requirement);
        assert!(usable.is_empty());
        assert!(!configurable_types.is_empty());
    }

    #[tokio::test]
    async fn resolve_model_dependency_fails_when_unsatisfiable_and_required() {
        let mut ctx = test_ctx();
        // No catalog model type has this family, so both `usable` and
        // `configurable_types` come back empty -- the true "nothing can
        // satisfy this, not even by configuring something new" path.
        let requirement = ModelRequirement {
            family: Some("NoSuchFamilyXYZ".to_string()),
            ..Default::default()
        };
        let result =
            CapabilityCommands::resolve_model_dependency(&mut ctx, &requirement, true).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No configured model satisfies")
        );
    }

    #[tokio::test]
    async fn resolve_model_dependency_returns_none_when_unsatisfiable_and_optional() {
        let mut ctx = test_ctx();
        let requirement = ModelRequirement {
            family: Some("NoSuchFamilyXYZ".to_string()),
            ..Default::default()
        };
        let result = CapabilityCommands::resolve_model_dependency(&mut ctx, &requirement, false)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn provider_candidates_offers_configurable_types_when_nothing_configured() {
        let ctx = test_ctx();
        let requirement = ProviderRequirement::default();
        let (existing, configurable_types) =
            CapabilityCommands::provider_candidates(&ctx, &requirement);
        assert!(existing.is_empty());
        assert!(!configurable_types.is_empty());
    }

    #[tokio::test]
    async fn resolve_provider_dependency_fails_when_unsatisfiable_and_required() {
        use crate::models::ModelFunction;
        // NOTE: Eventually, all functions will be supported by at least one provider,
        // so this test will be impossible to implement without a dummy function.

        let mut ctx = test_ctx();
        // No registered provider type supports Thinking, so both `existing`
        // and `configurable_types` come back empty -- the true "nothing can
        // satisfy this, not even by configuring something new" path.
        let requirement = ProviderRequirement {
            functions: vec![ModelFunction::KeywordBiasing],
            ..Default::default()
        };
        let result =
            CapabilityCommands::resolve_provider_dependency(&mut ctx, &requirement, true).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No configured provider satisfies")
        );
    }

    #[tokio::test]
    async fn resolve_provider_dependency_returns_none_when_unsatisfiable_and_optional() {
        // NOTE: Eventually, all functions will be supported by at least one provider,
        // so this test will be impossible to implement without a dummy function.
        use crate::models::ModelFunction;

        let mut ctx = test_ctx();
        let requirement = ProviderRequirement {
            functions: vec![ModelFunction::KeywordBiasing],
            ..Default::default()
        };
        let result = CapabilityCommands::resolve_provider_dependency(&mut ctx, &requirement, false)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    // -- remove -----------------------------------------------------------------

    #[test]
    fn remove_existing_capability_succeeds_and_disappears_from_list() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_capability("my-cap", "agent-model");
        assert!(ctx.config.get_capability("my-cap").is_some());

        CapabilityCommands::remove(&mut ctx, "my-cap").unwrap();

        assert!(ctx.config.get_capability("my-cap").is_none());
        let infos = infos!(ctx);
        assert!(
            infos
                .iter()
                .any(|m| m.contains("my-cap") && m.contains("removed"))
        );
    }

    #[test]
    fn remove_nonexistent_capability_returns_err() {
        let mut ctx = test_ctx();
        let result = CapabilityCommands::remove(&mut ctx, "doesnt-exist");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Nothing to remove")
        );
    }

    #[test]
    fn list_does_not_show_removed_capability() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_capability("my-cap", "agent-model");
        CapabilityCommands::remove(&mut ctx, "my-cap").unwrap();
        CapabilityCommands::list(&ctx).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert!(rows.is_empty());
    }
}
