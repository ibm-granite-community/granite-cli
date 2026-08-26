use crate::models::huggingface::hf_repo_id;
use crate::models::{ModelFunction, ModelMetadata, ModelVariant};
use crate::providers::base::{
    ApiEndpoint, ApiType, AuthType, HasProviderMetadata, HealthStatus, ModelFormat, Provider,
    ProviderError, ProviderMetadata, ProviderType, http_health_check,
};
use crate::registry::{ConfigConstructable, Secret};
use crate::utils::ui::Ui;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/*-- vLLM Provider Configuration ---------------------------------------------*/

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VllmProviderConfig {
    /// Base URL for the vLLM server
    #[serde(default = "default_vllm_url")]
    pub base_url: String,

    /// API key for authentication (optional)
    pub api_key: Option<Secret>,

    /// Timeout for health checks in seconds
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,

    /// Whether to verify SSL certificates
    #[serde(default = "default_verify_ssl")]
    pub verify_ssl: bool,

    /// Endpoint to use for health checks
    #[serde(default = "default_vllm_health_endpoint")]
    pub health_check_endpoint: String,
}

fn default_vllm_url() -> String {
    "http://localhost:8000".to_string()
}

fn default_timeout() -> u64 {
    10
}

fn default_verify_ssl() -> bool {
    true
}

fn default_vllm_health_endpoint() -> String {
    "/health".to_string()
}

impl Default for VllmProviderConfig {
    fn default() -> Self {
        Self {
            base_url: default_vllm_url(),
            api_key: None,
            timeout_secs: default_timeout(),
            verify_ssl: default_verify_ssl(),
            health_check_endpoint: default_vllm_health_endpoint(),
        }
    }
}

/*-- vLLM Provider Implementation --------------------------------------------*/

pub struct VllmProvider {
    instance_id: String,
    config: VllmProviderConfig,
    client: reqwest::Client,
}

impl VllmProvider {
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

        map
    }
}

impl ConfigConstructable for VllmProvider {
    type Config = VllmProviderConfig;

    fn new(
        instance_id: &str,
        cfg: &serde_json::Value,
        _global_config: &crate::config::Config,
    ) -> Self {
        let config: VllmProviderConfig = serde_json::from_value(cfg.clone()).unwrap_or_default();

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

impl crate::registry::Named for VllmProvider {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait]
impl Provider for VllmProvider {
    fn name(&self) -> &str {
        "vLLM"
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
        vec![ModelFormat::Safetensors]
    }

    fn can_run_model(&self, variant_format: &str, _variant_precision: &str) -> bool {
        variant_format.eq_ignore_ascii_case("safetensors")
    }

    fn model_alias(&self, variant: Option<&ModelVariant>) -> Option<String> {
        let v = variant?;
        hf_repo_id(&v.url).map(|repo| repo.to_string())
    }

    async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
        http_health_check(
            &self.client,
            &self.config.base_url,
            &self.config.health_check_endpoint,
            self.config.api_key.as_ref(),
        )
        .await
    }

    async fn pull_model(
        &self,
        model: &ModelMetadata,
        variant: &ModelVariant,
        _ui: &dyn Ui,
    ) -> Result<crate::providers::PullResult, ProviderError> {
        Ok(crate::providers::PullResult::Unsupported {
            message: format!(
                "vLLM does not support pulling models via the API. \
                 Download {} ({} {}) manually using `huggingface-cli download {}` \
                 and start vLLM with `vllm serve {}`.",
                model.family,
                variant.format,
                variant.precision,
                hf_repo_id(&variant.url).unwrap_or(&variant.url),
                hf_repo_id(&variant.url).unwrap_or(&variant.url),
            ),
        })
    }
}

impl HasProviderMetadata for VllmProvider {
    fn metadata() -> ProviderMetadata {
        ProviderMetadata {
            name: "vLLM".to_string(),
            description: "High-throughput local inference server for safetensors models with OpenAI API compatibility".to_string(),
            provider_type: ProviderType::Local,
            default_endpoint: "http://localhost:8000".to_string(),
            supported_api_types: vec![ApiType::OpenAI],
            default_function_endpoints: Self::default_function_endpoints(),
            supported_formats: vec![ModelFormat::Safetensors],
            authentication: vec![AuthType::None, AuthType::BearerToken],
            tags: vec![
                "vllm".to_string(),
                "local".to_string(),
                "safetensors".to_string(),
                "high-throughput".to_string(),
            ],
        }
    }
}

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = VllmProviderConfig::default();
        assert_eq!(config.base_url, "http://localhost:8000");
        assert!(config.api_key.is_none());
        assert_eq!(config.timeout_secs, 10);
        assert!(config.verify_ssl);
        assert_eq!(config.health_check_endpoint, "/health");
    }

    #[test]
    fn test_provider_metadata() {
        let meta = VllmProvider::metadata();
        assert_eq!(meta.name, "vLLM");
        assert!(meta.supported_api_types.contains(&ApiType::OpenAI));
        assert!(!meta.supported_api_types.contains(&ApiType::Anthropic));
        assert!(
            meta.default_function_endpoints
                .contains_key(&ModelFunction::Chat)
        );
    }

    #[test]
    fn test_provider_constructs_from_json() {
        let cfg = serde_json::json!({
            "base_url": "http://example.com:9000",
            "timeout_secs": 30
        });
        let provider = VllmProvider::new("my-vllm", &cfg, &crate::config::Config::default());
        assert_eq!(provider.config.base_url, "http://example.com:9000");
        assert_eq!(provider.config.timeout_secs, 30);
    }

    #[test]
    fn test_can_run_model_accepts_safetensors() {
        let provider = VllmProvider::new(
            "my-vllm",
            &serde_json::json!({}),
            &crate::config::Config::default(),
        );
        assert!(provider.can_run_model("safetensors", "bfloat16"));
        assert!(provider.can_run_model("Safetensors", "fp16"));
    }

    #[test]
    fn test_can_run_model_rejects_non_safetensors() {
        let provider = VllmProvider::new(
            "my-vllm",
            &serde_json::json!({}),
            &crate::config::Config::default(),
        );
        assert!(!provider.can_run_model("gguf", "Q4_K_M"));
        assert!(!provider.can_run_model("onnx", "fp32"));
    }

    #[test]
    fn test_model_alias_returns_hf_repo_for_safetensors_variant() {
        let provider = VllmProvider::new(
            "my-vllm",
            &serde_json::json!({}),
            &crate::config::Config::default(),
        );
        let variant = ModelVariant {
            format: "safetensors".to_string(),
            precision: "bfloat16".to_string(),
            size_gb: Some(16.1),
            url: "https://huggingface.co/ibm-granite/granite-4.1-8b-instruct".to_string(),
        };
        assert_eq!(
            provider.model_alias(Some(&variant)),
            Some("ibm-granite/granite-4.1-8b-instruct".to_string())
        );
    }

    #[test]
    fn test_model_alias_returns_none_when_no_variant() {
        let provider = VllmProvider::new(
            "my-vllm",
            &serde_json::json!({}),
            &crate::config::Config::default(),
        );
        assert_eq!(provider.model_alias(None), None);
    }
}
