// Third Party
use serde::{Deserialize, Serialize};

// Local
use crate::models::context_fit::{self, ContextFit};
use crate::registry::ConfigConstructable;
use crate::utils::Searchable;
use crate::utils::hardware::HardwareProfile;

/*-- ModelFunction Enum ------------------------------------------------------*/

/// Functional capabilities that models can provide
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub enum ModelFunction {
    /*-- Chat Functions --*/
    /// Text-based conversational interaction
    Chat,
    /// Tool inputs and invocations
    ToolCalling,
    /// Chain-of-thought reasoning
    Thinking,
    /// Visual content analysis and understanding
    ImageUnderstanding,
    /// Detect harms
    Guardian,

    /*-- Embedding Functions --*/
    /// Vector representation generation for text
    Embeddings,

    /*-- Audio Functions --*/
    /// Audio-to-text transcription
    Transcription,
    /// Audio translation
    Translation,
    /// Speaker attribution in audio
    SpeakerAttribution,
    /// Keyword biasing in audio
    KeywordBiasing,
}

impl std::fmt::Display for ModelFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelFunction::Chat => write!(f, "Chat"),
            ModelFunction::ToolCalling => write!(f, "ToolCalling"),
            ModelFunction::Thinking => write!(f, "Thinking"),
            ModelFunction::ImageUnderstanding => write!(f, "Image Understanding"),
            ModelFunction::Guardian => write!(f, "Guardian"),
            ModelFunction::Embeddings => write!(f, "Embeddings"),
            ModelFunction::Transcription => write!(f, "Transcription"),
            ModelFunction::Translation => write!(f, "Translation"),
            ModelFunction::SpeakerAttribution => write!(f, "Speaker Attribution"),
            ModelFunction::KeywordBiasing => write!(f, "Keyword Biasing"),
        }
    }
}

/*-- Architecture Types -------------------------------------------------------*/

/// The per-layer memory-shape category a transformer layer falls into. Each
/// variant carries whatever shape data its calculation needs; models hold
/// counts per kind rather than one entry per layer, since no known
/// architecture mixes different shapes within the same kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LayerKind {
    FullAttention,
    SlidingAttention { window: u64 },
    Recurrent(MambaShape),
}

/// Shape parameters for a Mamba/SSM recurrent layer's fixed-size state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MambaShape {
    pub d_conv: u64,
    pub d_state: u64,
    pub d_inner: u64,
    pub n_groups: u64,
}

/// A count of layers sharing one `LayerKind`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerTypeCount {
    pub kind: LayerKind,
    pub count: u64,
}

/// The architectural shape of a model, as derived from its config.json.
/// Sized purely for KV-cache/recurrent-state memory estimation -- MoE
/// routing fields are intentionally not represented here, since they affect
/// compute, not memory footprint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelArchitecture {
    pub num_hidden_layers: u64,
    pub hidden_size: u64,
    pub num_attention_heads: u64,
    pub num_key_value_heads: u64,
    pub head_dim: u64,
    pub layer_types: Vec<LayerTypeCount>,
}

/*-- Model Trait -------------------------------------------------------------*/

/// Core trait for model implementations.
/// All models must implement this trait along with ConfigConstructable.
pub trait Model: crate::registry::Named + Send + Sync {
    /// Get the model family name
    fn family(&self) -> &str;

    /// Get the model version
    fn version(&self) -> &str;

    /// Get the model size in parameters
    fn size(&self) -> u64;

    /// Get the context length
    fn context_length(&self) -> u64;

    /// Get the model type
    fn model_type(&self) -> &ModelType;

    /// Get the HuggingFace repository
    fn huggingface_repo(&self) -> &str;

    /// Get the model's native (training/checkpoint) numerical dtype, e.g.
    /// "bfloat16". Used only for KV-cache precision heuristics, never for
    /// weight-size estimation.
    fn native_dtype(&self) -> &str;

    /// Get the model's architectural shape (layer counts per `LayerKind`,
    /// head/hidden dims), used for context-fit memory estimation.
    fn architecture(&self) -> &ModelArchitecture;

