//! Launcher for the `hermes` coding agent CLI
//! (<https://hermes-agent.nousresearch.com>, NousResearch's agent CLI --
//! not the Hermes model family).
//!
//! Hermes reads its settings from `$HERMES_HOME/config.yaml` and offers no
//! way to point at a different config file without relocating the whole
//! home (which holds auth, memories, skills, sessions, ...). Redirecting
//! `HERMES_HOME` would also break wrappers that mount the real home (e.g. a
//! `docker run` script), so instead this launcher edits the real config for
//! the length of the session: the generated settings are merged over the
//! user's existing `config.yaml`, the original is moved aside to a
//! `config.yaml.bak-granite-cli[.N]` backup (plus a copy in the launcher state
//! dir), and it is moved back when `hermes` exits. See [`ConfigSwap`].

// Standard
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use anyhow::Context;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// Local
use crate::capabilities::{
    AgentModelBinding, ApiType, Binding, BindingRequest, BindingType, McpBinding,
    ResolvedCapability, VoiceInputBinding, VoiceInputBindingRequest,
};
use crate::launchers::base::{EnvBinding, LaunchContext, Launcher, LauncherMetadata, run_command};
use crate::launchers::shared::mcp_cli::{STDIO_AND_HTTP, mcp_binding_request, transport_set};
use crate::registry::{ConfigConstructable, ConstructError};
use crate::utils::install::{InstallInfo, InstallMethod, MethodKind, Os};
use crate::utils::resolve_shell_command;
use crate::utils::ui::Ui;

use_channel!("HRMES");

/*-- public --*/

#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct HermesLauncherConfig {
    /// Override path to the `hermes` binary for non-PATH installs.
    /// Leave unset to use PATH lookup.
    #[serde(default)]
    pub command_path: Option<String>,

    /// Extra keys merged (shallow, last-write-wins) into the generated
    /// hermes config file -- e.g. `model.context_length` or provider-specific
    /// overrides. Necessary because the entry is regenerated on every launch.
    #[serde(default)]
    pub model_overrides: Option<serde_json::Value>,
}

pub struct HermesLauncher {
    instance_id: String,
    config: HermesLauncherConfig,
    bound_binding: Option<AgentModelBinding>,
    /// `(server_name, binding)` for every MCP-capable capability bound to
    /// this launcher, written into the generated config's `mcp_servers` block.
    bound_mcp_bindings: Vec<(String, McpBinding)>,
    /// Speech-to-text binding, written into the generated config's `stt` block.
    bound_voice_binding: Option<VoiceInputBinding>,
}

impl ConfigConstructable for HermesLauncher {
    type Config = HermesLauncherConfig;

    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        let config: HermesLauncherConfig =
            serde_json::from_value(cfg.clone()).map_err(ConstructError::settings)?;
        Ok(Self {
            instance_id: instance_id.to_string(),
            config,
            bound_binding: None,
            bound_mcp_bindings: vec![],
            bound_voice_binding: None,
        })
    }
}

impl crate::registry::Named for HermesLauncher {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait]
impl Launcher for HermesLauncher {
    fn name(&self) -> &str {
        "Hermes CLI"
    }

    fn command(&self) -> &str {
        self.config.command_path.as_deref().unwrap_or("hermes")
    }

    async fn bind_capability(&mut self, capability: &dyn ResolvedCapability) -> anyhow::Result<()> {
        let supported = Self::metadata().supported_capabilities;
        let capability_types = capability.binding_types();
        alog_channel!(
            MessageLevel::Debug2,
            "Binding capability types {:#?}",
            capability_types
        );
        if !capability_types.is_subset(&supported) {
            anyhow::bail!(
                "capability supports {:?} which this launcher does not support",
                capability_types.difference(&supported).collect::<Vec<_>>()
            );
        }

        if capability_types.contains(&BindingType::Mcp) {
            let binding = capability
                .bind(mcp_binding_request(&Self::metadata().mcp_transports))
                .await?;
            match binding {
                Binding::Mcp(binding) => {
                    self.bound_mcp_bindings
                        .push((capability.instance_id().to_string(), binding));
                }
                other => anyhow::bail!("expected an Mcp binding, got {:?}", other.binding_type()),
            }
            return Ok(());
        }

        if capability_types.contains(&BindingType::VoiceInput) {
            let binding = capability
                .bind(BindingRequest::VoiceInput(VoiceInputBindingRequest {
                    api_type: ApiType::OpenAI,
                }))
                .await?;
            match binding {
                Binding::VoiceInput(binding) => self.bound_voice_binding = Some(binding),
                other => anyhow::bail!(
                    "expected a VoiceInput binding, got {:?}",
                    other.binding_type()
                ),
            }
            return Ok(());
        }

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
        resolve_shell_command(&self.config.command_path, "hermes")
    }

    /// Supplies the credentials the generated `config.yaml` interpolates.
    async fn env_overlay(&self, _ctx: &LaunchContext) -> anyhow::Result<Vec<EnvBinding>> {
        let mut overlay = vec![];
        if let Some(api_key) = self
            .bound_binding
            .as_ref()
            .and_then(|b| b.api_key.as_ref())
            .map(|k| k.0.clone())
            .filter(|k| !k.is_empty())
        {
            overlay.push(EnvBinding {
                key: API_KEY_ENV.to_string(),
                value: api_key,
            });
        }
        if let Some(api_key) = self
            .bound_voice_binding
            .as_ref()
            .and_then(|b| b.api_key.as_ref())
            .map(|k| k.0.clone())
            .filter(|k| !k.is_empty())
        {
            overlay.push(EnvBinding {
                key: STT_API_KEY_ENV.to_string(),
                value: api_key,
            });
        }
        Ok(overlay)
    }

