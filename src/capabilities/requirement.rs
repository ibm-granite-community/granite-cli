//! Concrete `Requirement`/`DependsOn` pairings that let a `Capability`
//! declare what it needs from the model, provider, and shell-command
//! worlds, plugging into the generic `dependency` module.

use crate::dependency::{DependsOn, Requirement};
use crate::models::{Model, ModelFunction, ModelMetadata, ModelType};
use crate::providers::{ApiEndpoint, ApiType, Provider, ProviderMetadata};
use serde::{Deserialize, Serialize};

/*-- ModelRequirement ----------------------------------------------------------*/

/// What a capability needs from a `Model`. Every field is optional/empty by
/// default (wildcard); non-default fields narrow the match with AND logic.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelRequirement {
    pub family: Option<String>,
    pub model_type: Option<ModelType>,
    pub min_context_length: Option<u64>,
    pub min_size: Option<u64>,
    pub tags: Vec<String>,
    pub supported_functions: Vec<ModelFunction>,
}

impl ModelRequirement {
    #[allow(clippy::too_many_arguments)]
    fn admits(
        &self,
        family: &str,
        model_type: &ModelType,
        context_length: u64,
        size: u64,
        tags: &[String],
        supported_functions: &[ModelFunction],
    ) -> bool {
        if let Some(f) = &self.family
            && f != family
        {
            return false;
        }
        if let Some(mt) = &self.model_type
            && mt != model_type
        {
            return false;
        }
        if let Some(min_cl) = self.min_context_length
            && context_length < min_cl
        {
            return false;
        }
        if let Some(min_size) = self.min_size
            && size < min_size
        {
            return false;
        }
        if !self.tags.iter().all(|t| tags.contains(t)) {
            return false;
        }
        if !self
            .supported_functions
            .iter()
            .all(|f| supported_functions.contains(f))
        {
            return false;
        }
        true
    }
}

impl Requirement<dyn Model> for ModelRequirement {
    fn admits_type(&self, metadata: &ModelMetadata) -> bool {
        self.admits(
            &metadata.family,
            &metadata.model_type,
            metadata.context_length,
            metadata.size,
            &metadata.tags,
            &metadata.supported_functions,
        )
    }

    fn admits_instance(&self, instance: &dyn Model) -> bool {
        self.admits(
            instance.family(),
            instance.model_type(),
            instance.context_length(),
            instance.size(),
            instance.tags(),
            instance.supported_functions(),
        )
    }
}

impl DependsOn<dyn Model> for ModelRequirement {
    type Requirement = Self;

    fn requirement(&self) -> Self {
        self.clone()
    }
}

/*-- ProviderRequirement --------------------------------------------------------*/

/// What a capability needs from a `Provider`. `api_types` is OR logic (must
/// support at least one, if non-empty); `functions`/`endpoints` are AND logic
/// (must support/expose all listed).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderRequirement {
    pub api_types: Vec<ApiType>,
    pub functions: Vec<ModelFunction>,
    pub endpoints: Vec<ApiEndpoint>,
}

impl ProviderRequirement {
    fn admits<'a>(
        &self,
        api_types: &[ApiType],
        supports_function: impl Fn(&ModelFunction) -> bool,
        function_endpoints: impl Iterator<Item = &'a Vec<ApiEndpoint>>,
    ) -> bool {
        if !self.api_types.is_empty() && !self.api_types.iter().any(|t| api_types.contains(t)) {
            return false;
        }
        if !self.functions.iter().all(supports_function) {
            return false;
        }
        let endpoint_lists: Vec<&Vec<ApiEndpoint>> = function_endpoints.collect();
        if !self
            .endpoints
            .iter()
            .all(|e| endpoint_lists.iter().any(|v| v.contains(e)))
        {
            return false;
        }
        true
    }
}

impl Requirement<dyn Provider> for ProviderRequirement {
    fn admits_type(&self, metadata: &ProviderMetadata) -> bool {
        self.admits(
            &metadata.supported_api_types,
            |f| metadata.default_function_endpoints.contains_key(f),
            metadata.default_function_endpoints.values(),
        )
    }

    fn admits_instance(&self, instance: &dyn Provider) -> bool {
        let endpoints = instance.function_endpoints();
        self.admits(
            &instance.supported_api_types(),
            |f| instance.supports_function(f),
            endpoints.values(),
        )
    }
}

impl DependsOn<dyn Provider> for ProviderRequirement {
    type Requirement = Self;

    fn requirement(&self) -> Self {
        self.clone()
    }
}

/*-- ShellCommandRequirement -----------------------------------------------------*/

/// What a capability needs from the host shell -- an external command
/// reachable on PATH (or at a user-configured explicit path). There is no
/// catalog of shell commands, so this is a standalone pass/fail check rather
/// than a `Requirement<U: Catalogued>` impl -- `admits_type`/`admits_instance`
/// would be vacuous with nothing to narrow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellCommandRequirement {
    pub command: String,
}

impl ShellCommandRequirement {
    pub fn resolve(&self) -> anyhow::Result<std::path::PathBuf> {
        crate::utils::resolve_shell_command(&None, &self.command)
    }

    pub fn is_satisfied(&self) -> bool {
        self.resolve().is_ok()
    }
}