    /// Get available variants
    fn variants(&self) -> &[ModelVariant];

    /// Get description if available
    fn description(&self) -> Option<&str>;

    /// Get tags
    fn tags(&self) -> &[String];

    /// Model functions this model supports (OR logic - any of these)
    fn supported_functions(&self) -> &[ModelFunction];

    /// Estimate whether `variant` will run on `hardware` at its full
    /// configured context length, at a reduced context length, or not at
    /// all. Derives KV-cache/recurrent-state memory from the model's actual
    /// per-layer-kind architecture rather than a flat per-parameter
    /// heuristic.
    fn context_fit(&self, variant: &ModelVariant, hardware: &HardwareProfile) -> ContextFit {
        context_fit::estimate(
            self.context_length(),
            self.architecture(),
            self.native_dtype(),
            variant,
            hardware,
        )
    }

    /// Resolved provider-construction data this instance was built with (see
    /// `ModelSource::from_config`). `None` for bare catalog instances that
    /// weren't constructed from a configured model.
    fn provider_config(&self) -> Option<&crate::config::ProviderConfig> {
        None
    }

    /// Construct this model's provider from its resolved `provider_config`.
    /// Consolidates the provider_id -> Provider construction that used to be
    /// duplicated at each call site in `commands/model.rs`.
    fn provider(&self) -> anyhow::Result<Box<dyn crate::providers::Provider>> {
        let pc = self
            .provider_config()
            .ok_or_else(|| anyhow::anyhow!("model has no configured provider"))?;
        crate::providers::PROVIDER_REGISTRY
            .construct(
                &pc.provider_type,
                &pc.provider_id,
                &pc.config,
                &crate::config::Config::default(),
            )
            .map_err(|e| anyhow::anyhow!(e))
    }
}

/*-- ConfiguredModel -----------------------------------------------------------*/

/// Resolves a capability's `model_id` config field into a live model plus
/// whatever variant the user pinned, and the provider/endpoint checks every
/// model-backed `Capability::bind()` needs -- shared by `AgentModelCapability`,
/// `VisionMCPCapability`, and `SubAgentCapability` so each doesn't
/// reimplement the same `ModelSource`/variant-resolution logic.
pub struct ConfiguredModel {
    pub model: std::sync::Arc<dyn Model>,
    /// The raw `"format/precision"` string from `ModelConfig.variant`, if the
    /// user configured a specific variant. Used at bind time to resolve the
    /// provider-specific model alias.
    configured_variant: Option<String>,
}

impl ConfiguredModel {
    /// Resolves `model_id` through `ModelSource::from_config`, which handles
    /// provider resolution (so `model.provider()` works at bind time) and,
    /// when a usage-tracking session is active, transparently wraps the
    /// model in a local tracking proxy. Panics if the model isn't
    /// found/constructible -- capabilities' `ConfigConstructable::new` is
    /// infallible by trait signature, so this preserves that contract
    /// exactly.
    pub fn resolve(model_id: &str, global_config: &crate::config::Config) -> Self {
        let mut source = crate::models::ModelSource::from_config(global_config);
        let model = source.take(model_id).unwrap_or_else(|| {
            panic!("Configured model '{model_id}' not found or could not be constructed")
        });
        let configured_variant = global_config
            .models
            .get(model_id)
            .and_then(|mc| mc.variant.clone());
        Self {
            model,
            configured_variant,
        }
    }

    /// Test-only escape hatch so capability unit tests can inject a fake
    /// model/provider without a real registry lookup.
    #[cfg(test)]
    pub(crate) fn for_test(
        model: std::sync::Arc<dyn Model>,
        configured_variant: Option<String>,
    ) -> Self {
        Self {
            model,
            configured_variant,
        }
    }

    /// Resolves `configured_variant` (stored as `"format/precision"`) to the
    /// matching `ModelVariant` in the model's catalog variants, using the
    /// same case-insensitive lookup as the pull command.
    pub fn resolve_variant(&self) -> Option<&ModelVariant> {
        let variant_str = self.configured_variant.as_deref()?;
        let (format, precision) = variant_str.split_once('/')?;
        self.model.variants().iter().find(|v| {
            v.format.eq_ignore_ascii_case(format) && v.precision.eq_ignore_ascii_case(precision)
        })
    }

