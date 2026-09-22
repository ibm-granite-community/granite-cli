//! In-process MCP server exposing bound `SubAgentBinding`s as callable tools,
//! backed by `pi` (via `PiLauncher::run_delegated_task`) -- one delegate `pi`
//! subprocess invocation per tool call. Lets `BobLauncher` hand Bob a set of
//! sub-agents without Bob needing to know anything about `pi` itself: Bob
//! just sees an MCP server with one tool per sub-agent.
//!
//! Mirrors `crate::capabilities::vision_mcp`'s in-process Streamable HTTP
//! server template, but is owned by the launcher (`bob.rs`) rather than by a
//! `Capability`, since the tools here wrap other launcher instances
//! (`PiLauncher`) rather than a model backend directly.

// Standard
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

// Third Party
use rmcp::model::{
    CallToolRequestMethod, CallToolRequestParams, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{MaybeSendFuture, RequestContext};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};

// Local
use crate::capabilities::{
    Binding, BindingRequest, BindingType, ResolvedCapability, SubAgentBinding,
};
use crate::launchers::base::{LaunchContext, Launcher};
use crate::launchers::pi::PiLauncher;
use crate::registry::ConfigConstructable;
use crate::utils::subserver::SubServer;
use crate::utils::ui::Ui;

/*-- private --*/

/// Builds a `LaunchContext` copying `working_dir`/`base_env`/`dry_run` from
/// `ctx`, but with its own `launcher_id` -- see module docs on why each
/// delegate sub-agent's backing `PiLauncher` needs a distinct one (its Pi
/// config cache dir is derived from it, and sharing one across concurrently
/// callable sub-agents would race on `models.json`).
fn derived_ctx(ctx: &LaunchContext, launcher_id: String) -> LaunchContext {
    LaunchContext {
        launcher_id,
        working_dir: ctx.working_dir.clone(),
        base_env: ctx.base_env.clone(),
        dry_run: ctx.dry_run,
        usage_tracker: ctx.usage_tracker.clone(),
        model_proxy: ctx.model_proxy.clone(),
    }
}

/// One bound sub-agent, ready to be called as an MCP tool: a `PiLauncher`
/// pre-bound to the sub-agent's model, plus everything `run_delegated_task`
/// needs to invoke it.
struct DelegateSubAgent {
    tool_name: String,
    description: String,
    system_prompt: String,
    tools: Vec<crate::capabilities::ToolName>,
    pi: PiLauncher,
    ctx: LaunchContext,
}

/// Tiny internal `ResolvedCapability` impl that hands a pre-resolved
/// `Binding` back unconditionally -- lets a `SubAgentBinding`'s
/// already-resolved `AgentModelBinding` be fed into
/// `PiLauncher::bind_capability` without duplicating that launcher's own
/// capability-resolution logic. It names nothing, so it is built resolved and
/// never implements `Capability`.
struct StaticCapabilityBinding {
    instance_id: String,
    binding: Binding,
}

impl crate::registry::Named for StaticCapabilityBinding {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

impl crate::capabilities::CapabilityInfo for StaticCapabilityBinding {
    fn name(&self) -> &str {
        "Static Binding"
    }

    fn description(&self) -> &str {
        "internal: hands a pre-resolved binding to a backing launcher"
    }

