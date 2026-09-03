// Standard
use std::collections::HashMap;
use std::time::Duration;

// Third Party
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// Local
use crate::models::ModelFunction;
use crate::providers::base::{
    ApiEndpoint, ApiType, AuthType, HasProviderMetadata, HealthStatus, ModelFormat, Provider,
    ProviderError, ProviderMetadata, ProviderType,
};
use crate::registry::{ConfigConstructable, Secret};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OpenRouterProviderConfig {
    pub base_url: String,
    pub api_key: Option<Secret>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_verify_ssl")]
    pub verify_ssl: bool,
    #[serde(default = "default_health_endpoint")]
    pub health_check_endpoint: String,
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

impl Default for OpenRouterProviderConfig {
    fn default() -> Self {
        Self {
            base_url: "https://openrouter.ai/api".to_string(),
            api_key: None,
            timeout_secs: 10,
            verify_ssl: true,
            health_check_endpoint: default_health_endpoint(),
        }
    }
}

pub struct OpenRouterProvider {
    instance_id: String,
    config: OpenRouterProviderConfig,
    client: reqwest::Client,
}

impl OpenRouterProvider {
    fn default_function_endpoints() -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        let mut map = HashMap::new();
        map.insert(ModelFunction::Chat, vec![ApiEndpoint::OpenAIChat]);
        map.insert(ModelFunction::ToolCalling, vec![ApiEndpoint::OpenAIChat]);
        map.insert(ModelFunction::Thinking, vec![ApiEndpoint::OpenAIChat]);
        map.insert(
            ModelFunction::ImageUnderstanding,
            vec![ApiEndpoint::OpenAIChat],
        );
        map.insert(ModelFunction::Guardian, vec![ApiEndpoint::OpenAIChat]);
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

impl ConfigConstructable for OpenRouterProvider {
    type Config = OpenRouterProviderConfig;

    fn new(
        instance_id: &str,
        cfg: &serde_json::Value,
        _global_config: &crate::config::Config,
    ) -> Self {
        let config: OpenRouterProviderConfig =
            serde_json::from_value(cfg.clone()).unwrap_or_default();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .danger_accept_invalid_certs(!config.verify_ssl)
            .build()
            .expect("Failed to create HTTP client");

        Self {
            instance_id: instance_id.to_string(),
            config,
            client,
        }
    }
}

impl crate::registry::Named for OpenRouterProvider {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait]
impl Provider for OpenRouterProvider {
    fn name(&self) -> &str {
        "OpenRouter"
    }

    fn function_endpoints(&self) -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        Self::default_function_endpoints()
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
        vec![ModelFormat::OpenRouter]
    }

    fn can_run_model(&self, variant_format: &str, _variant_precision: &str) -> bool {
        variant_format == "OpenRouter"
    }

    async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
        crate::providers::base::http_health_check(
            &self.client,
            &self.config.base_url,
            &self.config.health_check_endpoint,
            self.config.api_key.as_ref(),
        )
        .await
    }
}

impl HasProviderMetadata for OpenRouterProvider {
    fn metadata() -> ProviderMetadata {
        ProviderMetadata {
            name: "OpenRouter".to_string(),
            description: "Hosted provider for accessing multiple AI models via the OpenRouter API"
                .to_string(),
            provider_type: ProviderType::Hosted,
            default_endpoint: "https://openrouter.ai/api".to_string(),
            supported_api_types: vec![ApiType::OpenAI],
            default_function_endpoints: Self::default_function_endpoints(),
            supported_formats: vec![ModelFormat::OpenRouter],
            authentication: vec![AuthType::BearerToken],
            tags: vec!["openrouter".to_string(), "hosted".to_string()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = OpenRouterProviderConfig::default();
        assert_eq!(config.base_url, "https://openrouter.ai/api");
        assert!(config.api_key.is_none());
        assert_eq!(config.timeout_secs, 10);
        assert!(config.verify_ssl);
        assert_eq!(config.health_check_endpoint, "/v1/models");
    }

    #[test]
    fn test_provider_metadata() {
        let meta = OpenRouterProvider::metadata();
        assert_eq!(meta.name, "OpenRouter");
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
        assert_eq!(meta.provider_type, ProviderType::Hosted);
    }

    #[test]
    fn test_provider_constructs_from_json() {
        let cfg = serde_json::json!({
            "base_url": "https://api.openrouter.ai/v1",
            "api_key": "test-key",
            "timeout_secs": 30
        });
        let provider =
            OpenRouterProvider::new("my-openrouter", &cfg, &crate::config::Config::default());
        assert_eq!(provider.config.base_url, "https://api.openrouter.ai/v1");
        assert_eq!(
            provider.config.api_key,
            Some(Secret("test-key".to_string()))
        );
        assert_eq!(provider.config.timeout_secs, 30);
    }

    #[test]
    fn test_provider_function_endpoints() {
        let cfg = serde_json::json!({});
        let provider =
            OpenRouterProvider::new("my-openrouter", &cfg, &crate::config::Config::default());
        let endpoints = provider.function_endpoints();
        assert!(endpoints.contains_key(&ModelFunction::Chat));
        assert!(endpoints.contains_key(&ModelFunction::Embeddings));
        assert!(endpoints.contains_key(&ModelFunction::Transcription));
    }
}