    /// The common core of every model-backed `Capability::bind()`: resolves
    /// the provider, checks it supports `api_type`, checks the model
    /// supports `required_function`, finds the `api_type` endpoint for
    /// `endpoint_function`, and computes the provider-specific model
    /// name/alias to send. `required_function` and `endpoint_function` are
    /// separate parameters because a capability's model-support requirement
    /// and its endpoint lookup can differ (e.g. `VisionMCPCapability` needs
    /// `ImageUnderstanding` on the model but looks up the endpoint via
    /// `Chat`, since that's the endpoint that actually serves vision
    /// requests). `model_id` is used only for error messages.
    pub fn resolve_provider_endpoint(
        &self,
        model_id: &str,
        api_type: crate::providers::ApiType,
        required_function: ModelFunction,
        endpoint_function: ModelFunction,
    ) -> anyhow::Result<(
        Box<dyn crate::providers::Provider>,
        crate::providers::ApiEndpoint,
        String,
    )> {
        let provider = self
            .model
            .provider()
            .map_err(|e| anyhow::anyhow!("model '{model_id}' has no usable provider: {e}"))?;
        anyhow::ensure!(
            provider.supported_api_types().contains(&api_type),
            "provider for model '{model_id}' does not support {api_type}"
        );
        anyhow::ensure!(
            self.model
                .supported_functions()
                .contains(&required_function),
            "model '{model_id}' does not support {required_function}"
        );
        let endpoint = provider
            .endpoints_for_function(&endpoint_function)
            .into_iter()
            .find(|e| e.api_type() == api_type)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "provider for model '{model_id}' has no {api_type} endpoint for {endpoint_function}"
                )
            })?;
        let model_name = provider
            .model_alias(self.resolve_variant())
            .unwrap_or_else(|| model_id.to_string());
        Ok((provider, endpoint, model_name))
    }
}

/*-- Metadata Types ----------------------------------------------------------*/

/// Metadata describing a model implementation.
/// This is what the factory returns when querying model information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub family: String,
    pub version: String,
    pub size: u64,
    pub context_length: u64,
    pub model_type: ModelType,
    pub huggingface_repo: String,
    pub native_dtype: String,
    pub architecture: ModelArchitecture,
    pub variants: Vec<ModelVariant>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub supported_functions: Vec<ModelFunction>,
}

impl ModelMetadata {
    /// Format the parameter count as a human-readable string.
    /// Uses `M` (millions) for sub-billion models, `B` (billions) otherwise.
    pub fn format_size(&self) -> String {
        if self.size >= 1_000_000_000 {
            format!("{}B", self.size / 1_000_000_000)
        } else {
            format!("{}M", self.size / 1_000_000)
        }
    }
}

impl std::fmt::Display for ModelMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}) - {} params, {} context, Type: {}",
            self.family,
            self.format_size(),
            self.context_length,
            self.model_type
        )
    }
}

impl Searchable for ModelMetadata {
    fn search_fields(&self) -> Vec<&str> {
        let mut fields: Vec<&str> = vec![self.family.as_str()];
        if let Some(desc) = &self.description {
            fields.push(desc.as_str());
        }
        fields.extend(self.tags.iter().map(String::as_str));
        fields
    }
}

/*-- Supporting Types --------------------------------------------------------*/

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ModelType {
    Text,
    Vision,
    Speech,
    Embedding,
}

