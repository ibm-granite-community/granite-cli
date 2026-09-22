use crate::models::huggingface::hf_repo_id;
use crate::models::{ModelFunction, ModelMetadata, ModelVariant};
use crate::providers::base::{
    ApiEndpoint, ApiType, AuthType, HasProviderMetadata, HealthStatus, ModelFormat, Provider,
    ProviderError, ProviderMetadata, ProviderType, http_health_check,
};
use crate::registry::{ConfigConstructable, ConstructError, Secret};
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
    instance_id: String,
    config: OllamaProviderConfig,
    client: reqwest::Client,
    /// Same TLS settings as `client` but with no request timeout, used for
    /// the long-lived streaming `POST /api/pull` — `client`'s `timeout_secs`
    /// (default 10s) applies to the whole request including the streamed
    /// body, so it would otherwise abort the download long before it finishes.
    stream_client: reqwest::Client,
}

/// Extract the Ollama model reference from an `https://ollama.com/...` variant URL.
///
/// Two URL conventions are supported:
/// - `https://ollama.com/library/<name>[:<tag>]` → `<name>[:<tag>]`
///   (the `library` org is implicit and omitted in the ref)
/// - `https://ollama.com/<org>/<name>[:<tag>]` → `<org>/<name>[:<tag>]`
///   (explicit org is preserved in the ref)
///
/// When `:<tag>` is absent, `:latest` is appended — matching Ollama's own
/// convention for untagged model references.
fn ollama_model_ref(url: &str) -> Option<String> {
    let path = url
        .strip_prefix("https://ollama.com/")
        .or_else(|| url.strip_prefix("ollama.com/"))
        .filter(|s| !s.is_empty())?;

    // `library/` is Ollama's implicit org — strip it so the ref is just `<name>[:<tag>]`.
    // Any other path segment is an explicit org and is kept as-is.
    let model_ref = path.strip_prefix("library/").unwrap_or(path);
    if model_ref.is_empty() {
        return None;
    }

    // Ollama treats a tagless name as `<name>:latest` — make that explicit.
    Some(if model_ref.contains(':') {
        model_ref.to_string()
    } else {
        format!("{model_ref}:latest")
    })
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
            ModelFunction::ToolCalling,
            vec![
                ApiEndpoint::OpenAIChat,
                ApiEndpoint::OllamaChat,
                ApiEndpoint::AnthropicMessages,
            ],
        );

        map.insert(
            ModelFunction::Thinking,
            vec![
                ApiEndpoint::OpenAIChat,
                ApiEndpoint::OllamaChat,
                ApiEndpoint::AnthropicMessages,
            ],
        );

        map.insert(
            ModelFunction::ImageUnderstanding,
            vec![
                ApiEndpoint::OpenAIChat,
                ApiEndpoint::OllamaChat,
                ApiEndpoint::AnthropicMessages,
            ],
        );

        map.insert(
            ModelFunction::Guardian,
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
    type Config = OllamaProviderConfig;

    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        let config: OllamaProviderConfig =
            serde_json::from_value(cfg.clone()).map_err(ConstructError::settings)?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .danger_accept_invalid_certs(!config.verify_ssl)
            .build()
            .map_err(ConstructError::settings)?;

        let stream_client = reqwest::Client::builder()
            .danger_accept_invalid_certs(!config.verify_ssl)
            .build()
            .map_err(ConstructError::settings)?;

        Ok(Self {
            instance_id: instance_id.to_string(),
            config,
            client,
            stream_client,
        })
    }
}