    /// Swaps the generated settings into the user's `config.yaml` for the
    /// duration of the session, then execs `hermes` with the caller's
    /// arguments untouched and puts the original config back afterwards.
    async fn launch(
        &self,
        args: &[String],
        ctx: &LaunchContext,
        ui: &dyn Ui,
    ) -> anyhow::Result<std::process::ExitStatus> {
        let binary = self.validate_command()?;
        let overlay = self.env_overlay(ctx).await?;

        if !self.has_bindings() {
            return run_command(binary, &overlay, args, ctx, ui).await;
        }

        let generated = self.generate_config()?;
        let home = hermes_home_dir()?;
        let config_path = home.join(HERMES_CONFIG_FILE);

        if ctx.dry_run {
            ui.info(&format!(
                "Would merge these settings over {} for the session \
                 (the original would be backed up to {} and restored on exit):",
                config_path.display(),
                next_backup_path(&config_path).display()
            ));
            ui.info(&serde_json::to_string_pretty(&generated)?);
            return run_command(binary, &overlay, args, ctx, ui).await;
        }

        let mut swap = ConfigSwap::begin(&home, &hermes_state_dir(ctx)?, &generated, ui)?;
        let result = run_command(binary, &overlay, args, ctx, ui).await;
        match swap.restore() {
            Ok(Some(warning)) => ui.warn(&warning),
            Ok(None) => {}
            Err(e) => ui.warn(&format!(
                "Could not restore your Hermes config: {e:#}. {}",
                swap.recovery_hint()
            )),
        }
        result
    }
}

impl HasHermesLauncherMetadata for HermesLauncher {
    fn metadata() -> LauncherMetadata {
        LauncherMetadata {
            name: "Hermes CLI".to_string(),
            description: "Hermes Agent local CLI launcher".to_string(),
            default_command: "hermes".to_string(),
            supported_capabilities: HashSet::from([
                BindingType::AgentModel,
                BindingType::Mcp,
                BindingType::VoiceInput,
            ]),
            mcp_transports: transport_set(STDIO_AND_HTTP),
            tags: vec!["hermes".to_string(), "agent".to_string()],
            install: install_info(),
        }
    }
}

/*-- private --*/

use crate::launchers::base::HasLauncherMetadata as HasHermesLauncherMetadata;

/// Env var Hermes reads to locate its config directory -- and, per its own
/// docs, the boundary of an entire "profile" (config, secrets, auth,
/// memories, skills, sessions, logs), not just the one config file.
const HERMES_HOME_ENV: &str = "HERMES_HOME";

/// Env var the generated config's `model.api_key` field interpolates via
/// Hermes's `${VAR_NAME}` substitution syntax.
const API_KEY_ENV: &str = "GRANITE_CLI_HERMES_API_KEY";

/// Env var the generated config's `stt.openai.api_key` field interpolates.
const STT_API_KEY_ENV: &str = "GRANITE_CLI_HERMES_STT_API_KEY";

/// The generated Hermes config file's name, relative to the launcher state dir.
const HERMES_CONFIG_FILE: &str = "config.yaml";

/// Hermes's `base_url` is the API root it appends operation paths to (e.g.
/// `/chat/completions`), same convention as `opencode.rs`/`pi.rs`/
/// `openclaw.rs`'s equivalent helpers -- so the trailing operation is
/// dropped from the binding's full endpoint path here, keeping the version
/// prefix (e.g. `/v1`). Without this, `base_url` is left as the bare host
/// root and every request 404s, since Ollama's (and most OpenAI-compatible
/// servers') actual endpoints live under `/v1`.
fn hermes_base_url(binding: &AgentModelBinding) -> String {
    let root = binding.base_url.trim_end_matches('/');
    let prefix = binding
        .endpoint_path
        .strip_suffix("/chat/completions")
        .unwrap_or("");
    format!("{root}{prefix}")
}

/// Hermes's `stt.openai.base_url` is the API root the transcription
/// operation is appended to, so the trailing `/audio/transcriptions` is
/// dropped from the binding's endpoint path (keeping e.g. `/v1`).
fn hermes_stt_base_url(binding: &VoiceInputBinding) -> String {
    let root = binding.base_url.trim_end_matches('/');
    let prefix = binding
        .endpoint_path
        .strip_suffix("/audio/transcriptions")
        .unwrap_or("");
    format!("{root}{prefix}")
}

impl HermesLauncher {
    fn has_bindings(&self) -> bool {
        self.bound_binding.is_some()
            || !self.bound_mcp_bindings.is_empty()
            || self.bound_voice_binding.is_some()
    }

    /// The `stt` config block for a bound speech-to-text model.
    fn stt_config(binding: &VoiceInputBinding) -> serde_json::Value {
        let language = binding.language.clone().unwrap_or_default();
        let mut openai = serde_json::json!({
            "model": binding.model_name,
            "base_url": hermes_stt_base_url(binding),
            "language": language,
            "timeout": binding.timeout_seconds,
            "max_retries": binding.max_retries,
        });
        if binding
            .api_key
            .as_ref()
            .is_some_and(|key| !key.0.is_empty())
        {
            openai["api_key"] = serde_json::Value::String(format!("${{{STT_API_KEY_ENV}}}"));
        }
        serde_json::json!({
            "enabled": true,
            "provider": "openai",
            "language": language,
            "openai": openai,
        })
    }

