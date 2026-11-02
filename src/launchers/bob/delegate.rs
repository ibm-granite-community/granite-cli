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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

// Third Party
use rmcp::model::{
    CacheScope, CallToolRequestMethod, CallToolRequestParams, CallToolResult, ContentBlock,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerConfig,
    Tool,
};
use rmcp::service::{MaybeSendFuture, RequestContext};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use tokio::sync::{oneshot, watch};

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

/// How long one `tools/call` blocks on its sub-agent before replying "still
/// working". Bob's MCP client abandons any call after 300s (the largest
/// `timeout` it honors), so this stays comfortably below that.
const WAIT_BUDGET: Duration = Duration::from_secs(240);

/// Name of the tool that collects the result of a job reported as still working.
const WAIT_TOOL_NAME: &str = "sub-agent-wait";

/// Name of the tool that stops a running job.
const CANCEL_TOOL_NAME: &str = "sub-agent-cancel";

/// A finished job's result: the sub-agent's output, or the error text.
type JobOutcome = Result<String, String>;

/// What a caller gets back after waiting on a job.
#[derive(Debug, PartialEq)]
enum JobReply {
    Finished(JobOutcome),
    /// Still going; carries the job id to wait on.
    Running(String),
}

/// Why a job's result is an error, when it did not run to completion.
const CANCELLED_MESSAGE: &str = "the sub-agent job was cancelled";

/// One background sub-agent run, identified to callers by its table id.
struct Job {
    tool: String,
    task: String,
    outcome: watch::Receiver<Option<JobOutcome>>,
    /// Dropping or firing this stops the job's work (and so kills `pi`).
    cancel: Option<oneshot::Sender<()>>,
}

/// Background sub-agent runs, decoupled from the HTTP request that started
/// them. Bob abandons a request after 300s and then retries it; running the
/// work detached lets a retry (or `sub-agent-wait`) pick up the same run
/// instead of orphaning it and starting a duplicate.
#[derive(Default)]
struct JobTable {
    next_id: AtomicU64,
    jobs: Mutex<HashMap<String, Job>>,
}

impl JobTable {
    /// Starts `work` as a detached job, unless a job for the same tool and
    /// task is already in the table, in which case it attaches to that one
    /// and drops `work` unpolled.
    fn start_or_attach<F>(
        &self,
        tool: &str,
        task: &str,
        work: F,
    ) -> (String, watch::Receiver<Option<JobOutcome>>)
    where
        F: Future<Output = JobOutcome> + Send + 'static,
    {
        let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((id, job)) = jobs.iter().find(|(_, j)| j.tool == tool && j.task == task) {
            return (id.clone(), job.outcome.clone());
        }
        let id = format!("job-{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        let (tx, rx) = watch::channel(None);
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            // Losing the race drops `work`, which is what stops the sub-agent.
            // A dropped `cancel_tx` also wakes `cancel_rx`, so a job whose
            // table is gone (the launch ended) is cancelled too.
            let outcome = tokio::select! {
                outcome = work => outcome,
                _ = cancel_rx => Err(CANCELLED_MESSAGE.to_string()),
            };
            tx.send_replace(Some(outcome));
        });
        jobs.insert(
            id.clone(),
            Job {
                tool: tool.to_string(),
                task: task.to_string(),
                outcome: rx.clone(),
                cancel: Some(cancel_tx),
            },
        );
        (id, rx)
    }

    /// Stops a running job and forgets it, returning whether `id` named one.
    /// Any caller still waiting on it is told it was cancelled.
    fn cancel(&self, id: &str) -> bool {
        let job = self
            .jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        match job {
            Some(mut job) => {
                if let Some(cancel) = job.cancel.take() {
                    let _ = cancel.send(());
                }
                true
            }
            None => false,
        }
    }

    /// The receiver for an uncollected job, if `id` names one.
    fn receiver_for(&self, id: &str) -> Option<watch::Receiver<Option<JobOutcome>>> {
        let jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        jobs.get(id).map(|j| j.outcome.clone())
    }

    /// Waits up to `budget` for the job to finish. A finished job is removed
    /// from the table, so its result is delivered once.
    async fn await_outcome(
        &self,
        id: &str,
        mut rx: watch::Receiver<Option<JobOutcome>>,
        budget: Duration,
    ) -> JobReply {
        let outcome = match tokio::time::timeout(budget, rx.wait_for(Option::is_some)).await {
            Err(_) => return JobReply::Running(id.to_string()),
            Ok(Ok(value)) => value
                .clone()
                .unwrap_or_else(|| Err("sub-agent job ended unexpectedly".to_string())),
            // The job's task died without reporting (e.g. it panicked).
            Ok(Err(_)) => Err("sub-agent job ended unexpectedly".to_string()),
        };
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        JobReply::Finished(outcome)
    }
}

