//! Shared helpers for launchers that register MCP servers by shelling out to
//! the downstream tool's own `mcp add-json`/`mcp remove` CLI (`claude`,
//! `bob`) rather than writing into the tool's persistent config -- this
//! keeps registration and removal provably scoped to a single invocation.

use crate::capabilities::{BindingRequest, McpBinding, McpBindingRequest, McpTransportKind};
use crate::launchers::base::LaunchContext;
use crate::utils::ui::Ui;
use anyhow::Context;
use std::collections::HashSet;
use std::path::Path;

/// Transports a launcher that can hand the downstream tool a stdio command
/// or a Streamable HTTP URL, but has not been confirmed to speak SSE.
pub(crate) const STDIO_AND_HTTP: &[McpTransportKind] =
    &[McpTransportKind::Stdio, McpTransportKind::Http];

/// Transports for launchers whose downstream tool also takes SSE servers.
pub(crate) const STDIO_HTTP_AND_SSE: &[McpTransportKind] = &[
    McpTransportKind::Stdio,
    McpTransportKind::Http,
    McpTransportKind::Sse,
];

/// The set form of a transport list, as stored in `LauncherMetadata`.
pub(crate) fn transport_set(transports: &[McpTransportKind]) -> HashSet<McpTransportKind> {
    transports.iter().copied().collect()
}

/// Request built by launchers to bind MCP servers: `transports` is exactly
/// what that launcher can register with its downstream tool (its metadata's
/// `mcp_transports`), so a capability holding a server it cannot register
/// fails with a clear error instead of being mis-registered.
pub(crate) fn mcp_binding_request(transports: &HashSet<McpTransportKind>) -> BindingRequest {
    BindingRequest::Mcp(McpBindingRequest {
        supported_transports: transports.clone(),
    })
}

/// Runs `<binary> mcp add-json <name> <json> <scope_args...>`, or reports
/// what it would run under `--dry-run` instead of executing.
pub(crate) fn register_mcp_server(
    binary: &Path,
    name: &str,
    binding: &McpBinding,
    scope_args: &[&str],
    ctx: &LaunchContext,
    ui: &dyn Ui,
) -> anyhow::Result<()> {
    let json = binding.to_canonical_json().to_string();
    let mut args = vec![
        "mcp".to_string(),
        "add-json".to_string(),
        name.to_string(),
        json,
    ];
    args.extend(scope_args.iter().map(|s| s.to_string()));
    run_mcp_cli(binary, &args, "register", ctx, ui)
}

/// Runs `<binary> mcp remove <name> <scope_args...>`, best-effort: a failure
/// is reported to the user but does not fail the launch, since by the time
/// this runs the launch itself has already succeeded or failed on its own
/// terms.
pub(crate) fn remove_mcp_server(
    binary: &Path,
    name: &str,
    scope_args: &[&str],
    ctx: &LaunchContext,
    ui: &dyn Ui,
) {
    let mut args = vec!["mcp".to_string(), "remove".to_string(), name.to_string()];
    args.extend(scope_args.iter().map(|s| s.to_string()));
    if let Err(e) = run_mcp_cli(binary, &args, "remove", ctx, ui) {
        ui.warn(&format!("Failed to remove MCP server '{name}': {e}"));
    }
}

fn run_mcp_cli(
    binary: &Path,
    args: &[String],
    verb: &str,
    ctx: &LaunchContext,
    ui: &dyn Ui,
) -> anyhow::Result<()> {
    if ctx.dry_run {
        ui.info(&format!(
            "Would run: {} {}",
            binary.display(),
            args.join(" ")
        ));
        return Ok(());
    }
    let status = std::process::Command::new(binary)
        .args(args)
        .status()
        .with_context(|| {
            format!(
                "failed to {verb} MCP server via `{} {}`",
                binary.display(),
                args.join(" ")
            )
        })?;
    anyhow::ensure!(
        status.success(),
        "`{} {}` exited with status {}",
        binary.display(),
        args.join(" "),
        status
    );
    Ok(())
}

/*-- tests -----------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    fn offered(transports: &[McpTransportKind]) -> HashSet<McpTransportKind> {
        match mcp_binding_request(&transport_set(transports)) {
            BindingRequest::Mcp(request) => request.supported_transports,
            other => panic!("expected an Mcp request, got {other:?}"),
        }
    }

    #[test]
    fn request_offers_exactly_the_given_transports() {
        assert_eq!(
            offered(STDIO_AND_HTTP),
            HashSet::from([McpTransportKind::Stdio, McpTransportKind::Http])
        );
        assert_eq!(
            offered(STDIO_HTTP_AND_SSE),
            HashSet::from([
                McpTransportKind::Stdio,
                McpTransportKind::Http,
                McpTransportKind::Sse
            ])
        );
    }
}
