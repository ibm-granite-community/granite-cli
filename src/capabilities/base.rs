use crate::capabilities::requirement::{
    ModelRequirement, ProviderRequirement, ShellCommandRequirement,
};
use crate::providers::ApiType;
use crate::registry::{ConfigConstructable, Secret};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// Canonical launch-time types live in `launchers::base` -- re-exported here so
// capabilities and launchers share one `LaunchContext`/`EnvBinding` pair.
pub use crate::launchers::{EnvBinding, LaunchContext};

/*-- BindingType / BindingRequest / Binding -----------------------------------*/

/// Declares one binding surface a `Capability` can fill, together with the
/// request payload it takes and the result payload it produces. Expands into
/// matching variants of `BindingType` (payload-free, hashable), `BindingRequest`,
/// and `Binding` -- one macro invocation site, so a new binding surface can't
/// be added to one enum without the matching variant in the other two.
macro_rules! define_bindings {
    ($(
        $variant:ident {
            request: $request_ty:ty,
            result: $result_ty:ty,
            display: $display:literal,
        }
    ),+ $(,)?) => {
        /// Which binding surface a `Capability` can fill. Payload-free and
        /// hashable so a `Launcher` can declare `HashSet<BindingType>` for
        /// the surfaces it knows how to consume.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum BindingType {
            $($variant),+
        }

        impl std::fmt::Display for BindingType {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    $(BindingType::$variant => write!(f, $display),)+
                }
            }
        }

        /// A request for a capability to produce a `Binding` for a specific
        /// binding surface, parameterized by whatever detail that surface
        /// needs (e.g. which `ApiType` the launcher's environment expects).
        #[derive(Debug, Clone)]
        pub enum BindingRequest {
            $($variant($request_ty)),+
        }

        impl BindingRequest {
            pub fn binding_type(&self) -> BindingType {
                match self {
                    $(BindingRequest::$variant(_) => BindingType::$variant,)+
                }
            }
        }

        /// The result of a successful `Capability::bind` call.
        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub enum Binding {
            $($variant($result_ty)),+
        }

        impl Binding {
            pub fn binding_type(&self) -> BindingType {
                match self {
                    $(Binding::$variant(_) => BindingType::$variant,)+
                }
            }
        }
    };
}

/// Request payload for `BindingType::AgentModel` -- which `ApiType` the
/// launcher's environment expects.
#[derive(Debug, Clone)]
pub struct AgentModelBindingRequest {
    pub api_type: ApiType,
}

/// Result payload for `BindingType::AgentModel`: a configured model's
/// connection details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentModelBinding {
    pub api_type: ApiType,
    /// Configured name of the provider instance serving this model (e.g.
    /// `my-ollama`). Launchers that must name the endpoint in the wrapped tool's
    /// own config use this rather than inventing a name of their own.
    pub provider_name: String,
    pub base_url: String,
    pub model_name: String,
    pub endpoint_path: String,
    pub api_key: Option<Secret>,
    pub verify_ssl: bool,
    pub context_length: Option<u64>,
}

/// Request payload for `BindingType::SubAgent` -- which `ApiType` the
/// launcher wants the sub-agent's model to speak (mirrors
/// `AgentModelBindingRequest`).
#[derive(Debug, Clone)]
pub struct SubAgentBindingRequest {
    pub api_type: ApiType,
}

/// Launcher-agnostic tool-name concept for `SubAgentBinding.tools` /
/// `SubAgentCapabilityConfig.tools`. Each `Launcher` maps these to its own
/// native tool-name strings via `Launcher::map_tool_name`; `Other` is the
/// escape hatch for anything not covered by the canonical set, passed
/// through verbatim by every launcher's default mapping.
///
/// Deliberately a small starter set -- covers what a generic coding
/// sub-agent plausibly needs, expected to grow incrementally as pre-baked
/// sub-agent capabilities (e.g. "Explore," "Research") are added rather
/// than trying to be exhaustive now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum ToolName {
    FileRead,
    FileWrite,
    FileEdit,
    /// Content search (e.g. grep-style).
    Search,
    /// Filename/path search (e.g. glob-style).
    FileSearch,
    /// Execute a shell command.
    Shell,
    WebFetch,
    WebSearch,
    /// An MCP-provided tool. `server` is the MCP server's configured name
    /// (for a granite-cli-bound MCP capability, that's the capability's own
    /// `instance_id` -- see `ClaudeLauncher::bound_mcp_bindings`). `tool` is
    /// `None` for "every tool that server exposes," `Some(name)` for one
    /// specific tool.
    Mcp {
        server: String,
        tool: Option<String>,
    },
    /// Escape hatch: an exact, launcher-native tool-name string.
    Other(String),
}

