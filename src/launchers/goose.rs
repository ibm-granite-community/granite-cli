//! Launcher for the `goose` coding agent (<https://block.github.io/goose/>).
//!
//! Goose has no persistent-config mechanism for injecting a one-off model or
//! MCP server without touching the user's own `config.yaml`, so both are done
//! through the surfaces goose documents for exactly that: env vars for the
//! model (`GOOSE_PROVIDER`/`GOOSE_MODEL`/`OPENAI_HOST`/...), and the
//! session-scoped `--with-extension`/`--with-streamable-http-extension` CLI
//! flags for MCP servers (goose calls them "extensions").

// Standard
use std::collections::HashSet;
use std::path::PathBuf;

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// Local
use crate::capabilities::{
    AgentModelBinding, ApiType, Binding, BindingType, McpBinding, ResolvedCapability,
};
use crate::launchers::base::{EnvBinding, LaunchContext, Launcher, LauncherMetadata, run_command};
use crate::launchers::shared::mcp_cli::mcp_binding_request;
use crate::registry::{ConfigConstructable, ConstructError};
use crate::utils::resolve_shell_command;
use crate::utils::ui::Ui;

use_channel!("GOOSE");

/*-- public --*/

#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct GooseLauncherConfig {
    /// Override path to the `goose` binary for non-PATH installs.
    #[serde(default)]
    pub command_path: Option<String>,
}

pub struct GooseLauncher {
    instance_id: String,
    config: GooseLauncherConfig,
    bound_binding: Option<AgentModelBinding>,
    /// `(server_name, binding)` for every MCP-capable capability bound to
    /// this launcher, turned into `--with-extension`/
    /// `--with-streamable-http-extension` flags in `launch()`.
    bound_mcp_bindings: Vec<(String, McpBinding)>,
}

impl ConfigConstructable for GooseLauncher {
    type Config = GooseLauncherConfig;

    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        let config: GooseLauncherConfig =
            serde_json::from_value(cfg.clone()).map_err(ConstructError::settings)?;
        Ok(Self {
            instance_id: instance_id.to_string(),
            config,
            bound_binding: None,
            bound_mcp_bindings: vec![],
        })
    }
}

impl crate::registry::Named for GooseLauncher {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait]
impl Launcher for GooseLauncher {
    fn name(&self) -> &str {
        "Goose CLI"
    }

    fn command(&self) -> &str {
        self.config.command_path.as_deref().unwrap_or("goose")
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

        // Goose only recognizes a fixed set of built-in provider ids (see
        // `env_overlay`, which always sends `GOOSE_PROVIDER=openai`), and the
        // "openai" provider is the one that honors `OPENAI_HOST` for
        // OpenAI-compatible endpoints -- so that's what every granite-cli
        // provider is asked to speak here.
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
        resolve_shell_command(&self.config.command_path, "goose")
    }

