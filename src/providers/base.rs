use crate::models::{ModelFunction, ModelType, ModelVariant};
use crate::registry::{ConfigConstructable, Secret};
use crate::utils::Searchable;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/*-- ApiType Enum ------------------------------------------------------------*/

/// API protocol families that providers can implement
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum ApiType {
    /// OpenAI-compatible API protocol
    OpenAI,
    /// Ollama API protocol
    Ollama,
    /// Anthropic API protocol
    Anthropic,
    /// Jev Decision API
    Jev,
}

impl std::fmt::Display for ApiType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiType::OpenAI => write!(f, "OpenAI"),
            ApiType::Ollama => write!(f, "Ollama"),
            ApiType::Anthropic => write!(f, "Anthropic"),
            ApiType::Jev => write!(f, "Jev"),
        }
    }
}

/*-- ApiEndpoint Enum --------------------------------------------------------*/

/// Specific API endpoints within an API family
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) enum ApiEndpoint {
    /// /v1/chat/completions
    OpenAIChat,
    /// /v1/embeddings
    OpenAIEmbeddings,
    /// /v1/audio/transcriptions
    OpenAIAudioTranscription,
    /// /api/chat
    OllamaChat,
    /// /api/embeddings
    OllamaEmbeddings,
    /// /v1/messages
    AnthropicMessages,
    /// /v1/systemone
    JevSystemOne,
}

impl ApiEndpoint {
    /// Returns the API type this endpoint belongs to
    pub(crate) fn api_type(&self) -> ApiType {
        match self {
            ApiEndpoint::OpenAIChat
            | ApiEndpoint::OpenAIEmbeddings
            | ApiEndpoint::OpenAIAudioTranscription => ApiType::OpenAI,

            ApiEndpoint::OllamaChat | ApiEndpoint::OllamaEmbeddings => ApiType::Ollama,

            ApiEndpoint::AnthropicMessages => ApiType::Anthropic,
            ApiEndpoint::JevSystemOne => ApiType::Jev,
        }
    }

    /// Returns the endpoint path
    pub(crate) fn path(&self) -> &'static str {
        match self {
            ApiEndpoint::OpenAIChat => "/v1/chat/completions",
            ApiEndpoint::OpenAIEmbeddings => "/v1/embeddings",
            ApiEndpoint::OpenAIAudioTranscription => "/v1/audio/transcriptions",
            ApiEndpoint::OllamaChat => "/api/chat",
            ApiEndpoint::OllamaEmbeddings => "/api/embeddings",
            ApiEndpoint::AnthropicMessages => "/v1/messages",
            ApiEndpoint::JevSystemOne => "/v1/systemone",
        }
    }

    /// Returns the model functions this endpoint provides
    fn provides_functions(&self) -> Vec<ModelFunction> {
        match self {
            ApiEndpoint::OpenAIChat | ApiEndpoint::OllamaChat | ApiEndpoint::AnthropicMessages => {
                vec![
                    ModelFunction::Chat,
                    ModelFunction::ToolCalling,
                    ModelFunction::Thinking,
                    ModelFunction::ImageUnderstanding,
                    ModelFunction::Guardian,
                ]
            }

            ApiEndpoint::OpenAIEmbeddings | ApiEndpoint::OllamaEmbeddings => {
                vec![ModelFunction::Embeddings]
            }

            ApiEndpoint::OpenAIAudioTranscription => vec![
                ModelFunction::Transcription,
                ModelFunction::Translation,
                ModelFunction::SpeakerAttribution,
                ModelFunction::KeywordBiasing,
            ],

            ApiEndpoint::JevSystemOne => vec![ModelFunction::Decision],
        }
    }
}

