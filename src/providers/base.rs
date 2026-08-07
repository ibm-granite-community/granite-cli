use crate::models::ModelFunction;
use crate::registry::{ConfigConstructable, Secret};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/*-- ApiType Enum ------------------------------------------------------------*/

/// API protocol families that providers can implement
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ApiType {
    /// OpenAI-compatible API protocol
    OpenAI,
    /// Ollama API protocol
    Ollama,
    /// Anthropic API protocol
    Anthropic,
}

impl std::fmt::Display for ApiType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiType::OpenAI => write!(f, "OpenAI"),
            ApiType::Ollama => write!(f, "Ollama"),
            ApiType::Anthropic => write!(f, "Anthropic"),
        }
    }
}

/*-- ApiEndpoint Enum --------------------------------------------------------*/

/// Specific API endpoints within an API family
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub enum ApiEndpoint {
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
}

impl ApiEndpoint {
    /// Returns the API type this endpoint belongs to
    pub fn api_type(&self) -> ApiType {
        match self {
            ApiEndpoint::OpenAIChat
            | ApiEndpoint::OpenAIEmbeddings
            | ApiEndpoint::OpenAIAudioTranscription => ApiType::OpenAI,

            ApiEndpoint::OllamaChat | ApiEndpoint::OllamaEmbeddings => ApiType::Ollama,

            ApiEndpoint::AnthropicMessages => ApiType::Anthropic,
        }
    }

    /// Returns the endpoint path
    pub fn path(&self) -> &'static str {
        match self {
            ApiEndpoint::OpenAIChat => "/v1/chat/completions",
            ApiEndpoint::OpenAIEmbeddings => "/v1/embeddings",
            ApiEndpoint::OpenAIAudioTranscription => "/v1/audio/transcriptions",
            ApiEndpoint::OllamaChat => "/api/chat",
            ApiEndpoint::OllamaEmbeddings => "/api/embeddings",
            ApiEndpoint::AnthropicMessages => "/v1/messages",
        }
    }

    /// Returns the model functions this endpoint provides
    pub fn provides_functions(&self) -> Vec<ModelFunction> {
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
pub trait Provider: ConfigConstructable + Send + Sync {
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

    /// Returns whether this provider instance verifies SSL certificates.
    fn verify_ssl(&self) -> bool;

    // Model support
    fn supported_formats(&self) -> Vec<ModelFormat>;
    fn can_run_model(&self, _variant_format: &str, _variant_precision: &str) -> bool {
        true
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
}

/*-- Pull Result Types -------------------------------------------------------*/

/// Result of a `Provider::pull_model` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullResult {
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
    pub name: String,
    pub description: String,
    pub provider_type: ProviderType,
    pub default_endpoint: String,

    /// API types this provider implementation supports (AND logic)
    pub supported_api_types: Vec<ApiType>,

    /// Default function-to-endpoint mappings for this provider type
    pub default_function_endpoints: HashMap<ModelFunction, Vec<ApiEndpoint>>,

    pub supported_formats: Vec<ModelFormat>,
    pub authentication: Vec<AuthType>,
    pub tags: Vec<String>,
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

/*-- Shared Helpers ----------------------------------------------------------*/

/// Shared HTTP health check implementation for providers.
pub async fn http_health_check(
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
#[allow(non_camel_case_types)]
pub enum ModelFormat {
    Safetensors,
    Ollama,
    GGUF,
    ONNX,
    MLX,
}

impl std::fmt::Display for ModelFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelFormat::Safetensors => write!(f, "safetensors"),
            ModelFormat::Ollama => write!(f, "ollama"),
            ModelFormat::GGUF => write!(f, "GGUF"),
            ModelFormat::ONNX => write!(f, "ONNX"),
            ModelFormat::MLX => write!(f, "MLX"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderType {
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
pub enum AuthType {
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
pub struct HealthStatus {
    pub healthy: bool,
    pub latency: Duration,
    pub error: Option<String>,
}

/// Errors specific to provider operations.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Authentication failed: {0}")]
    Auth(String),

    #[error("Rate limited: {0}")]
    RateLimited(String),

    #[error("Model not found: {0}")]
    ModelNotFound(String),

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
    }
}
