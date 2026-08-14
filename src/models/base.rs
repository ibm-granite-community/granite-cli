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
pub trait Model: Send + Sync {
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

    /// The configured provider instance id this model was built with, if any.
    /// `None` for bare catalog instances that weren't constructed from a
    /// configured model. Used by `ModelSource::provider_for()` to resolve
    /// provider data at call time rather than baking a value copy into the struct.
    fn provider_id(&self) -> Option<&str> {
        None
    }

    /// Construct this model's provider.
    ///
    /// For models constructed within a `ModelSource`, prefer
    /// `ModelSource::provider_for(model)` which resolves from the source's
    /// live provider config map. This default impl always returns an error;
    /// concrete impls that manage their own provider may override it.
    fn provider(&self) -> anyhow::Result<Box<dyn crate::providers::Provider>> {
        Err(anyhow::anyhow!(
            "model has no configured provider — use ModelSource::provider_for(model)"
        ))
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
    pub size_gb: f64,
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
