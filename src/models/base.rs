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

    /// Snapshot this instance's data into a `ModelMetadata` value, using the
    /// same accessors the registry uses to describe a catalog entry. Lets
    /// command code display a model uniformly whether it came from a static
    /// catalog lookup or a live constructed instance (e.g. a `"custom"`
    /// model, whose real values only exist on the instance).
    fn to_metadata(&self) -> ModelMetadata {
        ModelMetadata {
            family: self.family().to_string(),
            version: self.version().to_string(),
            size: self.size(),
            context_length: self.context_length(),
            model_type: self.model_type().clone(),
            huggingface_repo: self.huggingface_repo().to_string(),
            native_dtype: self.native_dtype().to_string(),
            architecture: self.architecture().clone(),
            variants: self.variants().to_vec(),
            description: self.description().map(str::to_string),
            tags: self.tags().to_vec(),
            supported_functions: self.supported_functions().to_vec(),
        }
    }
}

/*-- ModelLookup ---------------------------------------------------------------*/

/// The narrow view of the model collection a dependent needs: turn a name into
/// the model it refers to, together with the provider and variant that go with
/// it. `ModelSource` is the production implementation; a capability sees only
/// this, so it can be resolved against a collection built for one launch
/// without knowing how that collection was built.
pub trait ModelLookup: Sync {
    /// The model configured under `model_id`, or an error naming what did not
    /// resolve. `requirement`, when given, is what the caller declared it
    /// needs of the model, and a model that does not satisfy it is an error
    /// rather than a silent mismatch found later at bind.
    fn resolve(
        &self,
        model_id: &str,
        requirement: Option<&crate::capabilities::ModelRequirement>,
    ) -> anyhow::Result<ConfiguredModel>;
}

/*-- ConfiguredModel -----------------------------------------------------------*/

/// Resolves a capability's `model_id` config field into a live model plus
/// whatever variant the user pinned, and the provider/endpoint checks every
/// model-backed `Capability::bind()` needs -- shared by `AgentModelCapability`,
/// `VisionMCPCapability`, and `SubAgentCapability` so each doesn't
/// reimplement the same `ModelSource`/variant-resolution logic.
pub struct ConfiguredModel {
    pub model: std::sync::Arc<dyn Model>,
    /// The provider `ModelConfig.provider_id` names, resolved alongside the
    /// model rather than rebuilt on each call.
    pub provider: std::sync::Arc<dyn crate::providers::Provider>,
    /// The raw `"format/precision"` string from `ModelConfig.variant`, if the
    /// user configured a specific variant. Used at bind time to resolve the
    /// provider-specific model alias.
    configured_variant: Option<String>,
}

/// Resolves a `"format/precision"` variant string (as stored in
/// `ModelConfig.variant`) to the matching `ModelVariant` in `variants`,
/// case-insensitively. Shared by `ConfiguredModel::resolve_variant` and
/// `ModelSource::take` (which needs the same lookup, on the model's real
/// unwrapped variants, to compute the provider alias used as a proxy route
/// key) so the matching rule lives in exactly one place.
pub(crate) fn find_variant<'a>(
    variants: &'a [ModelVariant],
    configured: Option<&str>,
) -> Option<&'a ModelVariant> {
    let variant_str = configured?;
    let (format, precision) = variant_str.split_once('/')?;
    variants.iter().find(|v| {
        v.format.eq_ignore_ascii_case(format) && v.precision.eq_ignore_ascii_case(precision)
    })
}

impl ConfiguredModel {
    /// The model, the provider it names, and the variant it was pinned to,
    /// assembled by the collection that owns all three.
    pub fn new(
        model: std::sync::Arc<dyn Model>,
        provider: std::sync::Arc<dyn crate::providers::Provider>,
        configured_variant: Option<String>,
    ) -> Self {
        Self {
            model,
            provider,
            configured_variant,
        }
    }

    /// Test-only escape hatch so capability unit tests can inject a fake
    /// model/provider without a real registry lookup.
    #[cfg(test)]
    pub(crate) fn for_test(
        model: std::sync::Arc<dyn Model>,
        provider: std::sync::Arc<dyn crate::providers::Provider>,
        configured_variant: Option<String>,
    ) -> Self {
        Self {
            model,
            provider,
            configured_variant,
        }
    }

    /// Resolves `configured_variant` (stored as `"format/precision"`) to the
    /// matching `ModelVariant` in the model's catalog variants, using the
    /// same case-insensitive lookup as the pull command.
    pub fn resolve_variant(&self) -> Option<&ModelVariant> {
        find_variant(self.model.variants(), self.configured_variant.as_deref())
    }

