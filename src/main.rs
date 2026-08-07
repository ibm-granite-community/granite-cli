pub mod capabilities;
pub mod commands;
pub mod config;
pub mod dependency;
pub mod launchers;
pub mod models;
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
        /// Model ID to set up
        model_id: String,
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
    Catalog,

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
        .construct(output, &serde_json::json!({}))
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
            )
            .await
            .map_err(|e| ctx.ui.error(&e.to_string()))
        }
        Some(Commands::Version) => {
            let _ctx =
                construct_context("warning", &log_level, &log_filters, log_json, log_thread_id);
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
        ModelSubcommands::Setup { model_id } => ModelCommands::setup(ctx, &model_id).await,
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
        ProviderSubcommands::Catalog => ProviderCommands::catalog(ctx),
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
) -> anyhow::Result<()> {
    use crate::launchers::LAUNCHER_REGISTRY;
    use crate::launchers::LaunchContext;

    // Load config fresh so we always pick up the latest saved state.
    let config = crate::config::Config::new()?;

    let lc = config.get_launcher(launcher_id).ok_or_else(|| {
        anyhow::anyhow!(
            "No launcher configured with id '{launcher_id}'. \
             Run `granite-cli launcher setup {launcher_id}` first."
        )
    })?;

    let launcher = LAUNCHER_REGISTRY
        .construct(&lc.launcher_type, &lc.config)
        .map_err(|e| anyhow::anyhow!("Failed to construct launcher: {e}"))?;

    let launch_ctx = LaunchContext {
        launcher_id: launcher_id.to_string(),
        working_dir: std::env::current_dir()?,
        base_env: std::collections::HashMap::new(),
        dry_run,
    };

    let status = launcher.launch(args, &launch_ctx, ui).await?;
    if !status.success() {
        anyhow::bail!(
            "'{}' exited with status {}",
            launcher_id,
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}
