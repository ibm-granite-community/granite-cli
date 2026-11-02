//! `McpServerCapability`: registers any user-supplied MCP server with a
//! launched coding agent.
//!
//! This is a configuration passthrough. The config mirrors the standard MCP
//! server entry (`command`/`args`/`env` for stdio, `url`/`headers` for
//! Streamable HTTP or SSE) and `bind()` hands it back as an [`McpBinding`];
//! each launcher renders that into its own native format. granite-cli runs
//! no server of its own and never performs an OAuth flow. A server that
//! uses OAuth needs only its `url`: the launcher's own client discovers the
//! authorization server and runs the flow.

use crate::capabilities::base::{
    Binding, BindingRequest, BindingType, Capability, CapabilityInfo, CapabilityMetadata,
    HasCapabilityMetadata, McpBinding, McpBindingRequest, McpTransportKind,
};
use crate::registry::{ConfigConstructable, ConstructError, Secret};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/*-- public ----------------------------------------------------------------------*/

/// Wire transport of the registered server. Accepts the spellings other MCP
/// clients use for Streamable HTTP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum McpServerType {
    Stdio,
    #[serde(alias = "streamable-http", alias = "streamableHttp")]
    Http,
    Sse,
}

impl McpServerType {
    fn kind(self) -> McpTransportKind {
        match self {
            McpServerType::Stdio => McpTransportKind::Stdio,
            McpServerType::Http => McpTransportKind::Http,
            McpServerType::Sse => McpTransportKind::Sse,
        }
    }
}

/// A standard MCP server entry. The instance id is the server name.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpServerCapabilityConfig {
    /// Transport: `stdio`, `http` (Streamable HTTP) or `sse`.
    #[serde(rename = "type")]
    pub(crate) server_type: McpServerType,
    /// Executable to spawn (stdio only).
    #[serde(default)]
    pub(crate) command: Option<String>,
    /// Arguments for `command` (stdio only).
    #[serde(default)]
    pub(crate) args: Vec<String>,
    /// Environment for the spawned process (stdio only).
    #[serde(default)]
    pub(crate) env: HashMap<String, Secret>,
    /// Server endpoint (http and sse only).
    #[serde(default)]
    pub(crate) url: Option<String>,
    /// Extra request headers, e.g. `Authorization` (http and sse only).
    #[serde(default)]
    pub(crate) headers: HashMap<String, Secret>,
    /// Request timeout in milliseconds.
    #[serde(default)]
    pub(crate) timeout: Option<u64>,
}

impl Default for McpServerCapabilityConfig {
    fn default() -> Self {
        Self {
            server_type: McpServerType::Stdio,
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            timeout: None,
        }
    }
}

pub(crate) struct McpServerCapability {
    instance_id: String,
    config: McpServerCapabilityConfig,
}

/// [`McpServerCapability`] has no references to resolve, so this only marks
/// that `resolve_refs` ran.
pub(crate) struct ResolvedMcpServerCapability {
    inner: McpServerCapability,
}

impl ConfigConstructable for McpServerCapability {
    type Config = McpServerCapabilityConfig;

    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        let mut config: McpServerCapabilityConfig =
            serde_json::from_value(cfg.clone()).map_err(ConstructError::settings)?;
        normalize(&mut config);
        validate(&config).map_err(ConstructError::settings)?;
        Ok(Self {
            instance_id: instance_id.to_string(),
            config,
        })
    }
}

impl crate::registry::Named for McpServerCapability {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

impl crate::registry::Named for ResolvedMcpServerCapability {
    fn instance_id(&self) -> &str {
        self.inner.instance_id()
    }
}

impl CapabilityInfo for McpServerCapability {
    fn binding_types(&self) -> HashSet<BindingType> {
        HashSet::from([BindingType::Mcp])
    }

    fn mcp_transport(&self) -> Option<McpTransportKind> {
        Some(self.config.server_type.kind())
    }
}

impl CapabilityInfo for ResolvedMcpServerCapability {
    fn binding_types(&self) -> HashSet<BindingType> {
        self.inner.binding_types()
    }

