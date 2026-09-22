//! Launcher for the `pi` coding agent harness (<https://pi.dev>).
//!
//! Unlike `claude`, Pi has no environment variable for the model endpoint: a
//! non-built-in provider must be declared in Pi's `models.json`, then selected
//! with `--provider`/`--model`. Rather than edit the user's real Pi config, this
//! launcher builds its own Pi config directory under `GRANITE_CLI_HOME` --
//! `models.json` written fresh from the bound model, everything else linked
//! through from the user's directory -- and points the child process at it with
//! `PI_CODING_AGENT_DIR`.

use crate::capabilities::{AgentModelBinding, BindingType, ResolvedCapability, ToolName};
use crate::launchers::base::{
    EnvBinding, LaunchContext, Launcher, LauncherMetadata, run_command, run_command_captured,
};
use crate::providers::ApiType;
use crate::registry::{ConfigConstructable, ConstructError};
use crate::utils::resolve_shell_command;
use crate::utils::ui::Ui;
use alog::{MessageLevel, alog_channel, use_channel};
use anyhow::Context;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use_channel!("LNCHR");

/*-- public --*/

#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct PiLauncherConfig {
    /// Override path to the `pi` binary for non-PATH installs.
    /// Leave unset to use PATH lookup.
    #[serde(default)]
    pub command_path: Option<String>,

    /// Extra keys merged (shallow, last-write-wins) into the generated Pi
    /// provider entry -- e.g. `compat` flags or other provider-level
    /// configuration a particular server needs. Necessary because the entry
    /// is regenerated on every launch.
    #[serde(default)]
    pub provider_overrides: Option<serde_json::Value>,
}

pub struct PiLauncher {
    instance_id: String,
    config: PiLauncherConfig,
    bound_binding: Option<AgentModelBinding>,
}

impl ConfigConstructable for PiLauncher {
    type Config = PiLauncherConfig;

    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        let config: PiLauncherConfig =
            serde_json::from_value(cfg.clone()).map_err(ConstructError::settings)?;
        Ok(Self {
            instance_id: instance_id.to_string(),
            config,
            bound_binding: None,
        })
    }
}

impl crate::registry::Named for PiLauncher {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait]
impl Launcher for PiLauncher {
    fn name(&self) -> &str {
        "Pi CLI"
    }

