// Standard
use std::collections::HashMap;
use std::fs;
use std::time::SystemTime;

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use anyhow::Context;
use serde::{Deserialize, Serialize};

// Local
use crate::capabilities::Dependency;
use crate::config::{self, Config};
use crate::proxy::UsageStats;

use_channel!("SESS");

/*-- public --*/

/// Metadata about one capability binding within a session.
/// Resolved from the capability's config to include explicit linkage to the
/// model and provider it uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCapabilityMeta {
    pub capability_id: String,
    #[serde(rename = "type")]
    pub capability_type: String,
    /// Model slots from the capability config, keyed by their dependency
    /// `config_key`. E.g. `{ "model_id": "my-model" }`. Empty when the
    /// capability has no model dependencies (e.g. a tool-only capability).
    pub models: HashMap<String, String>,
    /// Resolved model type from the first (or sole) model in `models`.
    /// `None` when `models` is empty or has no matching entry.
    pub model_type: Option<String>,
    /// Resolved provider id from the first (or sole) model in `models`.
    /// `None` when `models` is empty or has no matching entry.
    pub provider_id: Option<String>,
    /// Resolved provider type from the first (or sole) model in `models`.
    /// `None` when `models` is empty or has no matching entry.
    pub provider_type: Option<String>,
}

/// Full metadata for one launch session, persisted as `{session_id}.yaml`
/// under `GRANITE_CLI_HOME/sessions/`.
///
/// The `usage` field is updated reactively: a background writer task wakes on
/// every proxy `record()` call (via a watch channel notifier) and flushes the
/// latest snapshot to disk. A final flush occurs at clean shutdown. The file
/// therefore reflects near-current totals throughout the session and survives
/// even an abrupt termination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Unique identifier for this session. Format:
    /// `{escaped_cwd}@{YYYYMMDDTHHMMSS}_{hex8}`
    /// where path separators in `cwd` are replaced with `"---"`.
    pub session_id: String,
    /// When the session was launched (`YYYYMMDDTHHMMSS` in UTC).
    pub launched_at: String,
    /// When the session finished (`YYYYMMDDTHHMMSS` in UTC). `None` while the
    /// session is still running or if it was terminated abruptly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// When the session file was last written (`YYYYMMDDTHHMMSS` in UTC).
    /// Updated on every write (create, usage update, finish). Used to detect
    /// stale sessions that were never cleanly closed.
    #[serde(default)]
    pub updated_at: String,
    /// The working directory at session start.
    pub working_dir: String,
    /// The full CLI invocation that started this session.
    pub full_command: Vec<String>,
    /// The launcher ID that was launched.
    pub launcher_id: String,
    /// The launcher type (e.g. `"claude"`, `"opencode"`).
    pub launcher_type: String,
    /// Capability metadata ordered to match the launcher's `enabled_capabilities`.
    pub capabilities: Vec<SessionCapabilityMeta>,
    /// Usage snapshot keyed by binding label (same keys as in `UsageTracker`).
    /// Populated after the first proxy response; empty until then.
    pub usage: HashMap<String, UsageStats>,
}

/// Generate a unique session ID for this launch.
///
/// Format: `{escaped_cwd}@{YYYYMMDDTHHMMSS}_{hex8}`
/// where `escaped_cwd` has path separators and drive-letter colons replaced
/// with `"---"`, and `hex8` is 8 hex characters derived from the current
/// nanosecond timestamp, process ID, and a per-process monotonic counter.
pub fn generate_session_id() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let escaped_cwd = escape_working_dir(&cwd);
    alog_channel!(
        MessageLevel::Debug2,
        "Escaped CWD {} -> {}",
        cwd,
        escaped_cwd
    );

    let epoch = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time is before Unix epoch");

    let timestamp = format_utc_timestamp(epoch);
    let suffix = short_suffix(epoch);

    format!("{escaped_cwd}@{timestamp}_{suffix}")
}

