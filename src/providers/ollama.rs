use crate::models::huggingface::hf_repo_id;
use crate::models::{ModelFunction, ModelMetadata, ModelVariant};
use crate::providers::base::{
    ApiEndpoint, ApiType, AuthType, HasProviderMetadata, HealthStatus, ModelFormat, Provider,
    ProviderError, ProviderMetadata, ProviderType, http_health_check,
};
use crate::registry::{ConfigConstructable, Secret};
use crate::utils::ui::Ui;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/*-- Ollama Provider Configuration -------------------------------------------*/

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OllamaProviderConfig {
    /// Base URL for the Ollama API
    #[serde(default = "default_ollama_url")]
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
    #[serde(default = "default_ollama_health_endpoint")]
    pub health_check_endpoint: String,
}

fn default_ollama_url() -> String {
    "http://localhost:11434".to_string()
}

fn default_timeout() -> u64 {
    10
}

fn default_verify_ssl() -> bool {
    true
}

fn default_ollama_health_endpoint() -> String {
    "/api/tags".to_string()
}

impl Default for OllamaProviderConfig {
    fn default() -> Self {
        Self {
            base_url: default_ollama_url(),
            api_key: None,
            timeout_secs: default_timeout(),
            verify_ssl: default_verify_ssl(),
            health_check_endpoint: default_ollama_health_endpoint(),
        }
    }
}

/*-- Ollama Provider Implementation ------------------------------------------*/

pub struct OllamaProvider {
    config: OllamaProviderConfig,
    client: reqwest::Client,
    /// Same TLS settings as `client` but with no request timeout, used for
    /// the long-lived streaming `POST /api/pull` — `client`'s `timeout_secs`
    /// (default 10s) applies to the whole request including the streamed
    /// body, so it would otherwise abort the download long before it finishes.
    stream_client: reqwest::Client,
}

/// Extract the Ollama library model reference (e.g. `"granite4:1b"`) from a
/// `https://ollama.com/library/...` variant URL.
fn ollama_library_ref(url: &str) -> Option<&str> {
    url.strip_prefix("https://ollama.com/library/")
        .or_else(|| url.strip_prefix("ollama.com/library/"))
        .filter(|s| !s.is_empty())
}

/// A single NDJSON line from Ollama's `POST /api/pull` progress stream.
#[derive(Debug, Deserialize)]
struct OllamaPullProgress {
    status: String,
    total: Option<u64>,
    completed: Option<u64>,
    error: Option<String>,
}

impl OllamaProvider {
    fn default_function_endpoints() -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        let mut map = HashMap::new();

        map.insert(
            ModelFunction::Chat,
            vec![
                ApiEndpoint::OpenAIChat,
                ApiEndpoint::OllamaChat,
                ApiEndpoint::AnthropicMessages,
            ],
        );

        map.insert(
            ModelFunction::Embeddings,
            vec![ApiEndpoint::OpenAIEmbeddings, ApiEndpoint::OllamaEmbeddings],
        );

        map.insert(
            ModelFunction::Transcription,
            vec![ApiEndpoint::OpenAIAudioTranscription],
        );

        map
    }
}

impl ConfigConstructable for OllamaProvider {
    fn new(cfg: &serde_json::Value) -> Self {
        let config: OllamaProviderConfig = serde_json::from_value(cfg.clone()).unwrap_or_default();

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
impl Provider for OllamaProvider {
    fn name(&self) -> &str {
        "Ollama"
    }

    fn function_endpoints(&self) -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        Self::default_function_endpoints()
    }

    fn supported_api_types(&self) -> Vec<ApiType> {
        vec![ApiType::OpenAI, ApiType::Ollama, ApiType::Anthropic]
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
        variant_format.eq_ignore_ascii_case("gguf") || variant_format.eq_ignore_ascii_case("ollama")
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
        let model_ref = if let Some(name) = ollama_library_ref(&variant.url) {
            name.to_string()
        } else if let Some(repo) = hf_repo_id(&variant.url) {
            format!("hf.co/{}:{}", repo, variant.precision)
        } else {
            return Err(ProviderError::Other(format!(
                "cannot determine an Ollama model reference for {} variant {}/{}",
                model.family, variant.format, variant.precision
            )));
        };

        let label = format!(
            "{} ({} {})",
            model.family, variant.format, variant.precision
        );

        let url = format!("{}/api/pull", self.config.base_url);
        let mut request = self.stream_client.post(&url).json(&serde_json::json!({
            "model": model_ref,
            "stream": true,
        }));
        if let Some(key) = &self.config.api_key {
            request = request.bearer_auth(&key.0);
        }

        let response = request.send().await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Other(format!(
                "Ollama pull failed ({status}): {body}"
            )));
        }

