//! `SubAgentCapability`: defines a named sub-agent -- a prompt, a tool
//! allow-list, and a `Model`/`Provider` of its own -- that a launched coding
//! agent can delegate to independently of whichever model the main session
//! uses. See `docs/specs/0021-sub-agent-capability.md`.

use serde::{Deserialize, Serialize};
use serde_valid::Validate;

/*-- Macro: declare_sub_agent_basic -----------------------------------------------*/

/// Declares a sub-agent capability with a static prompt and static tools.
/// Config only has `description` and `model_id`.
#[macro_export]
macro_rules! declare_sub_agent_basic {
    (
        $name_struct:ident
        $config_struct:ident
        $name_cap:expr;
        $description_cap:expr;
        [$($tag:expr),* $(,)?]
        $description_expr:expr;
        $prompt_expr:expr;
        $tools_expr:expr;
        $known_type:expr
    ) => {
        #[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema, Validate)]
        pub struct $config_struct {
            /// Key into the configured models map (the user-chosen instance ID) for
            /// the model this sub-agent runs on.
            #[validate(min_length = 1)]
            pub model_id: String,
        }

        pub struct $name_struct {
            instance_id: String,
            config: $config_struct,
            /// Description shown to the parent agent for deciding when to delegate.
            pub description: String,
            /// Static prompt for this sub-agent.
            pub prompt: String,
            /// Static tool allow-list for this sub-agent.
            pub tools: Vec<$crate::capabilities::base::ToolName>,
        }

        $crate::paste::paste! {
            #[doc = concat!("[`", stringify!($name_struct), "`] with the model its `model_id` names. Built only by `resolve_refs`, so holding one is what says the name resolved and the model meets what this capability's metadata requires of it.")]
            pub struct [<Resolved $name_struct>] {
                inner: $name_struct,
                configured_model: $crate::models::ConfiguredModel,
            }

            impl $crate::registry::Named for [<Resolved $name_struct>] {
                fn instance_id(&self) -> &str {
                    $crate::registry::Named::instance_id(&self.inner)
                }
            }

            impl $crate::capabilities::CapabilityInfo for [<Resolved $name_struct>] {
                fn name(&self) -> &str {
                    $crate::capabilities::CapabilityInfo::name(&self.inner)
                }
                fn description(&self) -> &str {
                    $crate::capabilities::CapabilityInfo::description(&self.inner)
                }
                fn binding_types(&self) -> std::collections::HashSet<$crate::capabilities::base::BindingType> {
                    $crate::capabilities::CapabilityInfo::binding_types(&self.inner)
                }
            }
        }

        impl $crate::registry::ConfigConstructable for $name_struct {
            type Config = $config_struct;

            fn new(
                instance_id: &str,
                cfg: &serde_json::Value,
            ) -> Result<Self, $crate::registry::ConstructError> {
                let config: $config_struct = serde_json::from_value(cfg.clone())
                    .map_err($crate::registry::ConstructError::settings)?;
                let description = $description_expr;
                let prompt = $prompt_expr;
                let tools = $tools_expr;
                Ok(Self {
                    instance_id: instance_id.to_string(),
                    config,
                    description,
                    prompt,
                    tools,
                })
            }
        }

        impl $crate::registry::Named for $name_struct {
            fn instance_id(&self) -> &str {
                &self.instance_id
            }
        }

        impl $crate::capabilities::CapabilityInfo for $name_struct {
            fn name(&self) -> &str {
                $name_cap
            }

            fn description(&self) -> &str {
                $description_cap
            }

            fn binding_types(&self) -> std::collections::HashSet<$crate::capabilities::base::BindingType> {
                std::collections::HashSet::from([$crate::capabilities::base::BindingType::SubAgent])
            }
        }

        impl $crate::capabilities::Capability for $name_struct {
            fn resolve_refs(
                self: Box<Self>,
                models: &dyn $crate::models::ModelLookup,
            ) -> anyhow::Result<Box<dyn $crate::capabilities::ResolvedCapability>> {
                let configured_model = $crate::capabilities::base::resolve_declared_model(
                    models,
                    &<Self as $crate::capabilities::base::HasCapabilityMetadata>::metadata(),
                    &self.config.model_id,
                )?;
                $crate::paste::paste! {
                    Ok(Box::new([<Resolved $name_struct>] {
                        inner: *self,
                        configured_model,
                    }))
                }
            }
        }

        $crate::paste::paste! {
        #[async_trait::async_trait]
        impl $crate::capabilities::ResolvedCapability for [<Resolved $name_struct>] {
            async fn bind(&self, request: $crate::capabilities::base::BindingRequest) -> anyhow::Result<$crate::capabilities::base::Binding> {
                let api_type = match request {
                    $crate::capabilities::base::BindingRequest::SubAgent($crate::capabilities::base::SubAgentBindingRequest { api_type }) => api_type,
                    other => anyhow::bail!(
                        "{} does not handle {:?} binding requests",
                        stringify!($name_struct),
                        other.binding_type()
                    ),
                };
                let model_id = &self.inner.config.model_id;
                let configured_model = &self.configured_model;
                let (provider, endpoint, model_name) = configured_model.resolve_provider_endpoint(
                    model_id,
                    api_type.clone(),
                    $crate::models::ModelFunction::Chat,
                )?;
                Ok($crate::capabilities::base::Binding::SubAgent($crate::capabilities::base::SubAgentBinding {
                    description: self.inner.description.clone(),
                    prompt: self.inner.prompt.clone(),
                    tools: self.inner.tools.clone(),
                    model: $crate::capabilities::base::AgentModelBinding {
                        api_type,
                        provider_name: provider.instance_id().to_string(),
                        base_url: provider.base_url().to_string(),
                        model_name,
                        endpoint_path: endpoint.path().to_string(),
                        api_key: provider.api_key().cloned(),
                        verify_ssl: provider.verify_ssl(),
                        context_length: Some(configured_model.model.context_length()),
                        custom_headers: provider.custom_headers(),
                    },
                    known_type: $known_type,
                }))
            }
        }
        }

        impl $crate::capabilities::base::HasCapabilityMetadata for $name_struct {
            fn metadata() -> $crate::capabilities::base::CapabilityMetadata {
                $crate::capabilities::base::CapabilityMetadata {
                    name: $name_cap.to_string(),
                    description: $description_cap.to_string(),
                    dependencies: vec![$crate::capabilities::Dependency::Model {
                        config_key: "model_id".to_string(),
                        requirement: $crate::capabilities::ModelRequirement {
                            supported_functions: vec![$crate::models::ModelFunction::Chat, $crate::models::ModelFunction::ToolCalling],
                            ..Default::default()
                        },
                        resolved_id: None,
                        required: true,
                    }],
                    tags: vec![$($tag.to_string()),*],
                    supported_binding_types: std::collections::HashSet::from([$crate::capabilities::base::BindingType::SubAgent]),
                }
            }
        }
    };
}

