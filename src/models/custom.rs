// Third Party
use serde::{Deserialize, Serialize};

// Local
use crate::models::base::{
    HasModelMetadata, Model, ModelArchitecture, ModelFunction, ModelMetadata, ModelType,
    ModelVariant,
};
use crate::registry::{ConfigConstructable, ConstructError, Named};

/*-- CustomModelConfig ---------------------------------------------------------*/

/// User-supplied description of a model that isn't in the built-in catalog.
/// Every field here becomes one of the values a codegen'd catalog model
/// would otherwise get from `resources/models.yaml`. `architecture` is
/// deliberately not configurable -- `CustomModel` always reports an empty
/// one, so `context_fit` degrades to `ContextFit::None` for custom models
/// rather than prompting for a full per-layer memory shape.
#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct CustomModelConfig {
    pub family: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub context_length: u64,
    #[serde(default)]
    pub model_type: ModelType,
    #[serde(default)]
    pub huggingface_repo: String,
    #[serde(default)]
    pub native_dtype: String,
    /// Pullable artifact variants, if any. Most custom models describe an
    /// endpoint that's already running somewhere rather than something
    /// `granite-cli` would download -- leave this empty and `model setup`
    /// skips variant selection, letting any configured provider serve it.
    #[serde(default)]
    pub variants: Vec<ModelVariant>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub supported_functions: Vec<ModelFunction>,
}

fn empty_architecture() -> ModelArchitecture {
    ModelArchitecture {
        num_hidden_layers: 0,
        hidden_size: 0,
        num_attention_heads: 0,
        num_key_value_heads: 0,
        head_dim: 0,
        layer_types: vec![],
    }
}

/*-- CustomModel -----------------------------------------------------------------*/

/// Hand-authored `Model` implementation for user-defined models, registered
/// in `MODEL_REGISTRY` under the catalog key `"custom"`. Unlike every other
/// registered model (codegen'd from `resources/models.yaml` with real,
/// compile-time-known values), a `CustomModel` instance's real values only
/// exist once it's been constructed from a user's `CustomModelConfig` --
/// `HasModelMetadata::metadata()` below returns a placeholder used only for
/// catalog browsing before that.
pub struct CustomModel {
    instance_id: String,
    config: CustomModelConfig,
}

impl ConfigConstructable for CustomModel {
    type Config = CustomModelConfig;

    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        let config: CustomModelConfig =
            serde_json::from_value(cfg.clone()).map_err(ConstructError::settings)?;
        Ok(Self {
            instance_id: instance_id.to_string(),
            config,
        })
    }
}

impl Named for CustomModel {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

impl Model for CustomModel {
    fn family(&self) -> &str {
        &self.config.family
    }

    fn version(&self) -> &str {
        &self.config.version
    }

    fn size(&self) -> u64 {
        self.config.size
    }

    fn context_length(&self) -> u64 {
        self.config.context_length
    }

    fn model_type(&self) -> &ModelType {
        &self.config.model_type
    }

    fn huggingface_repo(&self) -> &str {
        &self.config.huggingface_repo
    }

    fn native_dtype(&self) -> &str {
        &self.config.native_dtype
    }

    fn architecture(&self) -> &ModelArchitecture {
        static ARCHITECTURE: std::sync::LazyLock<ModelArchitecture> =
            std::sync::LazyLock::new(empty_architecture);
        &ARCHITECTURE
    }

    fn variants(&self) -> &[ModelVariant] {
        &self.config.variants
    }

    fn description(&self) -> Option<&str> {
        self.config.description.as_deref()
    }

    fn tags(&self) -> &[String] {
        &self.config.tags
    }

    fn supported_functions(&self) -> &[ModelFunction] {
        &self.config.supported_functions
    }
}

impl HasModelMetadata for CustomModel {
    fn metadata() -> ModelMetadata {
        ModelMetadata {
            family: "Custom Model".to_string(),
            version: String::new(),
            size: 0,
            context_length: 0,
            model_type: ModelType::Text,
            huggingface_repo: String::new(),
            native_dtype: String::new(),
            architecture: empty_architecture(),
            variants: vec![],
            description: Some(
                "User-defined model. Run `model setup custom` to describe its family, size, \
                 context length, and (optionally) pullable variants."
                    .to_string(),
            ),
            tags: vec!["custom".to_string()],
            supported_functions: vec![],
        }
    }
}

/*-- tests -----------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_a_placeholder_with_no_variants() {
        let md = CustomModel::metadata();
        assert_eq!(md.family, "Custom Model");
        assert!(md.variants.is_empty());
        assert!(md.tags.contains(&"custom".to_string()));
    }

    #[test]
    fn new_populates_fields_from_config() {
        let cfg = serde_json::json!({
            "family": "My Local Model",
            "version": "1.0",
            "size": 7_000_000_000u64,
            "context_length": 8192,
            "model_type": "Text",
            "huggingface_repo": "me/my-model",
            "native_dtype": "bfloat16",
            "variants": [],
            "tags": ["chat"],
            "supported_functions": ["Chat"],
        });
        let model = CustomModel::new("my-nickname", &cfg).unwrap();
        assert_eq!(model.instance_id(), "my-nickname");
        assert_eq!(model.family(), "My Local Model");
        assert_eq!(model.size(), 7_000_000_000);
        assert_eq!(model.context_length(), 8192);
        assert_eq!(model.model_type(), &ModelType::Text);
        assert_eq!(model.huggingface_repo(), "me/my-model");
        assert!(model.variants().is_empty());
        assert_eq!(model.tags(), &["chat".to_string()]);
        assert_eq!(model.supported_functions(), &[ModelFunction::Chat]);
        assert!(model.architecture().layer_types.is_empty());
    }

    #[test]
    fn config_schema_exposes_expected_properties() {
        let schema = schemars::schema_for!(CustomModelConfig);
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("object schema with properties");
        for field in [
            "family",
            "context_length",
            "model_type",
            "variants",
            "supported_functions",
        ] {
            assert!(properties.contains_key(field), "missing field {field}");
        }
    }

    #[test]
    fn to_metadata_round_trips_configured_values() {
        let cfg = serde_json::json!({
            "family": "My Local Model",
            "context_length": 4096,
            "model_type": "Vision",
        });
        let model = CustomModel::new("nick", &cfg).unwrap();
        let md = model.to_metadata();
        assert_eq!(md.family, "My Local Model");
        assert_eq!(md.context_length, 4096);
        assert_eq!(md.model_type, ModelType::Vision);
    }
}
