pub mod capabilities;
pub mod commands;
pub mod config;
pub mod dependency;
pub mod launchers;
pub mod models;
pub mod proxy;
pub mod registry;
pub mod session;
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

#[derive(clap::Args, Debug)]
struct InternalArgs {
    #[command(subcommand)]
    subcommand: InternalSubcommands,
}

#[derive(Subcommand, Debug)]
enum InternalSubcommands {
    /// Read stdin and write it verbatim to `output_path`. Used as a
    /// cross-platform lifecycle-hook capture target so callers don't need to
    /// rely on shell-specific stdin-redirection syntax.
    BobHookCapture {
        /// File path to write stdin's contents to.
        output_path: std::path::PathBuf,
        /// Path to the PID lockfile written by `hook::register_or_fail`.
        /// If present, the subcommand reads this file to learn the registrant's
        /// PID and verifies (via process-tree introspection) that the current
        /// process is a descendant before writing the capture file.  This
        /// prevents an unmanaged `bob` process from accidentally overwriting
        /// our capture with the wrong session_id.
        #[arg(long)]
        ancestor_lock: Option<std::path::PathBuf>,
    },
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
        /// Adds to the existing configuration: an entry that is already
        /// configured is kept as it is and is not re-checked.
        #[arg(long)]
        auto: bool,

        /// Pull model weights at the end of the wizard without prompting.
        /// Mutually exclusive with --skip-pull.
        #[arg(long, conflicts_with = "skip_pull")]
        pull: bool,

        /// Skip the model weight pull step entirely.
        /// Mutually exclusive with --pull.
        #[arg(long, conflicts_with = "pull")]
        skip_pull: bool,
    },

    /// Show hardware profile and recommended precision
    Hardware,

    /// Launch a configured launcher with Granite overlay
    Launch(LaunchWithOutput),

    /// Show version information
    Version,

    /// Internal commands used by granite-cli itself. Not for direct use.
    #[command(hide = true)]
    Internal(InternalArgs),
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

    /// Show detailed provider information
    Info {
        /// Provider ID
        provider_id: String,
    },

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

    /// Show detailed launcher information
    Info {
        /// Launcher ID
        launcher_id: String,
    },

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
        .construct(output, output, &serde_json::json!({}))
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
        Some(Commands::Setup {
            auto,
            pull,
            skip_pull,
        }) => {
            let pull_opt = if pull {
                Some(true)
            } else if skip_pull {
                Some(false)
            } else {
                None
            };
            let mut ctx = construct_context(
                "terminal",
                &log_level,
                &log_filters,
                log_json,
                log_thread_id,
            );
            SetupCommands::run(&mut ctx, auto, pull_opt)
                .await
                .map_err(|e| ctx.ui.error(&e.to_string()))
        }
        Some(Commands::Launch(wrapper)) => {
            let mut ctx = construct_context(
                &wrapper.output,
                &log_level,
                &log_filters,
                log_json,
                log_thread_id,
            );
            run_launch(
                &mut ctx,
                &wrapper.launcher_id,
                &wrapper.args,
                wrapper.dry_run,
            )
            .await
            .map_err(|e| ctx.ui.error(&e.to_string()))
        }
        Some(Commands::Version) => {
            // Version info is simple text — no UI backend or config needed.
            println!("{}", version::version_string());
            Ok(())
        }
        Some(Commands::Internal(args)) => match args.subcommand {
            InternalSubcommands::BobHookCapture {
                output_path,
                ancestor_lock,
            } => {
                use std::io::Read;
                let mut buf = Vec::new();
                std::io::stdin()
                    .read_to_end(&mut buf)
                    .map_err(|e| eprintln!("Error reading stdin: {e}"))
                    .and_then(|_| {
                        // Ancestry gating: if this capture doesn't belong to
                        // the process that registered the hook, skip writing
                        // it. Exit 0 either way (never error: Bob's
                        // SessionStart hooks are fire-and-forget).
                        if !bob_hook_capture_should_write(ancestor_lock.as_deref()) {
                            return Ok(());
                        }

                        std::fs::write(&output_path, &buf)
                            .map_err(|e| eprintln!("Error writing {}: {e}", output_path.display()))
                    })
            }
        },
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
        ModelSubcommands::Info { model_id } => ModelCommands::info(ctx, &model_id).await,
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
            CapabilityCommands::info(ctx, &capability_id).await
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
        ProviderSubcommands::Info { provider_id } => ProviderCommands::info(ctx, &provider_id),
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
        LauncherSubcommands::Info { launcher_id } => LauncherCommands::info(ctx, &launcher_id),
        LauncherSubcommands::Setup {
            launcher_type,
            instance_id,
        } => LauncherCommands::setup(ctx, &launcher_type, instance_id.as_deref()).await,
        LauncherSubcommands::Remove { launcher_id } => LauncherCommands::remove(ctx, &launcher_id),
    }
}

