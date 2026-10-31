use crate::models::huggingface::hf_repo_id;
use crate::models::{ModelFunction, ModelMetadata, ModelType, ModelVariant};
use crate::providers::base::{
    ApiEndpoint, ApiType, AuthType, HasProviderMetadata, HealthStatus, ModelFormat, Provider,
    ProviderError, ProviderMetadata, ProviderModel, ProviderType, http_health_check,
    is_unsupported_listing_status, parse_parameter_count,
};
use crate::registry::{ConfigConstructable, ConstructError, Secret};
use crate::utils::ui::Ui;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// Extract the LM Studio model reference from an LM Studio variant URL.
///
/// `https://lmstudio.ai/models/{org}/{model}` -> `{org}/{model}`
/// (e.g. `https://lmstudio.ai/models/ibm/granite-4.1-30b` -> `ibm/granite-4.1-30b`)
///
/// Returns `None` for non-LM Studio URLs.
fn lmstudio_model_ref(url: &str) -> Option<String> {
    Some(
        url.strip_prefix("https://lmstudio.ai/models/")
            .or_else(|| url.strip_prefix("lmstudio.ai/mosels/"))
            .filter(|s| !s.is_empty())?
            .to_string(),
    )
}

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
pub(crate) struct LMStudioProviderConfig {
    /// Base URL for the LM Studio server
    #[serde(default = "default_base_url")]
    pub(crate) base_url: String,

    /// API key for authentication (optional)
    pub(crate) api_key: Option<Secret>,

    /// Timeout for health checks in seconds
    #[serde(default = "default_timeout")]
    timeout_secs: u64,

    /// Whether to verify SSL certificates
    #[serde(default = "default_verify_ssl")]
    pub(crate) verify_ssl: bool,

    /// Endpoint to use for health checks
    #[serde(default = "default_lmstudio_health_endpoint")]
    health_check_endpoint: String,

    /// Mirror the models this server reports into configured models.
    #[serde(default = "default_discover_models")]
    pub(crate) discover_models: bool,
}

fn default_base_url() -> String {
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

fn default_discover_models() -> bool {
    true
}

impl Default for LMStudioProviderConfig {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            api_key: None,
            timeout_secs: default_timeout(),
            verify_ssl: default_verify_ssl(),
            health_check_endpoint: default_lmstudio_health_endpoint(),
            discover_models: default_discover_models(),
        }
    }
}

/*-- LM Studio Provider Implementation ---------------------------------------*/

pub(crate) struct LMStudioProvider {
    instance_id: String,
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
            ModelFunction::ToolCalling,
            vec![ApiEndpoint::OpenAIChat, ApiEndpoint::AnthropicMessages],
        );

        map.insert(
            ModelFunction::Thinking,
            vec![ApiEndpoint::OpenAIChat, ApiEndpoint::AnthropicMessages],
        );

        map.insert(
            ModelFunction::ImageUnderstanding,
            vec![ApiEndpoint::OpenAIChat, ApiEndpoint::AnthropicMessages],
        );

        map.insert(
            ModelFunction::Guardian,
            vec![ApiEndpoint::OpenAIChat, ApiEndpoint::AnthropicMessages],
        );

        map.insert(
            ModelFunction::Embeddings,
            vec![ApiEndpoint::OpenAIEmbeddings],
        );

        map
    }

    fn default_formats() -> Vec<ModelFormat> {
        let mut formats = vec![ModelFormat::GGUF, ModelFormat::LMStudio];

        if cfg!(target_os = "macos") {
            formats.push(ModelFormat::MLX);
        }

        formats
    }
}

#[derive(Debug, Deserialize)]
struct LMStudioModelsResponse {
    models: Vec<LMStudioModel>,
}

#[derive(Debug, Deserialize)]
struct LMStudioModel {
    #[serde(default)]
    key: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    max_context_length: Option<u64>,
    #[serde(default)]
    size_bytes: Option<u64>,
    #[serde(default)]
    params_string: Option<String>,
    #[serde(default)]
    capabilities: Option<LMStudioCapabilities>,
}

#[derive(Debug, Deserialize)]
struct LMStudioCapabilities {
    #[serde(default)]
    vision: bool,
    #[serde(default)]
    trained_for_tool_use: bool,
    #[serde(default)]
    reasoning: Option<serde_json::Value>,
}

impl ConfigConstructable for LMStudioProvider {
    type Config = LMStudioProviderConfig;

    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        let config: LMStudioProviderConfig =
            serde_json::from_value(cfg.clone()).map_err(ConstructError::settings)?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .danger_accept_invalid_certs(!config.verify_ssl)
            .build()
            .map_err(ConstructError::settings)?;

        Ok(Self {
            instance_id: instance_id.to_string(),
            config,
            client,
        })
    }
}