/*-- Macro: declare_sub_agent_full ------------------------------------------------*/

/// Declares a sub-agent capability with configurable prompt and tools.
#[macro_export]
macro_rules! declare_sub_agent_full {
    (
        $name_struct:ident
        $config_struct:ident
        $name_cap:expr;
        $description_cap:expr;
        [$($tag:expr),* $(,)?]
        $known_type:expr;
        {$($config_fields:tt)*}
    ) => {
        #[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema, Validate)]
        pub struct $config_struct {
            /// Shown to the main agent so it can decide when to delegate to this
            /// sub-agent -- the same role Claude Code's own subagent `description`
            /// field plays.
            #[validate(min_length = 1)]
            pub description: String,
            /// Key into the configured models map (the user-chosen instance ID) for
            /// the model this sub-agent runs on.
            #[validate(min_length = 1)]
            pub model_id: String,
            $($config_fields)*
        }

        pub struct $name_struct {
            instance_id: String,
            config: $config_struct,
            /// Description shown to the parent agent for deciding when to delegate.
            pub description: String,
            /// Configurable prompt for this sub-agent.
            pub prompt: String,
            /// Configurable tool allow-list for this sub-agent.
            pub tools: Vec<$crate::capabilities::base::ToolName>,
        }

        $crate::paste::paste! {
            #[doc = concat!("[`", stringify!($name_struct), "`] with the model its `model_id` names. Built only by `resolve_refs`, so holding one is what says the name resolved and the model meets what this capability's metadata requires of it.")]
            pub struct [<Resolved $name_struct>] {
                inner: $name_struct,
                configured_model: $crate::models::ConfiguredModel,
            }

            impl $crate::registry::Named for [<Resolved $name_struct>] {
                fn instance_id(&self) -> &str {
                    $crate::registry::Named::instance_id(&self.inner)
                }
            }

            impl $crate::capabilities::CapabilityInfo for [<Resolved $name_struct>] {
                fn name(&self) -> &str {
                    $crate::capabilities::CapabilityInfo::name(&self.inner)
                }
                fn description(&self) -> &str {
                    $crate::capabilities::CapabilityInfo::description(&self.inner)
                }
                fn binding_types(&self) -> std::collections::HashSet<$crate::capabilities::base::BindingType> {
                    $crate::capabilities::CapabilityInfo::binding_types(&self.inner)
                }
            }
        }

        impl $crate::registry::ConfigConstructable for $name_struct {
            type Config = $config_struct;

            fn new(
                instance_id: &str,
                cfg: &serde_json::Value,
            ) -> Result<Self, $crate::registry::ConstructError> {
                let config: $config_struct = serde_json::from_value(cfg.clone())
                    .map_err($crate::registry::ConstructError::settings)?;
                let description = config.description.clone();
                let prompt = config.prompt.clone();
                let tools = config.tools.clone();
                Ok(Self {
                    instance_id: instance_id.to_string(),
                    config,
                    description,
                    prompt,
                    tools,
                })
            }
        }

        impl $crate::registry::Named for $name_struct {
            fn instance_id(&self) -> &str {
                &self.instance_id
            }
        }

        impl $crate::capabilities::CapabilityInfo for $name_struct {
            fn name(&self) -> &str {
                $name_cap
            }

            fn description(&self) -> &str {
                $description_cap
            }

            fn binding_types(&self) -> std::collections::HashSet<$crate::capabilities::base::BindingType> {
                std::collections::HashSet::from([$crate::capabilities::base::BindingType::SubAgent])
            }
        }

        impl $crate::capabilities::Capability for $name_struct {
            fn resolve_refs(
                self: Box<Self>,
                models: &dyn $crate::models::ModelLookup,
            ) -> anyhow::Result<Box<dyn $crate::capabilities::ResolvedCapability>> {
                let configured_model = $crate::capabilities::base::resolve_declared_model(
                    models,
                    &<Self as $crate::capabilities::base::HasCapabilityMetadata>::metadata(),
                    &self.config.model_id,
                )?;
                $crate::paste::paste! {
                    Ok(Box::new([<Resolved $name_struct>] {
                        inner: *self,
                        configured_model,
                    }))
                }
            }
        }

        $crate::paste::paste! {
        #[async_trait::async_trait]
        impl $crate::capabilities::ResolvedCapability for [<Resolved $name_struct>] {
            async fn bind(&self, request: $crate::capabilities::base::BindingRequest) -> anyhow::Result<$crate::capabilities::base::Binding> {
                let api_type = match request {
                    $crate::capabilities::base::BindingRequest::SubAgent($crate::capabilities::base::SubAgentBindingRequest { api_type }) => api_type,
                    other => anyhow::bail!(
                        "{} does not handle {:?} binding requests",
                        stringify!($name_struct),
                        other.binding_type()
                    ),
                };
                let model_id = &self.inner.config.model_id;
                let configured_model = &self.configured_model;
                let (provider, endpoint, model_name) = configured_model.resolve_provider_endpoint(
                    model_id,
                    api_type.clone(),
                    $crate::models::ModelFunction::Chat,
                )?;
                Ok($crate::capabilities::base::Binding::SubAgent($crate::capabilities::base::SubAgentBinding {
                    description: self.inner.description.clone(),
                    prompt: self.inner.prompt.clone(),
                    tools: self.inner.tools.clone(),
                    model: $crate::capabilities::base::AgentModelBinding {
                        api_type,
                        provider_name: provider.instance_id().to_string(),
                        base_url: provider.base_url().to_string(),
                        model_name,
                        endpoint_path: endpoint.path().to_string(),
                        api_key: provider.api_key().cloned(),
                        verify_ssl: provider.verify_ssl(),
                        context_length: Some(configured_model.model.context_length()),
                        custom_headers: provider.custom_headers(),
                    },
                    known_type: $known_type,
                }))
            }
        }
        }

        impl $crate::capabilities::base::HasCapabilityMetadata for $name_struct {
            fn metadata() -> $crate::capabilities::base::CapabilityMetadata {
                $crate::capabilities::base::CapabilityMetadata {
                    name: $name_cap.to_string(),
                    description: $description_cap.to_string(),
                    dependencies: vec![$crate::capabilities::Dependency::Model {
                        config_key: "model_id".to_string(),
                        requirement: $crate::capabilities::ModelRequirement {
                            supported_functions: vec![$crate::models::ModelFunction::Chat, $crate::models::ModelFunction::ToolCalling],
                            ..Default::default()
                        },
                        resolved_id: None,
                        required: true,
                    }],
                    tags: vec![$($tag.to_string()),*],
                    supported_binding_types: std::collections::HashSet::from([$crate::capabilities::base::BindingType::SubAgent]),
                }
            }
        }
    };
}

