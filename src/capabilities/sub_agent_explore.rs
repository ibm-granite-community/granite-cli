//! `ExploreSubAgentCapability`: defines a named exploration sub-agent with a static
//! prompt and fixed tool allow-list (FileRead, Search, Shell), and a `Model`/`Provider`
//! of its own. The prompt has a placeholder that the user can fill in later.

use serde::{Deserialize, Serialize};
use serde_valid::Validate;

use crate::capabilities::base::KnownSubAgent;
use crate::capabilities::base::ToolName;
use crate::declare_sub_agent_basic;

const EXPLORE_DESCRIPTION: &str = "Thoroughly search and explore the codebase to find files, read implementations, and understand patterns. Use when the user needs to find specific files, understand code structure, or locate implementations.";
// CITE: https://github.com/Piebald-AI/claude-code-system-prompts/blob/main/system-prompts/agent-prompt-explore.md
const EXPLORE_PROMPT: &str = "You are a file search specialist. You excel at thoroughly navigating and exploring codebases.

=== CRITICAL: READ-ONLY MODE - NO FILE MODIFICATIONS ===
This is a READ-ONLY exploration task. You are STRICTLY PROHIBITED from:
- Creating new files (no Write, touch, or file creation of any kind)
- Modifying existing files (no Edit operations)
- Deleting files (no rm or deletion)
- Moving or copying files (no mv or cp)
- Creating temporary files anywhere, including /tmp
- Using redirect operators (>, >>, |) or heredocs to write to files
- Running ANY commands that change system state

Your role is EXCLUSIVELY to search and analyze existing code. You do NOT have access to file editing tools - attempting to edit files will fail.

Your strengths:
- Rapidly finding files using glob patterns
- Searching code and text with powerful regex patterns
- Reading and analyzing file contents

Guidelines [file search / glob, search / grep]:
- Use file search tools when you know the specific file path you need to read
- Use shell tools ONLY for read-only operations (ls, git status, git log, git diff, find, grep, cat, head, tail, git status, git log, git diff)
- NEVER use shell tools for: mkdir, touch, rm, cp, mv, git add, git commit, npm install, pip install, git add, git commit, npm install, pip install, or any file creation/modification
- Adapt your search approach based on the thoroughness level specified by the caller
- Communicate your final report directly as a regular message - do NOT attempt to create files

NOTE: You are meant to be a fast agent that returns output as quickly as possible. In order to achieve this you must:
- Make efficient use of the tools that you have at your disposal: be smart about how you search for files and implementations
- Wherever possible you should try to spawn multiple parallel tool calls for grepping and reading files

Complete the user's search request efficiently and report your findings clearly.";

declare_sub_agent_basic!(
    ExploreSubAgentCapability
    ExploreSubAgentCapabilityConfig
    "Explore Sub-Agent";
    "Defines a named exploration sub-agent (static prompt, fixed tools, and model) that a launched coding agent can delegate to.";
    ["agent", "explore"]
    EXPLORE_DESCRIPTION.to_string();
    EXPLORE_PROMPT.to_string();
    vec![ToolName::FileRead, ToolName::Search, ToolName::Shell];
    Some(KnownSubAgent::Explore)
);

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::base::{
        Binding, BindingRequest, BindingType, Dependency, HasCapabilityMetadata,
        SubAgentBindingRequest,
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

    fn explore_capability_with_test_model(
        functions: Vec<ModelFunction>,
        provider: FakeProvider,
    ) -> ResolvedExploreSubAgentCapability {
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
        let cap = ExploreSubAgentCapability::new(
            "explorer",
            &serde_json::json!({
                "model_id": "granite-3.1-8b-instruct",
            }),
        )
        .unwrap();
        ResolvedExploreSubAgentCapability {
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
        let cap = ExploreSubAgentCapability::new(
            "explorer",
            &serde_json::json!({
                "model_id": "granite-3.1-8b-instruct",
            }),
        )
        .unwrap();
        let cap = ResolvedExploreSubAgentCapability {
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
        assert_eq!(binding.description, EXPLORE_DESCRIPTION);
        assert_eq!(binding.prompt, EXPLORE_PROMPT.to_string());
        assert_eq!(
            binding.tools,
            vec![ToolName::FileRead, ToolName::Search, ToolName::Shell,]
        );
        assert_eq!(binding.model.base_url, "http://localhost:11434");
        assert_eq!(binding.model.model_name, "granite-3.1-8b-instruct");
        assert_eq!(binding.model.api_type, ApiType::Anthropic);
    }

    #[test]
    fn binding_types_reports_sub_agent() {
        let cap = explore_capability_with_test_model(vec![ModelFunction::Chat], ok_provider());
        assert_eq!(cap.binding_types(), HashSet::from([BindingType::SubAgent]));
    }

    #[test]
    fn metadata_declares_a_required_model_dependency() {
        let deps = ExploreSubAgentCapability::metadata().dependencies;
        assert_eq!(deps.len(), 1);
        assert!(deps.iter().any(|d| matches!(
            d,
            Dependency::Model { config_key, required: true, .. } if config_key == "model_id"
        )));
    }

    #[test]
    fn metadata_reports_supported_binding_types_and_wildcard_dependency() {
        let meta = ExploreSubAgentCapability::metadata();
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
