//! Launcher for the `openclaw` self-hosted AI agent/gateway
//! (<https://docs.openclaw.ai>).
//!
//! OpenClaw's model/provider selection and MCP servers are both purely
//! config-file-driven (no env vars for either), so this launcher generates a
//! throwaway `openclaw.json` under the launcher state dir and points
//! `OPENCLAW_CONFIG_PATH` at it, exactly like `opencode.rs` does for
//! OpenCode's `OPENCODE_CONFIG`. The user's own `~/.openclaw/openclaw.json`
//! is never touched.

use crate::capabilities::{
    AgentModelBinding, ApiType, Binding, BindingType, McpBinding, ResolvedCapability,
};
use crate::launchers::base::{EnvBinding, LaunchContext, Launcher, LauncherMetadata, run_command};
use crate::launchers::shared::mcp_cli::mcp_binding_request;
use crate::registry::{ConfigConstructable, ConstructError};
use crate::utils::resolve_shell_command;
use crate::utils::ui::Ui;
use anyhow::Context;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/*-- public --*/

#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct OpenClawLauncherConfig {
    /// Override path to the `openclaw` binary for non-PATH installs.
    #[serde(default)]
    pub command_path: Option<String>,
}

pub struct OpenClawLauncher {
    instance_id: String,
    config: OpenClawLauncherConfig,
    bound_binding: Option<AgentModelBinding>,
    /// `(server_name, binding)` for every MCP-capable capability bound to
    /// this launcher, written into the generated config's `mcp.servers` block.
    bound_mcp_bindings: Vec<(String, McpBinding)>,
}

impl ConfigConstructable for OpenClawLauncher {
    type Config = OpenClawLauncherConfig;

    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        let config: OpenClawLauncherConfig =
            serde_json::from_value(cfg.clone()).map_err(ConstructError::settings)?;
        Ok(Self {
            instance_id: instance_id.to_string(),
            config,
            bound_binding: None,
            bound_mcp_bindings: vec![],
        })
    }
}

impl crate::registry::Named for OpenClawLauncher {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait]
impl Launcher for OpenClawLauncher {
    fn name(&self) -> &str {
        "OpenClaw CLI"
    }

    fn command(&self) -> &str {
        self.config.command_path.as_deref().unwrap_or("openclaw")
    }

    async fn bind_capability(&mut self, capability: &dyn ResolvedCapability) -> anyhow::Result<()> {
        let supported = Self::metadata().supported_capabilities;
        let capability_types = capability.binding_types();
        if !capability_types.is_subset(&supported) {
            anyhow::bail!(
                "capability supports {:?} which this launcher does not support",
                capability_types.difference(&supported).collect::<Vec<_>>()
            );
        }

        if capability_types.contains(&BindingType::Mcp) {
            let binding = capability.bind(mcp_binding_request()).await?;
            match binding {
                Binding::Mcp(binding) => {
                    self.bound_mcp_bindings
                        .push((capability.instance_id().to_string(), binding));
                }
                other => anyhow::bail!("expected an Mcp binding, got {:?}", other.binding_type()),
            }
            return Ok(());
        }

        // OpenClaw's custom-provider config speaks a plain OpenAI-compatible
        // dialect (`api: "openai-completions"`), which every granite-cli
        // provider can serve.
        let binding = capability
            .bind(crate::capabilities::BindingRequest::AgentModel(
                crate::capabilities::AgentModelBindingRequest {
                    api_type: ApiType::OpenAI,
                },
            ))
            .await?;
        match binding {
            Binding::AgentModel(binding) => {
                self.bound_binding = Some(binding);
            }
            other => anyhow::bail!(
                "expected an AgentModel binding, got {:?}",
                other.binding_type()
            ),
        }
        Ok(())
    }

    fn validate_command(&self) -> anyhow::Result<PathBuf> {
        resolve_shell_command(&self.config.command_path, "openclaw")
    }