    /// Env vars goose documents for overriding its OpenAI-compatible
    /// provider without touching `config.yaml`.
    ///
    /// `GOOSE_PROVIDER` is always the literal `"openai"` -- goose has no
    /// notion of an arbitrary custom provider id, only its fixed built-in
    /// providers, and `"openai"` is the one that reads `OPENAI_HOST`.
    /// `OPENAI_API_KEY` is required even for a local server that ignores it:
    /// goose panics with "No provider configured" if it's unset entirely
    /// (see block/goose#5138).
    async fn env_overlay(&self, _ctx: &LaunchContext) -> anyhow::Result<Vec<EnvBinding>> {
        let mut overlay = Vec::new();

        if let Some(binding) = &self.bound_binding {
            overlay.push(EnvBinding {
                key: "GOOSE_PROVIDER".to_string(),
                value: "openai".to_string(),
            });
            overlay.push(EnvBinding {
                key: "GOOSE_MODEL".to_string(),
                value: binding.model_name.clone(),
            });

            if !binding.base_url.is_empty() {
                // OPENAI_HOST is scheme+host only; goose appends its own
                // default operation path unless OPENAI_BASE_PATH overrides it.
                overlay.push(EnvBinding {
                    key: "OPENAI_HOST".to_string(),
                    value: binding.base_url.clone(),
                });
            }

            let path = binding.endpoint_path.trim_start_matches('/');
            if !path.is_empty() && path != DEFAULT_OPENAI_BASE_PATH {
                overlay.push(EnvBinding {
                    key: "OPENAI_BASE_PATH".to_string(),
                    value: path.to_string(),
                });
            }

            if let Some(context_length) = binding.context_length {
                overlay.push(EnvBinding {
                    key: "GOOSE_CONTEXT_LIMIT".to_string(),
                    value: context_length.to_string(),
                });
            }

            let api_key_val = binding
                .api_key
                .as_ref()
                .map(|api_key| api_key.0.clone())
                .filter(|key| !key.is_empty())
                .unwrap_or_else(|| PLACEHOLDER_API_KEY.to_string());
            overlay.push(EnvBinding {
                key: "OPENAI_API_KEY".to_string(),
                value: api_key_val,
            });

            // Add custom headers as OPENAI_CUSTOM_HEADERS env var if configured.
            // Format: HEADER_A=VALUE_A,HEADER_B=VALUE_B, sorted by key for deterministic output.
            if let Some(ref headers) = binding.custom_headers {
                if !headers.is_empty() {
                    let mut header_pairs: Vec<(String, String)> = headers
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                serde_json::to_value(v)
                                    .unwrap()
                                    .as_str()
                                    .unwrap()
                                    .to_string(),
                            )
                        })
                        .collect();
                    header_pairs.sort_by(|a, b| a.0.cmp(&b.0));
                    let header_lines: Vec<String> = header_pairs
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    overlay.push(EnvBinding {
                        key: "OPENAI_CUSTOM_HEADERS".to_string(),
                        value: header_lines.join(","),
                    });
                }
            }
        }
        alog_channel!(MessageLevel::Debug4, "Env Overlay: {:#?}", overlay);
        Ok(overlay)
    }

    /// Registers each bound MCP server as a session-scoped goose "extension"
    /// via `--with-extension`/`--with-streamable-http-extension`, inserted at
    /// whichever position in the caller's own args actually parses (see
    /// `insert_extension_flags`) -- goose has no way to register an
    /// extension without these flags on the invocation itself, so unlike
    /// `claude`/`bob` there is nothing to register before spawning and clean
    /// up after: the registration *is* the invocation.
    async fn launch(
        &self,
        args: &[String],
        ctx: &LaunchContext,
        ui: &dyn Ui,
    ) -> anyhow::Result<std::process::ExitStatus> {
        let binary = self.validate_command()?;
        let overlay = self.env_overlay(ctx).await?;

        let mut extension_flags: Vec<String> = vec![];
        for (_, binding) in &self.bound_mcp_bindings {
            match binding {
                McpBinding::Stdio {
                    command, args, env, ..
                } => {
                    // Goose's `--with-extension` takes a single shell-style
                    // command string; env vars are inlined as `KEY=value`
                    // prefixes ahead of the command per goose's own docs.
                    let mut parts: Vec<String> =
                        env.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    parts.push(command.clone());
                    parts.extend(args.iter().cloned());
                    extension_flags.push("--with-extension".to_string());
                    extension_flags.push(parts.join(" "));
                }
                McpBinding::Http { url, headers, .. } | McpBinding::Sse { url, headers, .. } => {
                    if !headers.is_empty() {
                        ui.warn(
                            "goose's --with-streamable-http-extension does not support \
                             custom headers; they will be dropped for this launch",
                        );
                    }
                    extension_flags.push("--with-streamable-http-extension".to_string());
                    extension_flags.push(url.clone());
                }
            }
        }
        let goose_args = insert_extension_flags(args, extension_flags, ui);

        run_command(binary, &overlay, &goose_args, ctx, ui).await
    }
}

impl HasGooseLauncherMetadata for GooseLauncher {
    fn metadata() -> LauncherMetadata {
        LauncherMetadata {
            name: "Goose CLI".to_string(),
            description: "Goose Agent CLI launcher".to_string(),
            default_command: "goose".to_string(),
            supported_capabilities: HashSet::from([BindingType::AgentModel, BindingType::Mcp]),
            tags: vec!["goose".to_string(), "agent".to_string()],
        }
    }
}