    fn command(&self) -> &str {
        self.config.command_path.as_deref().unwrap_or("pi")
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

        // Pi speaks several API dialects, but `openai-completions` is the one
        // every granite-cli provider can serve and the one Pi documents as most
        // compatible, so that is what we ask the capability for.
        let request = crate::capabilities::BindingRequest::AgentModel(
            crate::capabilities::AgentModelBindingRequest {
                api_type: ApiType::OpenAI,
            },
        );

        let binding = capability.bind(request).await?;
        match binding {
            crate::capabilities::Binding::AgentModel(binding) => {
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
        resolve_shell_command(&self.config.command_path, "pi")
    }

    /// Redirects Pi at the granite-cli-owned config directory and supplies the
    /// credential the generated `models.json` interpolates.
    ///
    /// The provider entry's `apiKey` is written as an environment reference
    /// (`$GRANITE_CLI_PI_API_KEY`) rather than a literal, so the secret stays out
    /// of the generated file and off Pi's command line. Pi only treats a model as
    /// selectable once *some* credential resolves, so providers with no key get a
    /// placeholder -- local servers ignore it.
    async fn env_overlay(&self, ctx: &LaunchContext) -> anyhow::Result<Vec<EnvBinding>> {
        let Some(binding) = &self.bound_binding else {
            return Ok(vec![]);
        };
        let api_key_val = binding
            .api_key
            .as_ref()
            .map(|api_key| api_key.0.clone())
            .filter(|key| !key.is_empty())
            .unwrap_or_else(|| PLACEHOLDER_API_KEY.to_string());
        Ok(vec![
            EnvBinding {
                key: CONFIG_DIR_ENV.to_string(),
                value: pi_state_dir(ctx)?.to_string_lossy().to_string(),
            },
            EnvBinding {
                key: API_KEY_ENV.to_string(),
                value: api_key_val,
            },
        ])
    }

    /// Materializes the granite-cli Pi config directory, then execs `pi` with the
    /// selection flags in front of the caller's arguments (Pi's own positional
    /// `@files`/messages must stay last).
    async fn launch(
        &self,
        args: &[String],
        ctx: &LaunchContext,
        ui: &dyn Ui,
    ) -> anyhow::Result<std::process::ExitStatus> {
        let binary = self.validate_command()?;
        let overlay = self.env_overlay(ctx).await?;
        alog_channel!(MessageLevel::Debug2, "Env Overlay: {:#?}", overlay);

        let mut pi_args = self.provider_prefix_args(ctx, ui).await?;
        pi_args.extend_from_slice(args);

        run_command(binary, &overlay, &pi_args, ctx, ui).await
    }
}

impl HasPiLauncherMetadata for PiLauncher {
    fn metadata() -> LauncherMetadata {
        LauncherMetadata {
            name: "Pi CLI".to_string(),
            description: "Pi terminal coding agent harness".to_string(),
            default_command: "pi".to_string(),
            supported_capabilities: HashSet::from([BindingType::AgentModel]),
            tags: vec!["pi".to_string(), "coding-agent".to_string()],
        }
    }
}

/*-- private --*/

// HasPiLauncherMetadata is the macro-generated trait; re-exported via mod.rs.
use crate::launchers::base::HasLauncherMetadata as HasPiLauncherMetadata;

/// Env var pointing Pi at a config directory other than `~/.pi/agent`.
const CONFIG_DIR_ENV: &str = "PI_CODING_AGENT_DIR";

/// Env var the generated provider entry interpolates its `apiKey` from.
const API_KEY_ENV: &str = "GRANITE_CLI_PI_API_KEY";

/// Stand-in credential for providers that need no auth. Pi hides models whose
/// provider has no resolvable key, so the value must be non-empty.
const PLACEHOLDER_API_KEY: &str = "granite-cli";

/// Pi's custom-model file, relative to its config directory.
const MODELS_JSON: &str = "models.json";

/// Pi's session store. Deliberately *not* linked through to the user's
/// directory: granite-cli launches keep their own history rather than writing
/// into the user's Pi state.
const SESSIONS_DIR: &str = "sessions";

impl PiLauncher {
    /// Builds the `models.json` provider entry describing the bound model.
    fn provider_entry(&self, binding: &AgentModelBinding) -> anyhow::Result<serde_json::Value> {
        let mut entry = serde_json::json!({
            "baseUrl": pi_base_url(binding),
            "api": pi_api_name(&binding.api_type)?,
            "apiKey": format!("${API_KEY_ENV}"),
            "models": [{
                "id": binding.model_name,
                "contextWindow": binding.context_length,
            }],
        });

        // Add custom headers from binding if present. They are merged at the
        // provider level in Pi's models.json.
        if let Some(ref headers) = binding.custom_headers {
            entry["headers"] = serde_json::to_value(headers)?;
        }

        // Shallow merge so a user override of e.g. `compat` doesn't clobber the
        // generated `baseUrl`/`models`, and vice versa.
        if let (Some(overrides), Some(target)) = (
            self.config
                .provider_overrides
                .as_ref()
                .and_then(serde_json::Value::as_object),
            entry.as_object_mut(),
        ) {
            for (key, value) in overrides {
                target.insert(key.clone(), value.clone());
            }
        }
        Ok(entry)
    }

    /// Materializes the granite-cli Pi config directory for the bound model (if
    /// any) and returns the `--provider`/`--model` args selecting it. Shared by
    /// `launch` and `run_delegated_task`, which both need Pi pointed at the same
    /// generated config before anything else on the command line.
    async fn provider_prefix_args(
        &self,
        ctx: &LaunchContext,
        ui: &dyn Ui,
    ) -> anyhow::Result<Vec<String>> {
        let mut pi_args: Vec<String> = vec![];
        if let Some(binding) = &self.bound_binding {
            let provider_name = &binding.provider_name;
            let entry = self.provider_entry(binding)?;
            let state_dir = pi_state_dir(ctx)?;
            let source_dir = pi_source_dir()?;

            if ctx.dry_run {
                ui.info(&format!(
                    "Would write Pi provider '{provider_name}' into {}:",
                    state_dir.join(MODELS_JSON).display()
                ));
                ui.info(&serde_json::to_string_pretty(&entry)?);
                ui.info(&format!(
                    "  (merged over {}, which is left unmodified)",
                    source_dir.join(MODELS_JSON).display()
                ));
            } else {
                materialize_pi_config(&state_dir, &source_dir, provider_name, entry, ui)?;
                ui.info(&format!(
                    "Wrote Pi provider '{provider_name}' to {}",
                    state_dir.join(MODELS_JSON).display()
                ));
            }
            pi_args.extend([
                "--provider".to_string(),
                provider_name.clone(),
                "--model".to_string(),
                binding.model_name.clone(),
            ]);
        }
        Ok(pi_args)
    }

    /// Runs `pi` headlessly for one delegated sub-agent task and returns its
    /// captured stdout. Requires a prior `bind_capability` call -- there is no
    /// sensible way to pick a model on the caller's behalf, so an unbound
    /// launcher here is a bug in the caller, not a runtime condition.
    pub(crate) async fn run_delegated_task(
        &self,
        binary: &Path,
        task: &str,
        system_prompt: &str,
        tools: &[ToolName],
        ctx: &LaunchContext,
        ui: &dyn Ui,
    ) -> anyhow::Result<String> {
        if self.bound_binding.is_none() {
            anyhow::bail!("run_delegated_task requires a bound model; call bind_capability first");
        }

        let mut full_args = self.provider_prefix_args(ctx, ui).await?;
        full_args.extend([
            "--print".to_string(),
            "--no-session".to_string(),
            "--system-prompt".to_string(),
            system_prompt.to_string(),
        ]);
        if let Some(csv) = tools_csv(tools) {
            full_args.push("--tools".to_string());
            full_args.push(csv);
        }
        full_args.push(task.to_string());

        let overlay = self.env_overlay(ctx).await?;
        let (status, stdout, stderr) =
            run_command_captured(binary.to_path_buf(), &overlay, &full_args, ctx).await?;
        if !status.success() {
            anyhow::bail!("pi exited with {status}: {stderr}");
        }
        Ok(stdout.trim_end().to_string())
    }
}

/// Maps a canonical `ToolName` onto Pi's own built-in tool-name strings
/// (`read, bash, edit, write, grep, find, ls`, per Pi's documentation). Pi has
/// no web-fetch/web-search built-ins and no MCP tool-name convention of its
/// own to target, so those map to `None`; `Other` is the escape hatch and
/// passes through verbatim, matching `Launcher::map_tool_name`'s default.
pub(crate) fn pi_tool_name(tool: &ToolName) -> Option<String> {
    match tool {
        ToolName::FileRead => Some("read".to_string()),
        ToolName::Shell => Some("bash".to_string()),
        ToolName::FileEdit => Some("edit".to_string()),
        ToolName::FileWrite => Some("write".to_string()),
        ToolName::Search => Some("grep".to_string()),
        ToolName::FileSearch => Some("find".to_string()),
        ToolName::WebFetch | ToolName::WebSearch | ToolName::Mcp { .. } => None,
        ToolName::Other(raw) => Some(raw.clone()),
    }
}

/// Builds the `--tools` value for a delegated task: each tool mapped through
/// [`pi_tool_name`], unmapped ones dropped, comma-joined. `None` (rather than
/// `Some(String::new())`) when nothing maps, so the caller omits `--tools`
/// entirely instead of passing an empty allow-list -- Pi would read that as
/// "no tools" rather than "no restriction."
fn tools_csv(tools: &[ToolName]) -> Option<String> {
    let mapped: Vec<String> = tools.iter().filter_map(pi_tool_name).collect();
    if mapped.is_empty() {
        None
    } else {
        Some(mapped.join(","))
    }
}

/// The cached `pi` binary's filename within the download cache directory.
fn pi_binary_filename() -> &'static str {
    if cfg!(windows) { "pi.exe" } else { "pi" }
}

/// Maps a (`std::env::consts::OS`, `std::env::consts::ARCH`) pair onto the
/// release asset Pi publishes for it. Parameterized rather than reading
/// `std::env::consts` directly so the mapping is testable on every host.
fn pi_release_asset_name_for(os: &str, arch: &str) -> anyhow::Result<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Ok("pi-darwin-arm64.tar.gz"),
        ("macos", "x86_64") => Ok("pi-darwin-x64.tar.gz"),
        ("linux", "aarch64") => Ok("pi-linux-arm64.tar.gz"),
        ("linux", "x86_64") => Ok("pi-linux-x64.tar.gz"),
        ("windows", "x86_64") => Ok("pi-windows-x64.zip"),
        ("windows", "aarch64") => Ok("pi-windows-arm64.zip"),
        _ => anyhow::bail!("no pi release binary is published for {os}/{arch}"),
    }
}

