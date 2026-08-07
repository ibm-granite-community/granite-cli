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

/// Response from `POST /api/v1/models/download`.
#[derive(Debug, Deserialize)]
struct LMStudioDownloadResponse {
    job_id: Option<String>,
    status: String,
    total_size_bytes: Option<u64>,
}

/// Response from `GET /api/v1/models/download/status/:job_id`.
#[derive(Debug, Deserialize)]
struct LMStudioJobStatus {
    status: String,
    downloaded_bytes: Option<u64>,
    total_size_bytes: Option<u64>,
    error: Option<String>,
}

/*-- LM Studio Provider Configuration ----------------------------------------*/

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LMStudioProviderConfig {
    /// Base URL for the LM Studio server
    #[serde(default = "default_lmstudio_url")]
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
    #[serde(default = "default_lmstudio_health_endpoint")]
    pub health_check_endpoint: String,
}

fn default_lmstudio_url() -> String {
    "http://localhost:1234".to_string()
}

fn default_timeout() -> u64 {
    10
}

fn default_verify_ssl() -> bool {
    true
}

fn default_lmstudio_health_endpoint() -> String {
    "/v1/models".to_string()
}

impl Default for LMStudioProviderConfig {
    fn default() -> Self {
        Self {
            base_url: default_lmstudio_url(),
            api_key: None,
            timeout_secs: default_timeout(),
            verify_ssl: default_verify_ssl(),
            health_check_endpoint: default_lmstudio_health_endpoint(),
        }
    }
}

/*-- LM Studio Provider Implementation ---------------------------------------*/

pub struct LMStudioProvider {
    config: LMStudioProviderConfig,
    client: reqwest::Client,
}

impl LMStudioProvider {
    fn default_function_endpoints() -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        let mut map = HashMap::new();

        map.insert(
            ModelFunction::Chat,
            vec![ApiEndpoint::OpenAIChat, ApiEndpoint::AnthropicMessages],
        );

        map.insert(
            ModelFunction::Embeddings,
            vec![ApiEndpoint::OpenAIEmbeddings],
        );

        map
    }

    fn default_formats() -> Vec<ModelFormat> {
        let mut formats = vec![ModelFormat::GGUF];

        if cfg!(target_os = "macos") {
            formats.push(ModelFormat::MLX);
        }

        formats
    }
}

impl ConfigConstructable for LMStudioProvider {
    fn new(cfg: &serde_json::Value) -> Self {
        let config: LMStudioProviderConfig =
            serde_json::from_value(cfg.clone()).unwrap_or_default();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .danger_accept_invalid_certs(!config.verify_ssl)
            .build()
            .expect("Failed to create HTTP client");

        Self { config, client }
    }
}

#[async_trait]
impl Provider for LMStudioProvider {
    fn name(&self) -> &str {
        "LM Studio"
    }

    fn function_endpoints(&self) -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        Self::default_function_endpoints()
    }

    fn supported_api_types(&self) -> Vec<ApiType> {
        vec![ApiType::OpenAI, ApiType::Anthropic]
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
        Self::default_formats()
    }

    fn can_run_model(&self, variant_format: &str, _variant_precision: &str) -> bool {
        let format = variant_format.to_lowercase();
        matches!(format.as_str(), "gguf" | "mlx" if format != "mlx" || cfg!(target_os = "macos"))
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
        ui: &dyn Ui,
    ) -> Result<crate::providers::PullResult, ProviderError> {
        let repo = hf_repo_id(&variant.url).ok_or_else(|| {
            ProviderError::Other(format!(
                "cannot determine a HuggingFace repo for {} variant {}/{}",
                model.family, variant.format, variant.precision
            ))
        })?;
        let label = format!(
            "{} ({} {})",
            model.family, variant.format, variant.precision
        );

        let url = format!("{}/api/v1/models/download", self.config.base_url);
        let mut request = self.client.post(&url).json(&serde_json::json!({
            "model": format!("https://huggingface.co/{}", repo),
            "quantization": variant.precision,
        }));
        if let Some(key) = &self.config.api_key {
            request = request.bearer_auth(&key.0);
        }

        let response = request.send().await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Other(format!(
                "LM Studio download request failed ({status}): {body}"
            )));
        }
        let started: LMStudioDownloadResponse = response.json().await?;

        let handle = ui.pull_start(&label, started.total_size_bytes);

        if started.status == "already_downloaded" {
            ui.pull_finish(handle, &label, None);
            return Ok(crate::providers::PullResult::Success);
        }

        let job_id = match started.job_id {
            Some(id) => id,
            None => {
                ui.pull_finish(handle, &label, None);
                return Ok(crate::providers::PullResult::Success);
            }
        };
        let status_url = format!(
            "{}/api/v1/models/download/status/{}",
            self.config.base_url, job_id
        );

        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;

            let mut status_request = self.client.get(&status_url);
            if let Some(key) = &self.config.api_key {
                status_request = status_request.bearer_auth(&key.0);
            }

            let status_response = status_request.send().await?;
            if !status_response.status().is_success() {
                let status = status_response.status();
                let body = status_response.text().await.unwrap_or_default();
                let err = format!("LM Studio status check failed ({status}): {body}");
                ui.pull_finish(handle, &label, Some(&err));
                return Err(ProviderError::Other(err));
            }
            let job: LMStudioJobStatus = status_response.json().await?;

            ui.pull_progress(
                handle,
                job.downloaded_bytes.unwrap_or(0),
                job.total_size_bytes.or(started.total_size_bytes),
            );

            match job.status.as_str() {
                "completed" => {
                    ui.pull_finish(handle, &label, None);
                    return Ok(crate::providers::PullResult::Success);
                }
                "failed" => {
                    let err = job.error.unwrap_or_else(|| "download failed".to_string());
                    ui.pull_finish(handle, &label, Some(&err));
                    return Err(ProviderError::Other(err));
                }
                _ => continue,
            }
        }
    }
}