impl std::fmt::Display for ApiEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} ({})",
            self.api_type(),
            self.path(),
            self.provides_functions()
                .iter()
                .map(|m| m.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/*-- Provider Trait ----------------------------------------------------------*/

/// Core trait for provider implementations.
/// All providers must implement this trait along with ConfigConstructable.
#[async_trait]
pub(crate) trait Provider: crate::registry::Named + Send + Sync {
    fn name(&self) -> &str;

    /// Returns the mapping of model functions to API endpoints this provider instance supports.
    /// This is runtime/configuration-specific.
    fn function_endpoints(&self) -> HashMap<ModelFunction, Vec<ApiEndpoint>>;

    /// Returns the API types this provider implementation supports (type-level).
    fn supported_api_types(&self) -> Vec<ApiType>;

    /// Returns the configured base URL for this provider instance.
    fn base_url(&self) -> &str;

    /// Returns the configured API key for this provider instance, if any.
    fn api_key(&self) -> Option<&Secret>;

    /// Returns the configured custom HTTP headers for this provider instance, if any.
    /// Custom headers are used as-is in outgoing requests. Keys are the header names
    /// (strings) and values are secret tokens.
    fn custom_headers(&self) -> Option<HashMap<String, Secret>> {
        None
    }

    /// Returns whether this provider instance verifies SSL certificates.
    fn verify_ssl(&self) -> bool;

    // Model support
    #[cfg_attr(not(test), allow(dead_code))]
    fn supported_formats(&self) -> Vec<ModelFormat>;
    fn can_run_model(&self, _variant_format: &str, _variant_precision: &str) -> bool {
        true
    }

    /// Returns the provider-specific model name for the given variant, if it
    /// differs from the catalog model ID. Providers whose server uses a different
    /// naming convention (e.g. Ollama's `granite4.1:8b` vs the catalog ID
    /// `granite-4.1-8b`) override this to derive the alias from the variant URL.
    /// The default returns `None`, meaning the catalog ID should be used as-is.
    fn model_alias(&self, _model_id: String, _variant: Option<&ModelVariant>) -> Option<String> {
        None
    }

    // Health
    async fn health_check(&self) -> Result<HealthStatus, ProviderError>;

    /// Helper: Check if this provider can serve a specific function
    fn supports_function(&self, function: &ModelFunction) -> bool {
        self.function_endpoints().contains_key(function)
    }

    /// Helper: Get endpoints for a specific function
    fn endpoints_for_function(&self, function: &ModelFunction) -> Vec<ApiEndpoint> {
        self.function_endpoints()
            .get(function)
            .cloned()
            .unwrap_or_default()
    }

    /// Pull/download a model variant so this provider can run it.
    ///
    /// Returns one of the `PullResult` variants on success, or `Err(ProviderError)`
    /// if the pull was attempted but failed. Local providers (Ollama, LM Studio,
    /// llama.cpp) override this to drive their server's native pull API,
    /// reporting progress through `ui`. The default implementation returns
    /// `PullResult::Unnecessary`.
    async fn pull_model(
        &self,
        _model: &crate::models::ModelMetadata,
        _variant: &crate::models::ModelVariant,
        _ui: &dyn crate::utils::ui::Ui,
    ) -> Result<PullResult, ProviderError> {
        Ok(PullResult::Unnecessary)
    }

    /// Whether this instance is configured to mirror the provider's reported
    /// model list into configured models. Providers whose type config has no
    /// `discover_models` setting keep the default, which also means "cannot
    /// list models".
    fn discover_models(&self) -> bool {
        false
    }

    /// Models this provider instance currently serves. The default says the
    /// provider cannot be asked, so a caller can tell "unsupported" from
    /// "supported and empty".
    async fn list_models(&self) -> Result<Vec<ProviderModel>, ProviderError> {
        Err(ProviderError::Unsupported("model discovery".to_string()))
    }
}

/*-- ProviderModel -----------------------------------------------------------*/

/// One model a provider reports as available, reduced to what its listing
/// endpoint says. Fields the provider does not report stay `None`, never
/// guessed here -- the importer decides defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderModel {
    /// The name requests must use. Always present.
    pub(crate) id: String,
    /// Human-facing name, when the provider gives one.
    pub(crate) display_name: Option<String>,
    /// Context window in tokens, when reported.
    pub(crate) context_length: Option<u64>,
    /// Parameter count (not download size), when reported.
    pub(crate) parameter_count: Option<u64>,
    /// On-disk/download size in bytes, for display only.
    pub(crate) disk_size_bytes: Option<u64>,
    pub(crate) model_type: Option<ModelType>,
    /// Empty when the listing says nothing about capabilities. Not a fallback
    /// list: falling back is the importer's decision, not the parser's.
    pub(crate) supported_functions: Vec<ModelFunction>,
}

/// Parses a human-readable parameter count such as `7B`, `8.0B`, or `258M`
/// into a count. Returns `None` for compound strings such as `26B-A4B` that
/// do not describe a single number.
pub(crate) fn parse_parameter_count(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let (number, multiplier) = match raw.chars().last()? {
        'B' | 'b' => (&raw[..raw.len() - 1], 1_000_000_000_f64),
        'M' | 'm' => (&raw[..raw.len() - 1], 1_000_000_f64),
        'K' | 'k' => (&raw[..raw.len() - 1], 1_000_f64),
        _ => return None,
    };
    let value: f64 = number.parse().ok()?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    Some((value * multiplier) as u64)
}

/// Whether a model-listing endpoint is absent rather than broken. A 404 or
/// 405 means the provider cannot be asked for its models, not that discovery
/// failed, so callers can skip it instead of reporting a write-affecting
/// failure.
pub(crate) fn is_unsupported_listing_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
}