/// [`pi_release_asset_name_for`] applied to the actual host.
fn pi_release_asset_name() -> anyhow::Result<&'static str> {
    pi_release_asset_name_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// Finds the checksum for `filename` in `sha256sum`-format text
/// (`<hex><whitespace><filename>` per line, filename optionally prefixed with
/// `*` for binary mode). Pure and network-free so the parsing logic is
/// testable without a live `SHA256SUMS` fetch.
fn find_sha256(sums_text: &str, filename: &str) -> Option<String> {
    sums_text.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        if name == filename {
            Some(hash.to_string())
        } else {
            None
        }
    })
}

/// Resolves the `pi` binary to run: an explicit override or `PATH` entry if
/// one resolves, otherwise a cached download under `cache_dir`, downloading
/// and checksum-verifying a fresh copy only if the cache is empty.
pub(crate) async fn ensure_pi_binary(
    override_path: &Option<String>,
    cache_dir: &Path,
    ui: &dyn Ui,
) -> anyhow::Result<PathBuf> {
    if let Ok(path) = resolve_shell_command(override_path, "pi") {
        return Ok(path);
    }

    if let Ok(cached) = locate_extracted_binary(cache_dir) {
        return Ok(cached);
    }

    let asset = pi_release_asset_name()?;
    ui.info(&format!("Downloading pi ({asset})..."));
    let bytes = reqwest::get(format!(
        "https://github.com/earendil-works/pi/releases/latest/download/{asset}"
    ))
    .await
    .with_context(|| format!("Failed to download {asset}"))?
    .error_for_status()
    .with_context(|| format!("Failed to download {asset}"))?
    .bytes()
    .await
    .with_context(|| format!("Failed to read downloaded {asset}"))?;

    let sums_text =
        reqwest::get("https://github.com/earendil-works/pi/releases/latest/download/SHA256SUMS")
            .await
            .context("Failed to download SHA256SUMS")?
            .error_for_status()
            .context("Failed to download SHA256SUMS")?
            .text()
            .await
            .context("Failed to read SHA256SUMS")?;

    ui.info("Verifying checksum...");
    let expected = find_sha256(&sums_text, asset)
        .ok_or_else(|| anyhow::anyhow!("SHA256SUMS has no entry for {asset}"))?;
    let actual = openssl::sha::sha256(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if !expected.eq_ignore_ascii_case(&actual) {
        anyhow::bail!("checksum mismatch for {asset}: expected {expected}, got {actual}");
    }

    ui.info("Extracting pi...");
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("Failed to create {}", cache_dir.display()))?;
    if asset.ends_with(".tar.gz") {
        let tar = flate2::read::GzDecoder::new(bytes.as_ref());
        tar::Archive::new(tar)
            .unpack(cache_dir)
            .with_context(|| format!("Failed to extract {asset}"))?;
    } else {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec()))
            .with_context(|| format!("Failed to open {asset} as a zip archive"))?;
        archive
            .extract(cache_dir)
            .with_context(|| format!("Failed to extract {asset}"))?;
    }

    let binary_path = locate_extracted_binary(cache_dir)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("Failed to mark {} executable", binary_path.display()))?;
    }

    ui.info(&format!("pi is ready at {}", binary_path.display()));
    Ok(binary_path)
}