        let handle = ui.pull_start(&label, None);
        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut success_observed = false;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buf.extend_from_slice(&chunk);

            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                let progress: OllamaPullProgress = match serde_json::from_str(line) {
                    Ok(p) => p,
                    Err(_) => continue,
                };

                if let Some(err) = progress.error {
                    ui.pull_finish(handle, &label, Some(&err));
                    return Err(ProviderError::Other(err));
                }
                if let (Some(total), Some(completed)) = (progress.total, progress.completed) {
                    ui.pull_progress(handle, completed, Some(total));
                }
                if progress.status == "success" {
                    success_observed = true;
                    break;
                }
            }
            if success_observed {
                break;
            }
        }

        if success_observed {
            ui.pull_finish(handle, &label, None);
            Ok(crate::providers::PullResult::Success)
        } else {
            ui.pull_finish(handle, &label, None);
            Err(ProviderError::Other(
                "Ollama pull stream ended without success status".to_string(),
            ))
        }
    }
}

impl HasProviderMetadata for OllamaProvider {
    fn metadata() -> ProviderMetadata {
        ProviderMetadata {
            name: "Ollama".to_string(),
            description: "Local inference server supporting multiple API protocols and GGUF models"
                .to_string(),
            provider_type: ProviderType::Local,
            default_endpoint: "http://localhost:11434".to_string(),
            supported_api_types: vec![ApiType::OpenAI, ApiType::Ollama, ApiType::Anthropic],
            default_function_endpoints: Self::default_function_endpoints(),
            supported_formats: vec![ModelFormat::GGUF, ModelFormat::Ollama],
            authentication: vec![AuthType::None, AuthType::BearerToken],
            tags: vec![
                "ollama".to_string(),
                "local".to_string(),
                "gguf".to_string(),
                "multi-api".to_string(),
            ],
        }
    }

    fn config_schema() -> schemars::Schema {
        schemars::schema_for!(OllamaProviderConfig)
    }

