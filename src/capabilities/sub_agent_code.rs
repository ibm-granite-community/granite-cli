//! `CodeSubAgentCapability`: defines a named coding sub-agent
use serde::{Deserialize, Serialize};
use serde_valid::Validate;

use crate::capabilities::base::KnownSubAgent;
use crate::capabilities::base::ToolName;
use crate::declare_sub_agent_basic;

const DESCRIPTION: &str = "Use this sub-agent to accomplish a narrowly scoped coding task. The task description must have clear file references, architectural guidance, and outcome descriptions. The task should not include modifying the local development environment.";
const PROMPT: &str = "You are a coding specialist. You excel at performing development tasks precisely. You require fully scoped tasks and execute them efficiently as specified.

=== CRITICAL: LOCAL-ONLY MODE ===
This is a READ/WRITE task for local files only. You are STRICTLY PROHIBITED from:
- Modifying anything outside of the current workspace
- Searching the internet for anything
- Modifying the local environment
- Running ANY commands that change system state outside of the current workspace

Your role is EXCLUSIVELY to write/modify code and run shell commands to verify your progress towards the stated development goals.

Your strengths:
- Implement code changes in the local project
- Run project-appropriate shell tools to validate your changes
- Add/modify tests where necessary to ensure your changes are working correctly and will not regress in the future

Guidelines [file search / glob, search / grep, shell]:
- Use file search tools when you know the specific file path you need to read
- Use shell tools for read-only operations (eg: ls, git status, git log, git diff, find, grep, cat, head, tail, git status, git log, git diff) when exploring the codebase to understand the scope of the changes you need to make
- Use shell tools for write operations (eg: mkdir, touch, rm, cp, mv) only when modifying files in the local workspace
- Use shell tools for build, execution, and testing (eg: python, pytest, uv run, npm run, make, cargo) when validating the state of your development task
- NEVER use shell tools to modify the local development environment (git add, git commit, npm install, pip install, git add, git commit, npm install, pip install)

NOTE: You are meant to be a fast agent that accomplishes your task as quickly as possible. In order to achieve this you must:
- Make efficient use of the tools that you have at your disposal: be smart about how you broadly you search the codebase versus modifying where directed

When complete, report back with a concise description of the changes made and tests run to validate the changes.";

