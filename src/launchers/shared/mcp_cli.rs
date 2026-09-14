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

/// Request built by launchers that register MCP servers via the downstream
/// tool's own CLI: they can either hand it a stdio command (which the tool
/// spawns itself whenever it starts the server) or a Streamable HTTP URL.
pub(crate) fn mcp_binding_request() -> BindingRequest {
    BindingRequest::Mcp(McpBindingRequest {
        supported_transports: HashSet::from([McpTransportKind::Stdio, McpTransportKind::Http]),
    })
}

/// Runs `<binary> mcp add-json <name> <json> <scope_args...>`, or reports
/// what it would run under `--dry-run` instead of executing.
///
/// Always does a best-effort remove first so that a stale entry left by a
/// previously crashed or suspended session (where `remove_mcp_server` never
/// ran) doesn't cause `add-json` to fail with "already exists".
pub(crate) fn register_mcp_server(
    binary: &Path,
    name: &str,
    binding: &McpBinding,
    scope_args: &[&str],
    ctx: &LaunchContext,
    ui: &dyn Ui,
) -> anyhow::Result<()> {
    // Best-effort: ignore failure — the server simply may not exist yet.
    remove_mcp_server(binary, name, scope_args, ctx, ui);

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