    fn binding_types(&self) -> std::collections::HashSet<BindingType> {
        std::collections::HashSet::from([self.binding.binding_type()])
    }
}

#[async_trait::async_trait]
impl ResolvedCapability for StaticCapabilityBinding {
    async fn bind(&self, _request: BindingRequest) -> anyhow::Result<Binding> {
        Ok(self.binding.clone())
    }
}

/// Builds one `DelegateSubAgent`: constructs a fresh `PiLauncher` scoped to
/// this sub-agent (`bob-delegate-{tool_name}`), binds it to the sub-agent's
/// resolved model via `StaticCapabilityBinding`, and derives this sub-agent's
/// own `LaunchContext` from the outer one.
async fn build_delegate_sub_agent(
    tool_name: String,
    binding: SubAgentBinding,
    outer_ctx: &LaunchContext,
) -> anyhow::Result<DelegateSubAgent> {
    let launcher_id = format!("bob-delegate-{tool_name}");
    // An empty settings blob is `PiLauncherConfig`'s default shape, so this
    // cannot report unreadable settings; it is mapped rather than unwrapped
    // so a later field with no default surfaces here instead of panicking.
    let mut pi = PiLauncher::new(&launcher_id, &serde_json::json!({}))
        .map_err(|e| anyhow::anyhow!("could not build the delegate launcher: {e}"))?;
    let wrapper = StaticCapabilityBinding {
        instance_id: tool_name.clone(),
        binding: Binding::AgentModel(binding.model.clone()),
    };
    pi.bind_capability(&wrapper).await?;
    Ok(DelegateSubAgent {
        ctx: derived_ctx(outer_ctx, launcher_id),
        tool_name,
        description: binding.description,
        system_prompt: binding.prompt,
        tools: binding.tools,
        pi,
    })
}

/// The JSON Schema every delegate tool advertises: a single required `task`
/// string, the free-text instruction handed to the sub-agent.
fn delegate_tool_input_schema() -> serde_json::Map<String, serde_json::Value> {
    serde_json::json!({
        "type": "object",
        "properties": {
            "task": {
                "type": "string",
                "description": "The task to delegate to this sub-agent.",
            },
        },
        "required": ["task"],
    })
    .as_object()
    .expect("object literal")
    .clone()
}

/// Handles MCP `tools/list`/`tools/call` for the bound delegate sub-agents.
/// `Clone` is cheap (two `Arc`s), which is what lets `call_tool` sidestep the
/// trait method's `&self` / `+ '_` lifetime by cloning `self` into an owned
/// `async move` block.
#[derive(Clone)]
pub(crate) struct DelegateToolRegistry {
    pi_binary: Arc<PathBuf>,
    agents: Arc<Vec<DelegateSubAgent>>,
}

impl DelegateToolRegistry {
    async fn call_tool_impl(
        &self,
        request: CallToolRequestParams,
    ) -> Result<CallToolResult, ErrorData> {
        let Some(agent) = self.agents.iter().find(|a| a.tool_name == request.name) else {
            return Err(ErrorData::method_not_found::<CallToolRequestMethod>());
        };

        let task = request
            .arguments
            .as_ref()
            .and_then(|args| args.get("task"))
            .and_then(|v| v.as_str());
        let Some(task) = task else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "missing required string argument 'task'",
            )]));
        };

        let plain_ui = crate::utils::ui::backends::plain::PlainOutput;
        match agent
            .pi
            .run_delegated_task(
                &self.pi_binary,
                task,
                &agent.system_prompt,
                &agent.tools,
                &agent.ctx,
                &plain_ui,
            )
            .await
        {
            Ok(output) => Ok(CallToolResult::success(vec![ContentBlock::text(output)])),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                e.to_string(),
            )])),
        }
    }
}

impl ServerHandler for DelegateToolRegistry {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self
            .agents
            .iter()
            .map(|agent| {
                Tool::new(
                    agent.tool_name.clone(),
                    agent.description.clone(),
                    delegate_tool_input_schema(),
                )
            })
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<rmcp::model::CallToolResponse, ErrorData>> + MaybeSendFuture + '_
    {
        let self_clone = self.clone();
        async move {
            self_clone
                .call_tool_impl(request)
                .await
                .map(rmcp::model::CallToolResponse::from)
        }
    }
}

/*-- public (crate) --*/