    fn default_config() -> serde_json::Value {
        serde_json::to_value(OllamaProviderConfig::default()).unwrap_or_default()
    }
}

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    /// Result of processing a batch of Ollama pull progress NDJSON lines.
    #[derive(Debug, PartialEq, Eq)]
    enum PullOutcome {
        Success,
        Error(String),
        Incomplete,
    }

    /// Process a sequence of NDJSON lines from an Ollama `/api/pull` stream.
    /// Returns `Success` when a `status == "success"` line is observed,
    /// `Error` when a line with a non-empty `error` field is encountered,
    /// or `Incomplete` when no terminal event appeared in any line.
    fn process_pull_lines(lines: impl IntoIterator<Item = String>) -> PullOutcome {
        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let progress: OllamaPullProgress = match serde_json::from_str(line) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if let Some(err) = progress.error {
                return PullOutcome::Error(err);
            }
            if progress.status == "success" {
                return PullOutcome::Success;
            }
        }
        PullOutcome::Incomplete
    }

    #[test]
    fn test_default_config() {
        let config = OllamaProviderConfig::default();
        assert_eq!(config.base_url, "http://localhost:11434");
        assert!(config.api_key.is_none());
        assert_eq!(config.timeout_secs, 10);
        assert!(config.verify_ssl);
        assert_eq!(config.health_check_endpoint, "/api/tags");
    }

    #[test]
    fn test_provider_metadata() {
        let meta = OllamaProvider::metadata();
        assert_eq!(meta.name, "Ollama");
        assert!(meta.supported_api_types.contains(&ApiType::OpenAI));
        assert!(meta.supported_api_types.contains(&ApiType::Ollama));
        assert!(meta.supported_api_types.contains(&ApiType::Anthropic));
        assert!(
            meta.default_function_endpoints
                .contains_key(&ModelFunction::Chat)
        );
    }

    #[test]
    fn test_provider_constructs_from_json() {
        let cfg = serde_json::json!({
            "base_url": "http://example.com:8080",
            "timeout_secs": 30
        });
        let provider = OllamaProvider::new(&cfg);
        assert_eq!(provider.config.base_url, "http://example.com:8080");
        assert_eq!(provider.config.timeout_secs, 30);
    }

    #[test]
    fn test_can_run_model_accepts_gguf() {
        let provider = OllamaProvider::new(&serde_json::json!({}));
        assert!(provider.can_run_model("gguf", "Q4_K_M"));
        assert!(provider.can_run_model("GGUF", "fp16"));
    }

    #[test]
    fn test_can_run_model_rejects_non_gguf() {
        let provider = OllamaProvider::new(&serde_json::json!({}));
        assert!(!provider.can_run_model("safetensors", "fp16"));
        assert!(!provider.can_run_model("onnx", "fp32"));
    }

    #[test]
    fn test_ollama_library_ref_parses_library_url() {
        assert_eq!(
            ollama_library_ref("https://ollama.com/library/granite4:1b"),
            Some("granite4:1b")
        );
    }

    #[test]
    fn test_ollama_library_ref_rejects_non_library_url() {
        assert_eq!(
            ollama_library_ref(
                "https://huggingface.co/ibm-granite/granite-4.1-30b-GGUF/blob/main/x.gguf"
            ),
            None
        );
    }

    #[test]
    fn test_ollama_pull_progress_parses_line() {
        let line = r#"{"status":"pulling manifest"}"#;
        let progress: OllamaPullProgress = serde_json::from_str(line).unwrap();
        assert_eq!(progress.status, "pulling manifest");
        assert!(progress.total.is_none());

        let line = r#"{"status":"downloading","digest":"sha256:abc","total":100,"completed":50}"#;
        let progress: OllamaPullProgress = serde_json::from_str(line).unwrap();
        assert_eq!(progress.total, Some(100));
        assert_eq!(progress.completed, Some(50));
    }

    #[test]
    fn test_pull_outcome_success() {
        assert_eq!(
            process_pull_lines(vec![
                r#"{"status":"pulling manifest"}"#.to_string(),
                r#"{"status":"downloading","digest":"sha256:abc"}"#.to_string(),
                r#"{"status":"success"}"#.to_string(),
            ]),
            PullOutcome::Success,
        );
    }

    #[test]
    fn test_pull_outcome_incomplete_stream_ended() {
        assert_eq!(
            process_pull_lines(vec![
                r#"{"status":"pulling manifest"}"#.to_string(),
                r#"{"status":"downloading","digest":"sha256:abc"}"#.to_string(),
            ]),
            PullOutcome::Incomplete,
        );
    }

    #[test]
    fn test_pull_outcome_empty_stream() {
        assert_eq!(
            process_pull_lines(Vec::<String>::new()),
            PullOutcome::Incomplete
        );
    }

    #[test]
    fn test_pull_outcome_error_from_stream() {
        let result = process_pull_lines(vec![
            r#"{"status":"pulling manifest"}"#.to_string(),
            r#"{"status":"failed","error":"disk full"}"#.to_string(),
        ]);
        if let PullOutcome::Error(msg) = result {
            assert_eq!(msg, "disk full");
        } else {
            panic!("expected PullOutcome::Error, got {result:?}");
        }
    }

    #[test]
    fn test_pull_outcome_ignores_empty_lines() {
        assert_eq!(
            process_pull_lines(vec![
                "".to_string(),
                "  ".to_string(),
                r#"{"status":"pulling manifest"}"#.to_string(),
            ]),
            PullOutcome::Incomplete,
        );
    }

    #[test]
    fn test_pull_outcome_ignores_invalid_json() {
        assert_eq!(
            process_pull_lines(vec![
                "not json".to_string(),
                r#"{"status":"pulling manifest"}"#.to_string(),
            ]),
            PullOutcome::Incomplete,
        );
    }

    #[test]
    fn test_pull_outcome_success_before_error() {
        let result = process_pull_lines(vec![
            r#"{"status":"success"}"#.to_string(),
            r#"{"status":"failed","error":"too late"}"#.to_string(),
        ]);
        assert_eq!(result, PullOutcome::Success);
    }

    #[test]
    fn test_pull_outcome_incomplete_after_error_but_error_wins() {
        // When stream ends without success but with an error line, error wins
        let result = process_pull_lines(vec![
            r#"{"status":"downloading","digest":"sha256:abc"}"#.to_string(),
            r#"{"status":"failed","error":"connection reset"}"#.to_string(),
        ]);
        if let PullOutcome::Error(msg) = result {
            assert_eq!(msg, "connection reset");
        } else {
            panic!("expected PullOutcome::Error, got {result:?}");
        }
    }
}