declare_sub_agent_basic!(
    CodeSubAgentCapability
    CodeSubAgentCapabilityConfig
    "Code Sub-Agent";
    "Defines a named coding sub-agent (static prompt, fixed tools, and model) that a launched coding agent can delegate to.";
    ["agent", "code"]
    DESCRIPTION.to_string();
    PROMPT.to_string();
    vec![ToolName::FileRead, ToolName::Search, ToolName::Shell, ToolName::FileWrite, ToolName::FileEdit];
    Some(KnownSubAgent::Code)
);

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::base::{
        Binding, BindingRequest, BindingType, Capability, Dependency, HasCapabilityMetadata,
        SubAgentBindingRequest,
    };
    use crate::config::{Config, ModelConfig};
    use crate::models::{Model, ModelFunction};
    use crate::providers::{
        ApiEndpoint, ApiType, HealthStatus, ModelFormat, Provider, ProviderError,
    };
    use crate::registry::ConfigConstructable;
    use crate::registry::Secret;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct FakeProvider {
        instance_id: String,
        base_url: String,
        api_key: Option<Secret>,
        verify_ssl: bool,
        api_types: Vec<ApiType>,
        endpoints: HashMap<ModelFunction, Vec<ApiEndpoint>>,
        alias: Option<String>,
    }

    impl ConfigConstructable for FakeProvider {
        type Config = crate::registry::NoConfig;
        fn new(_: &str, _: &serde_json::Value, _: &crate::config::Config) -> Self {
            unimplemented!("not used in tests")
        }
    }

    impl crate::registry::Named for FakeProvider {
        fn instance_id(&self) -> &str {
            &self.instance_id
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> &str {
            "Fake Provider"
        }
        fn function_endpoints(&self) -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
            self.endpoints.clone()
        }
        fn supported_api_types(&self) -> Vec<ApiType> {
            self.api_types.clone()
        }
        fn base_url(&self) -> &str {
            &self.base_url
        }
        fn api_key(&self) -> Option<&Secret> {
            self.api_key.as_ref()
        }
        fn verify_ssl(&self) -> bool {
            self.verify_ssl
        }
        fn supported_formats(&self) -> Vec<ModelFormat> {
            vec![]
        }
        fn model_alias(&self, _variant: Option<&crate::models::ModelVariant>) -> Option<String> {
            self.alias.clone()
        }
        async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
            unimplemented!("not used in tests")
        }
    }

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

    struct TestModel {
        supported_functions: Vec<ModelFunction>,
        provider: FakeProvider,
    }

    impl ConfigConstructable for TestModel {
        type Config = crate::registry::NoConfig;
        fn new(_: &str, _: &serde_json::Value, _: &crate::config::Config) -> Self {
            unimplemented!("not used in tests")
        }
    }

    impl crate::registry::Named for TestModel {
        fn instance_id(&self) -> &str {
            "granite-3.1-8b-instruct"
        }
    }

    impl Model for TestModel {
        fn family(&self) -> &str {
            "Test"
        }
        fn version(&self) -> &str {
            "1.0"
        }
        fn size(&self) -> u64 {
            1
        }
        fn context_length(&self) -> u64 {
            4096
        }
        fn model_type(&self) -> &crate::models::ModelType {
            &crate::models::ModelType::Text
        }
        fn huggingface_repo(&self) -> &str {
            "test/test"
        }
        fn native_dtype(&self) -> &str {
            "bfloat16"
        }
        fn architecture(&self) -> &crate::models::ModelArchitecture {
            unimplemented!("not used in tests")
        }
        fn variants(&self) -> &[crate::models::ModelVariant] {
            &[]
        }
        fn description(&self) -> Option<&str> {
            None
        }
        fn tags(&self) -> &[String] {
            &[]
        }
        fn supported_functions(&self) -> &[ModelFunction] {
            &self.supported_functions
        }
        fn provider(&self) -> anyhow::Result<Box<dyn Provider>> {
            Ok(Box::new(self.provider.clone()))
        }
    }

    fn code_capability_with_test_model(
        functions: Vec<ModelFunction>,
        provider: FakeProvider,
    ) -> CodeSubAgentCapability {
        let mut config = Config::default();
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                config: serde_json::json!({}),
                provider_id: None,
                variant: None,
            },
        );
        let cap = CodeSubAgentCapability::new(
            "coder",
            &serde_json::json!({
                "model_id": "granite-3.1-8b-instruct",
            }),
            &config,
        );
        CodeSubAgentCapability {
            instance_id: cap.instance_id,
            config: cap.config,
            configured_model: crate::models::ConfiguredModel::for_test(
                Arc::new(TestModel {
                    supported_functions: functions,
                    provider,
                }),
                None,
            ),
            description: cap.description,
            prompt: cap.prompt,
            tools: cap.tools,
        }
    }

    fn request(api_type: ApiType) -> BindingRequest {
        BindingRequest::SubAgent(SubAgentBindingRequest { api_type })
    }

    #[tokio::test]
    async fn bind_succeeds_and_carries_description_prompt_and_tools() {
        let mut config = Config::default();
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                config: serde_json::json!({}),
                provider_id: None,
                variant: None,
            },
        );
        let cap = CodeSubAgentCapability::new(
            "coder",
            &serde_json::json!({
                "model_id": "granite-3.1-8b-instruct",
            }),
            &config,
        );
        let cap = CodeSubAgentCapability {
            instance_id: cap.instance_id,
            config: cap.config,
            configured_model: crate::models::ConfiguredModel::for_test(
                Arc::new(TestModel {
                    supported_functions: vec![ModelFunction::Chat],
                    provider: ok_provider(),
                }),
                None,
            ),
            description: cap.description,
            prompt: cap.prompt,
            tools: cap.tools,
        };

        let binding = cap.bind(request(ApiType::Anthropic)).await.unwrap();
        let Binding::SubAgent(binding) = binding else {
            panic!("expected SubAgent binding")
        };
        assert_eq!(binding.description, DESCRIPTION);
        assert_eq!(binding.prompt, PROMPT.to_string());
        assert_eq!(
            binding.tools,
            vec![
                ToolName::FileRead,
                ToolName::Search,
                ToolName::Shell,
                ToolName::FileWrite,
                ToolName::FileEdit,
            ]
        );
        assert_eq!(binding.model.base_url, "http://localhost:11434");
        assert_eq!(binding.model.model_name, "granite-3.1-8b-instruct");
        assert_eq!(binding.model.api_type, ApiType::Anthropic);
    }

    #[test]
    fn binding_types_reports_sub_agent() {
        let cap = code_capability_with_test_model(vec![ModelFunction::Chat], ok_provider());
        assert_eq!(cap.binding_types(), HashSet::from([BindingType::SubAgent]));
    }

    #[test]
    fn dependencies_carry_resolved_model_id() {
        let cap = code_capability_with_test_model(vec![ModelFunction::Chat], ok_provider());
        let deps = cap.dependencies();
        assert_eq!(deps.len(), 1);
        assert!(deps.iter().any(|d| matches!(
            d,
            Dependency::Model { resolved_id: Some(id), .. } if id == "granite-3.1-8b-instruct"
        )));
    }

    #[test]
    fn metadata_reports_supported_binding_types_and_wildcard_dependency() {
        let meta = CodeSubAgentCapability::metadata();
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
