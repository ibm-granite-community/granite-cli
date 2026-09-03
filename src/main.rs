pub mod capabilities;
pub mod commands;
pub mod config;
pub mod dependency;
pub mod launchers;
pub mod models;
pub mod proxy;
pub mod registry;
pub mod utils;
pub mod version {
    include!(concat!(env!("OUT_DIR"), "/version.rs"));
}
// TODO: Re-enable once rewritten -- pub mod di;
pub mod providers;

// Third Party
use alog::{MessageLevel, alog};
use clap::{Parser, Subcommand};

// Local
use commands::{
    CapabilityCommands, HardwareCommands, LauncherCommands, ModelCommands, ProviderCommands,
    SetupCommands,
};
use utils::ui::{UI_REGISTRY, Ui, run_interactive_tui};

// Hoist paste macro for use in our own macros
extern crate paste;

#[derive(Parser, Debug)]
#[command(name = "granite-cli")]
#[command(about = "Universal Model Adapter with Capabilities", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Default logging level
    #[arg(
        short,
        long,
        global = true,
        default_value = "warning",
        env = "LOG_LEVEL"
    )]
    log_level: String,
    /// Per-level overrides
    #[arg(long, global = true, default_value = "", env = "LOG_FILTERS")]
    log_filters: String,
    /// Log with json format
    #[arg(long, global = true, env = "LOG_JSON")]
    log_json: bool,
    /// Include thread ID in log lines
    #[arg(long, global = true, env = "LOG_THREAD_ID")]
    log_thread_id: bool,
}

#[derive(clap::Args, Debug)]
struct ModelWithOutput {
    /// Output format: terminal (default), plain, json, markdown
    #[arg(short, long, global = true, default_value = "terminal")]
    output: String,

    #[command(subcommand)]
    subcommand: ModelSubcommands,
}

#[derive(clap::Args, Debug)]
struct CapabilityWithOutput {
    /// Output format: terminal (default), plain, json, markdown
    #[arg(short, long, global = true, default_value = "terminal")]
    output: String,

    #[command(subcommand)]
    subcommand: CapabilitySubcommands,
}

#[derive(clap::Args, Debug)]
struct ProviderWithOutput {
    /// Output format: terminal (default), plain, json, markdown
    #[arg(short, long, global = true, default_value = "terminal")]
    output: String,

    #[command(subcommand)]
    subcommand: ProviderSubcommands,
}

#[derive(clap::Args, Debug)]
struct LaunchWithOutput {
    /// Output format: terminal (default), plain, json, markdown
    #[arg(short, long, global = true, default_value = "terminal")]
    output: String,

    /// Launcher ID to launch
    launcher_id: String,

    /// Show overlay without launching
    #[arg(long)]
    dry_run: bool,

    /// Track token usage (input, output, and cache where available) via a
    /// local proxy sitting between the launched agent and its configured
    /// model. Off by default.
    #[arg(short = 'u', long = "usage-tracking")]
    usage_tracking: bool,

    /// Additional arguments to pass to the launcher
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,
}

#[derive(clap::Args, Debug)]
struct LauncherWithOutput {
    /// Output format: terminal (default), plain, json, markdown
    #[arg(short, long, global = true, default_value = "terminal")]
    output: String,

    #[command(subcommand)]
    subcommand: LauncherSubcommands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Model management commands
    Model(ModelWithOutput),

    /// Capability management commands
    Capability(CapabilityWithOutput),

    /// Provider management commands
    Provider(ProviderWithOutput),

    /// Launcher management commands
    Launcher(LauncherWithOutput),

    /// Unified setup wizard: discover and configure providers, models, launchers,
    /// and capabilities in a single guided flow.
    Setup {
        /// Auto-detect and configure everything that can be auto-configured.
        /// Consent is implied. Uses registry defaults for all config fields.
        #[arg(long)]
        auto: bool,

        /// Skip the model weight pull prompt at the end of the wizard.
        /// Model weights are never auto-pulled in --auto mode regardless.
        #[arg(long)]
        skip_pull: bool,
    },

    /// Show hardware profile and recommended precision
    Hardware,

    /// Launch a configured launcher with Granite overlay
    Launch(LaunchWithOutput),

    /// Show version information
    Version,
}

#[derive(Subcommand, Debug)]
enum ModelSubcommands {
    /// Show the catalog of all available models
    Catalog {
        /// Filter by model type
        #[arg(short, long)]
        r#type: Option<String>,
    },