/// Build a [`SessionCapabilityMeta`] from a configured capability.
///
/// Extracts the capability's model slots from `cap_cfg.config` using its
/// `dependencies` to determine the expected `config_key` names, then looks up
/// each resolved `ModelConfig` in `config.models` to obtain `model_type`
/// and `provider_id`, and finally looks up the `ProviderConfig` to obtain
/// `provider_type`. Any missing link is represented as `None`.
pub fn build_capability_meta(
    cap_cfg: &config::CapabilityConfig,
    dependencies: &[Dependency],
    config: &Config,
) -> SessionCapabilityMeta {
    let models = crate::utils::capability_model_ids(cap_cfg, dependencies);

    // Resolve model_type and provider_id from the first model slot for
    // backwards-compatibility metadata fields.
    let (model_type, provider_id) = models
        .values()
        .next()
        .and_then(|mid| config.models.get(mid.as_str()))
        .map(|mc| (Some(mc.model_type.clone()), Some(mc.provider_id.clone())))
        .unwrap_or((None, None));

    let provider_type = provider_id
        .as_deref()
        .and_then(|pid| config.providers.get(pid))
        .map(|pc| pc.provider_type.clone());

    SessionCapabilityMeta {
        capability_id: cap_cfg.capability_id.clone(),
        capability_type: cap_cfg.capability_type.clone(),
        models,
        model_type,
        provider_id,
        provider_type,
    }
}

/// Create a new [`SessionMeta`] for a launch.
///
/// `capabilities_with_deps` should be the ordered list of `(CapabilityConfig,
/// Vec<Dependency>)` tuples corresponding to `launcher_config.enabled_capabilities`,
/// where the dependencies come from `CAPABILITY_REGISTRY`.
pub fn create_session_meta(
    session_id: &str,
    config: &Config,
    launcher_config: &config::LauncherConfig,
    capabilities_with_deps: &[(config::CapabilityConfig, Vec<Dependency>)],
) -> SessionMeta {
    let launched_at = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(format_utc_timestamp)
        .unwrap_or_else(|_| "unknown".to_string());

    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let capabilities_meta = capabilities_with_deps
        .iter()
        .map(|(cap_cfg, deps)| build_capability_meta(cap_cfg, deps, config))
        .collect();

    SessionMeta {
        session_id: session_id.to_string(),
        launched_at: launched_at.clone(),
        finished_at: None,
        updated_at: launched_at,
        working_dir,
        full_command: std::env::args().collect(),
        launcher_id: launcher_config.launcher_id.clone(),
        launcher_type: launcher_config.launcher_type.clone(),
        capabilities: capabilities_meta,
        usage: HashMap::new(),
    }
}

/// Write `session_meta` to `{sessions_dir}/{session_id}.yaml`.
///
/// Best-effort: callers that don't want to fail the session on a write error
/// should use `.ok()` on the return value.
pub fn write_session_file(session_meta: &SessionMeta) -> anyhow::Result<()> {
    let sessions_dir = Config::sessions_dir()?;
    fs::create_dir_all(&sessions_dir)?;
    let path = sessions_dir.join(format!("{}.yaml", session_meta.session_id));
    let content = serde_yaml::to_string(session_meta)
        .with_context(|| "failed to serialize session metadata")?;
    fs::write(&path, content)
        .with_context(|| format!("failed to write session file: {}", path.display()))?;
    alog_channel!(
        MessageLevel::Debug3,
        "session file written: {}",
        path.display()
    );
    Ok(())
}

/// Update the `usage` field in an existing session file.
///
/// Reads the current file, replaces `usage` with the new snapshot, and
/// writes it back. Best-effort: callers should use `.ok()` on the return
/// value if a write failure must not interrupt the session.
pub fn update_session_usage(
    session_id: &str,
    usage: &HashMap<String, UsageStats>,
) -> anyhow::Result<()> {
    update_session_file(session_id, |meta| {
        meta.usage = usage.clone();
    })
}

/// Set `finished_at` and write the final `usage` snapshot in a single file
/// update. Called once at clean session teardown.
pub fn finish_session(session_id: &str, usage: &HashMap<String, UsageStats>) -> anyhow::Result<()> {
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(format_utc_timestamp)
        .unwrap_or_else(|_| "unknown".to_string());
    update_session_file(session_id, |meta| {
        meta.usage = usage.clone();
        meta.finished_at = Some(now);
    })
}

/// Read a session file by its ID.
pub fn read_session_file(session_id: &str) -> anyhow::Result<SessionMeta> {
    let sessions_dir = Config::sessions_dir()?;
    let path = sessions_dir.join(format!("{session_id}.yaml"));
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read session file: {}", path.display()))?;
    let meta: SessionMeta = serde_yaml::from_str(&content)
        .with_context(|| format!("failed to parse session file: {}", path.display()))?;
    Ok(meta)
}

/*-- private --*/