/// Finds the `pi`/`pi.exe` binary within a just-extracted cache directory.
/// Checks the flat layout first, then falls back to a shallow scan (the
/// directory itself, then one level of subdirectories) for archives that
/// wrap their contents in a top-level directory.
fn locate_extracted_binary(cache_dir: &Path) -> anyhow::Result<PathBuf> {
    let name = pi_binary_filename();
    let flat = cache_dir.join(name);
    if flat.is_file() {
        return Ok(flat);
    }

    let mut subdirs = vec![];
    if let Ok(entries) = std::fs::read_dir(cache_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.file_name().is_some_and(|f| f == name) {
                return Ok(path);
            }
            if path.is_dir() {
                subdirs.push(path);
            }
        }
    }
    for dir in subdirs {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    anyhow::bail!(
        "could not find extracted '{name}' binary under {}",
        cache_dir.display()
    )
}

/// Maps a granite-cli `ApiType` onto Pi's `api` discriminator.
fn pi_api_name(api_type: &ApiType) -> anyhow::Result<&'static str> {
    match api_type {
        ApiType::OpenAI => Ok("openai-completions"),
        ApiType::Anthropic => Ok("anthropic-messages"),
        ApiType::Ollama => anyhow::bail!(
            "Pi has no Ollama-native API client; bind an OpenAI-compatible endpoint instead"
        ),
    }
}

/// Pi's `baseUrl` is the API root it appends the operation path to (e.g.
/// `/chat/completions`), so drop that trailing operation from the binding's
/// full endpoint path and keep the version prefix.
fn pi_base_url(binding: &AgentModelBinding) -> String {
    let root = binding.base_url.trim_end_matches('/');
    let prefix = match binding.api_type {
        ApiType::OpenAI => binding.endpoint_path.strip_suffix("/chat/completions"),
        ApiType::Anthropic => binding.endpoint_path.strip_suffix("/messages"),
        ApiType::Ollama => None,
    }
    .unwrap_or("");
    format!("{root}{prefix}")
}

/// The granite-cli-owned Pi config directory for this launcher instance.
fn pi_state_dir(ctx: &LaunchContext) -> anyhow::Result<PathBuf> {
    crate::config::Config::launcher_state_dir(&ctx.launcher_id)
}

/// The user's own Pi config directory, which we only ever read from:
/// `$PI_CODING_AGENT_DIR` when set, else `~/.pi/agent`.
fn pi_source_dir() -> anyhow::Result<PathBuf> {
    if let Ok(val) = std::env::var(CONFIG_DIR_ENV)
        && !val.is_empty()
    {
        return Ok(PathBuf::from(val));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory for Pi's config"))?;
    Ok(home.join(".pi").join("agent"))
}

/// Builds `state_dir` into a usable Pi config directory: a generated
/// `models.json` carrying `provider_name`, plus pass-through links to the rest of
/// the user's Pi resources. Nothing under `source_dir` is written.
fn materialize_pi_config(
    state_dir: &Path,
    source_dir: &Path,
    provider_name: &str,
    entry: serde_json::Value,
    ui: &dyn Ui,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("Failed to create {}", state_dir.display()))?;

    // A nested launch can land here with source == state (the parent already
    // redirected PI_CODING_AGENT_DIR at us); linking a directory into itself is
    // meaningless, and models.json is already ours.
    let nested = same_dir(state_dir, source_dir);
    if !nested {
        link_pass_through_resources(state_dir, source_dir, ui);
    }

    let mut root = read_json_object(&source_dir.join(MODELS_JSON))?;
    let malformed = |what: &str| {
        anyhow::anyhow!(
            "{} in {} is not a JSON object",
            what,
            source_dir.join(MODELS_JSON).display()
        )
    };
    root.as_object_mut()
        .ok_or_else(|| malformed("the top-level value"))?
        .entry("providers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| malformed("`providers`"))?
        .insert(provider_name.to_string(), entry);

    write_owned_json(&state_dir.join(MODELS_JSON), &root)
}

/// Reads a JSON object from `path`, treating a missing or blank file as `{}`.
/// A file that exists but does not parse is an error rather than something to
/// silently discard -- it is the user's own config.
fn read_json_object(path: &Path) -> anyhow::Result<serde_json::Value> {
    match std::fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => Ok(serde_json::json!({})),
        Ok(content) => serde_json::from_str(&content)
            .with_context(|| format!("{} is not valid JSON", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e).with_context(|| format!("Failed to read {}", path.display())),
    }
}

/// Writes a file granite-cli owns outright. Any symlink already at `path` is
/// removed first -- writing through one would land in whatever it points at,
/// which is exactly the user's file we are trying not to touch.
fn write_owned_json(path: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|md| md.file_type().is_symlink()) {
        std::fs::remove_file(path)
            .with_context(|| format!("Failed to replace symlink at {}", path.display()))?;
    }
    let mut content = serde_json::to_string_pretty(value)?;
    content.push('\n');
    std::fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))
}