/// Starts the in-process delegate MCP server: resolves `pi`, builds one
/// `DelegateSubAgent` per bound sub-agent, and serves them all over one
/// Streamable HTTP endpoint.
///
/// `ui` here is the *real* UI from the caller's `launch()` -- used only for
/// `ensure_pi_binary`'s download progress, which is worth surfacing since it
/// runs once, synchronously, before anything is served. Once the server is
/// up, individual tool calls each build their own `PlainOutput` rather than
/// carrying this reference forward (see module docs on why).
pub(crate) async fn start_delegate_mcp_server(
    sub_agents: Vec<(String, SubAgentBinding)>,
    command_path: &Option<String>,
    ctx: &LaunchContext,
    ui: &dyn Ui,
) -> anyhow::Result<(crate::capabilities::McpBinding, SubServer)> {
    let cache_dir = crate::config::Config::launcher_state_dir("bob-pi-delegate")?;
    let binary = crate::launchers::pi::ensure_pi_binary(command_path, &cache_dir, ui).await?;

    let mut agents = Vec::with_capacity(sub_agents.len());
    for (tool_name, binding) in sub_agents {
        agents.push(build_delegate_sub_agent(tool_name, binding, ctx).await?);
    }

    let registry = DelegateToolRegistry {
        pi_binary: Arc::new(binary),
        agents: Arc::new(agents),
    };
    let service = StreamableHttpService::new(
        move || Ok(registry.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new().route_service("/mcp", service);
    let server = SubServer::spawn(router, "bob-sub-agent-delegate")?;
    let url = format!("http://{}/mcp", server.local_addr);
    Ok((
        crate::capabilities::McpBinding::Http {
            url,
            headers: HashMap::new(),
            timeout: Some(7_200_000), // 2 hours in milliseconds
        },
        server,
    ))
}

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{AgentModelBinding, ApiType};
    use crate::utils::ui::backends::plain::PlainOutput;

    fn agent_model_binding() -> AgentModelBinding {
        AgentModelBinding {
            api_type: ApiType::OpenAI,
            provider_name: "my-ollama".to_string(),
            base_url: "http://localhost:11434".to_string(),
            model_name: "granite4.1:8b".to_string(),
            endpoint_path: "/v1/chat/completions".to_string(),
            api_key: None,
            verify_ssl: true,
            context_length: Some(131072),
            custom_headers: None,
        }
    }

    fn sub_agent_binding(description: &str) -> SubAgentBinding {
        SubAgentBinding {
            description: description.to_string(),
            prompt: "You are a helpful sub-agent.".to_string(),
            tools: vec![],
            model: agent_model_binding(),
            known_type: None,
        }
    }

    fn dry_run_ctx() -> LaunchContext {
        LaunchContext {
            launcher_id: "test-bob".to_string(),
            working_dir: std::env::temp_dir(),
            base_env: HashMap::new(),
            dry_run: true,
            usage_tracker: None,
            model_proxy: None,
        }
    }

    async fn start_test_server(
        sub_agents: Vec<(String, SubAgentBinding)>,
    ) -> (crate::capabilities::McpBinding, SubServer) {
        let ctx = dry_run_ctx();
        let ui = PlainOutput;
        start_delegate_mcp_server(sub_agents, &Some("ls".to_string()), &ctx, &ui)
            .await
            .unwrap()
    }

    fn mcp_url(binding: &crate::capabilities::McpBinding) -> String {
        match binding {
            crate::capabilities::McpBinding::Http { url, .. } => url.clone(),
            other => panic!("expected an Http McpBinding, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn initialize_handshake_round_trips_over_http() {
        let (binding, server) = start_test_server(vec![(
            "explore".to_string(),
            sub_agent_binding("explore stuff"),
        )])
        .await;
        let url = mcp_url(&binding);

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "clientInfo": {"name": "smoke-test", "version": "0.0.0"},
                    },
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "status: {}", resp.status());
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("protocolVersion"),
            "expected an initialize result, got: {body}"
        );

        server.shutdown().await;
    }

    /// Performs `initialize` (required before any other request over
    /// Streamable HTTP) and returns the session id header for follow-up
    /// requests.
    async fn initialize(client: &reqwest::Client, url: &str) -> String {
        let resp = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "clientInfo": {"name": "smoke-test", "version": "0.0.0"},
                    },
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        resp.headers()
            .get("mcp-session-id")
            .expect("server should assign a session id")
            .to_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn list_tools_reports_one_tool_per_sub_agent() {
        let (binding, server) = start_test_server(vec![
            ("explore".to_string(), sub_agent_binding("explore the repo")),
            ("plan".to_string(), sub_agent_binding("plan the work")),
        ])
        .await;
        let url = mcp_url(&binding);

        let client = reqwest::Client::new();
        let session_id = initialize(&client, &url).await;

        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Mcp-Session-Id", &session_id)
            .body(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/list",
                    "params": {},
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "status: {}", resp.status());
        let body = resp.text().await.unwrap();

        assert!(body.contains("\"explore\""), "body: {body}");
        assert!(body.contains("explore the repo"), "body: {body}");
        assert!(body.contains("\"plan\""), "body: {body}");
        assert!(body.contains("plan the work"), "body: {body}");
        assert!(body.contains("\"task\""), "body: {body}");

        server.shutdown().await;
    }

    #[tokio::test]
    async fn call_tool_with_unknown_name_is_a_protocol_error() {
        let (binding, server) = start_test_server(vec![(
            "explore".to_string(),
            sub_agent_binding("explore stuff"),
        )])
        .await;
        let url = mcp_url(&binding);

        let client = reqwest::Client::new();
        let session_id = initialize(&client, &url).await;

        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Mcp-Session-Id", &session_id)
            .body(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": {"name": "does-not-exist", "arguments": {"task": "x"}},
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("\"error\""),
            "expected a JSON-RPC error, got: {body}"
        );
        assert!(
            !body.contains("\"result\""),
            "expected no result, got: {body}"
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn call_tool_with_known_name_and_dry_run_succeeds_with_empty_output() {
        let (binding, server) = start_test_server(vec![(
            "explore".to_string(),
            sub_agent_binding("explore stuff"),
        )])
        .await;
        let url = mcp_url(&binding);

        let client = reqwest::Client::new();
        let session_id = initialize(&client, &url).await;

        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Mcp-Session-Id", &session_id)
            .body(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "tools/call",
                    "params": {"name": "explore", "arguments": {"task": "look around"}},
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "status: {}", resp.status());
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("\"result\""),
            "expected a result, got: {body}"
        );
        assert!(!body.contains("\"isError\":true"), "body: {body}");

        server.shutdown().await;
    }

    #[tokio::test]
    async fn call_tool_missing_task_argument_is_a_tool_level_error() {
        let (binding, server) = start_test_server(vec![(
            "explore".to_string(),
            sub_agent_binding("explore stuff"),
        )])
        .await;
        let url = mcp_url(&binding);

        let client = reqwest::Client::new();
        let session_id = initialize(&client, &url).await;

        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Mcp-Session-Id", &session_id)
            .body(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "tools/call",
                    "params": {"name": "explore", "arguments": {}},
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("\"result\""),
            "expected a result, got: {body}"
        );
        assert!(body.contains("\"isError\":true"), "body: {body}");
        assert!(
            body.contains("missing required string argument"),
            "body: {body}"
        );

        server.shutdown().await;
    }
}