/*-- Pull Result Types -------------------------------------------------------*/

/// Result of a `Provider::pull_model` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PullResult {
    /// Pull was attempted and completed successfully.
    Success,
    /// Pull is not needed for this provider (e.g. hosted or auto-available models).
    Unnecessary,
    /// This provider does not support pulling; the user may need to take action.
    Unsupported { message: String },
}

/*-- Metadata Types ----------------------------------------------------------*/

/// Metadata describing a provider implementation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderMetadata {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) provider_type: ProviderType,
    pub(crate) default_base_urls: Vec<String>,

    /// API types this provider implementation supports (AND logic)
    pub(crate) supported_api_types: Vec<ApiType>,

    /// Default function-to-endpoint mappings for this provider type
    pub(crate) default_function_endpoints: HashMap<ModelFunction, Vec<ApiEndpoint>>,

    pub(crate) supported_formats: Vec<ModelFormat>,
    pub(crate) authentication: Vec<AuthType>,
    pub(crate) tags: Vec<String>,
}

impl ProviderMetadata {
    pub(crate) fn formatted_default_base_urls(&self) -> String {
        if self.default_base_urls.is_empty() {
            "-".to_string()
        } else {
            self.default_base_urls.join(", ")
        }
    }
}

impl std::fmt::Display for ProviderMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} - {}",
            self.provider_type, self.name, self.description
        )
    }
}

impl Searchable for ProviderMetadata {
    fn search_fields(&self) -> Vec<&str> {
        let mut fields: Vec<&str> = vec![self.name.as_str(), self.description.as_str()];
        fields.extend(self.tags.iter().map(String::as_str));
        fields
    }
}

/*-- Shared Helpers ----------------------------------------------------------*/

/// Shared HTTP health check implementation for providers.
pub(crate) async fn http_health_check(
    client: &reqwest::Client,
    base_url: &str,
    health_endpoint: &str,
    api_key: Option<&Secret>,
) -> Result<HealthStatus, ProviderError> {
    use std::time::Instant;

    let start = Instant::now();
    let url = format!("{base_url}{health_endpoint}");

    let mut request = client.get(&url);

    if let Some(key) = api_key {
        request = request.bearer_auth(&key.0);
    }

    match request.send().await {
        Ok(response) => {
            let latency = start.elapsed();

            if response.status().is_success() {
                Ok(HealthStatus {
                    healthy: true,
                    latency,
                    error: None,
                })
            } else {
                Ok(HealthStatus {
                    healthy: false,
                    latency,
                    error: Some(format!(
                        "HTTP {}: {}",
                        response.status(),
                        response.text().await.unwrap_or_default()
                    )),
                })
            }
        }
        Err(e) => {
            let latency = start.elapsed();
            Ok(HealthStatus {
                healthy: false,
                latency,
                error: Some(format!("Connection failed: {e}")),
            })
        }
    }
}

/*-- Supporting Types --------------------------------------------------------*/

/// Model formats that providers can serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
pub(crate) enum ModelFormat {
    Safetensors,
    GGUF,
    ONNX,
    MLX,
    // Provider specific formats
    Ollama,
    LMStudio,
    OpenRouter,
}

impl std::fmt::Display for ModelFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelFormat::Safetensors => write!(f, "safetensors"),
            ModelFormat::Ollama => write!(f, "ollama"),
            ModelFormat::GGUF => write!(f, "GGUF"),
            ModelFormat::ONNX => write!(f, "ONNX"),
            ModelFormat::MLX => write!(f, "MLX"),
            ModelFormat::LMStudio => write!(f, "LMStudio"),
            ModelFormat::OpenRouter => write!(f, "OpenRouter"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ProviderType {
    Hosted,
    Local,
}

impl std::fmt::Display for ProviderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderType::Hosted => write!(f, "Hosted"),
            ProviderType::Local => write!(f, "Local"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AuthType {
    ApiKey,
    BearerToken,
    None,
}

impl std::fmt::Display for AuthType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthType::ApiKey => write!(f, "API Key"),
            AuthType::BearerToken => write!(f, "Bearer Token"),
            AuthType::None => write!(f, "None"),
        }
    }
}

/// Health status from a provider health check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HealthStatus {
    pub(crate) healthy: bool,
    pub(crate) latency: Duration,
    pub(crate) error: Option<String>,
}

/// Errors specific to provider operations.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ProviderError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("Provider error: {0}")]
    Other(String),
}

/*-- Factory Definition ------------------------------------------------------*/

use crate::define_factory;