impl std::fmt::Display for ModelType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelType::Text => write!(f, "Text"),
            ModelType::Vision => write!(f, "Vision"),
            ModelType::Speech => write!(f, "Speech"),
            ModelType::Embedding => write!(f, "Embedding"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelVariant {
    pub format: String,
    pub precision: String,
    pub size_gb: Option<f64>,
    pub url: String,
}

/*-- Factory Definition ------------------------------------------------------*/

use crate::define_factory;

define_factory!(Model, ModelMetadata, ModelFactory);

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod format_size_tests {
    use super::*;

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

    fn metadata_with_size(size: u64) -> ModelMetadata {
        ModelMetadata {
            family: "Test".to_string(),
            version: "1.0".to_string(),
            size,
            context_length: 4096,
            model_type: ModelType::Text,
            huggingface_repo: "test/test".to_string(),
            native_dtype: "bfloat16".to_string(),
            architecture: test_architecture(),
            variants: vec![],
            description: None,
            tags: vec![],
            supported_functions: vec![],
        }
    }

    #[test]
    fn format_size_billions() {
        assert_eq!(metadata_with_size(8_000_000_000).format_size(), "8B");
    }

    #[test]
    fn format_size_millions() {
        assert_eq!(metadata_with_size(258_000_000).format_size(), "258M");
    }

    #[test]
    fn format_size_boundary_is_one_billion() {
        assert_eq!(metadata_with_size(1_000_000_000).format_size(), "1B");
        assert_eq!(metadata_with_size(999_999_999).format_size(), "999M");
    }

    #[test]
    fn format_size_30m_model() {
        assert_eq!(metadata_with_size(30_295_296).format_size(), "30M");
    }
}

#[cfg(test)]
mod searchable_tests {
    use super::*;

    fn metadata(family: &str, description: Option<&str>, tags: Vec<&str>) -> ModelMetadata {
        ModelMetadata {
            family: family.to_string(),
            version: "1.0".to_string(),
            size: 8_000_000_000,
            context_length: 4096,
            model_type: ModelType::Text,
            huggingface_repo: "ibm-granite/test".to_string(),
            native_dtype: "bfloat16".to_string(),
            architecture: ModelArchitecture {
                num_hidden_layers: 32,
                hidden_size: 4096,
                num_attention_heads: 32,
                num_key_value_heads: 8,
                head_dim: 128,
                layer_types: vec![LayerTypeCount {
                    kind: LayerKind::FullAttention,
                    count: 32,
                }],
            },
            variants: vec![],
            description: description.map(String::from),
            tags: tags.into_iter().map(String::from).collect(),
            supported_functions: vec![],
        }
    }

    #[test]
    fn searchable_fields_includes_family() {
        let m = metadata("Granite 3.1", None, vec![]);
        assert!(m.search_fields().contains(&"Granite 3.1"));
    }

    #[test]
    fn searchable_fields_includes_description_when_present() {
        let m = metadata("Granite 3.1", Some("A text model"), vec![]);
        assert!(m.search_fields().contains(&"A text model"));
    }

    #[test]
    fn searchable_fields_omits_description_when_absent() {
        let m = metadata("Granite 3.1", None, vec![]);
        assert_eq!(m.search_fields().len(), 1);
    }

    #[test]
    fn searchable_fields_includes_tags() {
        let m = metadata("Granite 3.1", None, vec!["instruct", "chat"]);
        let fields = m.search_fields();
        assert!(fields.contains(&"instruct"));
        assert!(fields.contains(&"chat"));
    }
}

#[cfg(test)]
mod configured_model_tests {
    use super::*;
    use crate::providers::{
        ApiEndpoint, ApiType, HealthStatus, ModelFormat, Provider, ProviderError,
    };
    use crate::registry::{ConfigConstructable, Secret};
    use std::collections::HashMap;

    #[derive(Clone, Default)]
    struct FakeProvider {
        instance_id: String,
        base_url: String,
        api_key: Option<Secret>,
        verify_ssl: bool,
        api_types: Vec<ApiType>,
        endpoints: HashMap<ModelFunction, Vec<ApiEndpoint>>,
        alias: Option<String>,
    }

    impl ConfigConstructable for FakeProvider {
        type Config = crate::registry::NoConfig;
        fn new(_: &str, _: &serde_json::Value, _: &crate::config::Config) -> Self {
            unimplemented!("not used in tests")
        }
    }

    impl crate::registry::Named for FakeProvider {
        fn instance_id(&self) -> &str {
            &self.instance_id
        }
    }

    #[async_trait::async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> &str {
            "Fake Provider"
        }
        fn function_endpoints(&self) -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
            self.endpoints.clone()
        }
        fn supported_api_types(&self) -> Vec<ApiType> {
            self.api_types.clone()
        }
        fn base_url(&self) -> &str {
            &self.base_url
        }
        fn api_key(&self) -> Option<&Secret> {
            self.api_key.as_ref()
        }
        fn verify_ssl(&self) -> bool {
            self.verify_ssl
        }
        fn supported_formats(&self) -> Vec<ModelFormat> {
            vec![]
        }
        fn model_alias(&self, _variant: Option<&ModelVariant>) -> Option<String> {
            self.alias.clone()
        }
        async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
            unimplemented!("not used in tests")
        }
    }

    fn ok_provider() -> FakeProvider {
        let mut endpoints = HashMap::new();
        endpoints.insert(ModelFunction::Chat, vec![ApiEndpoint::OpenAIChat]);
        FakeProvider {
            instance_id: "my-ollama".to_string(),
            base_url: "http://localhost:11434".to_string(),
            api_key: None,
            verify_ssl: true,
            api_types: vec![ApiType::OpenAI],
            endpoints,
            alias: None,
        }
    }

    struct TestModel {
        supported_functions: Vec<ModelFunction>,
        provider: FakeProvider,
        variants: Vec<ModelVariant>,
    }

    impl crate::registry::Named for TestModel {
        fn instance_id(&self) -> &str {
            "test-model"
        }
    }

    impl Model for TestModel {
        fn family(&self) -> &str {
            "Test"
        }
        fn version(&self) -> &str {
            "1.0"
        }
        fn size(&self) -> u64 {
            1
        }
        fn context_length(&self) -> u64 {
            4096
        }
        fn model_type(&self) -> &ModelType {
            &ModelType::Text
        }
        fn huggingface_repo(&self) -> &str {
            "test/test"
        }
        fn native_dtype(&self) -> &str {
            "bfloat16"
        }
        fn architecture(&self) -> &ModelArchitecture {
            unimplemented!("not used in tests")
        }
        fn variants(&self) -> &[ModelVariant] {
            &self.variants
        }
        fn description(&self) -> Option<&str> {
            None
        }
        fn tags(&self) -> &[String] {
            &[]
        }
        fn supported_functions(&self) -> &[ModelFunction] {
            &self.supported_functions
        }
        fn provider(&self) -> anyhow::Result<Box<dyn Provider>> {
            Ok(Box::new(self.provider.clone()))
        }
    }

    fn configured_model(
        functions: Vec<ModelFunction>,
        provider: FakeProvider,
        variant: Option<(&str, Vec<ModelVariant>)>,
    ) -> ConfiguredModel {
        let (variant_str, variants) = variant
            .map(|(s, v)| (Some(s.to_string()), v))
            .unwrap_or((None, vec![]));
        ConfiguredModel::for_test(
            std::sync::Arc::new(TestModel {
                supported_functions: functions,
                provider,
                variants,
            }),
            variant_str,
        )
    }

    #[test]
    fn resolve_provider_endpoint_succeeds_for_matching_provider_and_model() {
        let cm = configured_model(vec![ModelFunction::Chat], ok_provider(), None);
        let (provider, endpoint, model_name) = cm
            .resolve_provider_endpoint(
                "test-model",
                ApiType::OpenAI,
                ModelFunction::Chat,
                ModelFunction::Chat,
            )
            .unwrap();
        assert_eq!(provider.base_url(), "http://localhost:11434");
        assert_eq!(endpoint, ApiEndpoint::OpenAIChat);
        assert_eq!(model_name, "test-model");
    }

    #[test]
    fn resolve_provider_endpoint_fails_when_provider_lacks_api_type() {
        let cm = configured_model(
            vec![ModelFunction::Chat],
            FakeProvider {
                api_types: vec![ApiType::Ollama],
                ..ok_provider()
            },
            None,
        );
        let err = cm
            .resolve_provider_endpoint(
                "test-model",
                ApiType::OpenAI,
                ModelFunction::Chat,
                ModelFunction::Chat,
            )
            .err()
            .unwrap();
        assert!(err.to_string().contains("does not support OpenAI"));
    }

    #[test]
    fn resolve_provider_endpoint_fails_when_model_lacks_required_function() {
        let cm = configured_model(vec![ModelFunction::Embeddings], ok_provider(), None);
        let err = cm
            .resolve_provider_endpoint(
                "test-model",
                ApiType::OpenAI,
                ModelFunction::Chat,
                ModelFunction::Chat,
            )
            .err()
            .unwrap();
        assert!(err.to_string().contains("does not support Chat"));
    }

    #[test]
    fn resolve_provider_endpoint_fails_when_no_matching_endpoint() {
        let cm = configured_model(
            vec![ModelFunction::Chat],
            FakeProvider {
                endpoints: HashMap::from([(ModelFunction::Chat, vec![ApiEndpoint::OllamaChat])]),
                api_types: vec![ApiType::OpenAI, ApiType::Ollama],
                ..ok_provider()
            },
            None,
        );
        let err = cm
            .resolve_provider_endpoint(
                "test-model",
                ApiType::OpenAI,
                ModelFunction::Chat,
                ModelFunction::Chat,
            )
            .err()
            .unwrap();
        assert!(err.to_string().contains("has no OpenAI endpoint for Chat"));
    }

    #[test]
    fn resolve_provider_endpoint_allows_required_and_endpoint_functions_to_differ() {
        // Mirrors VisionMCPCapability: model must support ImageUnderstanding,
        // but the endpoint is looked up via Chat.
        let cm = configured_model(vec![ModelFunction::ImageUnderstanding], ok_provider(), None);
        let (_, endpoint, _) = cm
            .resolve_provider_endpoint(
                "test-model",
                ApiType::OpenAI,
                ModelFunction::ImageUnderstanding,
                ModelFunction::Chat,
            )
            .unwrap();
        assert_eq!(endpoint, ApiEndpoint::OpenAIChat);
    }

    #[test]
    fn resolve_provider_endpoint_uses_provider_alias_when_variant_matches() {
        let variant = ModelVariant {
            format: "Ollama".to_string(),
            precision: "Q4_K_M".to_string(),
            size_gb: Some(5.3),
            url: "https://ollama.com/library/granite4.1:8b".to_string(),
        };
        let cm = configured_model(
            vec![ModelFunction::Chat],
            FakeProvider {
                alias: Some("granite4.1:8b".to_string()),
                ..ok_provider()
            },
            Some(("Ollama/Q4_K_M", vec![variant])),
        );
        let (_, _, model_name) = cm
            .resolve_provider_endpoint(
                "test-model",
                ApiType::OpenAI,
                ModelFunction::Chat,
                ModelFunction::Chat,
            )
            .unwrap();
        assert_eq!(model_name, "granite4.1:8b");
    }

    #[test]
    fn resolve_provider_endpoint_falls_back_to_catalog_id_when_alias_is_none() {
        let variant = ModelVariant {
            format: "Ollama".to_string(),
            precision: "Q4_K_M".to_string(),
            size_gb: Some(5.3),
            url: "https://ollama.com/library/granite4.1:8b".to_string(),
        };
        let cm = configured_model(
            vec![ModelFunction::Chat],
            ok_provider(),
            Some(("Ollama/Q4_K_M", vec![variant])),
        );
        let (_, _, model_name) = cm
            .resolve_provider_endpoint(
                "test-model",
                ApiType::OpenAI,
                ModelFunction::Chat,
                ModelFunction::Chat,
            )
            .unwrap();
        assert_eq!(model_name, "test-model");
    }

    #[test]
    fn resolve_variant_returns_none_without_a_configured_variant() {
        let cm = configured_model(vec![ModelFunction::Chat], ok_provider(), None);
        assert!(cm.resolve_variant().is_none());
    }
}