/*-- tests -----------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{LayerKind, LayerTypeCount, ModelArchitecture};

    fn test_architecture() -> ModelArchitecture {
        ModelArchitecture {
            num_hidden_layers: 32,
            hidden_size: 4096,
            num_attention_heads: 32,
            num_key_value_heads: 8,
            head_dim: 128,
            layer_types: vec![LayerTypeCount {
                kind: LayerKind::FullAttention,
                count: 32,
            }],
        }
    }

    fn model_metadata(
        family: &str,
        model_type: ModelType,
        context_length: u64,
        size: u64,
        tags: Vec<&str>,
        supported_functions: Vec<ModelFunction>,
    ) -> ModelMetadata {
        ModelMetadata {
            family: family.to_string(),
            version: "1.0".to_string(),
            size,
            context_length,
            model_type,
            huggingface_repo: "test/test".to_string(),
            native_dtype: "bfloat16".to_string(),
            architecture: test_architecture(),
            variants: vec![],
            description: None,
            tags: tags.into_iter().map(String::from).collect(),
            supported_functions,
        }
    }

    #[test]
    fn model_requirement_wildcard_admits_anything() {
        let req = ModelRequirement::default();
        let meta = model_metadata(
            "Granite",
            ModelType::Text,
            4096,
            8_000_000_000,
            vec![],
            vec![],
        );
        assert!(req.admits_type(&meta));
    }

    #[test]
    fn model_requirement_rejects_wrong_family() {
        let req = ModelRequirement {
            family: Some("Granite".to_string()),
            ..Default::default()
        };
        let meta = model_metadata(
            "Llama",
            ModelType::Text,
            4096,
            8_000_000_000,
            vec![],
            vec![],
        );
        assert!(!req.admits_type(&meta));
    }

    #[test]
    fn model_requirement_rejects_below_minimum_context_length() {
        let req = ModelRequirement {
            min_context_length: Some(8192),
            ..Default::default()
        };
        let meta = model_metadata(
            "Granite",
            ModelType::Text,
            4096,
            8_000_000_000,
            vec![],
            vec![],
        );
        assert!(!req.admits_type(&meta));
    }

    #[test]
    fn model_requirement_requires_all_tags() {
        let req = ModelRequirement {
            tags: vec!["instruct".to_string(), "chat".to_string()],
            ..Default::default()
        };
        let meta = model_metadata(
            "Granite",
            ModelType::Text,
            4096,
            8_000_000_000,
            vec!["instruct"],
            vec![],
        );
        assert!(!req.admits_type(&meta));

        let meta_full = model_metadata(
            "Granite",
            ModelType::Text,
            4096,
            8_000_000_000,
            vec!["instruct", "chat"],
            vec![],
        );
        assert!(req.admits_type(&meta_full));
    }

    #[test]
    fn model_requirement_requires_all_supported_functions() {
        let req = ModelRequirement {
            supported_functions: vec![ModelFunction::Chat, ModelFunction::ToolCalling],
            ..Default::default()
        };
        let meta = model_metadata(
            "Granite",
            ModelType::Text,
            4096,
            8_000_000_000,
            vec![],
            vec![ModelFunction::Chat],
        );
        assert!(!req.admits_type(&meta));
    }

    fn provider_metadata(
        api_types: Vec<ApiType>,
        default_function_endpoints: std::collections::HashMap<ModelFunction, Vec<ApiEndpoint>>,
    ) -> ProviderMetadata {
        ProviderMetadata {
            name: "Test Provider".to_string(),
            description: "test".to_string(),
            provider_type: crate::providers::ProviderType::Local,
            default_endpoint: "http://localhost".to_string(),
            supported_api_types: api_types,
            default_function_endpoints,
            supported_formats: vec![],
            authentication: vec![],
            tags: vec![],
        }
    }

    #[test]
    fn provider_requirement_wildcard_admits_anything() {
        let req = ProviderRequirement::default();
        let meta = provider_metadata(vec![], std::collections::HashMap::new());
        assert!(req.admits_type(&meta));
    }

    #[test]
    fn provider_requirement_api_types_is_or_logic() {
        let req = ProviderRequirement {
            api_types: vec![ApiType::OpenAI, ApiType::Anthropic],
            ..Default::default()
        };
        let meta = provider_metadata(vec![ApiType::Anthropic], std::collections::HashMap::new());
        assert!(req.admits_type(&meta));

        let meta_none = provider_metadata(vec![ApiType::Ollama], std::collections::HashMap::new());
        assert!(!req.admits_type(&meta_none));
    }

    #[test]
    fn provider_requirement_functions_is_and_logic() {
        let mut endpoints = std::collections::HashMap::new();
        endpoints.insert(ModelFunction::Chat, vec![ApiEndpoint::OpenAIChat]);
        let req = ProviderRequirement {
            functions: vec![ModelFunction::Chat, ModelFunction::Embeddings],
            ..Default::default()
        };
        let meta = provider_metadata(vec![], endpoints);
        assert!(!req.admits_type(&meta));
    }

    #[test]
    fn shell_command_requirement_detects_missing_binary() {
        let req = ShellCommandRequirement {
            command: "this-binary-absolutely-does-not-exist-9x7z".to_string(),
        };
        assert!(!req.is_satisfied());
    }

    #[test]
    fn shell_command_requirement_detects_present_binary() {
        let req = ShellCommandRequirement {
            command: "ls".to_string(),
        };
        assert!(req.is_satisfied());
    }
}