/// Result payload for `BindingType::SubAgent`: a named sub-agent's prompt,
/// tool allow-list, and the connection details for the model it should run
/// on. Reuses `AgentModelBinding` by composition for the connection details
/// rather than duplicating those fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentBinding {
    pub description: String,
    pub prompt: String,
    /// Tool allow-list. Empty means "inherit all tools" -- callers should
    /// omit rather than send an empty list where the downstream tool
    /// distinguishes the two.
    pub tools: Vec<ToolName>,
    pub model: AgentModelBinding,
}

/// Which wire transport an MCP server binding uses. Payload-free and
/// hashable so a `Launcher` can declare which transports it can register
/// (per `McpBindingRequest::supported_transports`) and a `Capability` can
/// pick the best one it's able to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum McpTransportKind {
    Stdio,
    Http,
    Sse,
}

/// Request payload for `BindingType::Mcp` -- which transports the launcher
/// can actually register with the downstream tool. `bind()` picks the best
/// transport it can serve from this set.
#[derive(Debug, Clone)]
pub struct McpBindingRequest {
    pub supported_transports: HashSet<McpTransportKind>,
}

/// Result payload for `BindingType::Mcp`: enough detail for a launcher to
/// register the server with its downstream tool. Mirrors the de-facto MCP
/// server config shape shared by Claude Code, VS Code, and others
/// (see modelcontextprotocol/modelcontextprotocol#292), so a launcher can
/// serialize this almost directly into its own `mcp add-json`/config-file
/// format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum McpBinding {
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    Http {
        url: String,
        headers: HashMap<String, String>,
    },
    Sse {
        url: String,
        headers: HashMap<String, String>,
    },
}

define_bindings! {
    AgentModel {
        request: AgentModelBindingRequest,
        result: AgentModelBinding,
        display: "Agent Model",
    },
    Mcp {
        request: McpBindingRequest,
        result: McpBinding,
        display: "MCP Server",
    },
    SubAgent {
        request: SubAgentBindingRequest,
        result: SubAgentBinding,
        display: "Sub-Agent",
    },
}

impl McpBinding {
    /// The de-facto MCP server config shape shared by Claude Code, bob, and
    /// others' `mcp add-json` (see
    /// modelcontextprotocol/modelcontextprotocol#292): `{"type":
    /// "stdio"|"http"|"sse", ...}`.
    pub fn to_canonical_json(&self) -> serde_json::Value {
        match self {
            McpBinding::Stdio { command, args, env } => serde_json::json!({
                "type": "stdio",
                "command": command,
                "args": args,
                "env": env,
            }),
            McpBinding::Http { url, headers } => serde_json::json!({
                "type": "http",
                "url": url,
                "headers": headers,
            }),
            McpBinding::Sse { url, headers } => serde_json::json!({
                "type": "sse",
                "url": url,
                "headers": headers,
            }),
        }
    }
}

/*-- Capability Trait ----------------------------------------------------------*/