    fn mcp_transport(&self) -> Option<McpTransportKind> {
        self.inner.mcp_transport()
    }
}

impl Capability for McpServerCapability {
    fn resolve_refs(
        self: Box<Self>,
        _models: &dyn crate::models::ModelLookup,
    ) -> anyhow::Result<Box<dyn crate::capabilities::ResolvedCapability>> {
        Ok(Box::new(ResolvedMcpServerCapability { inner: *self }))
    }
}

#[async_trait]
impl crate::capabilities::ResolvedCapability for ResolvedMcpServerCapability {
    async fn bind(&self, request: BindingRequest) -> anyhow::Result<Binding> {
        let McpBindingRequest {
            supported_transports,
        } = match request {
            BindingRequest::Mcp(request) => request,
            other => anyhow::bail!(
                "McpServerCapability does not handle {:?} binding requests",
                other.binding_type()
            ),
        };
        self.bind_with_env(&supported_transports, &|name| std::env::var(name).ok())
    }
}

impl HasCapabilityMetadata for McpServerCapability {
    fn metadata() -> CapabilityMetadata {
        CapabilityMetadata {
            name: "MCP Server".to_string(),
            description: DESCRIPTION.to_string(),
            dependencies: vec![],
            tags: vec!["mcp".to_string(), "tools".to_string()],
            supported_binding_types: HashSet::from([BindingType::Mcp]),
            auto_configurable: false,
        }
    }
}

/*-- private ---------------------------------------------------------------------*/

const DESCRIPTION: &str =
    "Registers any MCP server (stdio, Streamable HTTP or SSE) with a launched coding agent.";

impl ResolvedMcpServerCapability {
    /// `bind` with the environment lookup injected, so `${VAR}` expansion is
    /// testable without touching the process environment.
    fn bind_with_env(
        &self,
        supported: &HashSet<McpTransportKind>,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> anyhow::Result<Binding> {
        let cfg = &self.inner.config;
        let kind = cfg.server_type.kind();
        anyhow::ensure!(
            supported.contains(&kind),
            "MCP server '{}' uses the {kind:?} transport, but the launcher can only register {supported:?}",
            self.inner.instance_id
        );

        let expand_all = |value: &str| expand(value, lookup);
        let expand_map =
            |map: &HashMap<String, Secret>| -> anyhow::Result<HashMap<String, String>> {
                map.iter()
                    .map(|(k, v)| Ok((k.clone(), expand_all(&v.0)?)))
                    .collect()
            };

        let binding = match cfg.server_type {
            McpServerType::Stdio => McpBinding::Stdio {
                // `validate` guarantees `command` is present for stdio.
                command: expand_all(cfg.command.as_deref().unwrap_or_default())?,
                args: cfg
                    .args
                    .iter()
                    .map(|a| expand_all(a))
                    .collect::<anyhow::Result<_>>()?,
                env: expand_map(&cfg.env)?,
                timeout: cfg.timeout,
            },
            McpServerType::Http | McpServerType::Sse => {
                let url = expand_all(cfg.url.as_deref().unwrap_or_default())?;
                let headers = expand_map(&cfg.headers)?;
                if cfg.server_type == McpServerType::Http {
                    McpBinding::Http {
                        url,
                        headers,
                        timeout: cfg.timeout,
                    }
                } else {
                    McpBinding::Sse {
                        url,
                        headers,
                        timeout: cfg.timeout,
                    }
                }
            }
        };
        Ok(Binding::Mcp(binding))
    }
}

/// Drops a `timeout` of `0`, which is how the setup prompt writes an optional
/// number it was not given (it treats an `Option<T>` as a `T`). A 0 ms timeout
/// means nothing, and would otherwise reach the launcher as a real value.
fn normalize(cfg: &mut McpServerCapabilityConfig) {
    if cfg.timeout == Some(0) {
        cfg.timeout = None;
    }
}

/// Checks the fields that only make sense for one side of the stdio/remote
/// split, so a mistyped entry fails at setup rather than at launch.
fn validate(cfg: &McpServerCapabilityConfig) -> Result<(), String> {
    match cfg.server_type {
        McpServerType::Stdio => {
            if cfg.command.as_deref().is_none_or(|c| c.trim().is_empty()) {
                return Err("command: required for the stdio transport".into());
            }
            for (field, present) in [
                ("url", cfg.url.is_some()),
                ("headers", !cfg.headers.is_empty()),
            ] {
                if present {
                    return Err(format!("{field}: not valid for the stdio transport"));
                }
            }
        }
        McpServerType::Http | McpServerType::Sse => {
            let Some(url) = cfg.url.as_deref().filter(|u| !u.trim().is_empty()) else {
                return Err("url: required for the http and sse transports".into());
            };
            // A `${VAR}` reference can stand in for the scheme or host, so
            // only fully literal URLs are checked.
            if !url.contains("${") && !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err("url: must start with http:// or https://".into());
            }
            for (field, present) in [
                ("command", cfg.command.is_some()),
                ("args", !cfg.args.is_empty()),
                ("env", !cfg.env.is_empty()),
            ] {
                if present {
                    return Err(format!(
                        "{field}: not valid for the http and sse transports"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Expands `${VAR}` and `${VAR:-default}`. An unset variable with no default
/// is an error naming it, rather than passing a literal `${VAR}` to a server
/// that would then fail in a harder-to-trace way. Text without `${` (including
/// a bare `$`) is untouched.
fn expand(input: &str, lookup: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| anyhow::anyhow!("unterminated '${{' in '{input}'"))?;
        let expr = &after[..end];
        let (name, default) = match expr.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (expr, None),
        };
        match lookup(name)
            .filter(|v| !v.is_empty())
            .or(default.map(str::to_string))
        {
            Some(value) => out.push_str(&value),
            None => anyhow::bail!("environment variable '{name}' is not set and has no default"),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/*-- tests -----------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::ResolvedCapability;
    use serde_json::json;

    fn build(cfg: serde_json::Value) -> Result<ResolvedMcpServerCapability, ConstructError> {
        McpServerCapability::new("srv", &cfg).map(|inner| ResolvedMcpServerCapability { inner })
    }

    fn all() -> HashSet<McpTransportKind> {
        HashSet::from([
            McpTransportKind::Stdio,
            McpTransportKind::Http,
            McpTransportKind::Sse,
        ])
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn bound(cap: &ResolvedMcpServerCapability) -> McpBinding {
        match cap.bind_with_env(&all(), &no_env).unwrap() {
            Binding::Mcp(b) => b,
            other => panic!("expected Mcp, got {other:?}"),
        }
    }

    #[test]
    fn stdio_entry_binds_to_stdio() {
        let cap = build(json!({
            "type": "stdio", "command": "npx", "args": ["-y", "srv"],
            "env": {"TOKEN": "abc"}, "timeout": 5000
        }))
        .unwrap();
        let McpBinding::Stdio {
            command,
            args,
            env,
            timeout,
        } = bound(&cap)
        else {
            panic!("expected Stdio");
        };
        assert_eq!(command, "npx");
        assert_eq!(args, ["-y", "srv"]);
        assert_eq!(env["TOKEN"], "abc");
        assert_eq!(timeout, Some(5000));
    }

    #[test]
    fn http_entry_carries_headers() {
        let cap = build(json!({
            "type": "http", "url": "https://x.example/mcp",
            "headers": {"Authorization": "Bearer t"}
        }))
        .unwrap();
        let McpBinding::Http { url, headers, .. } = bound(&cap) else {
            panic!("expected Http");
        };
        assert_eq!(url, "https://x.example/mcp");
        assert_eq!(headers["Authorization"], "Bearer t");
    }

    #[test]
    fn sse_entry_binds_to_sse() {
        let cap = build(json!({"type": "sse", "url": "http://localhost:1/sse"})).unwrap();
        assert!(matches!(bound(&cap), McpBinding::Sse { .. }));
    }

    #[test]
    fn streamable_http_spellings_are_accepted_for_http() {
        for spelling in ["http", "streamable-http", "streamableHttp"] {
            let cap = build(json!({"type": spelling, "url": "https://x.example/mcp"})).unwrap();
            assert!(matches!(bound(&cap), McpBinding::Http { .. }), "{spelling}");
        }
    }

    #[test]
    fn construction_rejects_fields_from_the_other_transport() {
        let cases = [
            (json!({"type": "stdio"}), "command"),
            (
                json!({"type": "stdio", "command": "x", "url": "http://a"}),
                "url",
            ),
            (
                json!({"type": "stdio", "command": "x", "headers": {"A": "b"}}),
                "headers",
            ),
            (json!({"type": "http"}), "url"),
            (json!({"type": "http", "url": "ftp://a"}), "http://"),
            (
                json!({"type": "sse", "url": "http://a", "command": "x"}),
                "command",
            ),
            (
                json!({"type": "sse", "url": "http://a", "env": {"A": "b"}}),
                "env",
            ),
        ];
        for (cfg, expect) in cases {
            let err = build(cfg.clone())
                .err()
                .expect("should be rejected")
                .to_string();
            assert!(err.contains(expect), "{cfg} -> {err}");
        }
    }

    #[test]
    fn construction_rejects_unknown_fields_by_name() {
        let err = build(json!({"type": "sse", "url": "http://a", "bogus": 1}))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("bogus"), "{err}");
    }

    #[tokio::test]
    async fn bind_errors_when_launcher_cannot_register_the_transport() {
        let cap = build(json!({"type": "sse", "url": "http://a"})).unwrap();
        let err = cap
            .bind(BindingRequest::Mcp(McpBindingRequest {
                supported_transports: HashSet::from([McpTransportKind::Stdio]),
            }))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("srv") && err.contains("Sse") && err.contains("Stdio"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn bind_rejects_non_mcp_requests() {
        let cap = build(json!({"type": "sse", "url": "http://a"})).unwrap();
        let err = cap
            .bind(BindingRequest::AgentModel(
                crate::capabilities::base::AgentModelBindingRequest {
                    api_type: crate::providers::ApiType::OpenAI,
                },
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not handle"));
    }

    #[test]
    fn env_references_expand_in_every_field() {
        let cap = build(json!({
            "type": "http", "url": "https://${HOST}/mcp",
            "headers": {"Authorization": "Bearer ${TOKEN}"}
        }))
        .unwrap();
        let lookup = |n: &str| match n {
            "HOST" => Some("h.example".to_string()),
            "TOKEN" => Some("t0k".to_string()),
            _ => None,
        };
        let Binding::Mcp(McpBinding::Http { url, headers, .. }) =
            cap.bind_with_env(&all(), &lookup).unwrap()
        else {
            panic!("expected Http");
        };
        assert_eq!(url, "https://h.example/mcp");
        assert_eq!(headers["Authorization"], "Bearer t0k");
    }

    #[test]
    fn expand_uses_defaults_and_fails_on_unset_without_one() {
        assert_eq!(expand("a${X:-fallback}b", &no_env).unwrap(), "afallbackb");
        assert_eq!(expand("price $5", &no_env).unwrap(), "price $5");
        let err = expand("${MISSING}", &no_env).unwrap_err().to_string();
        assert!(err.contains("MISSING"), "{err}");
        assert!(expand("${oops", &no_env).is_err());
    }

    #[test]
    fn prompt_filled_zero_timeout_is_treated_as_unset() {
        // What `capability setup` saves for a server whose optional timeout
        // was left alone (the schema prompt fills an `Option<T>` as a `T`).
        let cap = build(json!({
            "type": "http", "url": "http://127.0.0.1:9006/mcp",
            "args": [], "command": null, "env": {}, "headers": {}, "timeout": 0
        }))
        .unwrap();
        let McpBinding::Http { timeout, .. } = bound(&cap) else {
            panic!("expected Http");
        };
        assert_eq!(timeout, None);
    }

    #[test]
    fn a_real_timeout_survives_normalization() {
        let cap = build(json!({"type": "http", "url": "https://x.example/mcp", "timeout": 3000}))
            .unwrap();
        let McpBinding::Http { timeout, .. } = bound(&cap) else {
            panic!("expected Http");
        };
        assert_eq!(timeout, Some(3000));
    }

    #[test]
    fn oauth_is_not_a_setting() {
        // OAuth servers need only their url; the launcher's client runs the
        // flow. The field is rejected rather than silently ignored.
        let err = build(json!({
            "type": "http", "url": "https://x.example/mcp", "oauth": {"client_id": "abc"}
        }))
        .err()
        .expect("oauth is not a field")
        .to_string();
        assert!(err.contains("oauth"), "{err}");
    }

    #[test]
    fn binding_types_reports_mcp() {
        let cap = build(json!({"type": "sse", "url": "http://a"})).unwrap();
        assert_eq!(cap.binding_types(), HashSet::from([BindingType::Mcp]));
    }
}