/// Read-modify-write a session file, applying `f` to the parsed [`SessionMeta`]
/// before writing it back. Shared implementation for [`update_session_usage`]
/// and [`finish_session`].
fn update_session_file(session_id: &str, f: impl FnOnce(&mut SessionMeta)) -> anyhow::Result<()> {
    let sessions_dir = Config::sessions_dir()?;
    let path = sessions_dir.join(format!("{session_id}.yaml"));
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read session file: {}", path.display()))?;
    let mut meta: SessionMeta = serde_yaml::from_str(&content)
        .with_context(|| format!("failed to parse session file: {}", path.display()))?;
    f(&mut meta);
    meta.updated_at = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(format_utc_timestamp)
        .unwrap_or_else(|_| "unknown".to_string());
    let updated = serde_yaml::to_string(&meta)
        .with_context(|| "failed to serialize updated session metadata")?;
    fs::write(&path, updated)
        .with_context(|| format!("failed to update session file: {}", path.display()))?;
    alog_channel!(
        MessageLevel::Debug4,
        "session file updated: {}",
        path.display()
    );
    Ok(())
}

fn escape_working_dir(working_dir: &str) -> String {
    working_dir.replace(['/', '\\', ':'], config::PATH_DELIM)
}

/// Counter used by [`short_suffix`] to guarantee uniqueness across calls
/// within the same process.
static SUFFIX_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Format a `Duration` since the Unix epoch as `YYYYMMDDTHHMMSS` (UTC, no
/// colons or timezone designator — safe as a filename component).
fn format_utc_timestamp(epoch: std::time::Duration) -> String {
    let secs = epoch.as_secs() as i64;
    chrono::DateTime::from_timestamp(secs, 0)
        .expect("timestamp out of range")
        .format("%Y%m%dT%H%M%S")
        .to_string()
}

/// Generate an 8-character lowercase hex suffix unique within this process.
///
/// Combines the low 32 bits of the nanosecond timestamp with the process ID
/// and a per-process monotonic counter so that two calls in the same
/// nanosecond still produce different results.
fn short_suffix(epoch: std::time::Duration) -> String {
    let nanos = epoch.as_nanos() as u64;
    let pid = std::process::id() as u64;
    let counter = SUFFIX_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let combined = (nanos ^ pid.wrapping_mul(0x9e37_79b9) ^ counter) & 0xFFFF_FFFF;
    format!("{combined:08x}")
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CapabilityConfig, Config, ModelConfig, ProviderConfig, TestConfigHome};

    #[test]
    fn generate_session_id_contains_at_separator_and_hex_suffix() {
        let id = generate_session_id();
        assert!(id.contains('@'), "session id must contain '@': {id}");
        let suffix = id.split('_').next_back().unwrap_or("");
        assert_eq!(suffix.len(), 8, "suffix must be 8 chars: {id}");
        assert!(
            suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "suffix must be hex: {id}"
        );
    }

    #[test]
    fn consecutive_session_ids_are_distinct() {
        let a = generate_session_id();
        let b = generate_session_id();
        assert_ne!(a, b, "consecutive session IDs must differ");
    }

    #[test]
    fn windows_working_dir_produces_a_flat_session_filename() {
        let _home = TestConfigHome::new();
        let escaped = escape_working_dir(r"C:\Users\alice\project");
        assert_eq!(escaped, "C------Users---alice---project");

        let session_id = format!("{escaped}@20250101T120000_deadbeef");
        write_session_file(&make_meta(&session_id, "claude")).unwrap();
        assert!(
            Config::sessions_dir()
                .unwrap()
                .join(format!("{session_id}.yaml"))
                .is_file()
        );
        assert_eq!(
            read_session_file(&session_id).unwrap().session_id,
            session_id
        );
    }

    #[test]
    fn format_utc_timestamp_epoch_zero() {
        let ts = format_utc_timestamp(std::time::Duration::from_secs(0));
        assert_eq!(ts, "19700101T000000");
    }

    #[test]
    fn format_utc_timestamp_known_date() {
        // 2025-01-01 00:00:00 UTC = 1735689600 seconds since epoch
        let ts = format_utc_timestamp(std::time::Duration::from_secs(1_735_689_600));
        assert_eq!(ts, "20250101T000000");
    }

    #[test]
    fn build_capability_meta_full_resolution() {
        let mut config = Config::default();
        config.models.insert(
            "my-model".to_string(),
            ModelConfig {
                model_id: "my-model".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                provider_id: "my-ollama".to_string(),
                variant: None,
                config: serde_json::json!({}),
            },
        );
        config.providers.insert(
            "my-ollama".to_string(),
            ProviderConfig {
                provider_id: "my-ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({}),
            },
        );

        let cap_cfg = CapabilityConfig {
            capability_id: "chat".to_string(),
            capability_type: "agent-model".to_string(),
            config: serde_json::json!({ "model_id": "my-model" }),
        };
        let dependencies = vec![Dependency::Model {
            config_key: "model_id".to_string(),
            required: true,
            requirement: crate::capabilities::ModelRequirement::default(),
            resolved_id: None,
        }];

        let meta = build_capability_meta(&cap_cfg, &dependencies, &config);
        assert_eq!(meta.capability_id, "chat");
        assert_eq!(meta.capability_type, "agent-model");
        assert_eq!(
            meta.models.get("model_id").map(|s| s.as_str()),
            Some("my-model")
        );
        assert_eq!(meta.model_type.as_deref(), Some("granite-3.1-8b-instruct"));
        assert_eq!(meta.provider_id.as_deref(), Some("my-ollama"));
        assert_eq!(meta.provider_type.as_deref(), Some("ollama"));
    }

    #[test]
    fn build_capability_meta_missing_model_id_yields_empty_models() {
        let config = Config::default();
        let cap_cfg = CapabilityConfig {
            capability_id: "chat".to_string(),
            capability_type: "agent-model".to_string(),
            config: serde_json::json!({}),
        };
        let dependencies = vec![Dependency::Model {
            config_key: "model_id".to_string(),
            required: false,
            requirement: crate::capabilities::ModelRequirement::default(),
            resolved_id: None,
        }];
        let meta = build_capability_meta(&cap_cfg, &dependencies, &config);
        assert!(meta.models.is_empty());
        assert!(meta.model_type.is_none());
        assert!(meta.provider_id.is_none());
        assert!(meta.provider_type.is_none());
    }

    #[test]
    fn build_capability_meta_unknown_model_id_yields_nones_for_type_and_provider() {
        let config = Config::default();
        let cap_cfg = CapabilityConfig {
            capability_id: "chat".to_string(),
            capability_type: "agent-model".to_string(),
            config: serde_json::json!({ "model_id": "nonexistent" }),
        };
        let dependencies = vec![Dependency::Model {
            config_key: "model_id".to_string(),
            required: false,
            requirement: crate::capabilities::ModelRequirement::default(),
            resolved_id: None,
        }];
        let meta = build_capability_meta(&cap_cfg, &dependencies, &config);
        assert_eq!(
            meta.models.get("model_id").map(|s| s.as_str()),
            Some("nonexistent")
        );
        assert!(meta.model_type.is_none());
        assert!(meta.provider_id.is_none());
        assert!(meta.provider_type.is_none());
    }

    #[test]
    fn session_meta_round_trips_yaml() {
        let meta = SessionMeta {
            session_id: "test---home@20250101T120000_abcd1234".to_string(),
            launched_at: "20250101T120000".to_string(),
            finished_at: None,
            updated_at: "20250101T120000".to_string(),
            working_dir: "/home/test".to_string(),
            full_command: vec![
                "granite-cli".to_string(),
                "launch".to_string(),
                "claude".to_string(),
            ],
            launcher_id: "claude".to_string(),
            launcher_type: "claude".to_string(),
            capabilities: vec![],
            usage: HashMap::new(),
        };

        let yaml = serde_yaml::to_string(&meta).unwrap();
        let back: SessionMeta = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.session_id, meta.session_id);
        assert_eq!(back.launcher_id, meta.launcher_id);
        assert_eq!(back.full_command, meta.full_command);
        assert!(back.finished_at.is_none());
        assert!(back.usage.is_empty());
    }

    #[test]
    fn session_meta_finished_at_is_omitted_when_none() {
        let meta = SessionMeta {
            session_id: "s".to_string(),
            launched_at: "20250101T120000".to_string(),
            finished_at: None,
            updated_at: "20250101T120000".to_string(),
            working_dir: "/".to_string(),
            full_command: vec![],
            launcher_id: "claude".to_string(),
            launcher_type: "claude".to_string(),
            capabilities: vec![],
            usage: HashMap::new(),
        };
        let yaml = serde_yaml::to_string(&meta).unwrap();
        assert!(
            !yaml.contains("finished_at"),
            "finished_at must be absent from YAML when None: {yaml}"
        );
    }

    fn make_meta(session_id: &str, launcher_id: &str) -> SessionMeta {
        SessionMeta {
            session_id: session_id.to_string(),
            launched_at: "20250101T120000".to_string(),
            finished_at: None,
            updated_at: "20250101T120000".to_string(),
            working_dir: "/test".to_string(),
            full_command: vec!["granite-cli".to_string(), "launch".to_string()],
            launcher_id: launcher_id.to_string(),
            launcher_type: launcher_id.to_string(),
            capabilities: vec![],
            usage: HashMap::new(),
        }
    }

    #[test]
    fn write_and_read_session_file_round_trips() {
        let _home = TestConfigHome::new();
        Config::ensure_directories_for_test();

        let session_id = "test---home@20250101T120000_deadbeef";
        write_session_file(&make_meta(session_id, "claude")).unwrap();
        let read_back = read_session_file(session_id).unwrap();
        assert_eq!(read_back.session_id, session_id);
        assert_eq!(read_back.launcher_id, "claude");
        assert!(read_back.finished_at.is_none());
    }

    #[test]
    fn update_session_usage_writes_new_usage_and_preserves_other_fields() {
        let _home = TestConfigHome::new();
        Config::ensure_directories_for_test();

        let session_id = "test---home@20250101T130000_cafebabe";
        write_session_file(&make_meta(session_id, "opencode")).unwrap();

        let mut usage = HashMap::new();
        usage.insert(
            "agent".to_string(),
            UsageStats {
                requests: 3,
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_tokens: 0,
                cache_read_tokens: 10,
            },
        );
        update_session_usage(session_id, &usage).unwrap();

        let updated = read_session_file(session_id).unwrap();
        assert_eq!(updated.launcher_id, "opencode");
        // update_session_usage must not set finished_at
        assert!(updated.finished_at.is_none());
        let agent_usage = updated.usage.get("agent").unwrap();
        assert_eq!(agent_usage.requests, 3);
        assert_eq!(agent_usage.input_tokens, 100);
    }

    #[test]
    fn finish_session_sets_finished_at_and_final_usage() {
        let _home = TestConfigHome::new();
        Config::ensure_directories_for_test();

        let session_id = "test---home@20250101T140000_f1n15hed";
        write_session_file(&make_meta(session_id, "claude")).unwrap();

        let mut usage = HashMap::new();
        usage.insert(
            "main".to_string(),
            UsageStats {
                requests: 7,
                input_tokens: 200,
                output_tokens: 80,
                cache_creation_tokens: 5,
                cache_read_tokens: 15,
            },
        );
        finish_session(session_id, &usage).unwrap();

        let finished = read_session_file(session_id).unwrap();
        assert!(
            finished.finished_at.is_some(),
            "finished_at must be set after finish_session"
        );
        let ts = finished.finished_at.unwrap();
        assert_eq!(ts.len(), 15, "timestamp must be YYYYMMDDTHHMMSS: {ts}");
        assert!(ts.contains('T'), "timestamp must contain 'T': {ts}");
        assert_eq!(finished.usage.get("main").unwrap().requests, 7);
        // launched_at must be preserved unchanged
        assert_eq!(finished.launched_at, "20250101T120000");
    }

    #[test]
    fn session_meta_round_trips_yaml_with_updated_at() {
        let meta = SessionMeta {
            session_id: "s".to_string(),
            launched_at: "20250101T120000".to_string(),
            finished_at: None,
            updated_at: "20250101T120500".to_string(),
            working_dir: "/".to_string(),
            full_command: vec![],
            launcher_id: "claude".to_string(),
            launcher_type: "claude".to_string(),
            capabilities: vec![],
            usage: HashMap::new(),
        };
        let yaml = serde_yaml::to_string(&meta).unwrap();
        let back: SessionMeta = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.updated_at, "20250101T120500");
    }

    #[test]
    fn session_meta_updated_at_defaults_to_empty_string_when_missing_from_yaml() {
        // Simulate an old session file that has no updated_at field.
        let yaml = "session_id: s\nlaunched_at: 20250101T120000\nworking_dir: /\nfull_command: []\nlauncher_id: claude\nlauncher_type: claude\ncapabilities: []\nusage: {}\n";
        let meta: SessionMeta = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            meta.updated_at, "",
            "updated_at must default to empty string for old files"
        );
    }

    #[test]
    fn update_session_file_stamps_updated_at() {
        let _home = TestConfigHome::new();
        Config::ensure_directories_for_test();

        let session_id = "test---home@20250101T150000_updcheck";
        write_session_file(&make_meta(session_id, "claude")).unwrap();

        let usage = HashMap::new();
        update_session_usage(session_id, &usage).unwrap();

        let updated = read_session_file(session_id).unwrap();
        assert!(
            !updated.updated_at.is_empty(),
            "updated_at must be set after update_session_usage"
        );
        assert_eq!(
            updated.updated_at.len(),
            15,
            "updated_at must be YYYYMMDDTHHMMSS"
        );
        assert!(updated.updated_at.contains('T'));
    }
}