/// Renders a job reply as the tool result Bob sees: the final output, an
/// error, or a one-line pointer to `sub-agent-wait`. Nothing else is sent, so
/// the sub-agent's transcript never enters Bob's context.
fn job_reply_to_result(reply: JobReply) -> CallToolResult {
    match reply {
        JobReply::Finished(Ok(output)) => CallToolResult::success(vec![ContentBlock::text(output)]),
        JobReply::Finished(Err(error)) => CallToolResult::error(vec![ContentBlock::text(error)]),
        JobReply::Running(id) => CallToolResult::success(vec![ContentBlock::text(format!(
            "The sub-agent is still working (job {id}). Call `{WAIT_TOOL_NAME}` with \
             job_id \"{id}\" to collect its result."
        ))]),
    }
}

/// The `job_id` argument of a `sub-agent-wait`/`sub-agent-cancel` call.
fn job_id_argument(request: &CallToolRequestParams) -> Option<&str> {
    request
        .arguments
        .as_ref()
        .and_then(|args| args.get("job_id"))
        .and_then(|v| v.as_str())
}

/// The JSON Schema of the job-addressed tools (`sub-agent-wait` and
/// `sub-agent-cancel`): a single required `job_id`.
fn job_tool_input_schema() -> serde_json::Map<String, serde_json::Value> {
    serde_json::json!({
        "type": "object",
        "properties": {
            "job_id": {
                "type": "string",
                "description": "The job id from a 'still working' reply.",
            },
        },
        "required": ["job_id"],
    })
    .as_object()
    .expect("object literal")
    .clone()
}

/// Handles MCP `tools/list`/`tools/call` for the bound delegate sub-agents.
/// `Clone` is cheap (a few `Arc`s), which is what lets `call_tool` sidestep
/// the trait method's `&self` / `+ '_` lifetime by cloning `self` into an
/// owned `async move` block.
#[derive(Clone)]
struct DelegateToolRegistry {
    pi_binary: Arc<PathBuf>,
    agents: Arc<Vec<DelegateSubAgent>>,
    jobs: Arc<JobTable>,
    wait_budget: Duration,
}

impl DelegateToolRegistry {
    /// Runs the sub-agent at `index` on `task` to completion.
    async fn run_agent(&self, index: usize, task: &str) -> JobOutcome {
        let agent = &self.agents[index];
        let plain_ui = crate::utils::ui::backends::plain::PlainOutput;
        agent
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
            .map_err(|e| e.to_string())
    }

    /// `sub-agent-cancel`: stops a job and kills its sub-agent process.
    fn cancel_job(&self, request: &CallToolRequestParams) -> CallToolResult {
        let Some(id) = job_id_argument(request) else {
            return CallToolResult::error(vec![ContentBlock::text(
                "missing required string argument 'job_id'",
            )]);
        };
        if self.jobs.cancel(id) {
            CallToolResult::success(vec![ContentBlock::text(format!("Cancelled job {id}."))])
        } else {
            CallToolResult::error(vec![ContentBlock::text(format!(
                "no such job '{id}' (unknown, or it already finished and was delivered)"
            ))])
        }
    }