impl crate::registry::Named for LMStudioProvider {
    fn instance_id(&self) -> &str {
        &self.instance_id
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
        matches!(format.as_str(), "gguf" | "lmstudio" | "mlx" if format != "mlx" || cfg!(target_os = "macos"))
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

    fn discover_models(&self) -> bool {
        self.config.discover_models
    }

    async fn list_models(&self) -> Result<Vec<ProviderModel>, ProviderError> {
        let url = format!("{}/api/v1/models", self.config.base_url);
        let mut request = self.client.get(&url);
        if let Some(key) = &self.config.api_key {
            request = request.bearer_auth(&key.0);
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(if is_unsupported_listing_status(status) {
                ProviderError::Unsupported(format!("LM Studio model discovery ({status})"))
            } else {
                ProviderError::Other(format!("LM Studio model listing failed ({status}): {body}"))
            });
        }
        let payload: LMStudioModelsResponse = response.json().await.map_err(|e| {
            ProviderError::Other(format!("invalid LM Studio /api/v1/models response: {e}"))
        })?;

        let mut models = Vec::new();
        for model in payload.models {
            if model.key.is_empty() {
                continue;
            }
            let kind = model.kind.as_deref().unwrap_or("llm");
            let capabilities = model.capabilities.as_ref();
            let (model_type, supported_functions) = match kind {
                "embedding" => (ModelType::Embedding, vec![ModelFunction::Embeddings]),
                "decision" => (ModelType::Text, vec![ModelFunction::Decision]),
                _ => {
                    let mut functions = vec![ModelFunction::Chat];
                    if capabilities.is_some_and(|c| c.trained_for_tool_use) {
                        functions.push(ModelFunction::ToolCalling);
                    }
                    if capabilities.is_some_and(|c| c.vision) {
                        functions.push(ModelFunction::ImageUnderstanding);
                    }
                    if capabilities.is_some_and(|c| c.reasoning.is_some()) {
                        functions.push(ModelFunction::Thinking);
                    }
                    (ModelType::Text, functions)
                }
            };

            models.push(ProviderModel {
                display_name: Some(model.display_name.unwrap_or_else(|| model.key.clone())),
                id: model.key,
                context_length: model.max_context_length,
                parameter_count: model
                    .params_string
                    .as_deref()
                    .and_then(parse_parameter_count),
                disk_size_bytes: model.size_bytes,
                model_type: Some(model_type),
                supported_functions,
            });
        }

        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }

    async fn pull_model(
        &self,
        model: &ModelMetadata,
        variant: &ModelVariant,
        ui: &dyn Ui,
    ) -> Result<crate::providers::PullResult, ProviderError> {
        let model_ref = if let Some(ref_str) = lmstudio_model_ref(&variant.url) {
            ref_str
        } else if let Some(repo) = hf_repo_id(&variant.url) {
            format!("https://huggingface.co/{repo}")
        } else {
            return Err(ProviderError::Other(format!(
                "cannot determine a model reference for {} variant {}/{}",
                model.family, variant.format, variant.precision
            )));
        };
        let label = format!(
            "{} ({} {})",
            model.family, variant.format, variant.precision
        );

        let url = format!("{}/api/v1/models/download", self.config.base_url);
        let mut request = self.client.post(&url).json(&serde_json::json!({
            "model": model_ref,
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
        let mut formats = vec![ModelFormat::GGUF, ModelFormat::LMStudio];
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
            default_base_urls: vec!["http://localhost:1234".to_string()],
            supported_api_types: vec![ApiType::OpenAI, ApiType::Anthropic],
            default_function_endpoints: Self::default_function_endpoints(),
            supported_formats: formats,
            authentication: vec![AuthType::None, AuthType::BearerToken],
            tags,
        }
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
        assert!(config.discover_models);
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
        let provider = LMStudioProvider::new("my-lmstudio", &cfg).unwrap();
        assert_eq!(provider.config.base_url, "http://example.com:5678");
        assert_eq!(provider.config.timeout_secs, 30);
    }

    #[test]
    fn test_can_run_model_accepts_gguf() {
        let provider = LMStudioProvider::new("my-lmstudio", &serde_json::json!({})).unwrap();
        assert!(provider.can_run_model("gguf", "Q4_K_M"));
        assert!(provider.can_run_model("GGUF", "fp16"));
    }

    #[test]
    fn test_can_run_model_rejects_non_supported() {
        let provider = LMStudioProvider::new("my-lmstudio", &serde_json::json!({})).unwrap();
        assert!(!provider.can_run_model("safetensors", "fp16"));
        assert!(!provider.can_run_model("onnx", "fp32"));
    }

    #[test]
    fn test_mlx_formats_on_macos() {
        let provider = LMStudioProvider::new("my-lmstudio", &serde_json::json!({})).unwrap();
        let formats = provider.supported_formats();

        #[cfg(target_os = "macos")]
        assert!(formats.contains(&ModelFormat::MLX));

        #[cfg(not(target_os = "macos"))]
        assert!(!formats.contains(&ModelFormat::MLX));
    }

    #[test]
    fn test_can_run_mlx_model() {
        let provider = LMStudioProvider::new("my-lmstudio", &serde_json::json!({})).unwrap();

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

    #[test]
    fn test_lmstudio_model_ref_from_simple_url() {
        assert_eq!(
            lmstudio_model_ref("https://lmstudio.ai/models/ibm/granite-4.1-30b"),
            Some("ibm/granite-4.1-30b".to_string())
        );
    }

    #[test]
    fn test_lmstudio_model_ref_rejects_non_lmstudio_url() {
        assert_eq!(
            lmstudio_model_ref("https://huggingface.co/ibm-granite/granite-speech-4.1-2b"),
            None
        );
        assert_eq!(
            lmstudio_model_ref("https://ollama.com/library/granite4:1b"),
            None
        );
        assert_eq!(lmstudio_model_ref("https://lmstudio.ai/models/"), None);
    }

    #[test]
    fn test_pull_model_uses_lmstudio_ref() {
        // Tests that lmstudio_model_ref extracts the correct model reference
        // from LM Studio variant URLs, and that hf_repo_id falls back for
        // non-LMStudio URLs — the model_ref selection logic in pull_model.
        assert_eq!(
            lmstudio_model_ref("https://lmstudio.ai/models/ibm/granite-4.1-30b"),
            Some("ibm/granite-4.1-30b".to_string())
        );
        assert_eq!(
            lmstudio_model_ref("https://huggingface.co/ibm-granite/granite-speech-4.1-2b"),
            None
        );
        assert_eq!(
            hf_repo_id("https://huggingface.co/ibm-granite/granite-speech-4.1-2b"),
            Some("ibm-granite/granite-speech-4.1-2b")
        );
    }

    async fn discovery_server() -> String {
        use axum::{Json, Router, routing::get};

        async fn models() -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "models": [
                    {
                        "type": "llm",
                        "key": "google/gemma-4-26b-a4b",
                        "display_name": "Gemma 4 26B A4B",
                        "max_context_length": 262144u64,
                        "size_bytes": 17_990_911_801u64,
                        "params_string": "26B-A4B",
                        "capabilities": {
                            "vision": true,
                            "trained_for_tool_use": true,
                            "reasoning": { "allowed_options": ["off", "on"], "default": "on" }
                        }
                    },
                    {
                        "type": "llm",
                        "key": "deepseek-r1",
                        "display_name": "DeepSeek R1",
                        "max_context_length": 131072u64,
                        "size_bytes": 40_492_610_355u64,
                        "params_string": "671B",
                        "capabilities": {
                            "vision": false,
                            "trained_for_tool_use": true
                        }
                    },
                    {
                        "type": "embedding",
                        "key": "nomic-embed-text",
                        "display_name": "Nomic Embed Text",
                        "max_context_length": 2048u64,
                        "size_bytes": 274_290_560u64,
                        "params_string": null
                    }
                ]
            }))
        }

        let app = Router::new().route("/api/v1/models", get(models));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn list_models_maps_types_and_capabilities() {
        let provider = LMStudioProvider::new(
            "test",
            &serde_json::json!({ "base_url": discovery_server().await }),
        )
        .unwrap();
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 3);

        let gemma = models
            .iter()
            .find(|m| m.id == "google/gemma-4-26b-a4b")
            .unwrap();
        assert_eq!(gemma.model_type, Some(ModelType::Text));
        assert_eq!(gemma.context_length, Some(262144));
        assert_eq!(gemma.parameter_count, None);
        assert!(gemma.supported_functions.contains(&ModelFunction::Chat));
        assert!(
            gemma
                .supported_functions
                .contains(&ModelFunction::ToolCalling)
        );
        assert!(
            gemma
                .supported_functions
                .contains(&ModelFunction::ImageUnderstanding)
        );
        assert!(gemma.supported_functions.contains(&ModelFunction::Thinking));

        let r1 = models.iter().find(|m| m.id == "deepseek-r1").unwrap();
        assert_eq!(r1.parameter_count, Some(671_000_000_000));
        assert!(!r1.supported_functions.contains(&ModelFunction::Thinking));

        let embedding = models.iter().find(|m| m.id == "nomic-embed-text").unwrap();
        assert_eq!(embedding.model_type, Some(ModelType::Embedding));
        assert_eq!(
            embedding.supported_functions,
            vec![ModelFunction::Embeddings]
        );
    }

    #[tokio::test]
    async fn list_models_fails_on_a_malformed_response() {
        use axum::{Router, routing::get};
        async fn not_models() -> &'static str {
            "{}"
        }
        let app = Router::new().route("/api/v1/models", get(not_models));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let provider = LMStudioProvider::new(
            "test",
            &serde_json::json!({ "base_url": format!("http://{addr}") }),
        )
        .unwrap();
        assert!(provider.list_models().await.is_err());
    }
}
