use crate::models::ModelFunction;
use crate::providers::base::{
    ApiEndpoint, ApiType, AuthType, HasProviderMetadata, HealthStatus, ModelFormat, Provider,
    ProviderError, ProviderMetadata, ProviderType,
};
use crate::registry::{ConfigConstructable, Secret};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/*-- OpenAI Provider Configuration -------------------------------------------*/

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OpenAIProviderConfig {
    /// Base URL for the OpenAI-compatible API
    pub base_url: String,

    /// API key for authentication (optional for local providers)
    pub api_key: Option<Secret>,

    /// Timeout for health checks in seconds
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,

    /// Whether to verify SSL certificates
    #[serde(default = "default_verify_ssl")]
    pub verify_ssl: bool,

    /// Endpoint to use for health checks
    #[serde(default = "default_health_endpoint")]
    pub health_check_endpoint: String,

    /// Specific function-to-endpoint mappings this instance supports.
    /// If None, will use default OpenAI mappings.
    pub function_endpoints: Option<HashMap<ModelFunction, Vec<ApiEndpoint>>>,
}

fn default_timeout() -> u64 {
    10
}

fn default_verify_ssl() -> bool {
    true
}

fn default_health_endpoint() -> String {
    "/v1/models".to_string()
}

impl Default for OpenAIProviderConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:8080".to_string(),
            api_key: None,
            timeout_secs: 10,
            verify_ssl: true,
            health_check_endpoint: "/v1/models".to_string(),
            function_endpoints: None,
        }
    }
}

/*-- OpenAI Provider Implementation ------------------------------------------*/

pub struct OpenAIProvider {
    config: OpenAIProviderConfig,
    client: reqwest::Client,
    function_endpoints: HashMap<ModelFunction, Vec<ApiEndpoint>>,
}

impl OpenAIProvider {
    fn default_function_endpoints() -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        let mut map = HashMap::new();
        map.insert(ModelFunction::Chat, vec![ApiEndpoint::OpenAIChat]);
        map.insert(
            ModelFunction::Embeddings,
            vec![ApiEndpoint::OpenAIEmbeddings],
        );
        map.insert(
            ModelFunction::Transcription,
            vec![ApiEndpoint::OpenAIAudioTranscription],
        );
        map
    }
}

impl ConfigConstructable for OpenAIProvider {
    fn new(cfg: &serde_json::Value) -> Self {
        let config: OpenAIProviderConfig = serde_json::from_value(cfg.clone()).unwrap_or_default();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .danger_accept_invalid_certs(!config.verify_ssl)
            .build()
            .expect("Failed to create HTTP client");

        let function_endpoints = config
            .function_endpoints
            .clone()
            .unwrap_or_else(Self::default_function_endpoints);

        Self {
            config,
            client,
            function_endpoints,
        }
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn name(&self) -> &str {
        "OpenAI Compatible Provider"
    }

    fn function_endpoints(&self) -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        self.function_endpoints.clone()
    }

    fn supported_api_types(&self) -> Vec<ApiType> {
        vec![ApiType::OpenAI]
    }

    fn base_url(&self) -> &str {
        &self.config.base_url
    }

    fn api_key(&self) -> Option<&Secret> {
        self.config.api_key.as_ref()
    }

    fn verify_ssl(&self) -> bool {
        self.config.verify_ssl
    }

    fn supported_formats(&self) -> Vec<ModelFormat> {
        vec![ModelFormat::Safetensors, ModelFormat::GGUF]
    }

    fn can_run_model(&self, _variant_format: &str, _variant_precision: &str) -> bool {
        true
    }

    async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
        let start = Instant::now();

        let url = format!(
            "{}{}",
            self.config.base_url, self.config.health_check_endpoint
        );

        let mut request = self.client.get(&url);