    /// Builds the Hermes config describing the bound model and MCP servers.
    fn generate_config(&self) -> anyhow::Result<serde_json::Value> {
        let mut config = serde_json::json!({});

        if let Some(binding) = &self.bound_binding {
            let mut model = serde_json::json!({
                // `provider` is a discriminator Hermes recognizes (a
                // built-in id, or "custom") -- not a free-form label, so the
                // granite-cli provider name never goes here.
                "provider": "custom",
                "default": binding.model_name,
            });
            if !binding.base_url.is_empty() {
                model["base_url"] = serde_json::Value::String(hermes_base_url(binding));
            }
            if let Some(context_length) = binding.context_length {
                model["context_length"] = serde_json::json!(context_length);
            }
            if binding
                .api_key
                .as_ref()
                .is_some_and(|key| !key.0.is_empty())
            {
                model["api_key"] = serde_json::Value::String(format!("${{{API_KEY_ENV}}}"));
            }

            // Add custom headers as extra_headers if configured.
            // Sorted by key for deterministic output.
            if let Some(ref headers) = binding.custom_headers
                && !headers.is_empty()
            {
                model["extra_headers"] = serde_json::to_value(headers)?;
            }

            // Merge user-provided overrides on top so they win on conflict.
            // Special case: if the override key is "model", merge the inner
            // object into config["model"] rather than replacing it entirely.
            if let Some(overrides) = self
                .config
                .model_overrides
                .as_ref()
                .and_then(serde_json::Value::as_object)
            {
                for (key, value) in overrides {
                    if key == "model" {
                        if let Some(inner) = value.as_object()
                            && let Some(target) = model.as_object_mut()
                        {
                            for (k, v) in inner {
                                target.insert(k.clone(), v.clone());
                            }
                        }
                    } else if let Some(target) = config.as_object_mut() {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
            config["model"] = model;
        }

        if !self.bound_mcp_bindings.is_empty() {
            let mut mcp_servers = serde_json::Map::new();
            for (name, binding) in &self.bound_mcp_bindings {
                mcp_servers.insert(name.clone(), {
                    match binding {
                        McpBinding::Stdio {
                            command, args, env, ..
                        } => serde_json::json!({
                            "command": command,
                            "args": args,
                            "env": env,
                        }),
                        McpBinding::Http { url, headers, .. }
                        | McpBinding::Sse { url, headers, .. } => {
                            serde_json::json!({
                                "url": url,
                                "headers": headers,
                            })
                        }
                    }
                });
            }
            config["mcp_servers"] = serde_json::Value::Object(mcp_servers);
        }

        if let Some(binding) = &self.bound_voice_binding {
            let mut stt = Self::stt_config(binding);
            // A user-supplied `stt` in `model_overrides` wins key-by-key
            // (shallow), same as the other top-level overrides.
            if let Some(user) = config.get("stt").and_then(|v| v.as_object()).cloned()
                && let Some(target) = stt.as_object_mut()
            {
                target.extend(user);
            }
            config["stt"] = stt;
        }

        Ok(config)
    }
}

/// The granite-cli-owned state directory for this launcher instance. Holds
/// the safety copy of the user's original config.
fn hermes_state_dir(ctx: &LaunchContext) -> anyhow::Result<PathBuf> {
    crate::config::Config::launcher_state_dir(&ctx.launcher_id)
}

/// The user's Hermes home: `$HERMES_HOME` when set, else `~/.hermes`.
fn hermes_home_dir() -> anyhow::Result<PathBuf> {
    if let Ok(val) = std::env::var(HERMES_HOME_ENV)
        && !val.is_empty()
    {
        return Ok(PathBuf::from(val));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory for Hermes's config"))?;
    Ok(home.join(".hermes"))
}

/// Appended to `config.yaml` to name our backup of it. See [`next_backup_path`].
const BACKUP_SUFFIX: &str = ".bak-granite-cli";

/// Name of the safety copy of the original config in the launcher state dir.
const STATE_COPY_FILE: &str = "config.yaml.original";

/// First-line prefix of the temporary config. What follows is the path the
/// original was moved to (or `(none)`), which is how a later launch finds
/// the original again if this one never got to restore it.
const SWAP_MARKER: &str = "# granite-cli: temporary config for a running session; original at: ";

/// A temporarily replaced `config.yaml`.
///
/// `begin` moves the user's config to a `.bak[.N]` backup next to it, copies
/// it into the launcher state dir, and writes the merged config in its place.
/// `restore` (also run on drop, as a fallback) removes ours and moves the
/// original back. If the process dies before then, the marker line in our
/// file lets the next `begin` put things right.
struct ConfigSwap {
    config_path: PathBuf,
    /// Where the original was moved to, if there was one.
    backup: Option<PathBuf>,
    state_copy: PathBuf,
    active: bool,
}

impl ConfigSwap {
    fn begin(
        home: &Path,
        state_dir: &Path,
        generated: &serde_json::Value,
        ui: &dyn Ui,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(home)
            .with_context(|| format!("Failed to create {}", home.display()))?;
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("Failed to create {}", state_dir.display()))?;
        let config_path = home.join(HERMES_CONFIG_FILE);
        let state_copy = state_dir.join(STATE_COPY_FILE);

        recover_stale_swap(&config_path, &state_copy, ui);

        // Parse before moving anything so a config we can't read is reported
        // without having been touched.
        let existing = match std::fs::read_to_string(&config_path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e).with_context(|| format!("Failed to read {}", config_path.display()));
            }
        };
        let base = match existing.as_deref() {
            Some(text) => serde_yaml::from_str::<serde_json::Value>(text)
                .with_context(|| format!("{} is not valid YAML", config_path.display()))?,
            None => serde_json::Value::Null,
        };
        let merged = merge_config(base, generated);

        let backup = if existing.is_some() {
            std::fs::copy(&config_path, &state_copy).with_context(|| {
                format!("Failed to back up the config to {}", state_copy.display())
            })?;
            let backup = next_backup_path(&config_path);
            std::fs::rename(&config_path, &backup).with_context(|| {
                format!(
                    "Failed to move {} to {}",
                    config_path.display(),
                    backup.display()
                )
            })?;
            ui.warn(&format!(
                "Hermes config is temporarily replaced for this session. Your original is at {} \
                 (and a copy at {}); it is put back when Hermes exits.",
                backup.display(),
                state_copy.display()
            ));
            Some(backup)
        } else {
            None
        };

        let mut swap = Self {
            config_path,
            backup,
            state_copy,
            active: true,
        };
        if let Err(e) = swap.write_temporary(&merged) {
            // Put things back rather than leave the user without a config.
            let _ = swap.restore();
            return Err(e);
        }
        Ok(swap)
    }

    fn write_temporary(&self, merged: &serde_json::Value) -> anyhow::Result<()> {
        let original = self
            .backup
            .as_ref()
            .map_or("(none)".to_string(), |b| b.display().to_string());
        let body = serde_yaml::to_string(merged)
            .with_context(|| format!("Failed to serialize {}", self.config_path.display()))?;
        std::fs::write(
            &self.config_path,
            format!("{SWAP_MARKER}{original}\n{body}"),
        )
        .with_context(|| format!("Failed to write {}", self.config_path.display()))
    }

    /// Puts the original config back. Returns a warning to show when the
    /// config was changed behind our back (e.g. rewritten by a Hermes
    /// migration): that version is kept as another backup, not discarded.
    fn restore(&mut self) -> anyhow::Result<Option<String>> {
        if !self.active {
            return Ok(None);
        }
        self.active = false;

        let mut warning = None;
        if std::fs::symlink_metadata(&self.config_path).is_ok() {
            if has_swap_marker(&self.config_path) {
                std::fs::remove_file(&self.config_path)
                    .with_context(|| format!("Failed to remove {}", self.config_path.display()))?;
            } else {
                let aside = next_backup_path(&self.config_path);
                std::fs::rename(&self.config_path, &aside).with_context(|| {
                    format!("Failed to move {} aside", self.config_path.display())
                })?;
                warning = Some(format!(
                    "Hermes changed its config during the session; that version was kept at {}",
                    aside.display()
                ));
            }
        }
        if let Some(backup) = &self.backup {
            if backup.exists() {
                std::fs::rename(backup, &self.config_path).with_context(|| {
                    format!(
                        "Failed to move {} back to {}",
                        backup.display(),
                        self.config_path.display()
                    )
                })?;
            } else {
                // Something else removed the backup while Hermes ran. The
                // state-dir copy exists for exactly this.
                std::fs::copy(&self.state_copy, &self.config_path).with_context(|| {
                    format!(
                        "{} is gone and the copy at {} could not be restored",
                        backup.display(),
                        self.state_copy.display()
                    )
                })?;
                let lost = format!(
                    "The backup at {} disappeared during the session; restored your config from {}",
                    backup.display(),
                    self.state_copy.display()
                );
                warning = Some(match warning {
                    Some(earlier) => format!("{earlier}. {lost}"),
                    None => lost,
                });
            }
        }
        Ok(warning)
    }

    fn recovery_hint(&self) -> String {
        match &self.backup {
            Some(b) => format!(
                "Your original is at {} (copy at {})",
                b.display(),
                self.state_copy.display()
            ),
            None => "There was no original config".to_string(),
        }
    }
}

impl Drop for ConfigSwap {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Merges `generated` over the user's `base` config. `model` is replaced
/// outright (what we generate describes it completely, and stale keys beside
/// it, such as another provider's `api_key`, would contradict it); every other
/// section is merged key by key so the user's own settings survive.
fn merge_config(base: serde_json::Value, generated: &serde_json::Value) -> serde_json::Value {
    fn deep_merge(base: &mut serde_json::Value, over: &serde_json::Value) {
        match (base.as_object_mut(), over.as_object()) {
            (Some(base), Some(over)) => {
                for (key, value) in over {
                    match base.get_mut(key) {
                        Some(existing) => deep_merge(existing, value),
                        None => {
                            base.insert(key.clone(), value.clone());
                        }
                    }
                }
            }
            _ => *base = over.clone(),
        }
    }

    let mut merged = if base.is_object() {
        base
    } else {
        serde_json::json!({})
    };
    if let (Some(target), Some(generated)) = (merged.as_object_mut(), generated.as_object()) {
        for (key, value) in generated {
            if key == "model" {
                target.insert(key.clone(), value.clone());
            } else {
                match target.get_mut(key) {
                    Some(existing) => deep_merge(existing, value),
                    None => {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
        }
    }
    merged
}

/// The first of `config.yaml.bak-granite-cli`, `.bak-granite-cli.1`, ... that
/// does not exist yet, so an existing backup is never overwritten.
///
/// Deliberately not `config.yaml.bak[.N]`: Hermes moves every
/// `config.yaml.bak.<digits>` file out of its home into `backups/config/`
/// whenever it takes one of its own config backups (its "legacy sibling"
/// sweep), which pulled our backup out from under us mid-session. A
/// hand-named `config.yaml.bak-*` copy is one it documents as the user's and
/// never touches.
fn next_backup_path(config_path: &Path) -> PathBuf {
    let taken = |p: &Path| std::fs::symlink_metadata(p).is_ok();
    let name = config_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| HERMES_CONFIG_FILE.to_string());
    let first = config_path.with_file_name(format!("{name}{BACKUP_SUFFIX}"));
    if !taken(&first) {
        return first;
    }
    (1u32..)
        .map(|i| config_path.with_file_name(format!("{name}{BACKUP_SUFFIX}.{i}")))
        .find(|p| !taken(p))
        .expect("an unused backup name exists")
}

/// The first line of the file at `path`, if it can be read.
fn marker_line(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line).ok()?;
    line.starts_with(SWAP_MARKER)
        .then(|| line.trim_end().to_string())
}

fn has_swap_marker(path: &Path) -> bool {
    marker_line(path).is_some()
}

/// A previous launch that died before restoring leaves our temporary config
/// in place. Puts the original back, from the backup the marker names or
/// else the state-dir copy, before this launch takes its own backup (which
/// would otherwise treat our leftover as the user's config).
fn recover_stale_swap(config_path: &Path, state_copy: &Path, ui: &dyn Ui) {
    let Some(line) = marker_line(config_path) else {
        return;
    };
    let named = line
        .strip_prefix(SWAP_MARKER)
        .filter(|p| *p != "(none)")
        .map(PathBuf::from);

    if std::fs::remove_file(config_path).is_err() {
        return;
    }
    if let Some(backup) = named.filter(|p| p.exists()) {
        if std::fs::rename(&backup, config_path).is_ok() {
            ui.warn(&format!(
                "A previous session did not restore your Hermes config; restored it from {}",
                backup.display()
            ));
            return;
        }
    } else if line.ends_with("(none)") {
        // There was never an original: nothing to restore.
        return;
    }
    if state_copy.exists() && std::fs::copy(state_copy, config_path).is_ok() {
        ui.warn(&format!(
            "A previous session did not restore your Hermes config; restored it from {}",
            state_copy.display()
        ));
    } else {
        ui.warn(&format!(
            "A previous session left a temporary Hermes config at {} and the original could not be found",
            config_path.display()
        ));
    }
}

/// Install instructions for Hermes Agent.
///
/// Source, checked 2026-10-17: https://hermes-agent.nousresearch.com and its
/// install.sh, which may call `sudo`, runs `hermes setup` from the terminal,
/// and links `hermes` into `~/.local/bin`.
fn install_info() -> InstallInfo {
    InstallInfo::default()
        .program("hermes")
        .page("https://hermes-agent.nousresearch.com")
        .method(
            InstallMethod::new(
                Os::UNIX,
                MethodKind::Script,
                "curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash",
            )
            .needs("bash")
            .attended()
            .bin_dir("~/.local/bin"),
        )
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::BindingType;
    use crate::registry::{Named, Secret};
    use crate::utils::ui::base::tests::CaptureUi;

    fn launcher(cfg: serde_json::Value) -> HermesLauncher {
        HermesLauncher::new("hermes", &cfg).unwrap()
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
            supported_reasoning_efforts: vec![],
        }
    }

    fn bound(cfg: serde_json::Value, binding: AgentModelBinding) -> HermesLauncher {
        let mut l = launcher(cfg);
        l.bound_binding = Some(binding);
        l
    }

    fn ctx(dry_run: bool) -> LaunchContext {
        LaunchContext {
            launcher_id: "hermes".to_string(),
            working_dir: PathBuf::from("/tmp"),
            base_env: std::collections::HashMap::new(),
            dry_run,
            usage_tracker: None,
            model_proxy: None,
        }
    }

    // -- command resolution ----------------------------------------------------

    #[test]
    fn command_defaults_to_hermes() {
        assert_eq!(launcher(serde_json::json!({})).command(), "hermes");
    }

    #[test]
    fn command_uses_explicit_path_when_set() {
        let l = launcher(serde_json::json!({ "command_path": "/opt/bin/hermes" }));
        assert_eq!(l.command(), "/opt/bin/hermes");
    }

    #[test]
    fn validate_command_err_for_nonexistent_explicit_path() {
        let l = launcher(serde_json::json!({ "command_path": "/no/such/path/hermes" }));
        assert!(l.validate_command().is_err());
    }

    #[test]
    fn validate_command_falls_back_to_path_for_bare_command_name() {
        let l = launcher(serde_json::json!({ "command_path": "ls" }));
        assert!(l.validate_command().is_ok());
    }

    // -- voice input -----------------------------------------------------------

    fn voice_binding() -> VoiceInputBinding {
        VoiceInputBinding {
            api_type: ApiType::OpenAI,
            provider_name: "my-llama".to_string(),
            base_url: "http://localhost:9931/".to_string(),
            model_name: "granite-speech".to_string(),
            endpoint_path: "/v1/audio/transcriptions".to_string(),
            api_key: Some(crate::registry::Secret("sk-x".to_string())),
            verify_ssl: true,
            language: None,
            timeout_seconds: 60,
            max_retries: 1,
            custom_headers: None,
        }
    }

    #[test]
    fn generate_config_writes_stt_block_for_voice_binding() {
        let mut l = launcher(serde_json::json!({}));
        l.bound_voice_binding = Some(voice_binding());
        let cfg = l.generate_config().unwrap();
        assert!(cfg.get("model").is_none());
        assert_eq!(cfg["stt"]["enabled"], true);
        assert_eq!(cfg["stt"]["provider"], "openai");
        assert_eq!(cfg["stt"]["openai"]["model"], "granite-speech");
        assert_eq!(cfg["stt"]["openai"]["base_url"], "http://localhost:9931/v1");
        assert_eq!(cfg["stt"]["openai"]["language"], "");
        assert_eq!(cfg["stt"]["openai"]["timeout"], 60);
        assert_eq!(cfg["stt"]["openai"]["max_retries"], 1);
        assert_eq!(
            cfg["stt"]["openai"]["api_key"],
            format!("${{{STT_API_KEY_ENV}}}")
        );
    }

    #[tokio::test]
    async fn voice_binding_sets_stt_key_env_and_no_home_redirect() {
        let mut l = launcher(serde_json::json!({}));
        l.bound_voice_binding = Some(voice_binding());
        let overlay = l.env_overlay(&ctx(true)).await.unwrap();
        assert!(!overlay.iter().any(|e| e.key == HERMES_HOME_ENV));
        assert!(
            overlay
                .iter()
                .any(|e| e.key == STT_API_KEY_ENV && e.value == "sk-x")
        );
    }

    // -- metadata / schema -----------------------------------------------------

    #[test]
    fn metadata_name_is_hermes_cli() {
        let meta = HermesLauncher::metadata();
        assert_eq!(meta.name, "Hermes CLI");
        assert_eq!(meta.default_command, "hermes");
        assert!(
            meta.supported_capabilities
                .contains(&BindingType::AgentModel)
        );
        assert!(meta.supported_capabilities.contains(&BindingType::Mcp));
        assert!(
            meta.supported_capabilities
                .contains(&BindingType::VoiceInput)
        );
    }

    #[test]
    fn instance_id_round_trips_from_construction() {
        let l = HermesLauncher::new("hermes-local", &serde_json::json!({})).unwrap();
        assert_eq!(l.instance_id(), "hermes-local");
    }

    #[test]
    fn config_schema_exposes_command_path_and_model_overrides() {
        use crate::launchers::base::LauncherFactory;
        let mut factory = LauncherFactory::new();
        factory.register::<HermesLauncher>("hermes");
        let schema = factory.config_schema("hermes").unwrap();
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap();
        assert!(props.contains_key("command_path"));
        assert!(props.contains_key("model_overrides"));
    }

    // -- generate_config ------------------------------------------------------

    #[test]
    fn generate_config_sets_default_model_and_custom_provider() {
        let l = bound(serde_json::json!({}), binding());
        let config = l.generate_config().unwrap();
        // Key is `default`, not `model` -- and `provider` is always the
        // "custom" discriminator, never the granite-cli provider name.
        assert_eq!(config["model"]["default"], "granite4.1:8b");
        assert_eq!(config["model"]["provider"], "custom");
        // Version prefix kept, trailing operation dropped -- not the bare
        // host root, which would 404 against Ollama's /v1 endpoints.
        assert_eq!(config["model"]["base_url"], "http://localhost:11434/v1");
        assert_eq!(config["model"]["context_length"], serde_json::json!(131072));
    }

    #[test]
    fn base_url_keeps_version_prefix_and_drops_operation() {
        assert_eq!(hermes_base_url(&binding()), "http://localhost:11434/v1");
    }

    #[test]
    fn base_url_trims_trailing_slash_from_provider_url() {
        let b = AgentModelBinding {
            base_url: "http://localhost:1234/".to_string(),
            ..binding()
        };
        assert_eq!(hermes_base_url(&b), "http://localhost:1234/v1");
    }

    #[test]
    fn generate_config_omits_context_length_when_none() {
        let b = AgentModelBinding {
            context_length: None,
            ..binding()
        };
        let l = bound(serde_json::json!({}), b);
        let config = l.generate_config().unwrap();
        assert!(config["model"].get("context_length").is_none());
    }

    #[test]
    fn generate_config_interpolates_api_key_env_when_present() {
        let b = AgentModelBinding {
            api_key: Some(Secret::from("sk-test")),
            ..binding()
        };
        let l = bound(serde_json::json!({}), b);
        let config = l.generate_config().unwrap();
        assert_eq!(config["model"]["api_key"], "${GRANITE_CLI_HERMES_API_KEY}");
    }

    #[test]
    fn generate_config_omits_api_key_when_absent() {
        let l = bound(serde_json::json!({}), binding());
        let config = l.generate_config().unwrap();
        assert!(config["model"].get("api_key").is_none());
    }

    #[test]
    fn generate_config_merges_overrides() {
        let l = bound(
            serde_json::json!({
                "model_overrides": {
                    "model": {
                        "context_length": 4000
                    }
                }
            }),
            binding(),
        );
        let config = l.generate_config().unwrap();
        // Override wins
        assert_eq!(config["model"]["context_length"], serde_json::json!(4000));
        // Generated keys survive the merge
        assert_eq!(config["model"]["provider"], "custom");
        assert_eq!(config["model"]["default"], "granite4.1:8b");
    }

    #[test]
    fn generate_config_writes_mcp_servers_stdio_and_http() {
        let mut l = launcher(serde_json::json!({}));
        l.bound_mcp_bindings.push((
            "vision".to_string(),
            McpBinding::Stdio {
                command: "/usr/local/bin/granite-cli".to_string(),
                args: vec!["__mcp-serve".to_string(), "vision".to_string()],
                env: std::collections::HashMap::from([("FOO".to_string(), "bar".to_string())]),
                timeout: None,
            },
        ));
        l.bound_mcp_bindings.push((
            "remote".to_string(),
            McpBinding::Http {
                url: "http://127.0.0.1:9999".to_string(),
                headers: std::collections::HashMap::from([(
                    "Authorization".to_string(),
                    "Bearer x".to_string(),
                )]),
                timeout: None,
            },
        ));
        let config = l.generate_config().unwrap();
        assert_eq!(
            config["mcp_servers"]["vision"]["command"],
            "/usr/local/bin/granite-cli"
        );
        assert_eq!(config["mcp_servers"]["vision"]["args"][0], "__mcp-serve");
        assert_eq!(config["mcp_servers"]["vision"]["env"]["FOO"], "bar");
        assert_eq!(
            config["mcp_servers"]["remote"]["url"],
            "http://127.0.0.1:9999"
        );
        assert_eq!(
            config["mcp_servers"]["remote"]["headers"]["Authorization"],
            "Bearer x"
        );
        assert!(config.get("model").is_none());
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
    async fn env_overlay_never_redirects_hermes_home() {
        let overlay = bound(serde_json::json!({}), binding())
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        assert!(!overlay.iter().any(|b| b.key == HERMES_HOME_ENV));
    }

    #[tokio::test]
    async fn env_overlay_sets_api_key_env_when_key_present() {
        let b = AgentModelBinding {
            api_key: Some(Secret::from("sk-test")),
            ..binding()
        };
        let overlay = bound(serde_json::json!({}), b)
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        let key = overlay
            .iter()
            .find(|b| b.key == API_KEY_ENV)
            .expect("api key env");
        assert_eq!(key.value, "sk-test");
    }

    // -- config swap -------------------------------------------------------------

    /// (home, state) dirs inside one tempdir.
    fn dirs(tmp: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        (home, tmp.path().join("state"))
    }

    fn read_yaml(path: &Path) -> serde_json::Value {
        serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn merge_keeps_user_settings_and_replaces_model() {
        let base = serde_json::json!({
            "model": { "default": "old", "api_key": "stale" },
            "voice": { "record_key": "ctrl+b" },
            "stt": { "streaming": true, "openai": { "model": "whisper-1" } },
        });
        let generated = serde_json::json!({
            "model": { "default": "new" },
            "stt": { "enabled": true, "openai": { "model": "granite" } },
        });
        let merged = merge_config(base, &generated);
        assert_eq!(merged["model"], serde_json::json!({ "default": "new" }));
        assert_eq!(merged["voice"]["record_key"], "ctrl+b");
        assert_eq!(merged["stt"]["streaming"], true);
        assert_eq!(merged["stt"]["enabled"], true);
        assert_eq!(merged["stt"]["openai"]["model"], "granite");
    }

    #[test]
    fn merge_into_missing_or_non_mapping_config_yields_generated() {
        let generated = serde_json::json!({ "model": { "default": "x" } });
        assert_eq!(merge_config(serde_json::Value::Null, &generated), generated);
    }

    #[test]
    fn backup_name_is_not_one_hermes_sweeps_out_of_its_home() {
        // Hermes moves `config.yaml.bak.<digits>` siblings into
        // `backups/config/` whenever it takes a backup of its own.
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = tmp.path().join("config.yaml");
        for _ in 0..3 {
            let name = next_backup_path(&cfg);
            let name = name.file_name().unwrap().to_string_lossy().to_string();
            let hermes_sweeps = name
                .strip_prefix("config.yaml.bak.")
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
            assert!(!hermes_sweeps, "{name}");
            std::fs::write(tmp.path().join(name), "").unwrap();
        }
    }

    #[test]
    fn restore_recovers_from_the_state_copy_when_the_backup_vanishes() {
        // What actually happened: Hermes swept the backup out of its home
        // while the session ran, so the rename back had nothing to move.
        let tmp = tempfile::TempDir::new().unwrap();
        let (home, state) = dirs(&tmp);
        std::fs::write(home.join("config.yaml"), "orig: 1\n").unwrap();
        let mut swap = ConfigSwap::begin(
            &home,
            &state,
            &serde_json::json!({ "model": {} }),
            &CaptureUi::default(),
        )
        .unwrap();
        let backup = swap.backup.clone().unwrap();
        std::fs::remove_file(&backup).unwrap();

        let warning = swap.restore().unwrap().expect("a warning");
        assert!(warning.contains("disappeared"), "{warning}");
        assert_eq!(
            std::fs::read_to_string(home.join("config.yaml")).unwrap(),
            "orig: 1\n"
        );
    }

    #[test]
    fn next_backup_path_skips_existing_backups() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = tmp.path().join("config.yaml");
        assert_eq!(
            next_backup_path(&cfg),
            tmp.path().join("config.yaml.bak-granite-cli")
        );
        std::fs::write(tmp.path().join("config.yaml.bak-granite-cli"), "").unwrap();
        assert_eq!(
            next_backup_path(&cfg),
            tmp.path().join("config.yaml.bak-granite-cli.1")
        );
        std::fs::write(tmp.path().join("config.yaml.bak-granite-cli.1"), "").unwrap();
        assert_eq!(
            next_backup_path(&cfg),
            tmp.path().join("config.yaml.bak-granite-cli.2")
        );
    }

    #[test]
    fn swap_merges_over_existing_config_and_restores_it_byte_for_byte() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (home, state) = dirs(&tmp);
        let original = "# my notes\nvoice:\n  record_key: ctrl+b\n";
        std::fs::write(home.join("config.yaml"), original).unwrap();
        std::fs::write(home.join("config.yaml.bak"), "OLD BACKUP").unwrap();
        std::fs::write(home.join("config.yaml.bak-granite-cli"), "OLD OURS").unwrap();
        std::fs::write(home.join("auth.json"), "{}").unwrap();
        let ui = CaptureUi::default();

        let mut swap = ConfigSwap::begin(
            &home,
            &state,
            &serde_json::json!({ "model": { "default": "granite" } }),
            &ui,
        )
        .unwrap();

        let during = read_yaml(&home.join("config.yaml"));
        assert_eq!(during["model"]["default"], "granite");
        assert_eq!(during["voice"]["record_key"], "ctrl+b");
        // An existing backup is never clobbered; ours takes the next name.
        assert_eq!(
            std::fs::read_to_string(home.join("config.yaml.bak")).unwrap(),
            "OLD BACKUP"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("config.yaml.bak-granite-cli")).unwrap(),
            "OLD OURS"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("config.yaml.bak-granite-cli.1")).unwrap(),
            original
        );
        assert_eq!(
            std::fs::read_to_string(state.join(STATE_COPY_FILE)).unwrap(),
            original
        );
        // The rest of the home is untouched, and the user is told where things are.
        assert!(home.join("auth.json").exists());
        assert!(
            ui.warns
                .borrow()
                .iter()
                .any(|m| m.contains("config.yaml.bak-granite-cli.1"))
        );