/*-- SubAgentCapability ------------------------------------------------------------*/

// Configuration for the generic sub-agent capability. The prompt and tools
// are configurable via JSON, leaving only description and model_id.
declare_sub_agent_full!(
    SubAgentCapability
    SubAgentCapabilityConfig
    "Sub-Agent";
    "Defines a named sub-agent (prompt, tool allow-list, and model) that a launched coding agent can delegate to.";
    ["agent", "sub-agent"]
    None;
    {
        /// The sub-agent's system prompt.
        #[validate(min_length = 1)]
        pub prompt: String,
        /// Tool allow-list. Empty (the default) means "inherit all tools."
        #[serde(default)]
        pub tools: Vec<crate::capabilities::base::ToolName>,
    }
);

/*-- tests -------------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::base::{
        Binding, BindingRequest, BindingType, Dependency, HasCapabilityMetadata,
        SubAgentBindingRequest, ToolName,
    };
    use crate::capabilities::{CapabilityInfo, ResolvedCapability};
    use crate::config::{Config, ModelConfig, ProviderConfig};
    use crate::models::ModelFunction;
    use crate::providers::{ApiEndpoint, ApiType};
    use crate::registry::ConfigConstructable;
    use crate::utils::test_support::{FakeModel, FakeProvider};

    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::sync::Arc;

    fn ok_provider() -> FakeProvider {
        let mut endpoints = HashMap::new();
        endpoints.insert(
            ModelFunction::Chat,
            vec![ApiEndpoint::OpenAIChat, ApiEndpoint::AnthropicMessages],
        );
        FakeProvider {
            instance_id: "my-ollama".to_string(),
            base_url: "http://localhost:11434".to_string(),
            api_key: None,
            verify_ssl: true,
            api_types: vec![ApiType::OpenAI, ApiType::Anthropic],
            endpoints,
            alias: None,
        }
    }

    fn capability_with_test_model(
        functions: Vec<ModelFunction>,
        provider: FakeProvider,
    ) -> ResolvedSubAgentCapability {
        let mut config = Config::default();
        config.providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({}),
            },
        );
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                config: serde_json::json!({}),
                provider_id: "ollama".to_string(),
                variant: None,
            },
        );
        let cap = SubAgentCapability::new(
            "reviewer",
            &serde_json::json!({
                "description": "Reviews code",
                "prompt": "You are a meticulous code reviewer.",
                "model_id": "granite-3.1-8b-instruct",
            }),
        )
        .unwrap();
        ResolvedSubAgentCapability {
            inner: cap,
            configured_model: crate::models::ConfiguredModel::for_test(
                Arc::new(FakeModel::text(functions)),
                Arc::new(provider),
                None,
            ),
        }
    }

    fn request(api_type: ApiType) -> BindingRequest {
        BindingRequest::SubAgent(SubAgentBindingRequest { api_type })
    }

    #[tokio::test]
    async fn bind_succeeds_and_carries_description_prompt_and_tools() {
        let mut config = Config::default();
        config.providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({}),
            },
        );
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                config: serde_json::json!({}),
                provider_id: "ollama".to_string(),
                variant: None,
            },
        );
        let cap = SubAgentCapability::new(
            "reviewer",
            &serde_json::json!({
                "description": "Reviews code",
                "prompt": "You are a meticulous code reviewer.",
                "tools": ["FileRead", "Search", {"Other": "SomeRawClaudeTool"}],
                "model_id": "granite-3.1-8b-instruct",
            }),
        )
        .unwrap();
        let cap = ResolvedSubAgentCapability {
            inner: cap,
            configured_model: crate::models::ConfiguredModel::for_test(
                Arc::new(FakeModel::text(vec![ModelFunction::Chat])),
                Arc::new(ok_provider()),
                None,
            ),
        };

        let binding = cap.bind(request(ApiType::Anthropic)).await.unwrap();
        let Binding::SubAgent(binding) = binding else {
            panic!("expected SubAgent binding")
        };
        assert_eq!(binding.description, "Reviews code");
        assert_eq!(binding.prompt, "You are a meticulous code reviewer.");
        assert_eq!(
            binding.tools,
            vec![
                ToolName::FileRead,
                ToolName::Search,
                ToolName::Other("SomeRawClaudeTool".to_string()),
            ]
        );
        assert_eq!(binding.model.base_url, "http://localhost:11434");
        assert_eq!(binding.model.model_name, "granite-3.1-8b-instruct");
        assert_eq!(binding.model.api_type, ApiType::Anthropic);
    }

    #[tokio::test]
    async fn bind_fails_when_provider_lacks_requested_api_type() {
        let cap = capability_with_test_model(
            vec![ModelFunction::Chat],
            FakeProvider {
                api_types: vec![ApiType::OpenAI],
                ..ok_provider()
            },
        );
        let err = cap
            .bind(request(ApiType::Anthropic))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not support Anthropic"));
    }

    #[tokio::test]
    async fn bind_rejects_non_sub_agent_requests() {
        let cap = capability_with_test_model(vec![ModelFunction::Chat], ok_provider());
        let err = cap
            .bind(BindingRequest::AgentModel(
                crate::capabilities::base::AgentModelBindingRequest {
                    api_type: ApiType::Anthropic,
                },
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not handle"));
    }

    #[test]
    fn binding_types_reports_sub_agent() {
        let cap = capability_with_test_model(vec![ModelFunction::Chat], ok_provider());
        assert_eq!(cap.binding_types(), HashSet::from([BindingType::SubAgent]));
    }

    #[test]
    fn metadata_declares_a_required_model_dependency() {
        let deps = SubAgentCapability::metadata().dependencies;
        assert_eq!(deps.len(), 1);
        assert!(deps.iter().any(|d| matches!(
            d,
            Dependency::Model { config_key, required: true, .. } if config_key == "model_id"
        )));
    }

    #[test]
    fn metadata_reports_supported_binding_types_and_wildcard_dependency() {
        let meta = SubAgentCapability::metadata();
        assert_eq!(
            meta.supported_binding_types,
            HashSet::from([BindingType::SubAgent])
        );
        assert!(meta.dependencies.iter().any(|d| matches!(
            d,
            Dependency::Model {
                resolved_id: None,
                ..
            }
        )));
    }
}