/// Links every top-level entry of the user's Pi directory into `state_dir`, so
/// their settings, credentials, packages, extensions, skills and themes still
/// apply. `models.json` is skipped (granite-cli generates its own) and so is
/// the session store (see [`SESSIONS_DIR`]).
///
/// Best-effort: a platform or permission that refuses symlinks costs the user
/// those resources for granite-cli launches, not the launch itself.
fn link_pass_through_resources(state_dir: &Path, source_dir: &Path, ui: &dyn Ui) {
    let entries = match std::fs::read_dir(source_dir) {
        Ok(entries) => entries,
        // No Pi config of their own yet -- nothing to pass through.
        Err(_) => return,
    };

    let mut failed = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == MODELS_JSON || name == SESSIONS_DIR {
            continue;
        }
        // The link target must be absolute: a relative one resolves against the
        // *link's* directory, not ours, and would dangle.
        let Ok(target) = entry.path().canonicalize() else {
            failed += 1;
            continue;
        };
        let link = state_dir.join(&name);
        match std::fs::symlink_metadata(&link) {
            // Refresh our own link in case the target moved.
            Ok(md) if md.file_type().is_symlink() => {
                if std::fs::remove_file(&link).is_err() {
                    failed += 1;
                    continue;
                }
            }
            // Something real is sitting there; leave it be.
            Ok(_) => continue,
            Err(_) => {}
        }
        if symlink(&target, &link).is_err() {
            failed += 1;
        }
    }

    if failed > 0 {
        ui.warn(&format!(
            "Could not link {failed} Pi resource(s) from {} into {}; \
             settings, logins and extensions from there will not apply to this launch.",
            source_dir.display(),
            state_dir.display()
        ));
    }
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

