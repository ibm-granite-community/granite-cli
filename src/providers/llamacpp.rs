use crate::models::huggingface::hf_repo_id;
use crate::models::{ModelFunction, ModelMetadata, ModelVariant};
use crate::providers::base::{
    ApiEndpoint, ApiType, AuthType, HasProviderMetadata, HealthStatus, ModelFormat, Provider,
    ProviderError, ProviderMetadata, ProviderType, http_health_check,
};
use crate::registry::{ConfigConstructable, Secret};
use crate::utils::ui::Ui;
use crate::utils::ui::base::PullHandle;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// A single `data: <json>\n\n` frame from `GET /models/sse`.
#[derive(Debug, Deserialize)]
struct LlamaCppSseFrame {
    model: String,
    event: String,
    #[serde(default)]
    data: serde_json::Value,
}

/// Response body of `GET /models`.
#[derive(Debug, Deserialize)]
struct LlamaCppModelsResponse {
    data: Vec<LlamaCppModelEntry>,
}

#[derive(Debug, Deserialize)]
struct LlamaCppModelEntry {
    id: String,
    status: LlamaCppModelStatus,
}

#[derive(Debug, Deserialize)]
struct LlamaCppModelStatus {
    value: String,
    #[serde(default)]
    failed: bool,
}

/// Find the byte offset of the next `\n\n` frame terminator.
fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

/*-- llama.cpp Provider Configuration ----------------------------------------*/

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LlamaCppProviderConfig {
    /// Base URL for the llama.cpp server
    #[serde(default = "default_llamacpp_url")]
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
    #[serde(default = "default_llamacpp_health_endpoint")]
    pub health_check_endpoint: String,
}

fn default_llamacpp_url() -> String {
    "http://localhost:8080".to_string()
}

fn default_timeout() -> u64 {
    10
}

fn default_verify_ssl() -> bool {
    true
}

fn default_llamacpp_health_endpoint() -> String {
    "/health".to_string()
}

impl Default for LlamaCppProviderConfig {
    fn default() -> Self {
        Self {
            base_url: default_llamacpp_url(),
            api_key: None,
            timeout_secs: default_timeout(),
            verify_ssl: default_verify_ssl(),
            health_check_endpoint: default_llamacpp_health_endpoint(),
        }
    }
}

/*-- llama.cpp Provider Implementation ---------------------------------------*/

pub struct LlamaCppProvider {
    config: LlamaCppProviderConfig,
    client: reqwest::Client,
    /// Same TLS settings as `client` but with no request timeout, used for
    /// the long-lived `GET /models/sse` watch — `client`'s `timeout_secs`
    /// (default 10s) applies to the whole request including the streamed
    /// body, so it would otherwise abort the connection long before a
    /// multi-hundred-MB download finishes.
    stream_client: reqwest::Client,
}

impl LlamaCppProvider {
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

        map.insert(
            ModelFunction::Transcription,
            vec![ApiEndpoint::OpenAIAudioTranscription],
        );

