// Third Party
use anyhow::Result;

// Local
use crate::commands::ProviderCommands;
use crate::config::validation::RefKind;
use crate::dependency::{self, Configured, DependsOn, Requirement};
use crate::models::{
    ContextFit, MODEL_REGISTRY, ModelMetadata, ModelSource, ModelType, ModelVariant,
};
use crate::providers::{
    PROVIDER_REGISTRY, Provider, ProviderMetadata, ProviderSource, ProviderType, PullResult,
};
use crate::utils::Searchable;
use crate::utils::hardware::{HardwareProfile, detect_hardware};
use crate::utils::prompt_from_schema;
use crate::utils::ui::Ui;

/// Compare semantic versions in descending order (higher versions first).
/// Handles versions like "3.1", "4.0", "3.0.1".
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

/// Sort enriched rows by family (asc), version (desc), size (desc), id (asc).
fn sort_enriched_rows(rows: &mut [(Vec<String>, ModelMetadata)]) {
    rows.sort_by(|(row_a, meta_a), (row_b, meta_b)| {
        meta_a
            .family
            .cmp(&meta_b.family)
            .then_with(|| compare_versions_desc(&meta_a.version, &meta_b.version))
            .then_with(|| meta_b.size.cmp(&meta_a.size))
            .then_with(|| row_a[0].cmp(&row_b[0]))
    });
}

pub struct ModelCommands;

/*-- Model -> Provider dependency --------------------------------------------*/

/// What a model variant needs from a provider: support for its format and
/// precision. Concrete `Requirement`/`DependsOn` pairing for the abstract
/// dependency-resolution framework in `dependency::mod`.
#[derive(Clone)]
struct VariantRequirement {
    format: String,
    precision: String,
}

impl Requirement<dyn Provider> for VariantRequirement {
    fn admits_type(&self, metadata: &ProviderMetadata) -> bool {
        // Only gate on format - precision is checked by can_run_model
        metadata
            .supported_formats
            .iter()
            .any(|f| f.to_string().eq_ignore_ascii_case(&self.format))
    }

    fn admits_instance(&self, instance: &dyn Provider) -> bool {
        // This is where precision compatibility is actually determined
        instance.can_run_model(&self.format, &self.precision)
    }
}

impl DependsOn<dyn Provider> for VariantRequirement {
    type Requirement = Self;

    fn requirement(&self) -> Self {
        self.clone()
    }
}

/// Fallback dependency for a model with no declared variants -- the expected
/// shape for most custom models, which describe an endpoint that's already
/// running rather than an artifact `granite-cli` would pull. Admits every
/// provider type/instance unconditionally, so `select_provider` still offers
/// "use an existing provider" / "configure a new one" without filtering by
/// format/precision.
#[derive(Clone)]
struct AnyProviderRequirement;

impl Requirement<dyn Provider> for AnyProviderRequirement {
    fn admits_type(&self, _metadata: &ProviderMetadata) -> bool {
        true
    }

    fn admits_instance(&self, _instance: &dyn Provider) -> bool {
        true
    }
}

impl DependsOn<dyn Provider> for AnyProviderRequirement {
    type Requirement = Self;

    fn requirement(&self) -> Self {
        self.clone()
    }
}

impl ModelCommands {
    /// Rows for the model search table: [id, family, size, context, type].
    /// Shared by the CLI command and the TUI.
    pub(crate) fn search_rows(query: &str) -> Vec<Vec<String>> {
        let q = query.to_lowercase();
        let models = MODEL_REGISTRY.entries();
        let mut rows: Vec<(Vec<String>, ModelMetadata)> = models
            .iter()
            .filter(|(id, m)| {
                id.to_lowercase().contains(&q)
                    || m.search_fields()
                        .iter()
                        .any(|f| f.to_lowercase().contains(&q))
            })
            .map(|(id, m)| {
                let row = vec![
                    id.to_string(),
                    m.family.clone(),
                    m.format_size(),
                    m.context_length.to_string(),
                    m.model_type.to_string(),
                ];
                (row, m.clone())
            })
            .collect();
        sort_enriched_rows(&mut rows);
        rows.into_iter().map(|(row, _)| row).collect()
    }

    pub fn search(ctx: &crate::AppContext, query: &str) -> Result<()> {
        let rows = Self::search_rows(query);
        if rows.is_empty() {
            ctx.ui.info(&format!("No models found matching '{query}'."));
            return Ok(());
        }
        ctx.ui.table(
            &format!("Search results for '{}' ({} models)", query, rows.len()),
            &["ID", "FAMILY", "SIZE", "CONTEXT", "TYPE"],
            &rows,
        );
        Ok(())
    }

