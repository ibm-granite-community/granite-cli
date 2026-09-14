//! `DoclingMCPCapability`: installs and runs the
//! [docling-mcp](https://github.com/docling-project/docling-mcp) server via
//! `uvx`, wiring it to a configured granite-docling model's provider endpoint
//! so document-conversion tools are available to any launched coding agent.
//!
//! The capability resolves the model's provider at `bind()` time and injects
//! its base URL and API key as `DOCLING_MCP_SERVICE_URL` /
//! `DOCLING_MCP_SERVICE_API_KEY` environment variables, enabling docling-mcp's
//! remote mode against the serving endpoint. The model must advertise the
//! `DocumentConversion` function (only `granite-docling-*` models carry this
//! tag today).
//!
//! `bind()` returns a `McpBinding::Stdio` whose `command` is `uvx` — the
//! downstream launcher passes that directly to the tool it is wrapping,
//! which then spawns the server process.

use crate::capabilities::base::{
    Binding, BindingRequest, BindingType, Capability, CapabilityMetadata, Dependency,
    HasCapabilityMetadata, LaunchContext, McpBinding, McpBindingRequest, McpTransportKind,
};
use crate::capabilities::requirement::ModelRequirement;
use crate::models::{ConfiguredModel, ModelFunction, ModelType};
use crate::providers::ApiType;
use crate::registry::ConfigConstructable;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_valid::Validate;
use std::collections::HashSet;

/*-- DoclingMCPCapabilityConfig --------------------------------------------------*/

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, Validate)]
pub struct DoclingMCPCapabilityConfig {
    /// Key into the configured models map for the granite-docling model whose
    /// provider endpoint docling-mcp will call for document conversion.
    #[validate(min_length = 1)]
    pub model_id: String,
    /// docling-mcp package specifier passed to `uvx --from`. Defaults to the
    /// latest published release; pin to a specific version (e.g.
    /// `"docling-mcp==3.1.0"`) for reproducible environments.
    #[serde(default = "default_package")]
    pub package: String,
    /// Request timeout in seconds for docling-mcp's calls to the provider.
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
}

fn default_package() -> String {
    "docling-mcp".to_string()
}

fn default_timeout_seconds() -> u64 {
    300
}

impl Default for DoclingMCPCapabilityConfig {
    fn default() -> Self {
        Self {
            model_id: String::new(),
            package: default_package(),
            timeout_seconds: default_timeout_seconds(),
        }
    }
}

/*-- DoclingMCPCapability --------------------------------------------------------*/

pub struct DoclingMCPCapability {
    instance_id: String,
    config: DoclingMCPCapabilityConfig,
    configured_model: ConfiguredModel,
}

impl ConfigConstructable for DoclingMCPCapability {
    type Config = DoclingMCPCapabilityConfig;

    fn new(
        instance_id: &str,
        cfg: &serde_json::Value,
        global_config: &crate::config::Config,
    ) -> Self {
        let config: DoclingMCPCapabilityConfig =
            serde_json::from_value(cfg.clone()).unwrap_or_default();
        let configured_model = ConfiguredModel::resolve(&config.model_id, global_config);
        Self {
            instance_id: instance_id.to_string(),
            config,
            configured_model,
        }
    }
}

impl crate::registry::Named for DoclingMCPCapability {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait]
impl Capability for DoclingMCPCapability {
    fn name(&self) -> &str {
        "Docling MCP Server"
    }

    fn description(&self) -> &str {
        "Runs docling-mcp via uvx, wiring document-conversion tools to a granite-docling model provider for use by a launched coding agent."
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![
            Dependency::Model {
                config_key: "model_id".to_string(),
                requirement: model_selection_requirement(),
                resolved_id: Some(self.config.model_id.clone()),
                required: true,
            },
            Dependency::ExternalTool {
                requirement: crate::capabilities::requirement::ShellCommandRequirement {
                    command: "uvx".to_string(),
                },
                required: true,
            },
        ]
    }

    fn binding_types(&self) -> HashSet<BindingType> {
        HashSet::from([BindingType::Mcp])
    }