/*-- private --*/

use crate::launchers::base::HasLauncherMetadata as HasGooseLauncherMetadata;

/// Goose's default OpenAI operation path, appended automatically unless
/// `OPENAI_BASE_PATH` overrides it.
const DEFAULT_OPENAI_BASE_PATH: &str = "v1/chat/completions";

/// Stand-in credential for providers that need no auth. Goose panics with
/// "No provider configured" if `OPENAI_API_KEY` is unset entirely, even for a
/// local server that never checks it.
const PLACEHOLDER_API_KEY: &str = "granite-cli";

/// Subcommands whose clap parser actually accepts
/// `--with-extension`/`--with-streamable-http-extension`, per `goose <sub>
/// --help`. Their short aliases are included since either form is a valid
/// first token.
const EXTENSION_CAPABLE_SUBCOMMANDS: &[&str] = &["session", "s", "run"];

/// Every other goose subcommand goose ships today (from `goose --help`,
/// aliases included). None of these accept the extension flags, so if the
/// caller explicitly named one of these, injecting the flags would just
/// produce a parse error identical to bare `goose --with-extension ...`.
const OTHER_KNOWN_SUBCOMMANDS: &[&str] = &[
    "configure",
    "info",
    "doctor",
    "mcp",
    "acp",
    "serve",
    "recipe",
    "skills",
    "plugin",
    "schedule",
    "sched",
    "gateway",
    "gw",
    "update",
    "term",
    "tui",
    "local-models",
    "lm",
    "completion",
    "review",
    "help",
];