        assert!(swap.restore().unwrap().is_none());
        assert_eq!(
            std::fs::read_to_string(home.join("config.yaml")).unwrap(),
            original
        );
        assert!(!home.join("config.yaml.bak-granite-cli.1").exists());
        assert_eq!(
            std::fs::read_to_string(home.join("config.yaml.bak")).unwrap(),
            "OLD BACKUP"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("config.yaml.bak-granite-cli")).unwrap(),
            "OLD OURS"
        );
    }

    #[test]
    fn swap_without_an_existing_config_removes_ours_on_restore() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (home, state) = dirs(&tmp);
        let mut swap = ConfigSwap::begin(
            &home,
            &state,
            &serde_json::json!({ "model": { "default": "x" } }),
            &CaptureUi::default(),
        )
        .unwrap();
        assert!(home.join("config.yaml").exists());
        swap.restore().unwrap();
        assert!(!home.join("config.yaml").exists());
    }

    #[test]
    fn swap_refuses_an_unparseable_config_without_touching_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (home, state) = dirs(&tmp);
        std::fs::write(home.join("config.yaml"), "a: [unclosed").unwrap();
        let err = ConfigSwap::begin(&home, &state, &serde_json::json!({}), &CaptureUi::default())
            .err()
            .expect("should fail");
        assert!(err.to_string().contains("not valid YAML"));
        assert_eq!(
            std::fs::read_to_string(home.join("config.yaml")).unwrap(),
            "a: [unclosed"
        );
        assert!(!home.join("config.yaml.bak-granite-cli").exists());
    }

    #[test]
    fn dropping_a_swap_restores_the_original() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (home, state) = dirs(&tmp);
        std::fs::write(home.join("config.yaml"), "keep: me\n").unwrap();
        drop(
            ConfigSwap::begin(
                &home,
                &state,
                &serde_json::json!({ "model": {} }),
                &CaptureUi::default(),
            )
            .unwrap(),
        );
        assert_eq!(
            std::fs::read_to_string(home.join("config.yaml")).unwrap(),
            "keep: me\n"
        );
    }

    #[test]
    fn restore_keeps_a_config_hermes_rewrote_during_the_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (home, state) = dirs(&tmp);
        std::fs::write(home.join("config.yaml"), "orig: 1\n").unwrap();
        let mut swap = ConfigSwap::begin(
            &home,
            &state,
            &serde_json::json!({ "model": {} }),
            &CaptureUi::default(),
        )
        .unwrap();
        std::fs::write(home.join("config.yaml"), "migrated: 1\n").unwrap();

        let warning = swap.restore().unwrap().expect("a warning");
        assert!(warning.contains("config.yaml.bak-granite-cli"));
        assert_eq!(
            std::fs::read_to_string(home.join("config.yaml")).unwrap(),
            "orig: 1\n"
        );
        // The migrated version is preserved, not lost.
        assert_eq!(
            std::fs::read_to_string(home.join("config.yaml.bak-granite-cli.1")).unwrap(),
            "migrated: 1\n"
        );
    }

    #[test]
    fn next_launch_recovers_from_a_session_that_never_restored() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (home, state) = dirs(&tmp);
        std::fs::write(home.join("config.yaml"), "orig: 1\n").unwrap();
        let swap = ConfigSwap::begin(
            &home,
            &state,
            &serde_json::json!({ "model": {} }),
            &CaptureUi::default(),
        )
        .unwrap();
        // Simulate a crash: Drop never runs.
        std::mem::forget(swap);
        assert!(has_swap_marker(&home.join("config.yaml")));

        let ui = CaptureUi::default();
        let mut second =
            ConfigSwap::begin(&home, &state, &serde_json::json!({ "x": 1 }), &ui).unwrap();
        assert!(
            ui.warns
                .borrow()
                .iter()
                .any(|m| m.contains("did not restore"))
        );
        second.restore().unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join("config.yaml")).unwrap(),
            "orig: 1\n"
        );
        // Our own leftover was never mistaken for the user's config.
        assert!(!home.join("config.yaml.bak-granite-cli.1").exists());
    }

    #[tokio::test]
    async fn dry_run_launch_reports_without_writing_anything() {
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
                .any(|m| m.contains("Would merge these settings")),
            "expected a dry-run notice, got {infos:?}"
        );
        assert!(
            infos.iter().any(|m| {
                m.contains(r#""default": "granite4.1:8b""#) && m.contains(r#""provider": "custom""#)
            }),
            "expected the generated config to select the model, got {infos:?}"
        );
        assert!(
            infos.iter().any(|m| m.contains("args: --help")),
            "expected caller args to pass through unmodified, got {infos:?}"
        );
        assert!(
            ui.warns.borrow().is_empty(),
            "a dry run must not swap anything"
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
        assert!(infos.iter().any(|m| m.contains("--version")));
        assert!(
            !infos
                .iter()
                .any(|m| m.contains("Would merge these settings")),
            "expected no config write without binding"
        );
        // Without a binding there is no generated config, so hermes keeps
        // using its own config chain.
        assert!(!infos.iter().any(|m| m.contains(HERMES_HOME_ENV)));
    }
}