    /// `sub-agent-wait`: waits on an already-started job.
    async fn collect_job(&self, request: &CallToolRequestParams) -> CallToolResult {
        let Some(id) = job_id_argument(request) else {
            return CallToolResult::error(vec![ContentBlock::text(
                "missing required string argument 'job_id'",
            )]);
        };
        let Some(rx) = self.jobs.receiver_for(id) else {
            return CallToolResult::error(vec![ContentBlock::text(format!(
                "no such job '{id}' (unknown, or its result was already delivered)"
            ))]);
        };
        job_reply_to_result(self.jobs.await_outcome(id, rx, self.wait_budget).await)
    }

    async fn call_tool_impl(
        &self,
        request: CallToolRequestParams,
    ) -> Result<CallToolResult, ErrorData> {
        if request.name == WAIT_TOOL_NAME {
            return Ok(self.collect_job(&request).await);
        }
        if request.name == CANCEL_TOOL_NAME {
            return Ok(self.cancel_job(&request));
        }
        let Some(index) = self.agents.iter().position(|a| a.tool_name == request.name) else {
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

        let registry = self.clone();
        let owned_task = task.to_string();
        let (id, rx) = self.jobs.start_or_attach(&request.name, task, async move {
            registry.run_agent(index, &owned_task).await
        });
        Ok(job_reply_to_result(
            self.jobs.await_outcome(&id, rx, self.wait_budget).await,
        ))
    }
}

impl ServerHandler for DelegateToolRegistry {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools: Vec<Tool> = self
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
        tools.push(Tool::new(
            WAIT_TOOL_NAME,
            "Collect the result of a sub-agent call that replied it was still working. \
             Blocks until the sub-agent finishes or about four minutes pass; if it replies \
             still working again, call it again. Do not call it otherwise.",
            job_tool_input_schema(),
        ));
        tools.push(Tool::new(
            CANCEL_TOOL_NAME,
            "Stop a running sub-agent job and kill its process. Use only when the user asks \
             to abandon a sub-agent, or it is clearly hung.",
            job_tool_input_schema(),
        ));
        // The 2026-07-28 spec requires ttl_ms and cache_scope in tools/list
        // responses. Set them to sensible defaults when the protocol version
        // supports them; mirror the rmcp #[tool_handler] macro behavior.
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28);
        let result = ListToolsResult::with_all_items(tools);
        let result = if supports_cache_hints {
            result.with_ttl_ms(0).with_cache_scope(CacheScope::Public)
        } else {
            result
        };
        Ok(result)
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
        jobs: Arc::new(JobTable::default()),
        wait_budget: WAIT_BUDGET,
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
            // Bob's client gives up on a call after at most 300s no matter what
            // larger value is configured, and documents a fixed set of accepted
            // values that includes 300000. Calls longer than `WAIT_BUDGET` are
            // handled by the job table, not by a longer timeout.
            timeout: Some(300_000),
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

    /// Extract the JSON payload from an SSE response body, falling back to the
    /// raw body when it is plain JSON.
    fn extract_json_response(body: &str) -> String {
        body.lines()
            .find_map(|line| line.trim().strip_prefix("data: "))
            .map(|data| data.trim().to_string())
            .unwrap_or_else(|| body.to_string())
    }

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
            supported_reasoning_efforts: vec![],
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
    async fn a_job_that_finishes_within_budget_is_delivered_once() {
        let table = JobTable::default();
        let (id, rx) =
            table.start_or_attach("explore", "look", async { Ok("found it".to_string()) });
        let reply = table.await_outcome(&id, rx, Duration::from_secs(5)).await;
        assert_eq!(reply, JobReply::Finished(Ok("found it".to_string())));
        assert!(
            table.receiver_for(&id).is_none(),
            "result is delivered once"
        );
    }