    /// Points OpenClaw at the granite-cli-generated config file. There is no
    /// env var for provider/model/API-key -- OpenClaw reads all of it from
    /// `OPENCLAW_CONFIG_PATH` itself, written by `launch()`.
    async fn env_overlay(&self, ctx: &LaunchContext) -> anyhow::Result<Vec<EnvBinding>> {
        let mut overlay = vec![];
        if self.bound_binding.is_some() || !self.bound_mcp_bindings.is_empty() {
            overlay.push(EnvBinding {
                key: CONFIG_ENV.to_string(),
                value: openclaw_config_path(ctx)?.to_string_lossy().to_string(),
            });
        }
        Ok(overlay)
    }

    /// Writes the granite-cli OpenClaw config file, then execs `openclaw`
    /// with the caller's arguments untouched.
    async fn launch(
        &self,
        args: &[String],
        ctx: &LaunchContext,
        ui: &dyn Ui,
    ) -> anyhow::Result<std::process::ExitStatus> {
        if self.bound_binding.is_some() || !self.bound_mcp_bindings.is_empty() {
            let config = generate_config(self.bound_binding.as_ref(), &self.bound_mcp_bindings);
            let config_path = openclaw_config_path(ctx)?;

            if ctx.dry_run {
                ui.info(&format!(
                    "Would write OpenClaw config to {}:",
                    config_path.display()
                ));
                ui.info(&serde_json::to_string_pretty(&config)?);
            } else {
                write_openclaw_config(&config_path, &config)?;
                ui.info(&format!(
                    "Wrote OpenClaw config to {}",
                    config_path.display()
                ));
            }
        }

        let binary = self.validate_command()?;
        let overlay = self.env_overlay(ctx).await?;

        run_command(binary, &overlay, args, ctx, ui).await
    }
}

impl HasOpenClawLauncherMetadata for OpenClawLauncher {
    fn metadata() -> LauncherMetadata {
        LauncherMetadata {
            name: "OpenClaw CLI".to_string(),
            description: "OpenClaw self-hosted AI agent launcher".to_string(),
            default_command: "openclaw".to_string(),
            supported_capabilities: HashSet::from([BindingType::AgentModel, BindingType::Mcp]),
            tags: vec!["openclaw".to_string(), "agent".to_string()],
        }
    }
}

/*-- private --*/

use crate::launchers::base::HasLauncherMetadata as HasOpenClawLauncherMetadata;

/// Env var OpenClaw reads to override which config file it treats as active,
/// entirely replacing (not merging with) `~/.openclaw/openclaw.json` for the
/// life of the process.
const CONFIG_ENV: &str = "OPENCLAW_CONFIG_PATH";

/// The generated config file's name, relative to the launcher state dir.
const CONFIG_FILE: &str = "openclaw.json";

/// OpenClaw's `baseUrl` is the API root its OpenAI-compatible adapter appends
/// operation paths to, same derivation as `opencode.rs`'s equivalent.
fn openclaw_base_url(binding: &AgentModelBinding) -> String {
    let root = binding.base_url.trim_end_matches('/');
    let prefix = binding
        .endpoint_path
        .strip_suffix("/chat/completions")
        .unwrap_or("");
    format!("{root}{prefix}")
}

/// The granite-cli-owned OpenClaw config file this launcher instance writes
/// and points `OPENCLAW_CONFIG_PATH` at. Lives under the launcher state dir
/// rather than the user's own `~/.openclaw`, so it is never read by anything
/// else.
fn openclaw_config_path(ctx: &LaunchContext) -> anyhow::Result<PathBuf> {
    Ok(crate::config::Config::launcher_state_dir(&ctx.launcher_id)?.join(CONFIG_FILE))
}