    /// The common core of every model-backed `Capability::bind()`: checks the
    /// provider supports `api_type`, finds the `api_type` endpoint for
    /// `endpoint_function`, and computes the provider-specific model
    /// name/alias to send. `model_id` is used only for error messages.
    ///
    /// Whether the model supports what the capability needs is settled at
    /// resolution, against the `ModelRequirement` the capability's metadata
    /// declares, so binding makes no judgement about the model. What stays
    /// here is per-request: `api_type` arrives on the `BindingRequest`, and
    /// `endpoint_function` selects which endpoint to look up rather than
    /// stating what the model must support -- `VisionMCPCapability` requires
    /// `ImageUnderstanding` of its model but looks the endpoint up via
    /// `Chat`, since that's the endpoint that actually serves vision requests.
    pub fn resolve_provider_endpoint(
        &self,
        model_id: &str,
        api_type: crate::providers::ApiType,
        endpoint_function: ModelFunction,
    ) -> anyhow::Result<(
        std::sync::Arc<dyn crate::providers::Provider>,
        crate::providers::ApiEndpoint,
        String,
    )> {
        let provider = self.provider.clone();
        anyhow::ensure!(
            provider.supported_api_types().contains(&api_type),
            "provider for model '{model_id}' does not support {api_type}"
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
            .model_alias(model_id.to_string(), self.resolve_variant())
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub enum ModelType {
    #[default]
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

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
mod to_metadata_tests {
    use super::*;

    struct FullTestModel;

    impl crate::registry::Named for FullTestModel {
        fn instance_id(&self) -> &str {
            "full-test-model"
        }
    }

    impl Model for FullTestModel {
        fn family(&self) -> &str {
            "Test Family"
        }
        fn version(&self) -> &str {
            "9.9"
        }
        fn size(&self) -> u64 {
            42
        }
        fn context_length(&self) -> u64 {
            2048
        }
        fn model_type(&self) -> &ModelType {
            &ModelType::Vision
        }
        fn huggingface_repo(&self) -> &str {
            "test/full"
        }
        fn native_dtype(&self) -> &str {
            "float16"
        }
        fn architecture(&self) -> &ModelArchitecture {
            static ARCH: std::sync::LazyLock<ModelArchitecture> =
                std::sync::LazyLock::new(|| ModelArchitecture {
                    num_hidden_layers: 1,
                    hidden_size: 2,
                    num_attention_heads: 3,
                    num_key_value_heads: 4,
                    head_dim: 5,
                    layer_types: vec![LayerTypeCount {
                        kind: LayerKind::FullAttention,
                        count: 1,
                    }],
                });
            &ARCH
        }
        fn variants(&self) -> &[ModelVariant] {
            static VARIANTS: std::sync::LazyLock<Vec<ModelVariant>> =
                std::sync::LazyLock::new(|| {
                    vec![ModelVariant {
                        format: "GGUF".to_string(),
                        precision: "Q4_K_M".to_string(),
                        size_gb: Some(1.0),
                        url: "http://example.com".to_string(),
                    }]
                });
            &VARIANTS
        }
        fn description(&self) -> Option<&str> {
            Some("a description")
        }
        fn tags(&self) -> &[String] {
            static TAGS: std::sync::LazyLock<Vec<String>> =
                std::sync::LazyLock::new(|| vec!["tag1".to_string()]);
            &TAGS
        }
        fn supported_functions(&self) -> &[ModelFunction] {
            static FUNCS: std::sync::LazyLock<Vec<ModelFunction>> =
                std::sync::LazyLock::new(|| vec![ModelFunction::Chat]);
            &FUNCS
        }
    }

    #[test]
    fn to_metadata_round_trips_every_field() {
        let model = FullTestModel;
        let md = model.to_metadata();
        assert_eq!(md.family, "Test Family");
        assert_eq!(md.version, "9.9");
        assert_eq!(md.size, 42);
        assert_eq!(md.context_length, 2048);
        assert_eq!(md.model_type, ModelType::Vision);
        assert_eq!(md.huggingface_repo, "test/full");
        assert_eq!(md.native_dtype, "float16");
        assert_eq!(md.architecture.num_hidden_layers, 1);
        assert_eq!(md.variants.len(), 1);
        assert_eq!(md.variants[0].format, "GGUF");
        assert_eq!(md.description, Some("a description".to_string()));
        assert_eq!(md.tags, vec!["tag1".to_string()]);
        assert_eq!(md.supported_functions, vec![ModelFunction::Chat]);
    }
}

#[cfg(test)]
mod configured_model_tests {
    use super::*;
    use crate::providers::{ApiEndpoint, ApiType};
    use crate::utils::test_support::FakeProvider;
    use std::collections::HashMap;

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
                variants,
            }),
            std::sync::Arc::new(provider),
            variant_str,
        )
    }

    #[test]
    fn resolve_provider_endpoint_succeeds_for_matching_provider_and_model() {
        let cm = configured_model(vec![ModelFunction::Chat], ok_provider(), None);
        let (provider, endpoint, model_name) = cm
            .resolve_provider_endpoint("test-model", ApiType::OpenAI, ModelFunction::Chat)
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
            .resolve_provider_endpoint("test-model", ApiType::OpenAI, ModelFunction::Chat)
            .err()
            .unwrap();
        assert!(err.to_string().contains("does not support OpenAI"));
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
            .resolve_provider_endpoint("test-model", ApiType::OpenAI, ModelFunction::Chat)
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
            .resolve_provider_endpoint("test-model", ApiType::OpenAI, ModelFunction::Chat)
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
            .resolve_provider_endpoint("test-model", ApiType::OpenAI, ModelFunction::Chat)
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
            .resolve_provider_endpoint("test-model", ApiType::OpenAI, ModelFunction::Chat)
            .unwrap();
        assert_eq!(model_name, "test-model");
    }

    #[test]
    fn resolve_variant_returns_none_without_a_configured_variant() {
        let cm = configured_model(vec![ModelFunction::Chat], ok_provider(), None);
        assert!(cm.resolve_variant().is_none());
    }
}
