//! Pure reconciliation of a provider's reported model list with the models
//! configured against it.
//!
//! Nothing here does I/O: the caller supplies the provider's reported models,
//! the current model map, and the wire-name coverage it resolved through the
//! provider. The result is the additions and removals to apply in one batch.

// Standard
use std::collections::{HashMap, HashSet};

// Local
use crate::config::{DiscoveredModel, ModelConfig};
use crate::models::custom::CustomModelConfig;
use crate::models::{ModelFunction, ModelType};
use crate::providers::ProviderModel;

/*-- public --------------------------------------------------------------------*/

/// What one provider's successful listing means for the configured models.
#[derive(Debug, Clone)]
pub(crate) struct DiscoveryPlan {
    /// New model entries to insert, keyed by configured instance id.
    pub(crate) additions: Vec<(String, ModelConfig)>,
    /// Configured instance ids to remove, sorted.
    pub(crate) removals: Vec<String>,
    /// Managed entries that the provider still reports.
    pub(crate) present: usize,
    /// Instance ids that had to be namespaced because the natural id was
    /// taken by a different configured model.
    pub(crate) conflicts: Vec<String>,
}

impl DiscoveryPlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.additions.is_empty() && self.removals.is_empty()
    }
}

/// Builds the plan for one provider.
///
/// `covered` maps each wire name a configured model already answers to, to
/// the configured instance id. It includes managed models (`discovered`
/// entries) and user-owned models whose provider alias or instance id
/// matches. A reported id in `covered` is not added again.
pub(crate) fn plan(
    provider_id: &str,
    provider_type: &str,
    reported: &[ProviderModel],
    models: &HashMap<String, ModelConfig>,
    covered: &HashMap<String, String>,
) -> DiscoveryPlan {
    let mut reported_by_id: HashMap<&str, &ProviderModel> = HashMap::new();
    for model in reported {
        if model.id.is_empty() {
            continue;
        }
        // First occurrence wins; a duplicate is not worth failing a sync.
        reported_by_id.entry(model.id.as_str()).or_insert(model);
    }

    // Only entries discovery created for this provider are removable. A
    // user-owned model that happens to answer the same wire name is covered,
    // never managed.
    let mut managed: HashMap<&str, &str> = HashMap::new();
    for (instance_id, config) in models {
        if config.model_type != "custom" {
            continue;
        }
        let Some(discovered) = &config.discovered else {
            continue;
        };
        if discovered.provider_id != provider_id {
            continue;
        }
        managed.insert(discovered.provider_model_id.as_str(), instance_id.as_str());
    }

    let mut present = 0usize;
    for wire_id in managed.keys() {
        if reported_by_id.contains_key(wire_id) {
            present += 1;
        }
    }

    let mut removals: Vec<String> = managed
        .iter()
        .filter(|(wire_id, _)| !reported_by_id.contains_key(*wire_id))
        .map(|(_, instance_id)| instance_id.to_string())
        .collect();
    removals.sort();

    let mut ids: Vec<&str> = reported_by_id.keys().copied().collect();
    ids.sort_unstable();
    // An id held by a model this plan removes is free for an addition. Keep
    // user-owned and still-managed ids reserved.
    let removing: HashSet<&str> = removals.iter().map(String::as_str).collect();
    let mut reserved: HashSet<String> = models
        .keys()
        .filter(|id| !removing.contains(id.as_str()))
        .cloned()
        .collect();
    let mut additions = Vec::new();
    let mut conflicts = Vec::new();
    for wire_id in ids {
        if covered.contains_key(wire_id) || managed.contains_key(wire_id) {
            continue;
        }
        let model = reported_by_id[wire_id];
        let instance_id = available_id(provider_id, wire_id, &reserved);
        reserved.insert(instance_id.clone());
        if instance_id != wire_id {
            conflicts.push(instance_id.clone());
        }
        additions.push((
            instance_id.clone(),
            discovered_model_config(provider_id, provider_type, model, instance_id),
        ));
    }

    DiscoveryPlan {
        additions,
        removals,
        present,
        conflicts,
    }
}

/*-- private ------------------------------------------------------------------*/