    /// Rows for the recommend table: [id, size, variant, type, fit, providers]
    /// (or, when `wide`, [id, family, size, context, variant, type, fit, providers]).
    /// Shared by the CLI command and the TUI.
    ///
    /// `filter_providers`, when `Some`, restricts recommendations to variants
    /// that at least one provider in the slice can run (`None` disables the
    /// check entirely, e.g. for `--providers all`). `display_providers`
    /// always populates the PROVIDERS column with the ids of configured
    /// providers that can run the chosen variant, independent of any
    /// `--providers` narrowing applied via `filter_providers`.
    pub(crate) fn recommend_rows(
        filter_type: Option<&ModelType>,
        filter_providers: Option<&[&dyn Provider]>,
        display_providers: &[(String, std::sync::Arc<dyn Provider>)],
        wide: bool,
        ui: &dyn crate::utils::ui::base::Ui,
        profile: &HardwareProfile,
    ) -> Vec<Vec<String>> {
        let models = MODEL_REGISTRY.entries();

        let mut rows: Vec<(f64, Vec<String>)> = models
            .iter()
            .filter(|(_, m)| filter_type.is_none_or(|t| m.model_type == *t))
            .filter_map(|(id, m)| {
                let fit_rank = |fit: &ContextFit| match fit {
                    ContextFit::Full => 1,
                    ContextFit::Partial(_) => 0,
                    ContextFit::None => -1,
                };
                let best = m
                    .variants
                    .iter()
                    .map(|v| {
                        let fit = crate::models::context_fit::estimate(
                            m.context_length,
                            &m.architecture,
                            &m.native_dtype,
                            v,
                            profile,
                        );
                        (fit, v)
                    })
                    .filter(|(fit, _)| *fit != ContextFit::None)
                    .filter(|(_, v)| {
                        filter_providers.is_none_or(|ps| {
                            ps.iter().any(|p| p.can_run_model(&v.format, &v.precision))
                        })
                    })
                    .max_by(|(fit_a, a), (fit_b, b)| {
                        fit_rank(fit_a).cmp(&fit_rank(fit_b)).then_with(|| {
                            a.size_gb
                                .partial_cmp(&b.size_gb)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                    })?;
                let (fit, variant) = best;
                let variant_label = match (variant.size_gb, variant.precision.is_empty()) {
                    (Some(size), false) => format!(
                        "{} / {} ({:.1} GB)",
                        variant.format, variant.precision, size
                    ),
                    (Some(size), true) => format!("{} ({:.1} GB)", variant.format, size),
                    (None, false) => format!("{} / {}", variant.format, variant.precision),
                    (None, true) => variant.format.clone(),
                };
                let providers_str = {
                    let matching: Vec<&str> = display_providers
                        .iter()
                        .filter(|(_, p)| p.can_run_model(&variant.format, &variant.precision))
                        .map(|(pid, _)| pid.as_str())
                        .collect();
                    if matching.is_empty() {
                        "None".to_string()
                    } else {
                        matching.join(", ")
                    }
                };

                let fit_str = if matches!(fit, ContextFit::Partial(_)) {
                    ui.warn_mark(&fit.to_string())
                } else {
                    fit.to_string()
                };

                let row = if wide {
                    vec![
                        id.to_string(),
                        m.family.clone(),
                        m.format_size(),
                        m.context_length.to_string(),
                        variant_label,
                        m.model_type.to_string(),
                        fit_str,
                        providers_str,
                    ]
                } else {
                    vec![
                        id.to_string(),
                        m.format_size(),
                        variant_label,
                        m.model_type.to_string(),
                        fit_str,
                        providers_str,
                    ]
                };
                Some((variant.size_gb.unwrap_or(f64::MAX), row))
            })
            .collect();

        rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        rows.into_iter().map(|(_, row)| row).collect()
    }

    /// Resolves the `--providers` flag against configured providers.
    ///
    /// - `providers_arg` empty: use all configured+enabled providers.
    /// - `providers_arg == ["all"]` (case-insensitive): skip the provider
    ///   check entirely (`None`).
    /// - otherwise: restrict to the named provider ids, erroring if any is
    ///   not a configured+enabled provider.
    pub fn recommend(
        ctx: &crate::AppContext,
        filter_type: Option<ModelType>,
        providers_arg: &[String],
        wide: bool,
    ) -> Result<()> {
        let source = ProviderSource::from_config(&ctx.config);
        let instances = source.instances();

        let skip_all = providers_arg.iter().any(|p| p.eq_ignore_ascii_case("all"));
        if skip_all && providers_arg.len() > 1 {
            anyhow::bail!("--providers all cannot be combined with specific provider ids");
        }

        let providers: Option<Vec<&dyn Provider>> = if skip_all {
            None
        } else if providers_arg.is_empty() {
            Some(instances.iter().map(|(_, p)| &**p).collect())
        } else {
            let unknown: Vec<&str> = providers_arg
                .iter()
                .filter(|id| !instances.iter().any(|(iid, _)| iid == *id))
                .map(String::as_str)
                .collect();
            if !unknown.is_empty() {
                let configured = instances
                    .iter()
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "Unknown or disabled provider(s): {}. Configured providers: {}",
                    unknown.join(", "),
                    if configured.is_empty() {
                        "none".to_string()
                    } else {
                        configured
                    },
                );
            }
            Some(
                instances
                    .iter()
                    .filter(|(iid, _)| providers_arg.contains(iid))
                    .map(|(_, p)| &**p)
                    .collect(),
            )
        };

        let table_rows = Self::recommend_rows(
            filter_type.as_ref(),
            providers.as_deref(),
            &instances,
            wide,
            ctx.ui.as_ref(),
            &detect_hardware(),
        );
        if table_rows.is_empty() {
            let msg = if matches!(&providers, Some(list) if list.is_empty()) {
                "No models fit: no providers are configured. Use --providers all to ignore \
                 this check, or run `granite-cli provider setup` to add one."
            } else {
                "No models fit the current hardware profile and configured providers."
            };
            ctx.ui.info(msg);
            return Ok(());
        }
        let headers: &[&str] = if wide {
            &[
                "ID",
                "FAMILY",
                "SIZE",
                "CONTEXT",
                "VARIANT",
                "TYPE",
                "FIT",
                "PROVIDERS",
            ]
        } else {
            &["ID", "SIZE", "VARIANT", "TYPE", "FIT", "PROVIDERS"]
        };
        ctx.ui.table(
            &format!(
                "Recommended Models for this hardware ({} models)",
                table_rows.len()
            ),
            headers,
            &table_rows,
        );

        Ok(())
    }

    /// Rows for the model catalog table: [id, family, size, context, type].
    /// Shared by the CLI command and the TUI.
    pub(crate) fn catalog_rows(filter_type: Option<&ModelType>) -> Vec<Vec<String>> {
        let models = MODEL_REGISTRY.entries();
        let mut rows: Vec<(Vec<String>, ModelMetadata)> = models
            .iter()
            .filter(|(_, m)| filter_type.is_none_or(|t| m.model_type == *t))
            .map(|(id, m)| {
                let row = vec![
                    id.to_string(),
                    m.family.clone(),
                    m.format_size(),
                    m.context_length.to_string(),
                    m.model_type.to_string(),
                ];
                (row, m.clone())
            })
            .collect();
        sort_enriched_rows(&mut rows);
        rows.into_iter().map(|(row, _)| row).collect()
    }

    pub fn catalog(ctx: &crate::AppContext, filter_type: Option<ModelType>) -> Result<()> {
        let rows = Self::catalog_rows(filter_type.as_ref());
        if rows.is_empty() {
            ctx.ui.info(&format!(
                "No models found{}.",
                filter_type
                    .as_ref()
                    .map(|t| format!(" matching type: {t}"))
                    .unwrap_or_default()
            ));
            return Ok(());
        }
        ctx.ui.table(
            &format!("Model Catalog ({} models)", rows.len()),
            &["ID", "FAMILY", "SIZE", "CONTEXT", "TYPE"],
            &rows,
        );
        Ok(())
    }

    pub fn list(ctx: &crate::AppContext, filter_type: Option<ModelType>) -> Result<()> {
        let notes = crate::commands::shared::remediation::dangling_notes(ctx, RefKind::Model);
        let source = ModelSource::from_config(&ctx.config);
        let mut enriched: Vec<(Vec<String>, ModelMetadata)> = Vec::new();

        for (instance_id, model) in source.instances() {
            let md = model.to_metadata();
            if let Some(ref t) = filter_type
                && md.model_type != *t
            {
                continue;
            }
            let provider_id = ctx
                .config
                .get_model(&instance_id)
                .unwrap()
                .provider_id
                .clone();
            let row = vec![
                instance_id.clone(),
                md.family.clone(),
                md.format_size(),
                md.context_length.to_string(),
                md.model_type.to_string(),
                provider_id,
            ];
            enriched.push((row, md));
        }
        sort_enriched_rows(&mut enriched);

        let mut rows: Vec<Vec<String>> = enriched
            .into_iter()
            .map(|(mut row, _)| {
                row.push(notes.get(&row[0]).cloned().unwrap_or_default());
                row
            })
            .collect();

        // A model whose type is not in the registry cannot be constructed,
        // so it never reaches `instances()` and would drop out of the list
        // at exactly the moment something is wrong with it. Give it a row
        // carrying the reason instead. A filtered list leaves them out,
        // since there is no metadata to filter them on.
        if filter_type.is_none() {
            let mut unconstructed: Vec<Vec<String>> = notes
                .iter()
                .filter(|(id, _)| !rows.iter().any(|row| &&row[0] == id))
                .map(|(id, note)| {
                    let mut row = vec![id.clone()];
                    row.resize(6, "-".to_string());
                    row.push(note.clone());
                    row
                })
                .collect();
            unconstructed.sort_by(|a, b| a[0].cmp(&b[0]));
            rows.extend(unconstructed);
        }

        ctx.ui.table(
            &format!("Configured Models ({} models)", rows.len()),
            &[
                "ID", "FAMILY", "SIZE", "CONTEXT", "TYPE", "PROVIDER", "NOTES",
            ],
            &rows,
        );
        Ok(())
    }

    /// Key-value fields describing `model`'s data -- shared by the catalog
    /// path (`info_fields`, a static registry lookup) and `info`'s
    /// configured-instance path (a live constructed model's real values).
    fn metadata_fields(model: &ModelMetadata) -> Vec<(&'static str, String)> {
        let mut fields: Vec<(&'static str, String)> = vec![
            ("Family", model.family.clone()),
            ("Version", model.version.clone()),
            ("Size", format!("{} parameters", model.format_size())),
            ("Context Length", format!("{} tokens", model.context_length)),
            ("Type", model.model_type.to_string()),
            ("Hugging Face", model.huggingface_repo.clone()),
        ];
        if let Some(desc) = &model.description {
            fields.push(("Description", desc.clone()));
        }
        if !model.tags.is_empty() {
            fields.push(("Tags", model.tags.join(", ")));
        }
        let variants_str = model
            .variants
            .iter()
            .map(|v| match (v.size_gb, v.precision.is_empty()) {
                (Some(size), false) => format!("{} / {} ({:.1} GB)", v.format, v.precision, size),
                (Some(size), true) => format!("{} ({:.1} GB)", v.format, size),
                (None, false) => format!("{} / {}", v.format, v.precision),
                (None, true) => v.format.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        fields.push(("Variants", variants_str));
        let funcs_str = model
            .supported_functions
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        fields.push(("Supported Functions", funcs_str));
        fields
    }

    /// Key-value fields for a catalog model's detail. Returns `None` if the
    /// registry key is not in the registry. Shared by the CLI command and
    /// the TUI, which only ever browses the catalog, never a configured
    /// instance's live values.
    pub(crate) fn info_fields(model_type: &str) -> Option<Vec<(&'static str, String)>> {
        MODEL_REGISTRY
            .get(model_type)
            .map(|m| Self::metadata_fields(&m))
    }

    pub async fn info(ctx: &mut crate::AppContext, id: &str) -> Result<()> {
        // Only a configured instance can have a broken reference. An id that
        // names a catalog type is being browsed, not diagnosed.
        if ctx.config.get_model(id).is_some() {
            crate::commands::shared::remediation::remediate(
                ctx,
                RefKind::Model,
                id,
                crate::commands::shared::remediation::OnDecline::Skip,
                true,
            )
            .await?;

            // Removing it is one of the choices offered above, and leaves
            // nothing to show.
            if ctx.config.get_model(id).is_none() {
                return Ok(());
            }
        }

        // Prefer a configured instance's real, live values (e.g. a custom
        // model's user-entered fields aren't in the registry at all) --
        // fall back to pure catalog browsing by registry key.
        if let Some(model_config) = ctx.config.get_model(id) {
            let source = ModelSource::from_config(&ctx.config);
            if let Some((_, model)) = source.instances().into_iter().find(|(iid, _)| iid == id) {
                let md = model.to_metadata();
                let type_fields = Self::metadata_fields(&md);

                ctx.ui.detail("Type Metadata", &type_fields);

                let mut instance_fields: Vec<(&str, String)> = Vec::new();
                instance_fields.push(("Config: Type", model_config.model_type.clone()));
                instance_fields.push((
                    "Config: Provider",
                    format!("{:?}", model_config.provider_id),
                ));
                instance_fields.push(("Config: Variant", format!("{:?}", model_config.variant)));
                ctx.ui.detail(id, &instance_fields);
                return Ok(());
            }
        }

        match Self::info_fields(id) {
            Some(type_fields) => {
                ctx.ui.detail("Type Metadata", &type_fields);

                if let Some(configured) = ctx.config.get_model(id) {
                    let mut instance_fields: Vec<(&str, String)> = Vec::new();
                    instance_fields
                        .push(("Config: Provider", format!("{:?}", configured.provider_id)));
                    instance_fields.push(("Config: Variant", format!("{:?}", configured.variant)));

                    ctx.ui.detail(id, &instance_fields);
                }

                Ok(())
            }
            None => {
                ctx.ui.info(&format!("Model '{id}' not found in registry."));
                let available: Vec<_> = MODEL_REGISTRY
                    .entries()
                    .keys()
                    .map(|k| k.to_string())
                    .collect();
                ctx.ui
                    .info(&format!("Available models: {}", available.join(", ")));
                anyhow::bail!("Model not found");
            }
        }
    }

    /// Interactive model setup wizard. `model_type` is the catalog/registry
    /// key (e.g. `granite-3.1-8b-instruct`, or `custom`). `instance_id` is
    /// this instance's nickname, distinct from its type -- defaults to
    /// `model_type` when not given, but a caller may pass a different value
    /// to configure multiple named instances of one type (e.g. the same
    /// catalog model against two different providers, or several custom
    /// models).
    pub async fn setup(
        ctx: &mut crate::AppContext,
        model_type: &str,
        instance_id: Option<&str>,
    ) -> Result<()> {
        let Some(placeholder) = MODEL_REGISTRY.get(model_type) else {
            ctx.ui
                .error(&format!("Model type '{model_type}' not found in registry."));
            let available: Vec<_> = MODEL_REGISTRY
                .entries()
                .keys()
                .map(|k| k.to_string())
                .collect();
            ctx.ui
                .info(&format!("Available models: {}", available.join(", ")));
            anyhow::bail!("Model not found");
        };

        ctx.ui.info(&format!("\nSetting up model: {model_type}"));
        ctx.ui.info(
            placeholder
                .description
                .as_deref()
                .unwrap_or("No description available."),
        );
        ctx.ui.info("");
        ctx.ui.info(&format!(
            "Size: {} params, {} context",
            placeholder.format_size(),
            placeholder.context_length
        ));
        ctx.ui.info(&format!("Type: {}", placeholder.model_type));
        ctx.ui.info("");

        let instance_id = match instance_id {
            Some(id) => id.to_string(),
            None => ctx.ui.text("Instance name: ", model_type)?,
        };

        let existing_config = ctx.config.get_model(&instance_id).cloned();
        if existing_config.is_some() {
            let overwrite = ctx.ui.confirm(
                &format!("Model '{instance_id}' is already configured. Overwrite?"),
                false,
            )?;
            if !overwrite {
                ctx.ui.info("Model setup skipped.");
                return Ok(());
            }
        }

        let schema = MODEL_REGISTRY.config_schema(model_type).ok_or_else(|| {
            anyhow::anyhow!("No config schema registered for model type '{model_type}'")
        })?;
        let defaults = existing_config
            .as_ref()
            .map(|c| c.config.clone())
            .or_else(|| MODEL_REGISTRY.default_config(model_type))
            .unwrap_or_else(|| serde_json::json!({}));
        let model_specific_cfg = prompt_from_schema(&*ctx.ui, &schema, &defaults)?;

        // Construct a live, provider-less instance now -- this replaces the
        // placeholder as the source of truth for variants/description from
        // here on (real for catalog models, user-entered for custom).
        let live = MODEL_REGISTRY
            .construct(model_type, &instance_id, &model_specific_cfg)
            .map_err(|e| anyhow::anyhow!("Failed to construct model '{model_type}': {e}"))?;

        let selected_variant: Option<ModelVariant> = if live.variants().is_empty() {
            None
        } else {
            let variant_options: Vec<_> = live
                .variants()
                .iter()
                .map(|v| match (v.size_gb, v.precision.is_empty()) {
                    (Some(size), false) => {
                        format!("{} / {} ({:.1} GB)", v.format, v.precision, size)
                    }
                    (Some(size), true) => format!("{} ({:.1} GB)", v.format, size),
                    (None, false) => format!("{} / {}", v.format, v.precision),
                    (None, true) => v.format.clone(),
                })
                .collect();

            let variant_index = ctx
                .ui
                .select("Select model variant:", &variant_options, 0)?;

            let variant = live.variants()[variant_index].clone();
            ctx.ui.info(&format!(
                "\nSelected: {} / {}",
                variant.format, variant.precision
            ));
            Some(variant)
        };

        let provider_source = ProviderSource::from_config(&ctx.config);
        let current_provider = existing_config.as_ref().map(|c| c.provider_id.clone());
        let provider_id = if let Some(variant) = &selected_variant {
            let requirement = VariantRequirement {
                format: variant.format.clone(),
                precision: variant.precision.clone(),
            };
            let resolution = dependency::resolve(&requirement, &provider_source);
            Self::select_provider(ctx, &resolution, current_provider.as_deref()).await?
        } else {
            let resolution = dependency::resolve(&AnyProviderRequirement, &provider_source);
            Self::select_provider(ctx, &resolution, current_provider.as_deref()).await?
        };

        let model_config = crate::config::ModelConfig {
            model_id: instance_id.clone(),
            model_type: model_type.to_string(),
            provider_id: provider_id.clone(),
            variant: selected_variant
                .as_ref()
                .map(|v| format!("{}/{}", v.format, v.precision)),
            config: model_specific_cfg,
        };

        ctx.config
            .insert_model(&instance_id, model_config)
            .map_err(|e| anyhow::anyhow!("failed to save model config: {e}"))?;

        ctx.ui
            .info(&format!("\nModel '{instance_id}' configured successfully!"));

        if let (pid, Some(variant)) = (&provider_id, &selected_variant) {
            let is_local = ctx
                .config
                .get_provider(pid)
                .and_then(|pc| PROVIDER_REGISTRY.get(&pc.provider_type))
                .map(|meta| meta.provider_type == ProviderType::Local)
                .unwrap_or(false);

            if is_local {
                let pull_now = ctx.ui.confirm(
                    &format!(
                        "'{}' is a local provider. Pull '{} ({}/{})' now?",
                        pid, instance_id, variant.format, variant.precision
                    ),
                    true,
                )?;
                if pull_now {
                    let source = ModelSource::from_config(&ctx.config);
                    match source
                        .instances()
                        .into_iter()
                        .find(|(id, _)| id == &instance_id)
                        .map(|(id, _)| source.provider_for(&id))
                    {
                        Some(Ok(provider)) => {
                            ensure_model_pulled(
                                provider.as_ref(),
                                &live.to_metadata(),
                                variant,
                                ctx.ui.as_ref(),
                            )
                            .await?;
                        }
                        Some(Err(e)) => ctx.ui.warn(&format!(
                            "Provider '{pid}' is not available; skipping pull: {e}"
                        )),
                        None => ctx.ui.warn(&format!(
                            "Provider '{pid}' is not available; skipping pull."
                        )),
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn pull(ctx: &mut crate::AppContext, model_id: &str) -> Result<()> {
        let configured = ctx.config.get_model(model_id);
        if configured.is_none() && MODEL_REGISTRY.get(model_id).is_none() {
            ctx.ui
                .error(&format!("Model '{model_id}' not found in registry."));
            let available: Vec<_> = MODEL_REGISTRY
                .entries()
                .keys()
                .map(|k| k.to_string())
                .collect();
            ctx.ui
                .info(&format!("Available models: {}", available.join(", ")));
            anyhow::bail!("Model not found");
        }

        let (variant_str, provider_id) = match configured {
            Some(c) if c.variant.is_some() => (c.variant.clone().unwrap(), c.provider_id.clone()),
            _ => {
                anyhow::bail!(
                    "Model '{model_id}' is not configured yet. Run `model setup {model_id}` first."
                );
            }
        };

        let (format, precision) = variant_str.split_once('/').ok_or_else(|| {
            anyhow::anyhow!("Invalid stored variant '{variant_str}' for model '{model_id}'.")
        })?;

        let source = ModelSource::from_config(&ctx.config);
        let model = source
            .instances()
            .into_iter()
            .find(|(id, _)| id == model_id)
            .ok_or_else(|| anyhow::anyhow!("model '{model_id}' is not configured"))?
            .1;

        let variant: &ModelVariant = model
            .variants()
            .iter()
            .find(|v| v.format == format && v.precision == precision)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Variant '{format}/{precision}' is no longer available for model '{model_id}'."
                )
            })?;

        let provider = source.provider_for(model_id).map_err(|e| {
            anyhow::anyhow!(
                "Provider '{provider_id}' is not configured or enabled. Run `provider setup` first: {e}"
            )
        })?;

        let md = model.to_metadata();
        let result = ensure_model_pulled(provider.as_ref(), &md, variant, ctx.ui.as_ref()).await;
        match result {
            Ok(PullResult::Success) => {
                ctx.ui
                    .info(&format!("Model '{}' pulled successfully.", md.family));
            }
            Ok(PullResult::Unnecessary) => {
                ctx.ui.info(&format!(
                    "No pull needed for '{}' ({}).",
                    md.family,
                    provider.name()
                ));
            }
            Ok(PullResult::Unsupported { message }) => {
                ctx.ui.warn(&message);
            }
            Err(e) => {
                ctx.ui.error(&format!("Failed to pull model: {e}"));
                anyhow::bail!("Pull failed: {e}");
            }
        }

        Ok(())
    }

    /// Remove a configured model instance by ID.
    ///
    /// Deletes the model's config file and removes it from the in-memory
    /// config. After this call `model list` will no longer show the entry.
    pub fn remove(ctx: &mut crate::AppContext, model_id: &str) -> Result<()> {
        if ctx.config.get_model(model_id).is_none() {
            anyhow::bail!("No model configured with id '{model_id}'. Nothing to remove.");
        }

        // Anything pointing at it would be stranded by this removal.
        match crate::commands::shared::remediation::confirm_removal(ctx, RefKind::Model, model_id)?
        {
            crate::commands::shared::remediation::Removal::Cancel => {
                ctx.ui.info(&format!("Keeping model '{model_id}'."));
                return Ok(());
            }
            crate::commands::shared::remediation::Removal::Proceed { with } => {
                crate::commands::shared::remediation::remove_all(ctx, &with)?;
            }
        }

        if let Err(e) = ctx.config.remove_model(model_id) {
            ctx.ui
                .warn(&format!("failed to persist model removal: {e}"));
        }
        ctx.ui.info(&format!("Model '{model_id}' removed."));
        Ok(())
    }

    /// Resolve which provider instance to use for a model variant, prompting
    /// to configure a new one (with its own instance nickname, distinct from
    /// its catalog type) when no existing instance satisfies it.
    async fn select_provider(
        ctx: &mut crate::AppContext,
        resolution: &dependency::Resolution,
        current: Option<&str>,
    ) -> Result<String> {
        if resolution.is_unsatisfiable() {
            anyhow::bail!(
                "No provider type supports this model's format/precision; configure a compatible provider first, then set up this model."
            );
        }

        const CONFIGURE_NEW: &str = "Configure a new provider...";
        let mut options = resolution.existing_instances.clone();
        if !resolution.configurable_types.is_empty() {
            options.push(CONFIGURE_NEW.to_string());
        }

        let choice = if options.len() == 1 {
            0
        } else {
            let prompt = crate::commands::shared::remediation::prompt_with_current(
                ctx,
                "Select a provider for this model",
                RefKind::Provider,
                current,
            );
            ctx.ui.select(&prompt, &options, 0)?
        };

        if options[choice] != CONFIGURE_NEW {
            return Ok(options[choice].clone());
        }

        let provider_type = if resolution.configurable_types.len() == 1 {
            resolution.configurable_types[0]
        } else {
            let type_options: Vec<String> = resolution
                .configurable_types
                .iter()
                .map(|s| s.to_string())
                .collect();
            let type_index =
                ctx.ui
                    .select("Select a provider type to configure", &type_options, 0)?;
            resolution.configurable_types[type_index]
        };

        let nickname = ctx.ui.text("Name this provider instance", provider_type)?;

        ProviderCommands::setup(ctx, provider_type, Some(&nickname)).await?;

        // Setup reports success even when it configured nothing: a name that
        // collides with an existing provider, and an overwrite the user then
        // declines, both land here. Check what is configured under that id
        // rather than trusting the id we asked for.
        match ctx.config.get_provider(&nickname) {
            Some(provider) if provider.provider_type == provider_type => Ok(nickname),
            Some(provider) => anyhow::bail!(
                "Provider '{nickname}' is already configured as '{}', which is not what \
                 this model needs. Re-run setup and give the new provider a different name.",
                provider.provider_type
            ),
            None => anyhow::bail!(
                "Provider '{nickname}' was not configured, so this model would have \
                 nothing to run on. Re-run setup once the provider is in place."
            ),
        }
    }
}

/// Trigger `provider`'s native download/pull mechanism for `variant`,
/// reporting progress through `ui`. Separated from `ModelCommands::pull` so
/// a future `launch` pre-launch step can call it directly without going
/// through config lookup or CLI-specific error messages.
pub async fn ensure_model_pulled(
    provider: &dyn Provider,
    model: &ModelMetadata,
    variant: &ModelVariant,
    ui: &dyn Ui,
) -> Result<PullResult, crate::providers::ProviderError> {
    provider.pull_model(model, variant, ui).await
}

/*-- tests -----------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ModelConfig};
    use crate::providers::{ModelFormat, ProviderType};
    use crate::registry::ConfigConstructable;
    use crate::utils::ui::base::tests::CaptureUi;
    use std::sync::Arc;

    fn empty_ctx() -> crate::AppContext {
        crate::AppContext {
            config: Config::default(),
            ui: Arc::new(CaptureUi::default()),
        }
    }

    /// A canned hardware profile with generous RAM and no GPU, used so
    /// `recommend_rows` tests exercise the fit logic deterministically
    /// instead of depending on whatever hardware (and, on some CI runners,
    /// misdetected GPU) actually runs the test.
    fn generous_hardware_profile() -> HardwareProfile {
        HardwareProfile {
            os: "test".to_string(),
            cpu_cores: 8,
            cpu_arch: "test".to_string(),
            gpu_vendor: None,
            vram_gb: None,
            ram_gb: 512.0,
        }
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

    macro_rules! errors {
        ($ctx:expr) => {
            (&*($ctx.ui) as &dyn std::any::Any)
                .downcast_ref::<CaptureUi>()
                .unwrap()
                .errors
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

    fn ctx_with_model(id: &str, provider_id: Option<&str>) -> crate::AppContext {
        let mut ctx = empty_ctx();
        ctx.config.models.insert(
            id.to_string(),
            ModelConfig {
                model_id: id.to_string(),
                model_type: id.to_string(),
                provider_id: provider_id.unwrap_or("ollama").to_string(),
                variant: None,
                config: serde_json::json!({}),
            },
        );
        ctx
    }

    fn ctx_with_model_variant_and_provider(
        model_id: &str,
        variant: &str,
        provider_id: &str,
        provider_type: &str,
        provider_config: serde_json::Value,
    ) -> crate::AppContext {
        let mut config = config_with_provider(provider_id, provider_type, provider_config);
        config.models.insert(
            model_id.to_string(),
            ModelConfig {
                model_id: model_id.to_string(),
                model_type: model_id.to_string(),
                provider_id: provider_id.to_string(),
                variant: Some(variant.to_string()),
                config: serde_json::json!({}),
            },
        );
        ctx_with_config(config)
    }

    // -- version comparison ---------------------------------------------------

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
    fn sort_model_rows_by_family_version_size() {
        let rows = vec![
            vec![
                "granite-3.0-8b-instruct".to_string(),
                "Granite 3.0".to_string(),
                "8B".to_string(),
            ],
            vec![
                "granite-3.1-2b-instruct".to_string(),
                "Granite 3.1".to_string(),
                "2B".to_string(),
            ],
            vec![
                "granite-3.1-8b-instruct".to_string(),
                "Granite 3.1".to_string(),
                "8B".to_string(),
            ],
            vec![
                "granite-3.0-2b-instruct".to_string(),
                "Granite 3.0".to_string(),
                "2B".to_string(),
            ],
        ];
        let mut enriched: Vec<(Vec<String>, ModelMetadata)> = rows
            .into_iter()
            .filter_map(|row| MODEL_REGISTRY.get(&row[0]).map(|m| (row, m.clone())))
            .collect();
        sort_enriched_rows(&mut enriched);
        let sorted: Vec<String> = enriched
            .into_iter()
            .map(|(row, _)| row[0].clone())
            .collect();
        assert_eq!(
            sorted,
            vec![
                "granite-3.1-8b-instruct",
                "granite-3.1-2b-instruct",
                "granite-3.0-8b-instruct",
                "granite-3.0-2b-instruct",
            ]
        );
    }

    // -- catalog --------------------------------------------------------------

    #[test]
    fn catalog_table_has_correct_column_headers() {
        let ctx = empty_ctx();
        ModelCommands::catalog(&ctx, None).unwrap();
        let tables = tables!(ctx);
        assert_eq!(tables.len(), 1);
        let (_, headers, _) = &tables[0];
        assert!(headers.contains(&"ID".to_string()));
        assert!(headers.contains(&"FAMILY".to_string()));
        assert!(headers.contains(&"SIZE".to_string()));
        assert!(headers.contains(&"CONTEXT".to_string()));
        assert!(headers.contains(&"TYPE".to_string()));
    }

    #[test]
    fn catalog_no_filter_returns_all_models() {
        let ctx = empty_ctx();
        ModelCommands::catalog(&ctx, None).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert!(!rows.is_empty(), "expected at least one model in catalog");
    }

    #[test]
    fn catalog_text_filter_returns_only_text_models() {
        let ctx = empty_ctx();
        ModelCommands::catalog(&ctx, Some(ModelType::Text)).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        for row in rows {
            assert_eq!(row[4], "Text", "all filtered rows should be Text type");
        }
    }

    #[test]
    fn catalog_vision_filter_returns_only_vision_models() {
        let ctx = empty_ctx();
        ModelCommands::catalog(&ctx, Some(ModelType::Vision)).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        for row in rows {
            assert_eq!(row[4], "Vision");
        }
    }

    #[test]
    fn catalog_speech_filter_returns_only_speech_models() {
        let ctx = empty_ctx();
        ModelCommands::catalog(&ctx, Some(ModelType::Speech)).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        for row in rows {
            assert_eq!(row[4], "Speech");
        }
    }

    // -- list -----------------------------------------------------------------

    #[test]
    fn list_empty_config_renders_zero_rows() {
        let ctx = empty_ctx();
        ModelCommands::list(&ctx, None).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn list_configured_model_shows_provider_id() {
        let ctx = ctx_with_model("granite-3.1-8b-instruct", Some("my-ollama"));
        ModelCommands::list(&ctx, None).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert_eq!(rows.len(), 1);
        assert!(rows[0].iter().any(|c| c == "my-ollama"));
    }

    #[test]
    fn list_shows_a_model_whose_type_is_unknown_with_the_reason() {
        let ctx = ctx_with_model("this-model-does-not-exist", Some("p1"));
        ModelCommands::list(&ctx, None).unwrap();
        let tables = tables!(ctx);
        let (_, headers, rows) = &tables[0];
        // The type is not in MODEL_REGISTRY, so the model cannot be
        // constructed and has no metadata to fill the other columns. It still
        // gets a row: dropping it would hide a configured model at exactly
        // the moment something is wrong with it.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], "this-model-does-not-exist");
        let notes = headers.iter().position(|h| h == "NOTES").unwrap();
        assert!(rows[0][notes].contains("unknown model type"), "{rows:?}");
    }

    // -- info -----------------------------------------------------------------

    #[tokio::test]
    async fn info_known_model_renders_detail_with_key_fields() {
        let mut ctx = empty_ctx();
        ModelCommands::info(&mut ctx, "granite-3.1-8b-instruct")
            .await
            .unwrap();
        let details = details!(ctx);
        assert_eq!(details.len(), 1);
        let (title, fields) = &details[0];
        assert_eq!(title, "Type Metadata");
        assert!(fields.iter().any(|(k, _)| k == "Family"));
        assert!(fields.iter().any(|(k, _)| k == "Context Length"));
        assert!(fields.iter().any(|(k, _)| k == "Supported Functions"));
    }

    #[tokio::test]
    async fn info_unknown_model_returns_err_and_emits_error() {
        let mut ctx = empty_ctx();
        let result = ModelCommands::info(&mut ctx, "does-not-exist").await;
        assert!(result.is_err());
    }

    fn metadata_supporting(formats: Vec<ModelFormat>) -> ProviderMetadata {
        ProviderMetadata {
            name: "Test Provider".to_string(),
            description: "".to_string(),
            provider_type: ProviderType::Local,
            default_endpoint: "http://localhost".to_string(),
            supported_api_types: vec![],
            default_function_endpoints: std::collections::HashMap::new(),
            supported_formats: formats,
            authentication: vec![],
            tags: vec![],
        }
    }

    #[test]
    fn admits_type_only_checks_format_not_precision() {
        let requirement = VariantRequirement {
            format: "gguf".to_string(),
            precision: "some-exotic-precision".to_string(),
        };
        let metadata = metadata_supporting(vec![ModelFormat::GGUF]);
        assert!(requirement.admits_type(&metadata));
    }

    #[test]
    fn admits_type_rejects_unsupported_format() {
        let requirement = VariantRequirement {
            format: "gguf".to_string(),
            precision: "fp16".to_string(),
        };
        let metadata = metadata_supporting(vec![ModelFormat::Safetensors]);
        assert!(!requirement.admits_type(&metadata));
    }

    #[test]
    fn admits_type_matches_format_case_insensitively() {
        let requirement = VariantRequirement {
            format: "GGUF".to_string(),
            precision: "FP16".to_string(),
        };
        let metadata = metadata_supporting(vec![ModelFormat::GGUF]);
        assert!(requirement.admits_type(&metadata));
    }

    #[test]
    fn admits_instance_defers_to_the_provider_instance() {
        let requirement = VariantRequirement {
            format: "safetensors".to_string(),
            precision: "bfloat16".to_string(),
        };
        let provider = crate::providers::OpenAIProvider::new(
            "my-openai",
            &serde_json::json!({ "base_url": "http://localhost:8080" }),
        )
        .unwrap();
        assert!(requirement.admits_instance(&provider));
    }

    // -- search ---------------------------------------------------------------

    #[test]
    fn search_returns_matching_models_by_id() {
        let ctx = empty_ctx();
        // Use a query unique enough that it only appears in matching IDs, not in descriptions
        ModelCommands::search(&ctx, "granite-3.1-8b-instruct").unwrap();
        let tables = tables!(ctx);
        assert_eq!(tables.len(), 1);
        let (_, _, rows) = &tables[0];
        assert!(!rows.is_empty());
        // Every returned row must have matched; verify at least one row has the exact model ID
        assert!(rows.iter().any(|r| r[0] == "granite-3.1-8b-instruct"));
    }

    #[test]
    fn search_is_case_insensitive() {
        let ctx = empty_ctx();
        ModelCommands::search(&ctx, "GRANITE").unwrap();
        let tables = tables!(ctx);
        assert!(!tables.is_empty());
        let (_, _, rows) = &tables[0];
        assert!(!rows.is_empty());
    }

    #[test]
    fn search_no_match_emits_info_not_table() {
        let ctx = empty_ctx();
        ModelCommands::search(&ctx, "zzznomatch").unwrap();
        assert!(tables!(ctx).is_empty());
        assert!(!infos!(ctx).is_empty());
    }

    #[test]
    fn search_family_match_returns_rows() {
        let ctx = empty_ctx();
        // "Granite 3.3" is a family name
        ModelCommands::search(&ctx, "3.3").unwrap();
        let tables = tables!(ctx);
        assert!(!tables.is_empty());
        let (_, _, rows) = &tables[0];
        assert!(!rows.is_empty());
    }

    // ── recommend ─────────────────────────────────────────────────────────────

    /// `--providers all` sentinel: skip the provider-filter check entirely,
    /// reproducing pre-provider-filtering (hardware-only) behavior.
    fn all_providers() -> Vec<String> {
        vec!["all".to_string()]
    }

    fn config_with_provider(id: &str, provider_type: &str, config: serde_json::Value) -> Config {
        let mut config_obj = Config::default();
        config_obj.providers.insert(
            id.to_string(),
            crate::config::ProviderConfig {
                provider_id: id.to_string(),
                provider_type: provider_type.to_string(),
                config,
            },
        );
        config_obj
    }

    fn ctx_with_config(config: Config) -> crate::AppContext {
        crate::AppContext {
            config,
            ui: Arc::new(CaptureUi::default()),
        }
    }

    #[test]
    fn recommend_returns_table_or_info() {
        let ctx = empty_ctx();
        ModelCommands::recommend(&ctx, None, &all_providers(), false).unwrap();
        let has_table = !tables!(ctx).is_empty();
        let has_info = !infos!(ctx).is_empty();
        assert!(has_table || has_info, "expected a table or an info message");
    }

    #[test]
    fn recommend_all_rows_have_six_columns() {
        let ctx = empty_ctx();
        ModelCommands::recommend(&ctx, None, &all_providers(), false).unwrap();
        for (_, _, rows) in tables!(ctx).iter() {
            for row in rows {
                assert_eq!(row.len(), 6, "each row must have 6 columns");
            }
        }
    }

    #[test]
    fn recommend_wide_rows_have_eight_columns() {
        let ctx = empty_ctx();
        ModelCommands::recommend(&ctx, None, &all_providers(), true).unwrap();
        for (_, _, rows) in tables!(ctx).iter() {
            for row in rows {
                assert_eq!(row.len(), 8, "each wide row must have 8 columns");
            }
        }
    }

    #[test]
    fn recommend_fit_column_is_full_or_partial() {
        let ctx = empty_ctx();
        ModelCommands::recommend(&ctx, None, &all_providers(), false).unwrap();
        for (_, _, rows) in tables!(ctx).iter() {
            for row in rows {
                assert!(
                    row[4] == "Full" || row[4].starts_with("Partial"),
                    "fit column must be Full or Partial, got {}",
                    row[4]
                );
            }
        }
    }

    #[test]
    fn recommend_type_filter_limits_results() {
        let ctx = empty_ctx();
        ModelCommands::recommend(&ctx, Some(ModelType::Text), &all_providers(), false).unwrap();
        for (_, _, rows) in tables!(ctx).iter() {
            for row in rows {
                assert_eq!(row[3], "Text", "filtered rows must all be Text type");
            }
        }
    }

    #[test]
    fn recommend_rows_sorted_descending_by_variant_size() {
        let ctx = empty_ctx();
        ModelCommands::recommend(&ctx, None, &all_providers(), false).unwrap();
        for (_, _, rows) in tables!(ctx).iter() {
            // Extract the GB value from the VARIANT column "format / precision (N.N GB)"
            let sizes: Vec<f64> = rows
                .iter()
                .map(|r| {
                    let v = &r[2];
                    let start = v.rfind('(').unwrap() + 1;
                    let end = v.rfind(" GB)").unwrap();
                    v[start..end].parse::<f64>().expect("parseable GB value")
                })
                .collect();
            for window in sizes.windows(2) {
                assert!(
                    window[0] >= window[1],
                    "rows must be sorted descending by variant size"
                );
            }
        }
    }

    #[test]
    fn recommend_default_with_no_configured_providers_shows_no_providers_info() {
        let ctx = empty_ctx();
        ModelCommands::recommend(&ctx, None, &[], false).unwrap();
        assert!(tables!(ctx).is_empty());
        let infos = infos!(ctx);
        assert!(
            infos
                .iter()
                .any(|m| m.contains("no providers are configured"))
        );
    }

    #[test]
    fn recommend_default_with_permissive_provider_shows_results() {
        let ctx = ctx_with_config(config_with_provider(
            "openai",
            "openai-compatible",
            serde_json::json!({ "base_url": "http://localhost:8080" }),
        ));
        let source = ProviderSource::from_config(&ctx.config);
        let instances = source.instances();
        let providers: Vec<&dyn Provider> = instances.iter().map(|(_, p)| p.as_ref()).collect();
        let rows = ModelCommands::recommend_rows(
            None,
            Some(&providers),
            &instances,
            false,
            ctx.ui.as_ref(),
            &generous_hardware_profile(),
        );
        assert!(
            !rows.is_empty(),
            "a permissive configured provider should surface recommendations"
        );
    }

    #[test]
    fn recommend_default_with_gguf_only_provider_excludes_non_gguf_models() {
        let with_all = ctx_with_config(Config::default());
        ModelCommands::recommend(&with_all, None, &all_providers(), false).unwrap();
        let all_count: usize = tables!(with_all)
            .iter()
            .map(|(_, _, rows)| rows.len())
            .sum();

        let gguf_only = ctx_with_config(config_with_provider(
            "llama-cpp",
            "llama-cpp",
            serde_json::json!({}),
        ));
        ModelCommands::recommend(&gguf_only, None, &[], false).unwrap();
        let gguf_rows: Vec<Vec<String>> = tables!(gguf_only)
            .iter()
            .flat_map(|(_, _, rows)| rows.clone())
            .collect();

        assert!(
            gguf_rows.len() <= all_count,
            "gguf-only provider should not recommend more models than the unfiltered set",
        );
        for row in &gguf_rows {
            assert!(
                row[2].to_lowercase().starts_with("gguf"),
                "variant column must be gguf, got {}",
                row[2]
            );
        }
    }

    #[test]
    fn recommend_providers_flag_narrows_to_named_provider() {
        let mut config = config_with_provider("llama-cpp", "llama-cpp", serde_json::json!({}));
        config.providers.insert(
            "openai".to_string(),
            crate::config::ProviderConfig {
                provider_id: "openai".to_string(),
                provider_type: "openai-compatible".to_string(),
                config: serde_json::json!({ "base_url": "http://localhost:8080" }),
            },
        );
        let ctx = ctx_with_config(config);

        ModelCommands::recommend(&ctx, None, &["llama-cpp".to_string()], false).unwrap();
        let narrowed_rows: Vec<Vec<String>> = tables!(ctx)
            .iter()
            .flat_map(|(_, _, rows)| rows.clone())
            .collect();
        for row in &narrowed_rows {
            assert!(
                row[2].to_lowercase().starts_with("gguf"),
                "narrowed to llama-cpp should only show gguf, got {}",
                row[2]
            );
        }
    }

    #[test]
    fn recommend_unknown_provider_id_errors() {
        let ctx = empty_ctx();
        let result = ModelCommands::recommend(&ctx, None, &["does-not-exist".to_string()], false);
        assert!(result.is_err());
    }

    #[test]
    fn recommend_all_combined_with_named_provider_errors() {
        let ctx = empty_ctx();
        let result = ModelCommands::recommend(
            &ctx,
            None,
            &["all".to_string(), "llama-cpp".to_string()],
            false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn recommend_providers_column_lists_matching_configured_providers() {
        let ctx = ctx_with_config(config_with_provider(
            "llama-cpp",
            "llama-cpp",
            serde_json::json!({}),
        ));
        ModelCommands::recommend(&ctx, None, &[], false).unwrap();
        for (_, _, rows) in tables!(ctx).iter() {
            for row in rows {
                assert_eq!(
                    row[5], "llama-cpp",
                    "providers column should name the matching configured provider, got {}",
                    row[5]
                );
            }
        }
    }

    #[test]
    fn recommend_providers_column_is_none_with_no_display_providers() {
        let ui = Box::new(CaptureUi::default());
        let rows = ModelCommands::recommend_rows(
            None,
            None,
            &[],
            false,
            &*ui,
            &generous_hardware_profile(),
        );
        for row in &rows {
            assert_eq!(
                row[5], "None",
                "with no display providers, PROVIDERS column must be None, got {}",
                row[5]
            );
        }
    }

    // -- pull -------------------------------------------------------------------

    #[tokio::test]
    async fn pull_unknown_model_errors() {
        let mut ctx = empty_ctx();
        let result = ModelCommands::pull(&mut ctx, "does-not-exist").await;
        assert!(result.is_err());
        assert!(!errors!(ctx).is_empty());
    }

    #[tokio::test]
    async fn pull_unconfigured_model_errors_with_setup_hint() {
        let mut ctx = empty_ctx();
        let result = ModelCommands::pull(&mut ctx, "granite-3.1-8b-instruct").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("model setup"));
    }

    #[tokio::test]
    async fn pull_configured_model_missing_provider_or_variant_errors() {
        let mut ctx = ctx_with_model("granite-3.1-8b-instruct", None);
        let result = ModelCommands::pull(&mut ctx, "granite-3.1-8b-instruct").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn pull_with_unconfigured_provider_errors() {
        let mut ctx = ctx_with_model("granite-3.1-8b-instruct", Some("missing-provider"));
        ctx.config
            .models
            .get_mut("granite-3.1-8b-instruct")
            .unwrap()
            .variant = Some("safetensors/bfloat16".to_string());
        let result = ModelCommands::pull(&mut ctx, "granite-3.1-8b-instruct").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing-provider"));
    }

    #[tokio::test]
    async fn pull_with_unknown_variant_errors() {
        let mut ctx = ctx_with_model_variant_and_provider(
            "granite-3.1-8b-instruct",
            "safetensors/does-not-exist",
            "openai",
            "openai-compatible",
            serde_json::json!({ "base_url": "http://localhost:8080" }),
        );
        let result = ModelCommands::pull(&mut ctx, "granite-3.1-8b-instruct").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn pull_delegates_to_provider_pull_model() {
        let mut ctx = ctx_with_model_variant_and_provider(
            "granite-3.1-8b-instruct",
            "safetensors/bfloat16",
            "openai",
            "openai-compatible",
            serde_json::json!({ "base_url": "http://localhost:8080" }),
        );
        // OpenAIProvider::pull_model is a pure warning (no network call), so
        // this exercises the full lookup path end-to-end without needing a
        // live server.
        let result = ModelCommands::pull(&mut ctx, "granite-3.1-8b-instruct").await;
        assert!(result.is_ok());
        let warns = (&*(ctx.ui) as &dyn std::any::Any)
            .downcast_ref::<CaptureUi>()
            .unwrap()
            .warns
            .borrow();
        assert!(!warns.is_empty());
    }

    // -- remove -------------------------------------------------------------

    #[test]
    fn remove_existing_model_succeeds_and_disappears_from_list() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_model("granite-3.1-8b-instruct", Some("my-ollama"));
        assert!(ctx.config.get_model("granite-3.1-8b-instruct").is_some());

        ModelCommands::remove(&mut ctx, "granite-3.1-8b-instruct").unwrap();

        assert!(ctx.config.get_model("granite-3.1-8b-instruct").is_none());
        let infos = infos!(ctx);
        assert!(
            infos
                .iter()
                .any(|m| m.contains("granite-3.1-8b-instruct") && m.contains("removed"))
        );
    }

    #[test]
    fn remove_nonexistent_model_returns_err() {
        let mut ctx = empty_ctx();
        let result = ModelCommands::remove(&mut ctx, "doesnt-exist");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Nothing to remove")
        );
    }

    #[test]
    fn list_does_not_show_removed_model() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_model("granite-3.1-8b-instruct", Some("my-ollama"));
        ModelCommands::remove(&mut ctx, "granite-3.1-8b-instruct").unwrap();
        ModelCommands::list(&ctx, None).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert!(rows.is_empty());
    }

    // -- custom models --------------------------------------------------------

    /// Configures a `"custom"`-typed model instance directly (bypassing the
    /// interactive wizard) with `config_value` as its `CustomModelConfig`
    /// JSON, so `list`/`info`/`pull` tests can assert on its real values
    /// without needing to script every prompt.
    fn ctx_with_custom_model(
        instance_id: &str,
        config_value: serde_json::Value,
        provider_id: Option<&str>,
    ) -> crate::AppContext {
        let mut ctx = empty_ctx();
        ctx.config.models.insert(
            instance_id.to_string(),
            ModelConfig {
                model_id: instance_id.to_string(),
                model_type: "custom".to_string(),
                provider_id: provider_id.unwrap_or("ollama").to_string(),
                variant: None,
                config: config_value,
            },
        );
        ctx
    }

    #[test]
    fn list_shows_a_custom_models_real_values_not_the_registry_placeholder() {
        let ctx = ctx_with_custom_model(
            "my-custom",
            serde_json::json!({
                "family": "My Local Model",
                "context_length": 4096,
            }),
            None,
        );
        ModelCommands::list(&ctx, None).unwrap();
        let tables = tables!(ctx);
        let (_, _, rows) = &tables[0];
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], "my-custom");
        assert!(
            rows[0].iter().any(|c| c == "My Local Model"),
            "expected the configured family, got {:?}",
            rows[0]
        );
    }

    #[tokio::test]
    async fn info_shows_a_custom_models_real_values_and_config_type() {
        let mut ctx = ctx_with_custom_model(
            "my-custom",
            serde_json::json!({
                "family": "My Local Model",
                "context_length": 4096,
            }),
            None,
        );
        ModelCommands::info(&mut ctx, "my-custom").await.unwrap();
        let details = details!(ctx);
        assert_eq!(details.len(), 2);
        let (title1, fields1) = &details[0];
        assert_eq!(title1, "Type Metadata");
        assert!(
            fields1
                .iter()
                .any(|(k, v)| k == "Family" && v == "My Local Model")
        );

        let (title2, fields2) = &details[1];
        assert_eq!(title2, "my-custom");
        assert!(
            fields2
                .iter()
                .any(|(k, v)| k == "Config: Type" && v == "custom")
        );
    }

    #[tokio::test]
    async fn pull_uses_the_custom_models_own_variants_not_the_registry_placeholder() {
        let mut config = config_with_provider(
            "openai",
            "openai-compatible",
            serde_json::json!({ "base_url": "http://localhost:8080" }),
        );
        config.models.insert(
            "my-custom".to_string(),
            ModelConfig {
                model_id: "my-custom".to_string(),
                model_type: "custom".to_string(),
                provider_id: "openai".to_string(),
                variant: Some("safetensors/bfloat16".to_string()),
                config: serde_json::json!({
                    "family": "My Local Model",
                    "variants": [
                        { "format": "safetensors", "precision": "bfloat16", "size_gb": null, "url": "" }
                    ],
                }),
            },
        );
        let mut ctx = ctx_with_config(config);

        // OpenAIProvider::pull_model is a pure warning (no network call), so
        // this exercises the full lookup path -- proving the variant came
        // from the custom model's own config, not the (empty) registry
        // placeholder for "custom".
        let result = ModelCommands::pull(&mut ctx, "my-custom").await;
        assert!(result.is_ok(), "{result:?}");
        let warns = (&*(ctx.ui) as &dyn std::any::Any)
            .downcast_ref::<CaptureUi>()
            .unwrap()
            .warns
            .borrow();
        assert!(!warns.is_empty());
    }

    #[tokio::test]
    async fn setup_custom_model_with_no_variants_skips_variant_selection_and_picks_existing_provider()
     {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_config(config_with_provider(
            "my-openai",
            "openai-compatible",
            serde_json::json!({ "base_url": "http://localhost:8080" }),
        ));

        ModelCommands::setup(&mut ctx, "custom", Some("my-custom"))
            .await
            .unwrap();

        let configured = ctx
            .config
            .get_model("my-custom")
            .expect("custom model should be saved under its instance id");
        assert_eq!(configured.model_type, "custom");
        assert_eq!(
            configured.provider_id, "my-openai",
            "with no variants to filter by, the sole existing provider should be selected"
        );
        assert!(
            configured.variant.is_none(),
            "a custom model with no declared variants should have no stored variant"
        );

        // Never prompted for a variant -- `live.variants()` was empty --
        // even though the provider-selection prompt (auto-resolved to the
        // sole existing provider via its default index) still ran.
        assert!(
            !capture_select_prompts(&ctx)
                .iter()
                .any(|p| p.contains("variant")),
            "should not have prompted for a variant"
        );
    }

    /// A model that could not be saved must fail the setup, not report
    /// success over configuration that never reached disk.
    #[cfg(unix)]
    #[tokio::test]
    async fn setup_fails_when_config_cannot_be_saved() {
        let home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_config(config_with_provider(
            "my-openai",
            "openai-compatible",
            serde_json::json!({ "base_url": "http://localhost:8080" }),
        ));

        home.make_unwritable();
        let result = ModelCommands::setup(&mut ctx, "custom", Some("my-custom")).await;
        home.make_writable();

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("failed to save model config")
        );
        let infos = infos!(ctx);
        assert!(
            !infos.iter().any(|m| m.contains("configured successfully")),
            "{infos:?}"
        );
    }

    #[tokio::test]
    async fn select_provider_rejects_a_name_colliding_with_a_provider_it_did_not_overwrite() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = empty_ctx();
        ctx.config.providers.insert(
            "shared".to_string(),
            crate::config::ProviderConfig {
                provider_id: "shared".to_string(),
                provider_type: "openai-compatible".to_string(),
                config: serde_json::json!({}),
            },
        );
        // Configuring a new one is the only choice, and the name typed for it
        // collides. The overwrite confirmation inside provider setup defaults
        // to no, so nothing is written.
        let ui = (&*ctx.ui as &dyn std::any::Any)
            .downcast_ref::<CaptureUi>()
            .unwrap();
        ui.text_answers.borrow_mut().push_back("shared".to_string());

        let resolution = dependency::Resolution {
            existing_instances: vec![],
            configurable_types: vec!["ollama"],
        };
        let err = ModelCommands::select_provider(&mut ctx, &resolution, None)
            .await
            .unwrap_err();

        // Silently handing back the provider that was already there would
        // attach this model to something that cannot run it.
        assert!(err.to_string().contains("already configured as"), "{err}");
        assert_eq!(
            ctx.config.get_provider("shared").unwrap().provider_type,
            "openai-compatible"
        );
    }

    #[tokio::test]
    async fn select_provider_bails_when_no_provider_type_can_ever_satisfy_the_requirement() {
        let mut ctx = empty_ctx();
        let resolution = dependency::Resolution {
            existing_instances: vec![],
            configurable_types: vec![],
        };

        let err = ModelCommands::select_provider(&mut ctx, &resolution, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("No provider type supports"),
            "{err}"
        );
    }

    fn capture_select_prompts(ctx: &crate::AppContext) -> Vec<String> {
        (&*(ctx.ui) as &dyn std::any::Any)
            .downcast_ref::<CaptureUi>()
            .unwrap()
            .select_prompts
            .borrow()
            .iter()
            .map(|(prompt, _, _)| prompt.clone())
            .collect()
    }
}