/// Whether two paths denote the same directory, comparing canonical forms when
/// both exist and falling back to a literal comparison when they don't.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Named, Secret};
    use crate::utils::ui::base::tests::CaptureUi;

    fn launcher(cfg: serde_json::Value) -> PiLauncher {
        PiLauncher::new("pi", &cfg).unwrap()
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

    fn bound(cfg: serde_json::Value, binding: AgentModelBinding) -> PiLauncher {
        let mut l = launcher(cfg);
        l.bound_binding = Some(binding);
        l
    }

    fn ctx(dry_run: bool) -> LaunchContext {
        LaunchContext {
            launcher_id: "pi".to_string(),
            working_dir: PathBuf::from("/tmp"),
            base_env: std::collections::HashMap::new(),
            dry_run,
            usage_tracker: None,
            model_proxy: None,
        }
    }

    // -- command resolution ----------------------------------------------------

    #[test]
    fn command_defaults_to_pi() {
        assert_eq!(launcher(serde_json::json!({})).command(), "pi");
    }

    #[test]
    fn command_uses_explicit_path_when_set() {
        let l = launcher(serde_json::json!({ "command_path": "/opt/bin/pi" }));
        assert_eq!(l.command(), "/opt/bin/pi");
    }

    #[test]
    fn validate_command_err_for_nonexistent_explicit_path() {
        let l = launcher(serde_json::json!({ "command_path": "/no/such/path/pi" }));
        assert!(l.validate_command().is_err());
    }

    #[test]
    fn validate_command_falls_back_to_path_for_bare_command_name() {
        let l = launcher(serde_json::json!({ "command_path": "ls" }));
        assert!(l.validate_command().is_ok());
    }

    // -- metadata / schema -----------------------------------------------------

    #[test]
    fn metadata_name_is_pi_cli() {
        let meta = PiLauncher::metadata();
        assert_eq!(meta.name, "Pi CLI");
        assert_eq!(meta.default_command, "pi");
        assert!(
            meta.supported_capabilities
                .contains(&BindingType::AgentModel)
        );
    }

    #[test]
    fn instance_id_round_trips_from_construction() {
        let l = PiLauncher::new("pi-local", &serde_json::json!({})).unwrap();
        assert_eq!(l.instance_id(), "pi-local");
    }

    #[test]
    fn config_schema_exposes_command_path_overrides() {
        use crate::launchers::base::LauncherFactory;
        let mut factory = LauncherFactory::new();
        factory.register::<PiLauncher>("pi");
        let schema = factory.config_schema("pi").unwrap();
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap();
        assert!(props.contains_key("command_path"));
        assert!(props.contains_key("provider_overrides"));
        // The Pi provider name comes from the binding, never from launcher config.
        assert!(!props.contains_key("provider_name"));
    }

    // -- provider entry --------------------------------------------------------

    #[test]
    fn provider_entry_describes_bound_model() {
        let entry = launcher(serde_json::json!({}))
            .provider_entry(&binding())
            .unwrap();
        assert_eq!(entry["baseUrl"], "http://localhost:11434/v1");
        assert_eq!(entry["api"], "openai-completions");
        assert_eq!(entry["apiKey"], "$GRANITE_CLI_PI_API_KEY");
        assert_eq!(entry["models"][0]["id"], "granite4.1:8b");
        assert_eq!(entry["models"][0]["contextWindow"], 131072);
    }

    #[test]
    fn provider_entry_merges_overrides() {
        let l = launcher(serde_json::json!({
            "provider_overrides": { "compat": { "supportsDeveloperRole": false } }
        }));
        let entry = l.provider_entry(&binding()).unwrap();
        assert_eq!(entry["compat"]["supportsDeveloperRole"], false);
        // Generated keys survive the merge.
        assert_eq!(entry["baseUrl"], "http://localhost:11434/v1");
    }

    #[test]
    fn provider_entry_overrides_win_on_conflict() {
        let l = launcher(serde_json::json!({
            "provider_overrides": { "baseUrl": "http://proxy:8080/v1" }
        }));
        let entry = l.provider_entry(&binding()).unwrap();
        assert_eq!(entry["baseUrl"], "http://proxy:8080/v1");
    }

    #[test]
    fn provider_entry_headers_can_be_overridden_by_provider_overrides() {
        let l = launcher(serde_json::json!({
            "headers": { "X-Config": "config-value" },
            "provider_overrides": { "headers": { "X-Override": "override-value" } }
        }));
        let entry = l.provider_entry(&binding()).unwrap();
        // provider_overrides replaces the entire headers object since it's a
        // top-level key merge (shallow, last-write-wins).
        assert_eq!(entry["headers"]["X-Override"], "override-value");
        // config-level headers are not merged in.
        assert!(entry["headers"].get("X-Config").is_none());
    }

    // -- base url / api mapping ------------------------------------------------

    #[test]
    fn base_url_keeps_version_prefix_and_drops_operation() {
        assert_eq!(pi_base_url(&binding()), "http://localhost:11434/v1");
    }

    #[test]
    fn base_url_trims_trailing_slash_from_provider_url() {
        let b = AgentModelBinding {
            base_url: "http://localhost:1234/".to_string(),
            ..binding()
        };
        assert_eq!(pi_base_url(&b), "http://localhost:1234/v1");
    }

    #[test]
    fn base_url_for_anthropic_endpoint_drops_messages() {
        let b = AgentModelBinding {
            api_type: ApiType::Anthropic,
            endpoint_path: "/v1/messages".to_string(),
            ..binding()
        };
        assert_eq!(pi_base_url(&b), "http://localhost:11434/v1");
    }

    #[test]
    fn api_name_maps_supported_dialects_and_rejects_ollama() {
        assert_eq!(pi_api_name(&ApiType::OpenAI).unwrap(), "openai-completions");
        assert_eq!(
            pi_api_name(&ApiType::Anthropic).unwrap(),
            "anthropic-messages"
        );
        assert!(pi_api_name(&ApiType::Ollama).is_err());
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
    async fn env_overlay_redirects_config_dir_and_exports_api_key() {
        let b = AgentModelBinding {
            api_key: Some(Secret::from("sk-test")),
            ..binding()
        };
        let overlay = bound(serde_json::json!({}), b)
            .env_overlay(&ctx(false))
            .await
            .unwrap();

        let dir = overlay
            .iter()
            .find(|b| b.key == "PI_CODING_AGENT_DIR")
            .expect("config dir redirect");
        assert!(
            Path::new(&dir.value).ends_with(Path::new("launcher-state").join("pi")),
            "{}",
            dir.value
        );

        let key = overlay
            .iter()
            .find(|b| b.key == "GRANITE_CLI_PI_API_KEY")
            .expect("api key");
        assert_eq!(key.value, "sk-test");
    }

    #[tokio::test]
    async fn env_overlay_uses_placeholder_when_provider_has_no_key() {
        let overlay = bound(serde_json::json!({}), binding())
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        let key = overlay
            .iter()
            .find(|b| b.key == "GRANITE_CLI_PI_API_KEY")
            .unwrap();
        assert_eq!(key.value, "granite-cli");
    }

    #[tokio::test]
    async fn env_overlay_uses_placeholder_for_empty_key() {
        let b = AgentModelBinding {
            api_key: Some(Secret::from("")),
            ..binding()
        };
        let overlay = bound(serde_json::json!({}), b)
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        let key = overlay
            .iter()
            .find(|b| b.key == "GRANITE_CLI_PI_API_KEY")
            .unwrap();
        assert_eq!(key.value, "granite-cli");
    }

    // -- materialized config ---------------------------------------------------

    /// A (state_dir, source_dir) pair inside one tempdir.
    fn dirs(tmp: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        let source = tmp.path().join("user-pi");
        std::fs::create_dir_all(&source).unwrap();
        (tmp.path().join("state"), source)
    }

    fn read_models_json(dir: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(dir.join(MODELS_JSON)).unwrap()).unwrap()
    }

    #[test]
    fn materialize_writes_provider_under_the_binding_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        materialize_pi_config(
            &state,
            &source,
            "my-ollama",
            serde_json::json!({ "api": "openai-completions" }),
            &CaptureUi::default(),
        )
        .unwrap();

        let written = read_models_json(&state);
        assert_eq!(
            written["providers"]["my-ollama"]["api"],
            "openai-completions"
        );
    }

    #[test]
    fn materialize_never_writes_into_the_source_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        let source_models = source.join(MODELS_JSON);
        let original = r#"{"providers":{"my-vllm":{"api":"openai-completions"}}}"#;
        std::fs::write(&source_models, original).unwrap();

        materialize_pi_config(
            &state,
            &source,
            "my-ollama",
            serde_json::json!({ "api": "x" }),
            &CaptureUi::default(),
        )
        .unwrap();

        // The user's file is byte-identical.
        assert_eq!(std::fs::read_to_string(&source_models).unwrap(), original);
        // Ours carries both their provider and the generated one.
        let written = read_models_json(&state);
        assert_eq!(written["providers"]["my-vllm"]["api"], "openai-completions");
        assert_eq!(written["providers"]["my-ollama"]["api"], "x");
    }

    #[test]
    fn materialize_preserves_unrelated_top_level_keys_from_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::write(source.join(MODELS_JSON), r#"{"somethingElse":42}"#).unwrap();

        materialize_pi_config(
            &state,
            &source,
            "my-ollama",
            serde_json::json!({}),
            &CaptureUi::default(),
        )
        .unwrap();
        assert_eq!(read_models_json(&state)["somethingElse"], 42);
    }

    #[test]
    fn materialize_works_with_no_user_pi_config_at_all() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let source = tmp.path().join("does-not-exist");

        materialize_pi_config(
            &state,
            &source,
            "my-ollama",
            serde_json::json!({ "api": "x" }),
            &CaptureUi::default(),
        )
        .unwrap();
        assert_eq!(
            read_models_json(&state)["providers"]["my-ollama"]["api"],
            "x"
        );
    }

    #[test]
    fn materialize_is_idempotent_across_launches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::write(source.join("settings.json"), "{}").unwrap();

        for api in ["old", "new"] {
            materialize_pi_config(
                &state,
                &source,
                "my-ollama",
                serde_json::json!({ "api": api }),
                &CaptureUi::default(),
            )
            .unwrap();
        }
        assert_eq!(
            read_models_json(&state)["providers"]["my-ollama"]["api"],
            "new"
        );
        assert!(state.join("settings.json").exists());
    }

    #[test]
    fn materialize_refuses_malformed_source_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::write(source.join(MODELS_JSON), "{ not json").unwrap();

        let err = materialize_pi_config(
            &state,
            &source,
            "my-ollama",
            serde_json::json!({}),
            &CaptureUi::default(),
        )
        .expect_err("must fail");
        assert!(err.to_string().contains("not valid JSON"));
        assert!(!state.join(MODELS_JSON).exists());
    }

    #[test]
    fn materialize_refuses_non_object_providers_in_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::write(source.join(MODELS_JSON), r#"{"providers":[]}"#).unwrap();

        let err = materialize_pi_config(
            &state,
            &source,
            "my-ollama",
            serde_json::json!({}),
            &CaptureUi::default(),
        )
        .expect_err("must fail");
        assert!(err.to_string().contains("not a JSON object"));
    }

    #[cfg(unix)]
    #[test]
    fn materialize_links_user_resources_but_not_models_or_sessions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::write(source.join("settings.json"), r#"{"theme":"dark"}"#).unwrap();
        std::fs::write(source.join("auth.json"), "{}").unwrap();
        std::fs::create_dir(source.join("packages")).unwrap();
        std::fs::create_dir(source.join(SESSIONS_DIR)).unwrap();
        std::fs::write(source.join(MODELS_JSON), "{}").unwrap();

        materialize_pi_config(
            &state,
            &source,
            "my-ollama",
            serde_json::json!({}),
            &CaptureUi::default(),
        )
        .unwrap();

        for linked in ["settings.json", "auth.json", "packages"] {
            let path = state.join(linked);
            let md = std::fs::symlink_metadata(&path)
                .unwrap_or_else(|_| panic!("{linked} should be linked"));
            assert!(md.file_type().is_symlink(), "{linked} should be a symlink");
            // A relative target would resolve against the link's own directory
            // and dangle, so the link must point at an absolute path.
            let target = std::fs::read_link(&path).unwrap();
            assert!(
                target.is_absolute(),
                "{linked} -> {} must be absolute",
                target.display()
            );
            assert!(path.exists(), "{linked} link must resolve");
        }
        // Reading through the link sees the user's content.
        assert_eq!(
            std::fs::read_to_string(state.join("settings.json")).unwrap(),
            r#"{"theme":"dark"}"#
        );
        // Sessions stay local; models.json is ours, not a link.
        assert!(!state.join(SESSIONS_DIR).exists());
        assert!(
            !std::fs::symlink_metadata(state.join(MODELS_JSON))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_owned_json_replaces_a_symlink_instead_of_writing_through_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let victim = tmp.path().join("users-real-file.json");
        std::fs::write(&victim, "SACRED").unwrap();
        let link = tmp.path().join(MODELS_JSON);
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        write_owned_json(&link, &serde_json::json!({ "ours": true })).unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "SACRED");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&link).unwrap())
                .unwrap()["ours"],
            true
        );
    }

    #[test]
    fn materialize_tolerates_source_equal_to_state() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("both");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MODELS_JSON), r#"{"providers":{"keep":{}}}"#).unwrap();

        materialize_pi_config(
            &dir,
            &dir,
            "my-ollama",
            serde_json::json!({ "api": "x" }),
            &CaptureUi::default(),
        )
        .unwrap();

        let written = read_models_json(&dir);
        assert!(written["providers"]["keep"].is_object());
        assert_eq!(written["providers"]["my-ollama"]["api"], "x");
    }

    // -- launch ----------------------------------------------------------------

    // Deliberately reads whatever `GRANITE_CLI_HOME` is ambient rather than
    // setting it: env mutation would race the other tests in this binary that
    // point that var at their own tempdirs.
    #[tokio::test]
    async fn dry_run_launch_reports_without_writing_anything() {
        let state_dir = crate::config::Config::launcher_state_dir("pi").unwrap();
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
            infos.iter().any(|m| m.contains("Would write Pi provider")),
            "expected a dry-run notice, got {infos:?}"
        );
        assert!(
            infos.iter().any(|m| m.contains("left unmodified")),
            "expected the source file to be called out as untouched, got {infos:?}"
        );
        assert!(
            infos
                .iter()
                .any(|m| m.contains("--provider my-ollama --model granite4.1:8b --help")),
            "expected the selection flags ahead of caller args, got {infos:?}"
        );
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
        assert!(!infos.iter().any(|m| m.contains("--provider")));
        // Without a binding there is no generated config, so Pi keeps using the
        // user's own directory.
        assert!(!infos.iter().any(|m| m.contains(CONFIG_DIR_ENV)));
    }

    // -- pi_tool_name ------------------------------------------------------------

    #[test]
    fn pi_tool_name_covers_every_canonical_variant_and_passes_other_through() {
        assert_eq!(pi_tool_name(&ToolName::FileRead), Some("read".to_string()));
        assert_eq!(pi_tool_name(&ToolName::Shell), Some("bash".to_string()));
        assert_eq!(pi_tool_name(&ToolName::FileEdit), Some("edit".to_string()));
        assert_eq!(
            pi_tool_name(&ToolName::FileWrite),
            Some("write".to_string())
        );
        assert_eq!(pi_tool_name(&ToolName::Search), Some("grep".to_string()));
        assert_eq!(
            pi_tool_name(&ToolName::FileSearch),
            Some("find".to_string())
        );
        assert_eq!(pi_tool_name(&ToolName::WebFetch), None);
        assert_eq!(pi_tool_name(&ToolName::WebSearch), None);
        assert_eq!(
            pi_tool_name(&ToolName::Mcp {
                server: "vision".to_string(),
                tool: None,
            }),
            None
        );
        assert_eq!(
            pi_tool_name(&ToolName::Mcp {
                server: "vision".to_string(),
                tool: Some("analyze".to_string()),
            }),
            None
        );
        assert_eq!(
            pi_tool_name(&ToolName::Other("custom-tool".to_string())),
            Some("custom-tool".to_string())
        );
    }

    // -- tools_csv ---------------------------------------------------------------

    #[test]
    fn tools_csv_is_none_for_empty_input() {
        assert_eq!(tools_csv(&[]), None);
    }

    #[test]
    fn tools_csv_is_none_when_nothing_maps() {
        assert_eq!(tools_csv(&[ToolName::WebFetch, ToolName::WebSearch]), None);
    }

    #[test]
    fn tools_csv_joins_only_the_mapped_subset_in_order() {
        let tools = [
            ToolName::FileRead,
            ToolName::WebFetch,
            ToolName::Shell,
            ToolName::FileEdit,
        ];
        assert_eq!(tools_csv(&tools), Some("read,bash,edit".to_string()));
    }

    // -- pi_release_asset_name_for -----------------------------------------------

    #[test]
    fn release_asset_name_covers_every_supported_combo() {
        assert_eq!(
            pi_release_asset_name_for("macos", "aarch64").unwrap(),
            "pi-darwin-arm64.tar.gz"
        );
        assert_eq!(
            pi_release_asset_name_for("macos", "x86_64").unwrap(),
            "pi-darwin-x64.tar.gz"
        );
        assert_eq!(
            pi_release_asset_name_for("linux", "aarch64").unwrap(),
            "pi-linux-arm64.tar.gz"
        );
        assert_eq!(
            pi_release_asset_name_for("linux", "x86_64").unwrap(),
            "pi-linux-x64.tar.gz"
        );
        assert_eq!(
            pi_release_asset_name_for("windows", "x86_64").unwrap(),
            "pi-windows-x64.zip"
        );
        assert_eq!(
            pi_release_asset_name_for("windows", "aarch64").unwrap(),
            "pi-windows-arm64.zip"
        );
    }

    #[test]
    fn release_asset_name_rejects_unsupported_combo() {
        let err = pi_release_asset_name_for("freebsd", "x86_64")
            .expect_err("must fail")
            .to_string();
        assert!(err.contains("freebsd"), "{err}");
        assert!(err.contains("x86_64"), "{err}");
    }

    // -- find_sha256 ---------------------------------------------------------------

    const SUMS_SAMPLE: &str = "abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefab  pi-darwin-arm64.tar.gz\n789fed789fed789fed789fed789fed789fed789fed789fed789fed789fed78  pi-linux-x64.tar.gz\n";

    #[test]
    fn find_sha256_returns_hash_for_matching_filename() {
        assert_eq!(
            find_sha256(SUMS_SAMPLE, "pi-linux-x64.tar.gz"),
            Some("789fed789fed789fed789fed789fed789fed789fed789fed789fed789fed78".to_string())
        );
    }

    #[test]
    fn find_sha256_returns_none_for_unknown_filename() {
        assert_eq!(find_sha256(SUMS_SAMPLE, "pi-windows-x64.zip"), None);
    }

    #[test]
    fn find_sha256_strips_leading_binary_mode_marker() {
        let sums = "abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefab *pi-darwin-arm64.tar.gz\n";
        assert_eq!(
            find_sha256(sums, "pi-darwin-arm64.tar.gz"),
            Some("abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefab".to_string())
        );
    }

    // -- run_delegated_task --------------------------------------------------------

    #[tokio::test]
    async fn run_delegated_task_requires_a_bound_model() {
        let l = launcher(serde_json::json!({ "command_path": "ls" }));
        let ui = CaptureUi::default();
        let result = l
            .run_delegated_task(
                &PathBuf::from("ls"),
                "do the thing",
                "you are a helper",
                &[],
                &ctx(true),
                &ui,
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn run_delegated_task_dry_run_returns_empty_output() {
        let l = bound(serde_json::json!({ "command_path": "ls" }), binding());
        let ui = CaptureUi::default();
        let output = l
            .run_delegated_task(
                &PathBuf::from("ls"),
                "do the thing",
                "you are a helper",
                &[ToolName::FileRead, ToolName::Shell],
                &ctx(true),
                &ui,
            )
            .await
            .unwrap();
        assert_eq!(output, String::new());
    }
}