impl HasProviderMetadata for LMStudioProvider {
    fn metadata() -> ProviderMetadata {
        let mut formats = vec![ModelFormat::GGUF];
        let mut tags = vec![
            "lm-studio".to_string(),
            "local".to_string(),
            "gguf".to_string(),
        ];

        if cfg!(target_os = "macos") {
            formats.push(ModelFormat::MLX);
            tags.push("mlx".to_string());
            tags.push("apple-silicon".to_string());
        }

        ProviderMetadata {
            name: "LM Studio".to_string(),
            description:
                "User-friendly local inference server with GUI, supporting GGUF and MLX models"
                    .to_string(),
            provider_type: ProviderType::Local,
            default_endpoint: "http://localhost:1234".to_string(),
            supported_api_types: vec![ApiType::OpenAI, ApiType::Anthropic],
            default_function_endpoints: Self::default_function_endpoints(),
            supported_formats: formats,
            authentication: vec![AuthType::None, AuthType::BearerToken],
            tags,
        }
    }

    fn config_schema() -> schemars::Schema {
        schemars::schema_for!(LMStudioProviderConfig)
    }

    fn default_config() -> serde_json::Value {
        serde_json::to_value(LMStudioProviderConfig::default()).unwrap_or_default()
    }
}

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = LMStudioProviderConfig::default();
        assert_eq!(config.base_url, "http://localhost:1234");
        assert!(config.api_key.is_none());
        assert_eq!(config.timeout_secs, 10);
        assert!(config.verify_ssl);
        assert_eq!(config.health_check_endpoint, "/v1/models");
    }

    #[test]
    fn test_provider_metadata() {
        let meta = LMStudioProvider::metadata();
        assert_eq!(meta.name, "LM Studio");
        assert!(meta.supported_api_types.contains(&ApiType::OpenAI));
        assert!(meta.supported_api_types.contains(&ApiType::Anthropic));
        assert!(
            meta.default_function_endpoints
                .contains_key(&ModelFunction::Chat)
        );
    }

    #[test]
    fn test_provider_constructs_from_json() {
        let cfg = serde_json::json!({
            "base_url": "http://example.com:5678",
            "timeout_secs": 30
        });
        let provider = LMStudioProvider::new(&cfg);
        assert_eq!(provider.config.base_url, "http://example.com:5678");
        assert_eq!(provider.config.timeout_secs, 30);
    }

    #[test]
    fn test_can_run_model_accepts_gguf() {
        let provider = LMStudioProvider::new(&serde_json::json!({}));
        assert!(provider.can_run_model("gguf", "Q4_K_M"));
        assert!(provider.can_run_model("GGUF", "fp16"));
    }

    #[test]
    fn test_can_run_model_rejects_non_supported() {
        let provider = LMStudioProvider::new(&serde_json::json!({}));
        assert!(!provider.can_run_model("safetensors", "fp16"));
        assert!(!provider.can_run_model("onnx", "fp32"));
    }

    #[test]
    fn test_mlx_formats_on_macos() {
        let provider = LMStudioProvider::new(&serde_json::json!({}));
        let formats = provider.supported_formats();

        #[cfg(target_os = "macos")]
        assert!(formats.contains(&ModelFormat::MLX));

        #[cfg(not(target_os = "macos"))]
        assert!(!formats.contains(&ModelFormat::MLX));
    }

    #[test]
    fn test_can_run_mlx_model() {
        let provider = LMStudioProvider::new(&serde_json::json!({}));

        #[cfg(target_os = "macos")]
        assert!(provider.can_run_model("mlx", "fp16"));

        #[cfg(not(target_os = "macos"))]
        assert!(!provider.can_run_model("mlx", "fp16"));
    }

    #[test]
    fn test_lmstudio_download_response_parses() {
        let body = r#"{"job_id":"abc123","status":"downloading","total_size_bytes":1000}"#;
        let resp: LMStudioDownloadResponse = serde_json::from_str(body).unwrap();
        assert_eq!(resp.job_id, Some("abc123".to_string()));
        assert_eq!(resp.total_size_bytes, Some(1000));
    }

    #[test]
    fn test_lmstudio_job_status_parses() {
        let body = r#"{"status":"completed","downloaded_bytes":1000,"total_size_bytes":1000}"#;
        let job: LMStudioJobStatus = serde_json::from_str(body).unwrap();
        assert_eq!(job.status, "completed");
        assert_eq!(job.downloaded_bytes, Some(1000));
    }
}