define_factory!(Provider, ProviderMetadata, ProviderFactory);

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_api_endpoint_paths() {
        assert_eq!(ApiEndpoint::OpenAIChat.path(), "/v1/chat/completions");
        assert_eq!(ApiEndpoint::OpenAIEmbeddings.path(), "/v1/embeddings");
        assert_eq!(
            ApiEndpoint::OpenAIAudioTranscription.path(),
            "/v1/audio/transcriptions"
        );
        assert_eq!(ApiEndpoint::OllamaChat.path(), "/api/chat");
        assert_eq!(ApiEndpoint::OllamaEmbeddings.path(), "/api/embeddings");
        assert_eq!(ApiEndpoint::AnthropicMessages.path(), "/v1/messages");
        assert_eq!(ApiEndpoint::JevSystemOne.path(), "/v1/systemone");
    }

    #[test]
    fn test_api_endpoint_types() {
        assert!(matches!(
            ApiEndpoint::OpenAIChat.api_type(),
            ApiType::OpenAI
        ));
        assert!(matches!(
            ApiEndpoint::OllamaChat.api_type(),
            ApiType::Ollama
        ));
        assert!(matches!(
            ApiEndpoint::AnthropicMessages.api_type(),
            ApiType::Anthropic
        ));
        assert!(matches!(ApiEndpoint::JevSystemOne.api_type(), ApiType::Jev));
    }

    #[test]
    fn test_api_type_display() {
        assert_eq!(ApiType::OpenAI.to_string(), "OpenAI");
        assert_eq!(ApiType::Ollama.to_string(), "Ollama");
        assert_eq!(ApiType::Anthropic.to_string(), "Anthropic");
        assert_eq!(ApiType::Jev.to_string(), "Jev");
    }

    #[test]
    fn test_provides_functions() {
        let chat_functions = ApiEndpoint::OpenAIChat.provides_functions();
        assert!(chat_functions.contains(&ModelFunction::Chat));
        assert!(chat_functions.contains(&ModelFunction::ToolCalling));

        let embedding_functions = ApiEndpoint::OpenAIEmbeddings.provides_functions();
        assert_eq!(embedding_functions.len(), 1);
        assert!(embedding_functions.contains(&ModelFunction::Embeddings));

        let audio_functions = ApiEndpoint::OpenAIAudioTranscription.provides_functions();
        assert!(audio_functions.contains(&ModelFunction::Transcription));

        assert_eq!(
            ApiEndpoint::JevSystemOne.provides_functions(),
            vec![ModelFunction::Decision]
        );
    }

    #[test]
    fn test_model_function_display() {
        use crate::models::ModelFunction;
        assert_eq!(ModelFunction::Chat.to_string(), "Chat");
        assert_eq!(
            ModelFunction::ImageUnderstanding.to_string(),
            "Image Understanding"
        );
        assert_eq!(ModelFunction::Transcription.to_string(), "Transcription");
        assert_eq!(ModelFunction::Decision.to_string(), "Decision");
    }

    #[test]
    fn parse_parameter_count_understands_suffixes() {
        assert_eq!(parse_parameter_count("8.0B"), Some(8_000_000_000));
        assert_eq!(parse_parameter_count("7B"), Some(7_000_000_000));
        assert_eq!(parse_parameter_count("258M"), Some(258_000_000));
        assert_eq!(parse_parameter_count("1.5K"), Some(1_500));
    }

    #[test]
    fn parse_parameter_count_rejects_compound_and_invalid_values() {
        assert_eq!(parse_parameter_count("26B-A4B"), None);
        assert_eq!(parse_parameter_count("671B"), Some(671_000_000_000));
        assert_eq!(parse_parameter_count(""), None);
        assert_eq!(parse_parameter_count("many"), None);
        assert_eq!(parse_parameter_count("0B"), None);
    }

    #[test]
    fn missing_listing_endpoints_are_unsupported() {
        assert!(is_unsupported_listing_status(
            reqwest::StatusCode::NOT_FOUND
        ));
        assert!(is_unsupported_listing_status(
            reqwest::StatusCode::METHOD_NOT_ALLOWED
        ));
        assert!(!is_unsupported_listing_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
    }

    #[test]
    fn searchable_fields_includes_name_description_and_tags() {
        let meta = ProviderMetadata {
            name: "Ollama".to_string(),
            description: "Local Ollama server".to_string(),
            provider_type: ProviderType::Local,
            default_base_urls: vec!["http://localhost:11434".to_string()],
            supported_api_types: vec![ApiType::Ollama],
            default_function_endpoints: HashMap::new(),
            supported_formats: vec![],
            authentication: vec![],
            tags: vec!["local".to_string(), "gguf".to_string()],
        };
        let fields = meta.search_fields();
        assert!(fields.contains(&"Ollama"));
        assert!(fields.contains(&"Local Ollama server"));
        assert!(fields.contains(&"local"));
        assert!(fields.contains(&"gguf"));
    }
}