    async fn bind(&self, request: BindingRequest) -> anyhow::Result<Binding> {
        let McpBindingRequest {
            supported_transports,
        } = match request {
            BindingRequest::Mcp(r) => r,
            other => anyhow::bail!(
                "DoclingMCPCapability does not handle {:?} binding requests",
                other.binding_type()
            ),
        };
        anyhow::ensure!(
            supported_transports.contains(&McpTransportKind::Stdio),
            "no MCP transport in common with the requesting launcher (it offered \
             {supported_transports:?}; DoclingMCPCapability only serves Stdio)"
        );

        let model_id = &self.config.model_id;
        let (provider, endpoint, _model_name) = self.configured_model.resolve_provider_endpoint(
            model_id,
            ApiType::OpenAI,
            ModelFunction::ImageUnderstanding,
            ModelFunction::Chat,
        )?;

        // Build the service URL: strip the trailing operation path so docling-mcp
        // sees the API root (e.g. `http://localhost:11434/v1`), the same
        // convention used by VisionMCPCapability's `vlm_base_url`.
        let service_url = service_base_url(provider.base_url(), endpoint.path());

        let mut env = std::collections::HashMap::new();
        env.insert("DOCLING_MCP_SERVICE_URL".to_string(), service_url);
        env.insert(
            "DOCLING_MCP_CONVERSION_MODE".to_string(),
            "remote".to_string(),
        );
        env.insert(
            "DOCLING_MCP_SERVICE_TIMEOUT".to_string(),
            self.config.timeout_seconds.to_string(),
        );
        if let Some(key) = provider.api_key()
            && !key.0.is_empty()
        {
            env.insert("DOCLING_MCP_SERVICE_API_KEY".to_string(), key.0.clone());
        }

        Ok(Binding::Mcp(McpBinding::Stdio {
            command: "uvx".to_string(),
            args: vec![
                format!("--from={}", self.config.package),
                "docling-mcp-server".to_string(),
                "--transport".to_string(),
                "stdio".to_string(),
            ],
            env,
        }))
    }

    async fn on_shutdown(&self, _context: &LaunchContext) -> anyhow::Result<()> {
        Ok(())
    }
}

impl HasCapabilityMetadata for DoclingMCPCapability {
    fn metadata() -> CapabilityMetadata {
        CapabilityMetadata {
            name: "Docling MCP Server".to_string(),
            description: "Runs docling-mcp via uvx, wiring document-conversion tools to a granite-docling model provider for use by a launched coding agent.".to_string(),
            dependencies: vec![
                Dependency::Model {
                    config_key: "model_id".to_string(),
                    requirement: model_selection_requirement(),
                    resolved_id: None,
                    required: true,
                },
                Dependency::ExternalTool {
                    requirement: crate::capabilities::requirement::ShellCommandRequirement {
                        command: "uvx".to_string(),
                    },
                    required: true,
                },
            ],
            tags: vec!["docling".to_string(), "document".to_string(), "mcp".to_string()],
            supported_binding_types: HashSet::from([BindingType::Mcp]),
        }
    }
}

/// Strips the trailing chat-completions operation path from the endpoint so
/// docling-mcp gets the API root (e.g. `http://localhost:11434/v1`), matching
/// the convention used in `VisionMCPCapability`.
fn service_base_url(base_url: &str, endpoint_path: &str) -> String {
    let root = base_url.trim_end_matches('/');
    let prefix = endpoint_path
        .strip_suffix("/chat/completions")
        .unwrap_or("");
    format!("{root}{prefix}")
}

/// Requirement used for model selection, catalog display, and the post-setup
/// usability check in `model_candidates`. Filters by `family` to pin to
/// granite-docling models specifically, and by `Chat`/`ImageUnderstanding`
/// which inference providers (Ollama, llama.cpp) expose as real endpoints.
/// `DocumentConversion` is a model-classification tag that no provider exposes
/// as an endpoint, so it is not included here — it is checked separately in
/// `bind()` via `resolve_provider_endpoint`.
fn model_selection_requirement() -> ModelRequirement {
    ModelRequirement {
        family: Some("Granite Docling".to_string()),
        model_type: Some(ModelType::Vision),
        supported_functions: vec![ModelFunction::Chat, ModelFunction::ImageUnderstanding],
        ..Default::default()
    }
}

/*-- tests -----------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_base_url_strips_chat_completions_suffix() {
        assert_eq!(
            service_base_url("http://localhost:11434", "/v1/chat/completions"),
            "http://localhost:11434/v1"
        );
    }

    #[test]
    fn service_base_url_trims_trailing_slash() {
        assert_eq!(
            service_base_url("http://localhost:1234/", "/v1/chat/completions"),
            "http://localhost:1234/v1"
        );
    }

    #[test]
    fn service_base_url_no_path_prefix() {
        assert_eq!(
            service_base_url("http://localhost:8080", "/chat/completions"),
            "http://localhost:8080"
        );
    }

    #[test]
    fn binding_types_reports_mcp() {
        use crate::config::{Config, ModelConfig};
        use crate::registry::ConfigConstructable;

        let mut config = Config::default();
        config.models.insert(
            "granite-docling-258M".to_string(),
            ModelConfig {
                model_id: "granite-docling-258M".to_string(),
                model_type: "granite-docling-258M".to_string(),
                provider_id: None,
                variant: None,
                config: serde_json::json!({}),
            },
        );
        let cap = DoclingMCPCapability::new(
            "docling",
            &serde_json::json!({ "model_id": "granite-docling-258M" }),
            &config,
        );
        assert_eq!(cap.binding_types(), HashSet::from([BindingType::Mcp]));
    }

    #[test]
    fn metadata_has_expected_tags() {
        let meta = DoclingMCPCapability::metadata();
        assert!(meta.tags.contains(&"docling".to_string()));
        assert!(meta.tags.contains(&"mcp".to_string()));
        assert!(meta.supported_binding_types.contains(&BindingType::Mcp));
    }
}