        if let Some(ref api_key) = self.config.api_key {
            request = request.bearer_auth(&api_key.0);
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

    async fn pull_model(
        &self,
        model: &crate::models::ModelMetadata,
        variant: &crate::models::ModelVariant,
        _ui: &dyn crate::utils::ui::Ui,
    ) -> Result<crate::providers::PullResult, ProviderError> {
        let message = format!(
            "Generic OpenAI-compatible provider '{}' does not support pulling models. \
             Pull '{} ({} {})' manually using whatever mechanism your specific server requires, then restart it.",
            self.name(),
            model.family,
            variant.format,
            variant.precision
        );
        Ok(crate::providers::PullResult::Unsupported { message })
    }
}

impl HasProviderMetadata for OpenAIProvider {
    fn metadata() -> ProviderMetadata {
        let mut default_mappings = HashMap::new();
        default_mappings.insert(ModelFunction::Chat, vec![ApiEndpoint::OpenAIChat]);
        default_mappings.insert(
            ModelFunction::Embeddings,
            vec![ApiEndpoint::OpenAIEmbeddings],
        );
        default_mappings.insert(
            ModelFunction::Transcription,
            vec![ApiEndpoint::OpenAIAudioTranscription],
        );

        ProviderMetadata {
            name: "OpenAI Compatible Provider".to_string(),
            description: "Provider for OpenAI-compatible API endpoints supporting chat, embeddings, and audio transcription".to_string(),
            provider_type: ProviderType::Local,
            default_endpoint: "http://localhost:8080".to_string(),
            supported_api_types: vec![ApiType::OpenAI],
            default_function_endpoints: default_mappings,
            supported_formats: vec![
                ModelFormat::Safetensors,
                ModelFormat::GGUF,
            ],
            authentication: vec![
                AuthType::BearerToken,
                AuthType::None,
            ],
            tags: vec![
                "openai".to_string(),
                "compatible".to_string(),
                "local".to_string(),
            ],
        }
    }

    fn config_schema() -> schemars::Schema {
        schemars::schema_for!(OpenAIProviderConfig)
    }

    fn default_config() -> serde_json::Value {
        serde_json::to_value(OpenAIProviderConfig::default()).unwrap_or_default()
    }
}

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = OpenAIProviderConfig::default();
        assert_eq!(config.base_url, "http://localhost:8080");
        assert!(config.api_key.is_none());
        assert_eq!(config.timeout_secs, 10);
        assert!(config.verify_ssl);
    }

    #[test]
    fn test_provider_config_schema_reflects_real_config_struct() {
        let schema = OpenAIProvider::config_schema();
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("object schema with properties");
        assert!(properties.contains_key("base_url"));
        assert!(properties.contains_key("api_key"));
        assert!(properties.contains_key("timeout_secs"));
        assert!(properties.contains_key("verify_ssl"));
    }

    #[test]
    fn test_provider_metadata() {
        let meta = OpenAIProvider::metadata();
        assert_eq!(meta.name, "OpenAI Compatible Provider");
        assert!(meta.supported_api_types.contains(&ApiType::OpenAI));
        assert!(
            meta.default_function_endpoints
                .contains_key(&ModelFunction::Chat)
        );
        assert!(
            meta.default_function_endpoints
                .contains_key(&ModelFunction::Embeddings)
        );
        assert!(
            meta.default_function_endpoints
                .contains_key(&ModelFunction::Transcription)
        );
    }

    #[test]
    fn test_provider_constructs_from_json() {
        let cfg = serde_json::json!({
            "base_url": "http://example.com:8080",
            "api_key": "test-key",
            "timeout_secs": 30
        });
        let provider = OpenAIProvider::new(&cfg);
        assert_eq!(provider.config.base_url, "http://example.com:8080");
        assert_eq!(
            provider.config.api_key,
            Some(Secret("test-key".to_string()))
        );
        assert_eq!(provider.config.timeout_secs, 30);
    }

    #[test]
    fn test_provider_function_endpoints() {
        let cfg = serde_json::json!({});
        let provider = OpenAIProvider::new(&cfg);
        let endpoints = provider.function_endpoints();
        assert!(endpoints.contains_key(&ModelFunction::Chat));
        assert!(endpoints.contains_key(&ModelFunction::Embeddings));
        assert!(endpoints.contains_key(&ModelFunction::Transcription));
    }

    #[test]
    fn test_custom_function_endpoints() {
        // Create provider with default config, then manually set function_endpoints
        let cfg = serde_json::json!({
            "base_url": "http://example.com:8080"
        });
        let provider = OpenAIProvider::new(&cfg);
        let endpoints = provider.function_endpoints();
        // Default config has all three functions
        assert!(endpoints.contains_key(&ModelFunction::Chat));
        assert!(endpoints.contains_key(&ModelFunction::Embeddings));
        assert!(endpoints.contains_key(&ModelFunction::Transcription));
    }
}