/// The configured entry for a newly discovered model, with the defaults the
/// import path documents. `discovered` is what makes it managed by sync.
fn discovered_model_config(
    provider_id: &str,
    provider_type: &str,
    model: &ProviderModel,
    instance_id: String,
) -> ModelConfig {
    let supported_functions = if model.supported_functions.is_empty() {
        // No API signal: chat/tool calling is the optimistic default that
        // matches what agent-model and sub-agent capabilities require. The
        // user can edit the resulting custom config.
        vec![ModelFunction::Chat, ModelFunction::ToolCalling]
    } else {
        model.supported_functions.clone()
    };
    let config = CustomModelConfig {
        family: model
            .display_name
            .clone()
            .unwrap_or_else(|| model.id.clone()),
        version: String::new(),
        size: model.parameter_count.unwrap_or(0),
        context_length: model.context_length.unwrap_or(0),
        model_type: model.model_type.clone().unwrap_or(ModelType::Text),
        huggingface_repo: String::new(),
        native_dtype: String::new(),
        variants: Vec::new(),
        description: Some(format!("Discovered from provider '{provider_id}'.")),
        tags: vec!["discovered".to_string(), provider_type.to_string()],
        supported_functions,
        supported_reasoning_efforts: Vec::new(),
    };
    ModelConfig {
        model_id: instance_id,
        model_type: "custom".to_string(),
        provider_id: provider_id.to_string(),
        variant: None,
        discovered: Some(DiscoveredModel {
            provider_id: provider_id.to_string(),
            provider_model_id: model.id.clone(),
        }),
        config: serde_json::to_value(config).unwrap_or_default(),
    }
}