        map
    }

    /// Watch `GET /models/sse` for progress/terminal events for `model_ref`.
    /// Returns `Ok(true)` once a `download_finished` event is seen (already
    /// reported to `ui`), `Ok(false)` if the stream ends or errors before any
    /// terminal event (caller should fall back to polling), or `Err` if a
    /// `download_failed` event is seen (already reported to `ui`).
    async fn watch_pull_via_sse(
        &self,
        model_ref: &str,
        handle: PullHandle,
        label: &str,
        ui: &dyn Ui,
    ) -> Result<bool, ProviderError> {
        let sse_url = format!("{}/models/sse", self.config.base_url);
        let mut request = self.stream_client.get(&sse_url);
        if let Some(key) = &self.config.api_key {
            request = request.bearer_auth(&key.0);
        }

        let response = match request.send().await {
            Ok(r) if r.status().is_success() => r,
            _ => return Ok(false),
        };

        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(_) => return Ok(false),
            };
            buf.extend_from_slice(&chunk);

            while let Some(pos) = find_double_newline(&buf) {
                let frame_bytes: Vec<u8> = buf.drain(..pos + 2).collect();
                let frame_text = String::from_utf8_lossy(&frame_bytes);
                let Some(json_str) = frame_text.trim().strip_prefix("data:") else {
                    continue;
                };
                let json_str = json_str.trim();
                if json_str.is_empty() {
                    continue;
                }
                let frame: LlamaCppSseFrame = match serde_json::from_str(json_str) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                if frame.model != model_ref {
                    continue;
                }

                match frame.event.as_str() {
                    "download_progress" => {
                        if let Some(progress) =
                            frame.data.get("progress").and_then(|p| p.as_object())
                        {
                            let mut done_sum = 0u64;
                            let mut total_sum = 0u64;
                            for entry in progress.values() {
                                done_sum += entry.get("done").and_then(|v| v.as_u64()).unwrap_or(0);
                                total_sum +=
                                    entry.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
                            }
                            ui.pull_progress(
                                handle,
                                done_sum,
                                if total_sum > 0 { Some(total_sum) } else { None },
                            );
                        }
                    }
                    "download_finished" => {
                        ui.pull_finish(handle, label, None);
                        return Ok(true);
                    }
                    "download_failed" => {
                        let err = "download failed".to_string();
                        ui.pull_finish(handle, label, Some(&err));
                        return Err(ProviderError::Other(err));
                    }
                    _ => {}
                }
            }
        }

        Ok(false)
    }

    /// Fallback for when the SSE stream ends without a terminal event: poll
    /// `GET /models` and watch the target model's `status.value` until it's
    /// no longer `"downloading"`.
    async fn watch_pull_via_polling(
        &self,
        repo: &str,
        handle: PullHandle,
        label: &str,
        ui: &dyn Ui,
    ) -> Result<crate::providers::PullResult, ProviderError> {
        let models_url = format!("{}/models", self.config.base_url);
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;

            let mut request = self.client.get(&models_url);
            if let Some(key) = &self.config.api_key {
                request = request.bearer_auth(&key.0);
            }

            let response = request.send().await?;
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                let err = format!("llama.cpp models status check failed ({status}): {body}");
                ui.pull_finish(handle, label, Some(&err));
                return Err(ProviderError::Other(err));
            }
            let parsed: LlamaCppModelsResponse = response.json().await?;
            let Some(entry) = parsed.data.iter().find(|e| e.id.contains(repo)) else {
                continue;
            };

            if entry.status.failed {
                let err = "download failed".to_string();
                ui.pull_finish(handle, label, Some(&err));
                return Err(ProviderError::Other(err));
            }
            if entry.status.value != "downloading" {
                ui.pull_finish(handle, label, None);
                return Ok(crate::providers::PullResult::Success);
            }
        }
    }
}

impl ConfigConstructable for LlamaCppProvider {
    fn new(cfg: &serde_json::Value) -> Self {
        let config: LlamaCppProviderConfig =
            serde_json::from_value(cfg.clone()).unwrap_or_default();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .danger_accept_invalid_certs(!config.verify_ssl)
            .build()
            .expect("Failed to create HTTP client");

        let stream_client = reqwest::Client::builder()
            .danger_accept_invalid_certs(!config.verify_ssl)
            .build()
            .expect("Failed to create HTTP client");

        Self {
            config,
            client,
            stream_client,
        }
    }
}

#[async_trait]
impl Provider for LlamaCppProvider {
    fn name(&self) -> &str {
        "llama.cpp"
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
        vec![ModelFormat::GGUF]
    }

    fn can_run_model(&self, variant_format: &str, _variant_precision: &str) -> bool {
        variant_format.eq_ignore_ascii_case("gguf")
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
        let model_ref = format!("{}:{}", repo, variant.precision);
        let label = format!(
            "{} ({} {})",
            model.family, variant.format, variant.precision
        );

        let post_url = format!("{}/models", self.config.base_url);
        let mut request = self
            .client
            .post(&post_url)
            .json(&serde_json::json!({ "model": model_ref }));
        if let Some(key) = &self.config.api_key {
            request = request.bearer_auth(&key.0);
        }

        let response = request.send().await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::BAD_REQUEST && body.contains("already exists") {
                ui.info(&format!("{label} is already downloaded."));
                return Ok(crate::providers::PullResult::Success);
            }
            return Err(ProviderError::Other(format!(
                "llama.cpp model pull request failed ({status}): {body}"
            )));
        }

        let handle = ui.pull_start(&label, None);

        match self
            .watch_pull_via_sse(&model_ref, handle, &label, ui)
            .await
        {
            Ok(true) => Ok(crate::providers::PullResult::Success),
            Ok(false) => self.watch_pull_via_polling(repo, handle, &label, ui).await,
            Err(e) => Err(e),
        }
    }
}