    /// List all configured models
    List {
        /// Filter by model type
        #[arg(short, long)]
        r#type: Option<String>,
    },

    /// Search the model catalog by ID or family
    Search {
        /// Case-insensitive substring to search for
        query: String,
    },

    /// Recommend models that fit current hardware
    Recommend {
        /// Filter by model type
        #[arg(short, long)]
        r#type: Option<String>,

        /// Configured provider id(s) to check against (comma-separated or
        /// repeatable), or "all" to skip the provider check and show every
        /// model that fits the hardware regardless of configured providers
        #[arg(short = 'p', long = "providers", value_delimiter = ',')]
        providers: Vec<String>,

        /// Show all columns, including family and full context length
        #[arg(long)]
        wide: bool,
    },

    /// Show detailed model information
    Info {
        /// Model ID
        model_id: String,
    },

    /// Interactive model setup wizard
    Setup {
        /// Catalog model type to set up (e.g. `granite-3.1-8b-instruct`, or `custom`)
        model_type: String,

        /// Nickname for this model instance. Defaults to `model_type`;
        /// pass a distinct value to configure multiple named instances of
        /// the same catalog type (e.g. two precisions of the same model
        /// against different providers, or several custom models).
        #[arg(long = "id")]
        instance_id: Option<String>,
    },

    /// Pull (download) a configured model's weights via its provider
    Pull {
        /// Model ID to pull
        model_id: String,
    },