/// The natural wire id when free, otherwise `<provider>/<wire>` and then a
/// numeric suffix. Deterministic for a given configuration.
fn available_id(provider_id: &str, wire_id: &str, reserved: &HashSet<String>) -> String {
    let mut candidate = wire_id.to_string();
    if !reserved.contains(&candidate) {
        return candidate;
    }
    candidate = format!("{provider_id}/{wire_id}");
    if !reserved.contains(&candidate) {
        return candidate;
    }
    let mut suffix = 2u32;
    loop {
        let candidate = format!("{provider_id}/{wire_id}-{suffix}");
        if !reserved.contains(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

/// Wire names that duplicate an entry the caller already saw, for a report.
pub(crate) fn duplicates(reported: &[ProviderModel]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut duplicates = Vec::new();
    for model in reported {
        if !seen.insert(model.id.as_str()) {
            duplicates.push(model.id.clone());
        }
    }
    duplicates.sort();
    duplicates.dedup();
    duplicates
}

/*-- tests --------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn reported(id: &str) -> ProviderModel {
        ProviderModel {
            id: id.to_string(),
            display_name: Some(format!("{id} display")),
            context_length: Some(8192),
            parameter_count: Some(8_000_000_000),
            disk_size_bytes: None,
            model_type: Some(ModelType::Text),
            supported_functions: vec![ModelFunction::Chat],
        }
    }

    fn managed(instance_id: &str, provider_id: &str, wire_id: &str) -> ModelConfig {
        ModelConfig {
            model_id: instance_id.to_string(),
            model_type: "custom".to_string(),
            provider_id: provider_id.to_string(),
            variant: None,
            discovered: Some(DiscoveredModel {
                provider_id: provider_id.to_string(),
                provider_model_id: wire_id.to_string(),
            }),
            config: serde_json::json!({}),
        }
    }

    fn user_model(instance_id: &str, provider_id: &str, model_type: &str) -> ModelConfig {
        ModelConfig {
            model_id: instance_id.to_string(),
            model_type: model_type.to_string(),
            provider_id: provider_id.to_string(),
            variant: None,
            discovered: None,
            config: serde_json::json!({}),
        }
    }

    fn model_map(models: Vec<ModelConfig>) -> HashMap<String, ModelConfig> {
        models
            .into_iter()
            .map(|m| (m.model_id.clone(), m))
            .collect()
    }

    #[test]
    fn adds_only_uncovered_models() {
        let models = model_map(vec![user_model("granite", "p1", "granite-3.1-8b")]);
        let covered = HashMap::from([("granite-3.1-8b".to_string(), "granite".to_string())]);
        let plan = plan(
            "p1",
            "ollama",
            &[reported("granite-3.1-8b"), reported("llama3:8b")],
            &models,
            &covered,
        );
        assert_eq!(plan.additions.len(), 1);
        assert_eq!(plan.additions[0].0, "llama3:8b");
        assert!(plan.removals.is_empty());
        assert_eq!(plan.present, 0);
    }

    #[test]
    fn removes_only_managed_models_that_are_absent() {
        let models = model_map(vec![
            managed("mine", "p1", "wire-a"),
            user_model("user", "p1", "wire-b"),
        ]);
        let covered = HashMap::from([
            ("wire-a".to_string(), "mine".to_string()),
            ("wire-b".to_string(), "user".to_string()),
        ]);
        let plan = plan("p1", "ollama", &[reported("wire-b")], &models, &covered);
        assert_eq!(plan.removals, vec!["mine".to_string()]);
        assert!(plan.additions.is_empty());
    }

    #[test]
    fn ignores_models_managed_by_another_provider() {
        let models = model_map(vec![managed("other", "p2", "wire-a")]);
        let plan = plan("p1", "ollama", &[], &models, &HashMap::new());
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn reuses_an_instance_id_freed_by_a_removal() {
        let models = model_map(vec![managed("x", "p1", "old")]);
        let covered = HashMap::from([("old".to_string(), "x".to_string())]);
        let plan = plan("p1", "ollama", &[reported("x")], &models, &covered);
        assert_eq!(plan.removals, vec!["x".to_string()]);
        assert_eq!(plan.additions.len(), 1);
        assert_eq!(plan.additions[0].0, "x");
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn namespaces_a_taken_instance_id() {
        let models = model_map(vec![user_model("llama3:8b", "p1", "custom")]);
        let plan = plan(
            "p1",
            "ollama",
            &[reported("llama3:8b")],
            &models,
            &HashMap::new(),
        );
        assert_eq!(plan.additions[0].0, "p1/llama3:8b");
        assert_eq!(plan.conflicts, vec!["p1/llama3:8b".to_string()]);
    }

    #[test]
    fn appends_a_suffix_when_the_namespaced_id_is_taken_too() {
        let models = model_map(vec![
            user_model("llama3:8b", "p1", "custom"),
            user_model("p1/llama3:8b", "p1", "custom"),
        ]);
        let plan = plan(
            "p1",
            "ollama",
            &[reported("llama3:8b")],
            &models,
            &HashMap::new(),
        );
        assert_eq!(plan.additions[0].0, "p1/llama3:8b-2");
    }

    #[test]
    fn addition_carries_defaults_and_provenance() {
        let plan = plan(
            "p1",
            "ollama",
            &[reported("llama3:8b")],
            &HashMap::new(),
            &HashMap::new(),
        );
        let (_, config) = &plan.additions[0];
        assert_eq!(config.model_type, "custom");
        assert_eq!(config.provider_id, "p1");
        assert_eq!(
            config.discovered,
            Some(DiscoveredModel {
                provider_id: "p1".to_string(),
                provider_model_id: "llama3:8b".to_string(),
            })
        );
        let custom: CustomModelConfig = serde_json::from_value(config.config.clone()).unwrap();
        assert_eq!(custom.family, "llama3:8b display");
        assert_eq!(custom.context_length, 8192);
        assert_eq!(custom.size, 8_000_000_000);
        assert_eq!(custom.supported_functions, vec![ModelFunction::Chat]);
        assert!(custom.variants.is_empty());
        assert!(custom.tags.contains(&"discovered".to_string()));
    }

    #[test]
    fn empty_functions_fall_back_to_chat_and_tools() {
        let mut model = reported("x");
        model.supported_functions.clear();
        let plan = plan("p1", "ollama", &[model], &HashMap::new(), &HashMap::new());
        let custom: CustomModelConfig =
            serde_json::from_value(plan.additions[0].1.config.clone()).unwrap();
        assert_eq!(
            custom.supported_functions,
            vec![ModelFunction::Chat, ModelFunction::ToolCalling]
        );
    }

    #[test]
    fn additions_do_not_collide_with_each_other() {
        let models = model_map(vec![user_model("a", "p1", "custom")]);
        let plan = plan(
            "p1",
            "ollama",
            &[reported("a"), reported("p1/a")],
            &models,
            &HashMap::new(),
        );
        let ids: Vec<&str> = plan.additions.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["p1/a", "p1/p1/a"]);
    }

    #[test]
    fn present_counts_reported_managed_models() {
        let models = model_map(vec![
            managed("a", "p1", "wire-a"),
            managed("b", "p1", "wire-b"),
        ]);
        let plan = plan(
            "p1",
            "ollama",
            &[reported("wire-a"), reported("wire-b")],
            &models,
            &HashMap::new(),
        );
        assert_eq!(plan.present, 2);
        assert!(plan.is_empty());
    }

    #[test]
    fn duplicate_reported_ids_are_reported_once() {
        assert_eq!(
            duplicates(&[reported("a"), reported("a"), reported("b")]),
            vec!["a".to_string()]
        );
    }
}