/// Splices MCP extension flags into `args` at a position goose's clap parser
/// will actually accept.
///
/// `--with-extension`/`--with-streamable-http-extension` only parse under
/// goose's `session` and `run` subcommands -- bare `goose` (which otherwise
/// behaves like `session` for a plain interactive launch, per goose's own
/// default) rejects *any* flag at all unless a subcommand is named first.
/// So: if the caller already named `session`/`run`, the flags are inserted
/// right after it; if the caller named some other subcommand that goose
/// documents as not accepting these flags, the extensions are dropped with a
/// warning rather than corrupting the invocation; otherwise (no subcommand,
/// or the first token isn't a recognized one -- e.g. free-form prompt text)
/// `session` is inserted explicitly so the flags have somewhere to attach.
fn insert_extension_flags(args: &[String], flags: Vec<String>, ui: &dyn Ui) -> Vec<String> {
    if flags.is_empty() {
        return args.to_vec();
    }
    match args.first().map(String::as_str) {
        Some(sub) if EXTENSION_CAPABLE_SUBCOMMANDS.contains(&sub) => {
            let mut out = vec![args[0].clone()];
            out.extend(flags);
            out.extend_from_slice(&args[1..]);
            out
        }
        Some(sub) if OTHER_KNOWN_SUBCOMMANDS.contains(&sub) => {
            ui.warn(&format!(
                "goose subcommand '{sub}' does not accept MCP extension flags; \
                 bound MCP servers will not be attached for this launch"
            ));
            args.to_vec()
        }
        _ => {
            let mut out = vec!["session".to_string()];
            out.extend(flags);
            out.extend_from_slice(args);
            out
        }
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Named;
    use crate::utils::ui::base::tests::CaptureUi;
    use std::collections::HashMap;

    fn launcher(cfg: serde_json::Value) -> GooseLauncher {
        GooseLauncher::new("goose", &cfg).unwrap()
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

    fn bound(cfg: serde_json::Value, binding: AgentModelBinding) -> GooseLauncher {
        let mut l = launcher(cfg);
        l.bound_binding = Some(binding);
        l
    }

    fn with_mcp(cfg: serde_json::Value, name: &str, binding: McpBinding) -> GooseLauncher {
        let mut l = launcher(cfg);
        l.bound_mcp_bindings.push((name.to_string(), binding));
        l
    }

    fn ctx(dry_run: bool) -> LaunchContext {
        LaunchContext {
            launcher_id: "goose".to_string(),
            working_dir: PathBuf::from("/tmp"),
            base_env: std::collections::HashMap::new(),
            dry_run,
            usage_tracker: None,
            model_proxy: None,
        }
    }

    // -- command resolution ----------------------------------------------------

    #[test]
    fn command_defaults_to_goose() {
        assert_eq!(launcher(serde_json::json!({})).command(), "goose");
    }

    #[test]
    fn command_uses_explicit_path_when_set() {
        let l = launcher(serde_json::json!({ "command_path": "/opt/bin/goose" }));
        assert_eq!(l.command(), "/opt/bin/goose");
    }

    #[test]
    fn validate_command_falls_back_to_path_for_bare_command_name() {
        let l = launcher(serde_json::json!({ "command_path": "ls" }));
        assert!(l.validate_command().is_ok());
    }

    // -- metadata / schema -----------------------------------------------------

    #[test]
    fn metadata_name_is_goose_cli() {
        let meta = GooseLauncher::metadata();
        assert_eq!(meta.name, "Goose CLI");
        assert_eq!(meta.default_command, "goose");
        assert!(
            meta.supported_capabilities
                .contains(&BindingType::AgentModel)
        );
        assert!(meta.supported_capabilities.contains(&BindingType::Mcp));
    }

    #[test]
    fn instance_id_round_trips_from_construction() {
        let l = GooseLauncher::new("goose-local", &serde_json::json!({})).unwrap();
        assert_eq!(l.instance_id(), "goose-local");
    }

    #[test]
    fn config_schema_exposes_command_path() {
        use crate::launchers::base::LauncherFactory;
        let mut factory = LauncherFactory::new();
        factory.register::<GooseLauncher>("goose");
        let schema = factory.config_schema("goose").unwrap();
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap();
        assert!(props.contains_key("command_path"));
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
    async fn env_overlay_always_uses_openai_as_goose_provider() {
        let overlay = bound(serde_json::json!({}), binding())
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        let provider = overlay
            .iter()
            .find(|b| b.key == "GOOSE_PROVIDER")
            .expect("GOOSE_PROVIDER env");
        // Not the granite-cli provider name ("my-ollama") -- goose only
        // knows its own fixed built-in provider ids.
        assert_eq!(provider.value, "openai");
        let model = overlay
            .iter()
            .find(|b| b.key == "GOOSE_MODEL")
            .expect("GOOSE_MODEL env");
        assert_eq!(model.value, "granite4.1:8b");
    }

    #[tokio::test]
    async fn env_overlay_sets_openai_host_for_base_url() {
        let overlay = bound(serde_json::json!({}), binding())
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        let host = overlay
            .iter()
            .find(|b| b.key == "OPENAI_HOST")
            .expect("OPENAI_HOST env");
        assert_eq!(host.value, "http://localhost:11434");
    }

    #[tokio::test]
    async fn env_overlay_omits_base_path_for_default_operation_path() {
        let overlay = bound(serde_json::json!({}), binding())
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        assert!(!overlay.iter().any(|b| b.key == "OPENAI_BASE_PATH"));
    }

    #[tokio::test]
    async fn env_overlay_sets_base_path_for_nonstandard_operation_path() {
        let b = AgentModelBinding {
            endpoint_path: "/v1/responses".to_string(),
            ..binding()
        };
        let overlay = bound(serde_json::json!({}), b)
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        let path = overlay
            .iter()
            .find(|b| b.key == "OPENAI_BASE_PATH")
            .expect("OPENAI_BASE_PATH env");
        assert_eq!(path.value, "v1/responses");
    }

    #[tokio::test]
    async fn env_overlay_sets_goose_context_limit() {
        let overlay = bound(serde_json::json!({}), binding())
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        let limit = overlay
            .iter()
            .find(|b| b.key == "GOOSE_CONTEXT_LIMIT")
            .expect("GOOSE_CONTEXT_LIMIT env");
        assert_eq!(limit.value, "131072");
    }

    #[tokio::test]
    async fn env_overlay_uses_placeholder_api_key_when_none_configured() {
        let overlay = bound(serde_json::json!({}), binding())
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        let key = overlay
            .iter()
            .find(|b| b.key == "OPENAI_API_KEY")
            .expect("OPENAI_API_KEY env is required by goose even for local servers");
        assert_eq!(key.value, "granite-cli");
    }

    #[tokio::test]
    async fn env_overlay_includes_custom_headers() {
        let mut b = binding();
        let mut headers = HashMap::new();
        headers.insert(
            "X-Custom-Header".to_string(),
            crate::registry::Secret::from("value1"),
        );
        headers.insert(
            "User-Agent".to_string(),
            crate::registry::Secret::from("my-agent/1.0"),
        );
        b.custom_headers = Some(headers);
        let l = bound(serde_json::json!({}), b);
        let overlay = l.env_overlay(&ctx(false)).await.unwrap();
        let headers_entry = overlay
            .iter()
            .find(|b| b.key == "OPENAI_CUSTOM_HEADERS")
            .expect("OPENAI_CUSTOM_HEADERS env");
        // Headers are formatted as "Name=Value" pairs, comma-separated, sorted by key.
        assert_eq!(
            headers_entry.value,
            "User-Agent=my-agent/1.0,X-Custom-Header=value1"
        );
    }

    #[tokio::test]
    async fn env_overlay_omits_custom_headers_when_none_set() {
        let l = bound(serde_json::json!({}), binding());
        let overlay = l.env_overlay(&ctx(false)).await.unwrap();
        let headers_entry = overlay.iter().find(|b| b.key == "OPENAI_CUSTOM_HEADERS");
        assert!(headers_entry.is_none());
    }

    #[tokio::test]
    async fn env_overlay_omits_custom_headers_when_empty_map() {
        let mut b = binding();
        b.custom_headers = Some(HashMap::new());
        let l = bound(serde_json::json!({}), b);
        let overlay = l.env_overlay(&ctx(false)).await.unwrap();
        let headers_entry = overlay.iter().find(|b| b.key == "OPENAI_CUSTOM_HEADERS");
        assert!(headers_entry.is_none());
    }

    #[tokio::test]
    async fn env_overlay_uses_real_api_key_when_configured() {
        let b = AgentModelBinding {
            api_key: Some(crate::registry::Secret::from("sk-test")),
            ..binding()
        };
        let overlay = bound(serde_json::json!({}), b)
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        let key = overlay.iter().find(|b| b.key == "OPENAI_API_KEY").unwrap();
        assert_eq!(key.value, "sk-test");
    }

    // -- MCP bindings ------------------------------------------------------------

    #[tokio::test]
    async fn launch_prepends_with_extension_for_stdio_mcp_binding() {
        let l = with_mcp(
            serde_json::json!({ "command_path": "ls" }),
            "vision",
            McpBinding::Stdio {
                command: "granite-cli".to_string(),
                args: vec!["__mcp-serve".to_string(), "vision".to_string()],
                env: std::collections::HashMap::from([("FOO".to_string(), "bar".to_string())]),
                timeout: None,
            },
        );
        let ui = CaptureUi::default();
        l.launch(&["--version".to_string()], &ctx(true), &ui)
            .await
            .unwrap();

        let infos = ui.infos.borrow();
        assert!(
            infos.iter().any(|m| {
                m.contains("--with-extension")
                    && m.contains("FOO=bar granite-cli __mcp-serve vision")
            }),
            "expected the stdio extension flag, got {infos:?}"
        );
        // Caller args still land after the extension flags.
        assert!(infos.iter().any(|m| m.trim_end().ends_with("--version")));
    }

    #[tokio::test]
    async fn launch_uses_streamable_http_flag_for_http_mcp_binding() {
        let l = with_mcp(
            serde_json::json!({ "command_path": "ls" }),
            "vision",
            McpBinding::Http {
                url: "http://127.0.0.1:54321/mcp".to_string(),
                headers: Default::default(),
                timeout: None,
            },
        );
        let ui = CaptureUi::default();
        l.launch(&[], &ctx(true), &ui).await.unwrap();

        let infos = ui.infos.borrow();
        assert!(
            infos.iter().any(|m| {
                m.contains("--with-streamable-http-extension")
                    && m.contains("http://127.0.0.1:54321/mcp")
            }),
            "expected the streamable-http extension flag, got {infos:?}"
        );
    }

    #[tokio::test]
    async fn launch_warns_and_drops_headers_for_http_mcp_binding() {
        let l = with_mcp(
            serde_json::json!({ "command_path": "ls" }),
            "vision",
            McpBinding::Http {
                url: "http://127.0.0.1:54321/mcp".to_string(),
                headers: std::collections::HashMap::from([(
                    "Authorization".to_string(),
                    "Bearer x".to_string(),
                )]),
                timeout: None,
            },
        );
        let ui = CaptureUi::default();
        l.launch(&[], &ctx(true), &ui).await.unwrap();

        let warns = ui.warns.borrow();
        assert!(
            warns.iter().any(|m| m.contains("headers")),
            "expected a warning about dropped headers, got {warns:?}"
        );
    }

    // -- extension flag insertion -----------------------------------------------

    #[test]
    fn insert_extension_flags_returns_args_unchanged_when_no_flags() {
        let args = vec!["session".to_string(), "--resume".to_string()];
        let out = insert_extension_flags(&args, vec![], &CaptureUi::default());
        assert_eq!(out, args);
    }

    #[test]
    fn insert_extension_flags_inserts_session_when_args_are_empty() {
        let out = insert_extension_flags(
            &[],
            vec!["--with-extension".to_string(), "foo".to_string()],
            &CaptureUi::default(),
        );
        assert_eq!(out, vec!["session", "--with-extension", "foo"]);
    }

    #[test]
    fn insert_extension_flags_inserts_session_ahead_of_non_subcommand_args() {
        // Mirrors the bug this fixes: bare `goose --version` (no subcommand)
        // used to get `--with-extension`/`--with-streamable-http-extension`
        // spliced in ahead of it with no subcommand at all, which goose's
        // clap parser rejects outright ("unexpected argument").
        let out = insert_extension_flags(
            &["--version".to_string()],
            vec!["--with-extension".to_string(), "foo".to_string()],
            &CaptureUi::default(),
        );
        assert_eq!(out, vec!["session", "--with-extension", "foo", "--version"]);
    }

    #[test]
    fn insert_extension_flags_inserts_after_explicit_session_subcommand() {
        let out = insert_extension_flags(
            &["session".to_string(), "--resume".to_string()],
            vec!["--with-extension".to_string(), "foo".to_string()],
            &CaptureUi::default(),
        );
        assert_eq!(out, vec!["session", "--with-extension", "foo", "--resume"]);
    }

    #[test]
    fn insert_extension_flags_inserts_after_explicit_run_subcommand() {
        let out = insert_extension_flags(
            &["run".to_string(), "-t".to_string(), "hi".to_string()],
            vec![
                "--with-streamable-http-extension".to_string(),
                "url".to_string(),
            ],
            &CaptureUi::default(),
        );
        assert_eq!(
            out,
            vec!["run", "--with-streamable-http-extension", "url", "-t", "hi"]
        );
    }

    #[test]
    fn insert_extension_flags_warns_and_drops_for_non_extension_capable_subcommand() {
        let ui = CaptureUi::default();
        let out = insert_extension_flags(
            &["configure".to_string()],
            vec!["--with-extension".to_string(), "foo".to_string()],
            &ui,
        );
        assert_eq!(out, vec!["configure"]);
        let warns = ui.warns.borrow();
        assert!(
            warns.iter().any(|m| m.contains("configure")),
            "expected a warning naming the incompatible subcommand, got {warns:?}"
        );
    }

    // -- launch ---------------------------------------------------------------

    #[tokio::test]
    async fn launch_without_binding_passes_args_through_unchanged() {
        let l = launcher(serde_json::json!({ "command_path": "ls" }));
        let ui = CaptureUi::default();
        l.launch(&["--version".to_string()], &ctx(true), &ui)
            .await
            .unwrap();

        let infos = ui.infos.borrow();
        assert!(infos.iter().any(|m| m.contains("--version")));
        // Without a binding there is no GOOSE_* env override.
        assert!(!infos.iter().any(|m| m.contains("GOOSE_PROVIDER")));
    }
}