    /// Remove a configured model instance
    Remove {
        /// Configured model ID to remove
        model_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum CapabilitySubcommands {
    /// Show the catalog of all available capabilities
    Catalog,

    /// List all configured capabilities
    List,

    /// Show detailed capability information
    Info {
        /// Capability ID
        capability_id: String,
    },

    /// Interactive capability setup wizard
    Setup {
        /// Catalog capability type to set up (e.g. `agent-model`)
        capability_type: String,

        /// Nickname for this capability instance. Defaults to
        /// `capability_type`; pass a distinct value to configure multiple
        /// named instances of the same catalog type (e.g. `--id chat`).
        #[arg(long = "id")]
        instance_id: Option<String>,
    },

    /// Remove a configured capability instance
    Remove {
        /// Configured capability ID to remove
        capability_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ProviderSubcommands {
    /// Show the catalog of all available providers
    Catalog {
        /// Show all columns, including description and endpoints
        #[arg(long)]
        wide: bool,
    },

    /// List all configured providers
    List,

    /// Interactive provider setup wizard
    Setup {
        /// Catalog provider type to set up (e.g. `openai-compatible`)
        provider_type: String,

        /// Nickname for this provider instance. Defaults to `provider_type`;
        /// pass a distinct value to configure multiple named instances of
        /// the same catalog type (e.g. `--id ollama`, `--id lm-studio`).
        #[arg(long = "id")]
        instance_id: Option<String>,
    },

    /// Check provider health
    Health {
        /// Provider ID (optional, checks all if not specified)
        provider_id: Option<String>,
    },

    /// Remove a configured provider instance
    Remove {
        /// Configured provider instance ID to remove
        provider_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum LauncherSubcommands {
    /// Show the catalog of all available launcher types
    Catalog,

    /// List all configured launcher instances
    List,

    /// Interactive launcher setup wizard
    Setup {
        /// Catalog launcher type to set up (e.g. `claude`)
        launcher_type: String,

        /// Nickname for this launcher instance. Defaults to `launcher_type`;
        /// pass a distinct value to configure multiple named instances of
        /// the same catalog type (e.g. `--id claude-local`).
        #[arg(long = "id")]
        instance_id: Option<String>,
    },

    /// Remove a configured launcher instance
    Remove {
        /// Configured launcher instance ID to remove
        launcher_id: String,
    },
}

pub struct AppContext {
    pub config: config::Config,
    pub ui: std::sync::Arc<dyn Ui>,
}

/// Construct the `Ui` backend for `--output`, exiting on an unrecognized
/// format. No `Ui` exists yet at this point, so this is the one place in
/// `main` that still reports via `eprintln!` rather than `ctx.ui`.
fn construct_ui(output: &str) -> Box<dyn Ui> {
    UI_REGISTRY
        .construct(
            output,
            output,
            &serde_json::json!({}),
            &crate::config::Config::default(),
        )
        .unwrap_or_else(|_| {
            eprintln!("Unknown output format '{output}'. Valid: terminal, plain, json, markdown");
            std::process::exit(1);
        })
}

fn construct_context(
    output: &str,
    log_level: &str,
    log_filters: &str,
    log_json: bool,
    log_thread_id: bool,
) -> AppContext {
    // Set up Ui
    let ui = construct_ui(output);
    let ui: std::sync::Arc<dyn Ui> = std::sync::Arc::from(ui);

    // Configure logging
    let formatter_kind = if log_json {
        alog::FormatterKind::Json
    } else {
        alog::FormatterKind::Pretty
    };
    let ui_arc_clone = Arc::clone(&ui);
    let ui_writer = UiWriter {
        ui: Arc::clone(&ui),
    };
    alog::configure(alog::Config {
        default_level: log_level.parse().unwrap(),
        filters: alog::Filters::Spec(log_filters.to_string()),
        formatter: alog::FormatterKind::Custom(Box::new(UiFormatter::new(
            formatter_kind,
            ui_arc_clone,
        ))),
        writer: alog::Writer::Custom(Box::new(ui_writer)),
        thread_id: log_thread_id,
    });
    alog!("MAIN", MessageLevel::Debug, "Welcome to granite-cli!");

    // Initialize config
    let config = config::Config::new().unwrap_or_else(|e| {
        ui.error(&format!("Failed to load config: {e}"));
        std::process::exit(1);
    });
    AppContext { config, ui }
}

/*-- private --*/

use std::io::{self, Write};
use std::sync::Arc;

/// A log sink that routes formatted records through a [`Ui`] backend.
struct UiWriter {
    ui: Arc<dyn Ui>,
}

impl Write for UiWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = std::str::from_utf8(buf).unwrap_or("");
        // The formatter adds a trailing newline; split and route each line.
        for line in text.split('\n') {
            if line.is_empty() {
                continue;
            }
            self.ui.info(line);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Wraps an alog formatter and delegates to it.
struct UiFormatter {
    inner: Box<dyn alog::Formatter>,
    ui: Arc<dyn Ui>,
}

impl UiFormatter {
    fn new(kind: alog::FormatterKind, ui: Arc<dyn Ui>) -> Self {
        Self {
            inner: match kind {
                alog::FormatterKind::Pretty => Box::new(alog::PrettyFormatter::default()),
                alog::FormatterKind::Json => Box::new(alog::JsonFormatter),
                alog::FormatterKind::Custom(c) => c,
            },
            ui,
        }
    }
}

impl alog::Formatter for UiFormatter {
    fn format(&self, record: &alog::LogRecord<'_>) -> String {
        let formatted = self.inner.format(record).trim_end_matches('\n').to_string();
        match record.level {
            MessageLevel::Fatal | MessageLevel::Error => self.ui.error_mark(&formatted),
            MessageLevel::Warning => self.ui.warn_mark(&formatted),
            MessageLevel::Info => formatted,
            _ => self.ui.detail_mark(&formatted),
        }
    }
}

#[tokio::main]
async fn main() {
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = dialoguer::console::Term::stderr().show_cursor();
            std::process::exit(130);
        }
    });

    let cli = Cli::parse();
    let log_level = cli.log_level.clone();
    let log_filters = cli.log_filters.clone();
    let log_json = cli.log_json;
    let log_thread_id = cli.log_thread_id;
    let command = cli.command;

    let result: Result<(), ()> = match command {
        Some(Commands::Model(wrapper)) => {
            let mut ctx = construct_context(
                &wrapper.output,
                &log_level,
                &log_filters,
                log_json,
                log_thread_id,
            );
            run_model_command(&mut ctx, wrapper.subcommand)
                .await
                .map_err(|e| ctx.ui.error(&e.to_string()))
        }
        Some(Commands::Capability(wrapper)) => {
            let mut ctx = construct_context(
                &wrapper.output,
                &log_level,
                &log_filters,
                log_json,
                log_thread_id,
            );
            run_capability_command(&mut ctx, wrapper.subcommand)
                .await
                .map_err(|e| ctx.ui.error(&e.to_string()))
        }
        Some(Commands::Provider(wrapper)) => {
            let mut ctx = construct_context(
                &wrapper.output,
                &log_level,
                &log_filters,
                log_json,
                log_thread_id,
            );
            run_provider_command(&mut ctx, wrapper.subcommand)
                .await
                .map_err(|e| ctx.ui.error(&e.to_string()))
        }
        Some(Commands::Hardware) => {
            let ctx = construct_context(
                "terminal",
                &log_level,
                &log_filters,
                log_json,
                log_thread_id,
            );
            HardwareCommands::show(&ctx).map_err(|e| ctx.ui.error(&e.to_string()))
        }
        Some(Commands::Launcher(wrapper)) => {
            let mut ctx = construct_context(
                &wrapper.output,
                &log_level,
                &log_filters,
                log_json,
                log_thread_id,
            );
            run_launcher_command(&mut ctx, wrapper.subcommand)
                .await
                .map_err(|e| ctx.ui.error(&e.to_string()))
        }
        Some(Commands::Setup { auto, skip_pull }) => {
            let mut ctx = construct_context(
                "terminal",
                &log_level,
                &log_filters,
                log_json,
                log_thread_id,
            );
            SetupCommands::run(&mut ctx, auto, skip_pull)
                .await
                .map_err(|e| ctx.ui.error(&e.to_string()))
        }
        Some(Commands::Launch(wrapper)) => {
            let ctx = construct_context(
                &wrapper.output,
                &log_level,
                &log_filters,
                log_json,
                log_thread_id,
            );
            run_launch(
                &*ctx.ui,
                &wrapper.launcher_id,
                &wrapper.args,
                wrapper.dry_run,
                wrapper.usage_tracking,
            )
            .await
            .map_err(|e| ctx.ui.error(&e.to_string()))
        }
        Some(Commands::Version) => {
            // Version info is simple text — no UI backend or config needed.
            println!("{}", version::version_string());
            Ok(())
        }
        None => {
            // `ctx` (and its `ui`) is consumed by value into the TUI `App`
            // before any error can occur, so it can't be used to report one.
            let ctx = construct_context(
                "terminal",
                &log_level,
                &log_filters,
                log_json,
                log_thread_id,
            );
            run_interactive_tui(ctx)
                .await
                .map_err(|e| eprintln!("Error: {e}"))
        }
    };

    if result.is_err() {
        std::process::exit(1);
    }
}

async fn run_model_command(ctx: &mut AppContext, subcmd: ModelSubcommands) -> anyhow::Result<()> {
    match subcmd {
        ModelSubcommands::Catalog { r#type } => {
            let filter = match r#type.as_deref() {
                Some("text") => Some(models::ModelType::Text),
                Some("vision") => Some(models::ModelType::Vision),
                Some("speech") => Some(models::ModelType::Speech),
                Some("embedding") => Some(models::ModelType::Embedding),
                Some(t) => {
                    anyhow::bail!(
                        "Unknown model type: {t}. Valid types: text, vision, speech, embedding"
                    );
                }
                None => None,
            };
            ModelCommands::catalog(ctx, filter)
        }
        ModelSubcommands::List { r#type } => {
            let filter = match r#type.as_deref() {
                Some("text") => Some(models::ModelType::Text),
                Some("vision") => Some(models::ModelType::Vision),
                Some("speech") => Some(models::ModelType::Speech),
                Some("embedding") => Some(models::ModelType::Embedding),
                Some(t) => {
                    anyhow::bail!(
                        "Unknown model type: {t}. Valid types: text, vision, speech, embedding"
                    );
                }
                None => None,
            };
            ModelCommands::list(ctx, filter)
        }
        ModelSubcommands::Search { query } => ModelCommands::search(ctx, &query),
        ModelSubcommands::Recommend {
            r#type,
            providers,
            wide,
        } => {
            let filter = match r#type.as_deref() {
                Some("text") => Some(models::ModelType::Text),
                Some("vision") => Some(models::ModelType::Vision),
                Some("speech") => Some(models::ModelType::Speech),
                Some("embedding") => Some(models::ModelType::Embedding),
                Some(t) => {
                    anyhow::bail!(
                        "Unknown model type: {t}. Valid types: text, vision, speech, embedding"
                    );
                }
                None => None,
            };
            ModelCommands::recommend(ctx, filter, &providers, wide)
        }
        ModelSubcommands::Info { model_id } => ModelCommands::info(ctx, &model_id),
        ModelSubcommands::Setup {
            model_type,
            instance_id,
        } => ModelCommands::setup(ctx, &model_type, instance_id.as_deref()).await,
        ModelSubcommands::Pull { model_id } => ModelCommands::pull(ctx, &model_id).await,
        ModelSubcommands::Remove { model_id } => ModelCommands::remove(ctx, &model_id),
    }
}

async fn run_capability_command(
    ctx: &mut AppContext,
    subcmd: CapabilitySubcommands,
) -> anyhow::Result<()> {
    match subcmd {
        CapabilitySubcommands::Catalog => CapabilityCommands::catalog(ctx),
        CapabilitySubcommands::List => CapabilityCommands::list(ctx),
        CapabilitySubcommands::Info { capability_id } => {
            CapabilityCommands::info(ctx, &capability_id)
        }
        CapabilitySubcommands::Setup {
            capability_type,
            instance_id,
        } => CapabilityCommands::setup(ctx, &capability_type, instance_id.as_deref()).await,
        CapabilitySubcommands::Remove { capability_id } => {
            CapabilityCommands::remove(ctx, &capability_id)
        }
    }
}

async fn run_provider_command(
    ctx: &mut AppContext,
    subcmd: ProviderSubcommands,
) -> anyhow::Result<()> {
    match subcmd {
        ProviderSubcommands::Catalog { wide } => ProviderCommands::catalog(ctx, wide),
        ProviderSubcommands::List => ProviderCommands::list(ctx),
        ProviderSubcommands::Setup {
            provider_type,
            instance_id,
        } => ProviderCommands::setup(ctx, &provider_type, instance_id.as_deref()).await,
        ProviderSubcommands::Health { provider_id } => {
            ProviderCommands::health(ctx, provider_id.as_deref()).await
        }
        ProviderSubcommands::Remove { provider_id } => ProviderCommands::remove(ctx, &provider_id),
    }
}

async fn run_launcher_command(
    ctx: &mut AppContext,
    subcmd: LauncherSubcommands,
) -> anyhow::Result<()> {
    match subcmd {
        LauncherSubcommands::Catalog => LauncherCommands::catalog(ctx),
        LauncherSubcommands::List => LauncherCommands::list(ctx),
        LauncherSubcommands::Setup {
            launcher_type,
            instance_id,
        } => LauncherCommands::setup(ctx, &launcher_type, instance_id.as_deref()).await,
        LauncherSubcommands::Remove { launcher_id } => LauncherCommands::remove(ctx, &launcher_id),
    }
}

async fn run_launch(
    ui: &dyn Ui,
    launcher_id: &str,
    args: &[String],
    dry_run: bool,
    usage_tracking: bool,
) -> anyhow::Result<()> {
    use crate::capabilities::{BindingType, CAPABILITY_REGISTRY};
    use crate::launchers::LAUNCHER_REGISTRY;
    use crate::launchers::LaunchContext;
    use crate::proxy::ProxyServer;

    // Load config fresh so we always pick up the latest saved state.
    let mut config = crate::config::Config::new()?;

    let lc = config
        .get_launcher(launcher_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No launcher configured with id '{launcher_id}'. \
                 Run `granite-cli launcher setup {launcher_id}` first."
            )
        })?
        .clone();

    // The session proxy boots whenever usage tracking was requested OR the
    // `claude` launcher has a sub-agent capability bound (any of
    // `SubAgentCapability`/`ExploreSubAgentCapability`/`PlanSubAgentCapability`
    // -- checked via `BindingType::SubAgent` rather than a specific
    // capability-type string, so it covers all of them). That routing is
    // structurally required only for Claude Code: it has exactly one
    // `ANTHROPIC_BASE_URL` for the whole session, so every sub-agent's model
    // must be multiplexed through the mini-router regardless of whether `-u`
    // was passed. `opencode` (and any other launcher that later gains
    // `BindingType::SubAgent` support) configures each sub-agent's own
    // provider directly in its multi-provider config, so it never needs this.
    // Skipped entirely under `dry_run`: there's no subprocess to point a
    // proxy at, and showing the real upstream URL in the overlay is more
    // useful than a not-yet-running one. When booted, it's threaded through
    // `config.model_proxy` so that any capability which resolves its model
    // through `ModelSource` (see `AgentModelCapability::new`) is
    // transparently routed through it (and tracked, if a tracker is active)
    // -- no per-capability wrapping needed here.
    let needs_sub_agent_routing = lc.launcher_type == "claude"
        && lc.enabled_capabilities.iter().any(|id| {
            config
                .get_capability(id)
                .and_then(|c| CAPABILITY_REGISTRY.get(&c.capability_type))
                .is_some_and(|meta| {
                    meta.supported_binding_types
                        .contains(&BindingType::SubAgent)
                })
        });
    let boot_proxy = (usage_tracking || needs_sub_agent_routing) && !dry_run;
    let proxy_server = if boot_proxy {
        Some(ProxyServer::start()?)
    } else {
        None
    };
    if let Some(server) = &proxy_server {
        config.model_proxy = Some(server.handle.clone());
    }

    let mut launcher = LAUNCHER_REGISTRY
        .construct(&lc.launcher_type, &lc.launcher_id, &lc.config, &config)
        .map_err(|e| anyhow::anyhow!("Failed to construct launcher: {e}"))?;

    let launch_ctx = LaunchContext {
        launcher_id: launcher_id.to_string(),
        working_dir: std::env::current_dir()?,
        base_env: std::collections::HashMap::new(),
        dry_run,
    };

    // Bind each enabled capability to the launcher before launching. Kept
    // alive (not dropped at the end of this loop) so a capability that owns
    // a process-scoped resource -- e.g. `VisionMCPCapability`'s in-process
    // MCP server -- survives long enough for `on_shutdown` to tear it down
    // after the launched process exits, not before it starts.
    let mut bound_capabilities: Vec<Box<dyn crate::capabilities::Capability>> = Vec::new();
    for cap_id in &lc.enabled_capabilities {
        let cap_cfg = config.get_capability(cap_id).ok_or_else(|| {
            anyhow::anyhow!(
                "Launcher '{launcher_id}' references capability '{cap_id}' \
                 which is not configured. Run `granite-cli capability setup` first."
            )
        })?;
        let capability = CAPABILITY_REGISTRY
            .construct(
                &cap_cfg.capability_type,
                &cap_cfg.capability_id,
                &cap_cfg.config,
                &config,
            )
            .map_err(|e| anyhow::anyhow!("Failed to construct capability '{cap_id}': {e}"))?;
        capability.on_setup().await?;
        launcher.bind_capability(capability.as_ref()).await?;
        bound_capabilities.push(capability);
    }

    for capability in &bound_capabilities {
        capability.on_pre_launch(&launch_ctx).await?;
    }

    let launch_result = launcher.launch(args, &launch_ctx, ui).await;

    // Run post-launch/shutdown hooks regardless of how the launch went, so a
    // capability's background resources (e.g. an in-process MCP server) are
    // always torn down. Failures here are reported, not propagated -- the
    // launch itself already succeeded or failed on its own terms.
    for capability in bound_capabilities.iter().rev() {
        if let Err(e) = capability.on_post_launch(&launch_ctx).await {
            ui.warn(&format!(
                "on_post_launch failed for capability '{}': {e}",
                capability.instance_id()
            ));
        }
        if let Err(e) = capability.on_shutdown(&launch_ctx).await {
            ui.warn(&format!(
                "on_shutdown failed for capability '{}': {e}",
                capability.instance_id()
            ));
        }
    }

    let status = launch_result?;

    if let Some(server) = proxy_server {
        if usage_tracking {
            print_usage_summary(ui, &server.handle.tracker());
        }
        server.shutdown().await;
    }

    if !status.success() {
        anyhow::bail!(
            "'{}' exited with status {}",
            launcher_id,
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

/// Print a per-binding + total usage table, skipped entirely if nothing was
/// recorded (e.g. the launched agent never made a request).
fn print_usage_summary(ui: &dyn Ui, tracker: &proxy::UsageTracker) {
    let snapshot = tracker.snapshot();
    if snapshot.is_empty() {
        return;
    }

    let mut rows: Vec<Vec<String>> = snapshot
        .iter()
        .map(|(label, s)| {
            vec![
                label.clone(),
                s.requests.to_string(),
                s.input_tokens.to_string(),
                s.output_tokens.to_string(),
                s.cache_creation_tokens.to_string(),
                s.cache_read_tokens.to_string(),
            ]
        })
        .collect();
    rows.sort_by(|a, b| a[0].cmp(&b[0]));

    let total = snapshot
        .values()
        .fold(proxy::UsageStats::default(), |mut acc, s| {
            acc.requests += s.requests;
            acc.input_tokens += s.input_tokens;
            acc.output_tokens += s.output_tokens;
            acc.cache_creation_tokens += s.cache_creation_tokens;
            acc.cache_read_tokens += s.cache_read_tokens;
            acc
        });
    rows.push(vec![
        "Total".to_string(),
        total.requests.to_string(),
        total.input_tokens.to_string(),
        total.output_tokens.to_string(),
        total.cache_creation_tokens.to_string(),
        total.cache_read_tokens.to_string(),
    ]);

    ui.table(
        "Usage",
        &[
            "Binding",
            "Requests",
            "Input Tokens",
            "Output Tokens",
            "Cache Write",
            "Cache Read",
        ],
        &rows,
    );
}