/// Builds the top-level `openclaw.json` shape (see
/// docs.openclaw.ai/gateway/configuration-reference and
/// docs.openclaw.ai/gateway/config-agents): a `models.providers.<name>` entry
/// plus `agents.defaults.model` set to `"<provider>/<model>"` (if bound), and
/// an `mcp.servers` block (if any MCP servers are bound) using OpenClaw's
/// stdio (`command`/`args`/`env`) and remote (`url`/`transport`/`headers`)
/// server shapes.
fn generate_config(
    binding: Option<&AgentModelBinding>,
    mcp_bindings: &[(String, McpBinding)],
) -> serde_json::Value {
    let mut config = serde_json::json!({});
    if let Some(binding) = binding {
        let mut model_entry = serde_json::json!({
            "id": binding.model_name,
            "name": binding.model_name,
        });
        if let Some(context_length) = binding.context_length {
            model_entry["contextWindow"] = serde_json::json!(context_length);
        }

        let mut provider_entry = serde_json::json!({
            "baseUrl": openclaw_base_url(binding),
            "api": "openai-completions",
            "models": [model_entry],
        });
        // API Key is required even if the provider doesn't use it
        let mut api_key_val = serde_json::Value::String("unused".to_string());
        if let Some(api_key) = binding
            .api_key
            .as_ref()
            .map(|api_key| api_key.0.clone())
            .filter(|key| !key.is_empty())
        {
            api_key_val = serde_json::Value::String(api_key);
        }
        provider_entry["apiKey"] = api_key_val;

        // Add custom headers from binding if present. Omit entirely when
        // custom_headers is empty to match the behavior of other launchers.
        if let Some(headers) = &binding.custom_headers {
            if !headers.is_empty() {
                let header_map: serde_json::Map<String, serde_json::Value> = headers
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.0.clone())))
                    .collect();
                provider_entry["headers"] = serde_json::Value::Object(header_map);
            }
        }

        let mut providers = serde_json::Map::new();
        providers.insert(binding.provider_name.clone(), provider_entry);
        config["models"] = serde_json::json!({ "providers": providers });
        config["agents"] = serde_json::json!({
            "defaults": {
                "model": format!("{}/{}", binding.provider_name, binding.model_name),
            }
        });
    }
    if !mcp_bindings.is_empty() {
        let mut servers = serde_json::Map::new();
        for (name, binding) in mcp_bindings {
            servers.insert(name.clone(), {
                match binding {
                    McpBinding::Stdio {
                        command, args, env, ..
                    } => serde_json::json!({
                        "command": command,
                        "args": args,
                        "env": env,
                    }),
                    McpBinding::Http { url, headers, .. } => serde_json::json!({
                        "url": url,
                        "transport": "streamable-http",
                        "headers": headers,
                    }),
                    McpBinding::Sse { url, headers, .. } => serde_json::json!({
                        "url": url,
                        "transport": "sse",
                        "headers": headers,
                    }),
                }
            });
        }
        config["mcp"] = serde_json::json!({ "servers": servers });
    }
    config
}