async fn run_launch(
    ctx: &mut AppContext,
    launcher_id: &str,
    args: &[String],
    dry_run: bool,
) -> anyhow::Result<()> {
    use crate::capabilities::CAPABILITY_REGISTRY;
    use crate::launchers::LAUNCHER_REGISTRY;
    use crate::launchers::LaunchContext;
    use crate::proxy::ProxyServer;
    use crate::session;

    // Load config fresh so we always pick up the latest saved state.
    ctx.config = crate::config::Config::new()?;

    // Configuration integrity first, before anything about the environment.
    LauncherCommands::prelaunch(ctx, launcher_id).await?;

    let ui: &dyn Ui = &*ctx.ui;
    let config = ctx.config.clone();

    let lc = config
        .get_launcher(launcher_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No launcher configured with id '{launcher_id}'. \
                 Run `granite-cli launcher setup {launcher_id}` first."
            )
        })?
        .clone();

    // The session proxy boots for every non-dry-run launch. Usage tracking is
    // always on: the proxy's tracker accumulates stats throughout the session
    // and writes them to a persistent session file so they survive even an
    // abrupt ctrl-c.
    //
    // The `claude` launcher additionally requires the proxy for sub-agent
    // routing when any `BindingType::SubAgent` capability is enabled: it has
    // exactly one `ANTHROPIC_BASE_URL` for the whole session, so every
    // sub-agent's model must be multiplexed through the mini-router.
    // `opencode` (and any other launcher that later gains `BindingType::SubAgent`
    // support) configures each sub-agent's own provider directly in its
    // multi-provider config, so it never needs this path -- but the proxy still
    // boots for usage tracking.
    //
    // Skipped entirely under `dry_run`: there is no subprocess to point a
    // proxy at, and showing the real upstream URL in the overlay is more
    // useful than a not-yet-running one.
    let boot_proxy = !dry_run;
    let proxy_server = if boot_proxy {
        Some(ProxyServer::start()?)
    } else {
        None
    };
    if let Some(server) = &proxy_server {
        crate::proxy::register_proxy_routes(&config, &lc.enabled_capabilities, &server.handle, ui);
    }
    let model_proxy = proxy_server.as_ref().map(|s| s.handle.clone());

    // Compute the tracker early so the LaunchContext can hold it (e.g. for Bob,
    // which has no model-configuration capability and so never makes a request
    // the proxy could intercept). Reused at the writer-task setup below.
    let tracker = proxy_server.as_ref().map(|s| s.handle.tracker());

    // Build capability configs with their dependencies for session metadata
    // before consuming them in the binding loop below.
    let capabilities_with_deps: Vec<(
        crate::config::CapabilityConfig,
        Vec<crate::capabilities::Dependency>,
    )> = lc
        .enabled_capabilities
        .iter()
        .filter_map(|id| {
            let cap_cfg = config.get_capability(id)?;
            let cap_meta = CAPABILITY_REGISTRY.get(&cap_cfg.capability_type)?;
            Some((cap_cfg.clone(), cap_meta.dependencies.clone()))
        })
        .collect();

    // Generate a unique session ID and write the initial session file (empty
    // usage). Best-effort: a write failure must not prevent the session from
    // starting.
    let session_id = session::generate_session_id();
    let session_meta =
        session::create_session_meta(&session_id, &config, &lc, &capabilities_with_deps);
    session::write_session_file(&session_meta).ok();

    let mut launcher = LAUNCHER_REGISTRY
        .construct(&lc.launcher_type, &lc.launcher_id, &lc.config)
        .map_err(|e| anyhow::anyhow!("Failed to construct launcher: {e}"))?;

    let launch_ctx = LaunchContext {
        launcher_id: launcher_id.to_string(),
        working_dir: std::env::current_dir()?,
        base_env: std::collections::HashMap::new(),
        dry_run,
        usage_tracker: tracker.clone(),
        model_proxy: model_proxy.clone(),
    };

    // Bind each enabled capability to the launcher before launching. Kept
    // alive (not dropped at the end of this loop) so a capability that owns
    // a process-scoped resource -- e.g. `VisionMCPCapability`'s in-process
    // MCP server -- survives long enough for `on_shutdown` to tear it down
    // after the launched process exits, not before it starts.
    let mut bound_capabilities: Vec<Box<dyn crate::capabilities::ResolvedCapability>> = Vec::new();
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
            )
            .map_err(|e| anyhow::anyhow!("Failed to construct capability '{cap_id}': {e}"))?;
        // This path builds through the registry rather than through
        // `CapabilitySource`, so it wires the capability to what it names
        // itself. `models` is built from the launch's own configuration, so a
        // proxied launch resolves proxied providers. Resolution returns the
        // form that binds, so the bind below cannot run against an
        // unresolved capability.
        let capability = capability
            .resolve_refs(&crate::models::ModelSource::with_proxy(
                &config,
                model_proxy.clone(),
            ))
            .map_err(|e| anyhow::anyhow!("Capability '{cap_id}': {e}"))?;
        capability.on_setup().await?;
        launcher.bind_capability(capability.as_ref()).await?;
        bound_capabilities.push(capability);
    }

    for capability in &bound_capabilities {
        capability.on_pre_launch(&launch_ctx).await?;
    }

    // Set up reactive session-file flushing. A watch channel acts as a
    // single-element overwrite queue: the proxy's UsageTracker fires the
    // sender (non-blocking) on every record() call; the background writer
    // task blocks on changed() and flushes to disk whenever a new value
    // arrives. Rapid successive records coalesce into one write because watch
    // stores only the latest notification — the writer is never on the
    // response-to-client critical path.
    // `tracker` was computed earlier so the LaunchContext can hold it; reuse here.
    let writer_handle = if let Some(ref t) = tracker {
        let (tx, mut rx) = tokio::sync::watch::channel(());
        t.set_notifier(tx);
        let writer_session_id = session_id.clone();
        let writer_tracker = t.clone();
        Some(tokio::spawn(async move {
            loop {
                // Block until a record() fires the notifier, then write once.
                if rx.changed().await.is_err() {
                    // Sender dropped — session is shutting down.
                    break;
                }
                let snapshot = writer_tracker.snapshot();
                session::update_session_usage(&writer_session_id, &snapshot).ok();
            }
        }))
    } else {
        None
    };

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

    // Shut down the background writer: dropping the tracker's notifier sender
    // (by dropping `tracker` after the final flush) will cause rx.changed() to
    // return Err and the writer task to exit its loop naturally. We abort here
    // as a belt-and-suspenders measure to avoid a dangling task during proxy
    // shutdown, then do a final flush with the definitive usage snapshot.
    if let Some(handle) = writer_handle {
        handle.abort();
    }
    if let Some(ref t) = tracker {
        session::finish_session(&session_id, &t.snapshot()).ok();
    }

    let status = launch_result?;

    if let Some(server) = proxy_server {
        print_usage_summary(ui, &server.handle.tracker());
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

/// Check whether the `BobHookCapture` ancestry gating allows writing the
/// capture file. `ancestor_lock`, when present, points at the lockfile
/// written by `hook::register_or_fail` containing the registering process's
/// PID; if the current process isn't a descendant of that PID, the capture
/// belongs to some other (unmanaged, or a different granite-cli instance's)
/// `bob` session and must not be written. Every failure mode (no lock,
/// unreadable/unparseable lock, inconclusive ancestry lookup) proceeds to
/// write — this check exists only to close a narrow race, never to become a
/// new source of false negatives.
///
/// Shared by the real dispatch handler below and its unit tests, so the
/// tested logic is exactly what runs in production.
fn bob_hook_capture_should_write(ancestor_lock: Option<&std::path::Path>) -> bool {
    if let Some(lock_path) = ancestor_lock {
        let lock_content = match std::fs::read_to_string(lock_path) {
            Ok(c) => c,
            Err(_) => return true, // best-effort: can't read → proceed
        };
        let registrant_pid: u32 = match lock_content.trim().parse() {
            Ok(p) => p,
            Err(_) => return true, // best-effort: can't parse → proceed
        };

        if registrant_pid == 0 {
            return true; // no PID → proceed (no gating)
        }

        match crate::launchers::bob::hook::is_process_ancestor(registrant_pid, 16) {
            Some(false) => false,      // definitely not a descendant → skip
            Some(true) | None => true, // descendant or inconclusive → write
        }
    } else {
        true // no lock → always write
    }
}

#[cfg(test)]
mod hook_capture_tests {
    use super::*;

    #[test]
    fn gating_skips_when_lock_pid_not_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_file = tmp.path().join("lock");
        // Write a PID that's definitely not our ancestor.
        std::fs::write(&lock_file, "999999999").unwrap();

        assert!(!bob_hook_capture_should_write(Some(&lock_file)));
    }

    #[test]
    fn gating_allows_when_no_lock_path() {
        assert!(bob_hook_capture_should_write(None));
    }

    #[test]
    fn gating_allows_when_lock_unreadable() {
        // Path doesn't exist → best-effort read fails → proceed.
        assert!(bob_hook_capture_should_write(Some(std::path::Path::new(
            "/no/such/file.lock"
        ))));
    }

    #[test]
    fn gating_allows_when_lock_contains_non_numeric() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_file = tmp.path().join("lock");
        std::fs::write(&lock_file, "not-a-pid").unwrap();

        assert!(bob_hook_capture_should_write(Some(&lock_file)));
    }

    #[test]
    fn gating_allows_when_lock_pid_is_current_process() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_file = tmp.path().join("lock");
        std::fs::write(&lock_file, std::process::id().to_string()).unwrap();

        assert!(bob_hook_capture_should_write(Some(&lock_file)));
    }

    #[test]
    fn gating_allows_when_lock_pid_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_file = tmp.path().join("lock");
        std::fs::write(&lock_file, "0").unwrap();

        assert!(bob_hook_capture_should_write(Some(&lock_file)));
    }
}