impl HasProviderMetadata for LlamaCppProvider {
    fn metadata() -> ProviderMetadata {
        ProviderMetadata {
            name: "llama.cpp".to_string(),
            description: "High-performance local inference server for GGUF models with OpenAI and Anthropic API compatibility".to_string(),
            provider_type: ProviderType::Local,
            default_endpoint: "http://localhost:8080".to_string(),
            supported_api_types: vec![ApiType::OpenAI, ApiType::Anthropic],
            default_function_endpoints: Self::default_function_endpoints(),
            supported_formats: vec![ModelFormat::GGUF],
            authentication: vec![AuthType::None, AuthType::BearerToken],
            tags: vec![
                "llama.cpp".to_string(),
                "local".to_string(),
                "gguf".to_string(),
                "high-performance".to_string(),
            ],
        }
    }

    fn config_schema() -> schemars::Schema {
        schemars::schema_for!(LlamaCppProviderConfig)
    }

    fn default_config() -> serde_json::Value {
        serde_json::to_value(LlamaCppProviderConfig::default()).unwrap_or_default()
    }
}

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = LlamaCppProviderConfig::default();
        assert_eq!(config.base_url, "http://localhost:8080");
        assert!(config.api_key.is_none());
        assert_eq!(config.timeout_secs, 10);
        assert!(config.verify_ssl);
        assert_eq!(config.health_check_endpoint, "/health");
    }

    #[test]
    fn test_provider_metadata() {
        let meta = LlamaCppProvider::metadata();
        assert_eq!(meta.name, "llama.cpp");
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
            "base_url": "http://example.com:9000",
            "timeout_secs": 30
        });
        let provider = LlamaCppProvider::new(&cfg);
        assert_eq!(provider.config.base_url, "http://example.com:9000");
        assert_eq!(provider.config.timeout_secs, 30);
    }

    #[test]
    fn test_can_run_model_accepts_gguf() {
        let provider = LlamaCppProvider::new(&serde_json::json!({}));
        assert!(provider.can_run_model("gguf", "Q4_K_M"));
        assert!(provider.can_run_model("GGUF", "fp16"));
    }

    #[test]
    fn test_can_run_model_rejects_non_gguf() {
        let provider = LlamaCppProvider::new(&serde_json::json!({}));
        assert!(!provider.can_run_model("safetensors", "fp16"));
        assert!(!provider.can_run_model("onnx", "fp32"));
    }

    #[test]
    fn test_find_double_newline() {
        assert_eq!(find_double_newline(b"data: {}\n\nmore"), Some(8));
        assert_eq!(find_double_newline(b"no terminator here"), None);
    }

    #[test]
    fn test_sse_frame_parses_download_progress() {
        let json = r#"{"model":"owner/repo:Q4_K_M","event":"download_progress","data":{"progress":{"https://x/a.gguf":{"done":50,"total":100}}}}"#;
        let frame: LlamaCppSseFrame = serde_json::from_str(json).unwrap();
        assert_eq!(frame.model, "owner/repo:Q4_K_M");
        assert_eq!(frame.event, "download_progress");
        let progress = frame.data.get("progress").unwrap().as_object().unwrap();
        let entry = progress.values().next().unwrap();
        assert_eq!(entry.get("done").unwrap().as_u64(), Some(50));
        assert_eq!(entry.get("total").unwrap().as_u64(), Some(100));
    }

    #[test]
    fn test_sse_frame_parses_terminal_events() {
        let json = r#"{"model":"owner/repo:Q4_K_M","event":"download_finished","data":{}}"#;
        let frame: LlamaCppSseFrame = serde_json::from_str(json).unwrap();
        assert_eq!(frame.event, "download_finished");
    }

    #[test]
    fn test_models_response_parses_status() {
        let json = r#"{"data":[{"id":"owner/repo:Q4_K_M","status":{"value":"downloading"}}],"object":"list"}"#;
        let parsed: LlamaCppModelsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.data[0].id, "owner/repo:Q4_K_M");
        assert_eq!(parsed.data[0].status.value, "downloading");
        assert!(!parsed.data[0].status.failed);
    }

    #[test]
    fn test_models_response_parses_failed_status() {
        let json = r#"{"data":[{"id":"owner/repo:Q4_K_M","status":{"value":"error","failed":true}}],"object":"list"}"#;
        let parsed: LlamaCppModelsResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.data[0].status.failed);
    }
}