impl crate::registry::Named for OllamaProvider {
    fn instance_id(&self) -> &str {
        &self.instance_id
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

    fn model_alias(
        &self,
        _model_id: String,
        variant: Option<&crate::models::ModelVariant>,
    ) -> Option<String> {
        variant.and_then(|v| ollama_model_ref(&v.url))
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
        let model_ref = if let Some(name) = ollama_model_ref(&variant.url) {
            name
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
        let provider = OllamaProvider::new("my-ollama", &cfg).unwrap();
        assert_eq!(provider.config.base_url, "http://example.com:8080");
        assert_eq!(provider.config.timeout_secs, 30);
    }

    #[test]
    fn test_can_run_model_accepts_gguf() {
        let provider = OllamaProvider::new("my-ollama", &serde_json::json!({})).unwrap();
        assert!(provider.can_run_model("gguf", "Q4_K_M"));
        assert!(provider.can_run_model("GGUF", "fp16"));
    }

    #[test]
    fn test_can_run_model_rejects_non_gguf() {
        let provider = OllamaProvider::new("my-ollama", &serde_json::json!({})).unwrap();
        assert!(!provider.can_run_model("safetensors", "fp16"));
        assert!(!provider.can_run_model("onnx", "fp32"));
    }

    #[test]
    fn test_ollama_model_ref_parses_library_url() {
        assert_eq!(
            ollama_model_ref("https://ollama.com/library/granite4:1b"),
            Some("granite4:1b".to_string())
        );
    }

    #[test]
    fn test_ollama_model_ref_appends_latest_when_tag_absent_library() {
        assert_eq!(
            ollama_model_ref("https://ollama.com/library/granite4.1"),
            Some("granite4.1:latest".to_string())
        );
    }

    #[test]
    fn test_ollama_model_ref_parses_org_scoped_url() {
        assert_eq!(
            ollama_model_ref("https://ollama.com/ibm/granite4.1:8b"),
            Some("ibm/granite4.1:8b".to_string())
        );
    }

    #[test]
    fn test_ollama_model_ref_appends_latest_when_tag_absent_org() {
        assert_eq!(
            ollama_model_ref("https://ollama.com/ibm/granite4.1"),
            Some("ibm/granite4.1:latest".to_string())
        );
    }

    #[test]
    fn test_ollama_model_ref_rejects_non_ollama_url() {
        assert_eq!(
            ollama_model_ref(
                "https://huggingface.co/ibm-granite/granite-4.1-30b-GGUF/blob/main/x.gguf"
            ),
            None
        );
    }

    #[test]
    fn test_ollama_model_ref_rejects_empty_path() {
        assert_eq!(ollama_model_ref("https://ollama.com/"), None);
        assert_eq!(ollama_model_ref("https://ollama.com/library/"), None);
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
    fn test_model_alias_returns_library_ref_for_ollama_variant() {
        let provider = OllamaProvider::new("my-ollama", &serde_json::json!({})).unwrap();
        let variant = ModelVariant {
            format: "Ollama".to_string(),
            precision: "Q4_K_M".to_string(),
            size_gb: Some(5.3),
            url: "https://ollama.com/library/granite4.1:8b".to_string(),
        };
        assert_eq!(
            provider.model_alias("unused".to_string(), Some(&variant)),
            Some("granite4.1:8b".to_string())
        );
    }

    #[test]
    fn test_model_alias_returns_org_scoped_ref_for_org_url() {
        let provider = OllamaProvider::new("my-ollama", &serde_json::json!({})).unwrap();
        let variant = ModelVariant {
            format: "Ollama".to_string(),
            precision: "Q4_K_M".to_string(),
            size_gb: Some(5.3),
            url: "https://ollama.com/ibm/granite4.1:8b".to_string(),
        };
        assert_eq!(
            provider.model_alias("unused".to_string(), Some(&variant)),
            Some("ibm/granite4.1:8b".to_string())
        );
    }

    #[test]
    fn test_model_alias_returns_none_for_non_ollama_variant() {
        let provider = OllamaProvider::new("my-ollama", &serde_json::json!({})).unwrap();
        let variant = ModelVariant {
            format: "GGUF".to_string(),
            precision: "Q4_K_M".to_string(),
            size_gb: Some(5.3),
            url: "https://huggingface.co/ibm-granite/granite-4.1-8b-GGUF/blob/main/granite-4.1-8b-Q4_K_M.gguf".to_string(),
        };
        assert_eq!(
            provider.model_alias("unused".to_string(), Some(&variant)),
            None
        );
    }

    #[test]
    fn test_model_alias_returns_none_when_no_variant() {
        let provider = OllamaProvider::new("my-ollama", &serde_json::json!({})).unwrap();
        assert_eq!(provider.model_alias("unused".to_string(), None), None);
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