fn write_openclaw_config(path: &Path, config: &serde_json::Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let mut content = serde_json::to_string_pretty(config)?;
    content.push('\n');
    std::fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Named, Secret};
    use crate::utils::ui::base::tests::CaptureUi;

    fn launcher(cfg: serde_json::Value) -> OpenClawLauncher {
        OpenClawLauncher::new("openclaw", &cfg).unwrap()
    }

    fn binding() -> AgentModelBinding {
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

    fn bound(cfg: serde_json::Value, binding: AgentModelBinding) -> OpenClawLauncher {
        let mut l = launcher(cfg);
        l.bound_binding = Some(binding);
        l
    }

    fn ctx(dry_run: bool) -> LaunchContext {
        LaunchContext {
            launcher_id: "openclaw".to_string(),
            working_dir: PathBuf::from("/tmp"),
            base_env: std::collections::HashMap::new(),
            dry_run,
            usage_tracker: None,
            model_proxy: None,
        }
    }

    // -- command resolution ----------------------------------------------------

    #[test]
    fn command_defaults_to_openclaw() {
        assert_eq!(launcher(serde_json::json!({})).command(), "openclaw");
    }

    #[test]
    fn command_uses_explicit_path_when_set() {
        let l = launcher(serde_json::json!({ "command_path": "/opt/bin/openclaw" }));
        assert_eq!(l.command(), "/opt/bin/openclaw");
    }

    #[test]
    fn validate_command_falls_back_to_path_for_bare_command_name() {
        let l = launcher(serde_json::json!({ "command_path": "ls" }));
        assert!(l.validate_command().is_ok());
    }

    // -- metadata / schema -----------------------------------------------------

    #[test]
    fn metadata_name_is_openclaw_cli() {
        let meta = OpenClawLauncher::metadata();
        assert_eq!(meta.name, "OpenClaw CLI");
        assert_eq!(meta.default_command, "openclaw");
        assert!(
            meta.supported_capabilities
                .contains(&BindingType::AgentModel)
        );
        assert!(meta.supported_capabilities.contains(&BindingType::Mcp));
    }

    #[test]
    fn instance_id_round_trips_from_construction() {
        let l = OpenClawLauncher::new("openclaw-local", &serde_json::json!({})).unwrap();
        assert_eq!(l.instance_id(), "openclaw-local");
    }

    #[test]
    fn config_schema_exposes_command_path() {
        use crate::launchers::base::LauncherFactory;
        let mut factory = LauncherFactory::new();
        factory.register::<OpenClawLauncher>("openclaw");
        let schema = factory.config_schema("openclaw").unwrap();
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap();
        assert!(props.contains_key("command_path"));
    }

    // -- base url ----------------------------------------------------------

    #[test]
    fn base_url_keeps_version_prefix_and_drops_operation() {
        assert_eq!(openclaw_base_url(&binding()), "http://localhost:11434/v1");
    }

    // -- generate_config -----------------------------------------------------

    #[test]
    fn generate_config_nests_provider_under_models_providers() {
        let config = generate_config(Some(&binding()), &[]);
        assert_eq!(
            config["models"]["providers"]["my-ollama"]["baseUrl"],
            "http://localhost:11434/v1"
        );
        assert_eq!(
            config["models"]["providers"]["my-ollama"]["api"],
            "openai-completions"
        );
        assert_eq!(
            config["models"]["providers"]["my-ollama"]["models"][0]["id"],
            "granite4.1:8b"
        );
        assert_eq!(
            config["models"]["providers"]["my-ollama"]["models"][0]["contextWindow"],
            131072
        );
        assert_eq!(
            config["agents"]["defaults"]["model"],
            "my-ollama/granite4.1:8b"
        );
        // No key means no apiKey field at all.
        assert_eq!(
            config["models"]["providers"]["my-ollama"]["apiKey"],
            "unused"
        );
    }

    #[test]
    fn generate_config_includes_api_key_when_present() {
        let b = AgentModelBinding {
            api_key: Some(Secret::from("sk-test")),
            ..binding()
        };
        let config = generate_config(Some(&b), &[]);
        assert_eq!(
            config["models"]["providers"]["my-ollama"]["apiKey"],
            "sk-test"
        );
    }

    #[test]
    fn generate_config_writes_mcp_servers_block_without_a_model_binding() {
        let mcp_binding = McpBinding::Http {
            url: "http://127.0.0.1:9999".to_string(),
            headers: Default::default(),
            timeout: None,
        };
        let config = generate_config(None, &[("vision".to_string(), mcp_binding)]);
        assert!(config.get("models").is_none());
        assert_eq!(
            config["mcp"]["servers"]["vision"]["url"],
            "http://127.0.0.1:9999"
        );
        assert_eq!(
            config["mcp"]["servers"]["vision"]["transport"],
            "streamable-http"
        );
    }

    #[test]
    fn generate_config_writes_stdio_mcp_server_with_command_args_env() {
        let mcp_binding = McpBinding::Stdio {
            command: "/usr/local/bin/granite-cli".to_string(),
            args: vec!["__mcp-serve".to_string(), "vision".to_string()],
            env: std::collections::HashMap::from([("FOO".to_string(), "bar".to_string())]),
            timeout: None,
        };
        let config = generate_config(None, &[("vision".to_string(), mcp_binding)]);
        assert_eq!(
            config["mcp"]["servers"]["vision"]["command"],
            "/usr/local/bin/granite-cli"
        );
        assert_eq!(config["mcp"]["servers"]["vision"]["args"][0], "__mcp-serve");
        assert_eq!(config["mcp"]["servers"]["vision"]["env"]["FOO"], "bar");
    }

    #[test]
    fn generate_config_includes_headers_when_present() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("X-Custom".to_string(), Secret::from("value1"));
        headers.insert("Authorization".to_string(), Secret::from("Bearer token"));
        let b = AgentModelBinding {
            custom_headers: Some(headers),
            ..binding()
        };
        let config = generate_config(Some(&b), &[]);
        assert_eq!(
            config["models"]["providers"]["my-ollama"]["headers"]["Authorization"],
            "Bearer token"
        );
        assert_eq!(
            config["models"]["providers"]["my-ollama"]["headers"]["X-Custom"],
            "value1"
        );
    }

    #[test]
    fn generate_config_omits_headers_when_none() {
        let config = generate_config(Some(&binding()), &[]);
        assert!(
            config["models"]["providers"]["my-ollama"]
                .get("headers")
                .is_none()
        );
    }

    #[test]
    fn generate_config_omits_headers_when_empty() {
        let b = AgentModelBinding {
            custom_headers: Some(std::collections::HashMap::new()),
            ..binding()
        };
        let config = generate_config(Some(&b), &[]);
        assert!(
            config["models"]["providers"]["my-ollama"]
                .get("headers")
                .is_none()
        );
    }

    // -- env overlay -----------------------------------------------------------

    #[tokio::test]
    async fn env_overlay_is_empty_without_a_binding() {
        let overlay = launcher(serde_json::json!({}))
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        assert!(overlay.is_empty());
    }

    #[tokio::test]
    async fn env_overlay_redirects_config_path_when_bound() {
        let overlay = bound(serde_json::json!({}), binding())
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        let config = overlay
            .iter()
            .find(|b| b.key == "OPENCLAW_CONFIG_PATH")
            .expect("config redirect");
        assert!(
            Path::new(&config.value).ends_with(
                Path::new("launcher-state")
                    .join("openclaw")
                    .join("openclaw.json")
            ),
            "{}",
            config.value
        );
    }

    // -- launch ----------------------------------------------------------------

    // Deliberately reads whatever `GRANITE_CLI_HOME` is ambient rather than
    // setting it: env mutation would race the other tests in this binary that
    // point that var at their own tempdirs.
    #[tokio::test]
    async fn dry_run_launch_reports_without_writing_anything() {
        let state_dir = crate::config::Config::launcher_state_dir("openclaw").unwrap();
        let existed_before = state_dir.exists();

        let l = bound(serde_json::json!({ "command_path": "ls" }), binding());
        let ui = CaptureUi::default();
        let status = l
            .launch(&["--help".to_string()], &ctx(true), &ui)
            .await
            .unwrap();
        assert!(status.success());

        let infos = ui.infos.borrow();
        assert!(
            infos
                .iter()
                .any(|m| m.contains("Would write OpenClaw config")),
            "expected a dry-run notice, got {infos:?}"
        );
        assert!(
            infos
                .iter()
                .any(|m| m.contains(r#""model": "my-ollama/granite4.1:8b""#)),
            "expected the generated config to select the model, got {infos:?}"
        );
        assert!(infos.iter().any(|m| m.contains("args: --help")));
        assert_eq!(
            state_dir.exists(),
            existed_before,
            "dry run must not create {}",
            state_dir.display()
        );
    }

    #[tokio::test]
    async fn launch_without_binding_passes_args_through_unchanged() {
        let l = launcher(serde_json::json!({ "command_path": "ls" }));
        let ui = CaptureUi::default();
        l.launch(&["--version".to_string()], &ctx(true), &ui)
            .await
            .unwrap();

        let infos = ui.infos.borrow();
        assert!(infos.iter().any(|m| m.contains("args: --version")));
        assert!(
            !infos
                .iter()
                .any(|m| m.contains("Would write OpenClaw config"))
        );
        assert!(!infos.iter().any(|m| m.contains(CONFIG_ENV)));
    }
}