/// Core trait for capability implementations.
/// All capabilities must implement this trait along with ConfigConstructable.
#[async_trait]
pub trait Capability: crate::registry::Named + Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn dependencies(&self) -> Vec<Dependency>;

    /// Which binding surfaces this capability instance can fill.
    fn binding_types(&self) -> HashSet<BindingType>;

    /// Resolve a `BindingRequest` into a concrete `Binding`.
    async fn bind(&self, request: BindingRequest) -> anyhow::Result<Binding>;

    // Execution hooks (all optional with NoOp defaults)
    async fn on_setup(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn on_pre_launch(&self, _context: &LaunchContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn on_post_launch(&self, _context: &LaunchContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn on_shutdown(&self, _context: &LaunchContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn runtime_bindings(&self) -> Vec<EnvBinding> {
        vec![]
    }
}

/*-- Metadata Types ----------------------------------------------------------*/

/// Metadata describing a capability implementation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityMetadata {
    pub name: String,
    pub description: String,
    pub dependencies: Vec<Dependency>,
    pub tags: Vec<String>,
    /// Binding surfaces this capability *type* can support (superset); a
    /// concrete instance may choose to support only a subset via
    /// `Capability::binding_types`.
    pub supported_binding_types: HashSet<BindingType>,
}

impl std::fmt::Display for CapabilityMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.description)
    }
}

/*-- Supporting Types --------------------------------------------------------*/

/// A capability's declared dependency on a model, provider, or external shell
/// command. `resolved_id` is `None` at the type level (catalog display,
/// before any instance is configured) and `Some(id)` once a concrete
/// instance has picked a specific dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Dependency {
    Model {
        /// The JSON key in this capability's own config that the resolved
        /// model id is stored under.
        config_key: String,
        requirement: ModelRequirement,
        resolved_id: Option<String>,
        required: bool,
    },
    Provider {
        /// The JSON key in this capability's own config that the resolved
        /// provider id is stored under.
        config_key: String,
        requirement: ProviderRequirement,
        resolved_id: Option<String>,
        required: bool,
    },
    ExternalTool {
        requirement: ShellCommandRequirement,
        required: bool,
    },
}

impl std::fmt::Display for Dependency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Dependency::Model {
                resolved_id,
                required,
                ..
            } => {
                write!(
                    f,
                    "Model: {}{}",
                    resolved_id.as_deref().unwrap_or("<unresolved>"),
                    if *required { " (required)" } else { "" }
                )
            }
            Dependency::Provider {
                resolved_id,
                required,
                ..
            } => {
                write!(
                    f,
                    "Provider: {}{}",
                    resolved_id.as_deref().unwrap_or("<unresolved>"),
                    if *required { " (required)" } else { "" }
                )
            }
            Dependency::ExternalTool {
                requirement,
                required,
            } => {
                write!(
                    f,
                    "ExternalTool: {}{}",
                    requirement.command,
                    if *required { " (required)" } else { "" }
                )
            }
        }
    }
}

/*-- Factory Definition ------------------------------------------------------*/

use crate::define_factory;

define_factory!(Capability, CapabilityMetadata, CapabilityFactory);

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod mcp_binding_tests {
    use super::*;

    fn stdio_binding() -> McpBinding {
        McpBinding::Stdio {
            command: "/usr/local/bin/granite-cli".to_string(),
            args: vec!["__mcp-serve".to_string(), "vision".to_string()],
            env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
        }
    }

    fn http_binding() -> McpBinding {
        McpBinding::Http {
            url: "http://127.0.0.1:54321/mcp".to_string(),
            headers: HashMap::from([("X-Test".to_string(), "1".to_string())]),
        }
    }

    #[test]
    fn canonical_json_stdio_matches_mcp_add_json_shape() {
        let json = stdio_binding().to_canonical_json();
        assert_eq!(json["type"], "stdio");
        assert_eq!(json["command"], "/usr/local/bin/granite-cli");
        assert_eq!(json["args"][0], "__mcp-serve");
        assert_eq!(json["args"][1], "vision");
        assert_eq!(json["env"]["FOO"], "bar");
    }

    #[test]
    fn canonical_json_http_matches_mcp_add_json_shape() {
        let json = http_binding().to_canonical_json();
        assert_eq!(json["type"], "http");
        assert_eq!(json["url"], "http://127.0.0.1:54321/mcp");
        assert_eq!(json["headers"]["X-Test"], "1");
    }

    #[test]
    fn canonical_json_sse_uses_sse_type() {
        let json = McpBinding::Sse {
            url: "http://127.0.0.1:1/sse".to_string(),
            headers: HashMap::new(),
        }
        .to_canonical_json();
        assert_eq!(json["type"], "sse");
    }
}