    #[tokio::test]
    async fn a_job_past_its_budget_reports_running_and_can_be_collected_later() {
        let table = JobTable::default();
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        let (id, rx) = table.start_or_attach("explore", "look", async move {
            let _ = gate.await;
            Ok("late".to_string())
        });

        let first = table
            .await_outcome(&id, rx, Duration::from_millis(20))
            .await;
        assert_eq!(first, JobReply::Running(id.clone()));
        assert!(
            table.receiver_for(&id).is_some(),
            "running job stays collectable"
        );

        release.send(()).unwrap();
        let rx = table.receiver_for(&id).unwrap();
        let second = table.await_outcome(&id, rx, Duration::from_secs(5)).await;
        assert_eq!(second, JobReply::Finished(Ok("late".to_string())));
    }

    #[tokio::test]
    async fn a_retry_of_the_same_call_attaches_instead_of_starting_a_second_job() {
        let table = JobTable::default();
        let runs = Arc::new(AtomicU64::new(0));
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        let counter = runs.clone();
        let (first_id, _rx) = table.start_or_attach("explore", "look", async move {
            counter.fetch_add(1, Ordering::SeqCst);
            let _ = gate.await;
            Ok("once".to_string())
        });
        let counter = runs.clone();
        let (second_id, rx) = table.start_or_attach("explore", "look", async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok("twice".to_string())
        });
        assert_eq!(first_id, second_id);

        release.send(()).unwrap();
        let reply = table
            .await_outcome(&second_id, rx, Duration::from_secs(5))
            .await;
        assert_eq!(reply, JobReply::Finished(Ok("once".to_string())));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn different_tasks_get_different_jobs() {
        let table = JobTable::default();
        let (a, _) = table.start_or_attach("explore", "one", async { Ok(String::new()) });
        let (b, _) = table.start_or_attach("explore", "two", async { Ok(String::new()) });
        let (c, _) = table.start_or_attach("plan", "one", async { Ok(String::new()) });
        assert!(a != b && a != c && b != c);
    }

    #[tokio::test]
    async fn a_failed_job_reports_its_error() {
        let table = JobTable::default();
        let (id, rx) = table.start_or_attach("explore", "look", async { Err("boom".to_string()) });
        let reply = table.await_outcome(&id, rx, Duration::from_secs(5)).await;
        assert_eq!(reply, JobReply::Finished(Err("boom".to_string())));
    }

    #[test]
    fn running_reply_points_at_the_wait_tool_and_nothing_else() {
        let result = job_reply_to_result(JobReply::Running("job-7".to_string()));
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("job-7"), "{json}");
        assert!(json.contains(WAIT_TOOL_NAME), "{json}");
        assert!(!json.contains("\"isError\":true"), "{json}");
    }

    /// Signals when dropped, standing in for the sub-agent process that
    /// dropping a job's work future is meant to kill.
    struct DropSignal(Option<oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    #[tokio::test]
    async fn cancelling_a_job_drops_its_work_and_tells_waiters() {
        let table = JobTable::default();
        let (dropped_tx, dropped_rx) = oneshot::channel::<()>();
        let (started_tx, started_rx) = oneshot::channel::<()>();
        let (id, rx) = table.start_or_attach("explore", "look", async move {
            let _guard = DropSignal(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<JobOutcome>().await
        });
        // Cancelling before the work first runs would drop it unstarted, with
        // nothing to signal; wait until it is actually running.
        started_rx.await.unwrap();

        assert!(table.cancel(&id));
        tokio::time::timeout(Duration::from_secs(5), dropped_rx)
            .await
            .expect("work should be dropped promptly")
            .unwrap();
        let reply = table.await_outcome(&id, rx, Duration::from_secs(5)).await;
        assert_eq!(
            reply,
            JobReply::Finished(Err(CANCELLED_MESSAGE.to_string()))
        );
        assert!(table.receiver_for(&id).is_none());
    }

    #[tokio::test]
    async fn cancelling_an_unknown_job_reports_false() {
        assert!(!JobTable::default().cancel("job-404"));
    }

    #[tokio::test]
    async fn a_finished_job_cannot_be_cancelled() {
        let table = JobTable::default();
        let (id, rx) = table.start_or_attach("explore", "look", async { Ok("done".to_string()) });
        table.await_outcome(&id, rx, Duration::from_secs(5)).await;
        assert!(!table.cancel(&id));
    }

    #[tokio::test]
    async fn a_cancelled_task_can_be_started_again_as_a_fresh_job() {
        let table = JobTable::default();
        let (first, _rx) = table.start_or_attach("explore", "look", async {
            std::future::pending::<JobOutcome>().await
        });
        assert!(table.cancel(&first));
        let (second, rx) =
            table.start_or_attach("explore", "look", async { Ok("again".to_string()) });
        assert_ne!(first, second);
        let reply = table
            .await_outcome(&second, rx, Duration::from_secs(5))
            .await;
        assert_eq!(reply, JobReply::Finished(Ok("again".to_string())));
    }

    #[tokio::test]
    async fn dropping_the_table_cancels_running_jobs() {
        let table = JobTable::default();
        let (dropped_tx, dropped_rx) = oneshot::channel::<()>();
        let (started_tx, started_rx) = oneshot::channel::<()>();
        let _ = table.start_or_attach("explore", "look", async move {
            let _guard = DropSignal(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<JobOutcome>().await
        });
        started_rx.await.unwrap();
        drop(table);
        tokio::time::timeout(Duration::from_secs(5), dropped_rx)
            .await
            .expect("work should be dropped when the table goes away")
            .unwrap();
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

        // Under older protocol versions, ttlMs and cacheScope must be absent.
        assert!(body.contains("\"explore\""), "body: {body}");
        assert!(body.contains("explore the repo"), "body: {body}");
        assert!(body.contains("\"plan\""), "body: {body}");
        assert!(body.contains("plan the work"), "body: {body}");
        assert!(body.contains("\"task\""), "body: {body}");
        assert!(
            !body.contains("ttlMs"),
            "ttlMs must be absent under older protocol versions: {body}"
        );
        assert!(
            !body.contains("cacheScope"),
            "cacheScope must be absent under older protocol versions: {body}"
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn list_tools_reports_ttl_and_cache_scope_for_2026_07_28() {
        // 2026-07-28 is sessionless: no initialize call needed.
        let (binding, server) = start_test_server(vec![
            ("explore".to_string(), sub_agent_binding("explore the repo")),
            ("plan".to_string(), sub_agent_binding("plan the work")),
        ])
        .await;
        let url = mcp_url(&binding);

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Mcp-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "tools/list")
            .body(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/list",
                    "params": {
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientCapabilities": {},
                        }
                    },
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "status: {}", resp.status());
        let body = extract_json_response(&resp.text().await.unwrap());

        assert!(body.contains("\"explore\""), "body: {body}");
        assert!(body.contains("explore the repo"), "body: {body}");
        assert!(body.contains("\"plan\""), "body: {body}");
        assert!(body.contains("plan the work"), "body: {body}");

        // The 2026-07-28 spec requires ttlMs and cacheScope.
        let result: serde_json::Value = serde_json::from_str(&body).unwrap();
        let result_obj = result.get("result").and_then(|r| r.as_object());
        assert_eq!(
            result_obj
                .and_then(|r| r.get("ttlMs"))
                .and_then(|v| v.as_u64()),
            Some(0),
            "ttlMs should be 0"
        );
        assert_eq!(
            result_obj
                .and_then(|r| r.get("cacheScope"))
                .and_then(|v| v.as_str()),
            Some("public"),
            "cacheScope should be public"
        );

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
