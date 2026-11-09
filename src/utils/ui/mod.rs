pub(crate) mod app;
pub(crate) mod backends;
pub(crate) mod base;
mod hints;
pub(crate) mod prompt;
mod setup_pane;
mod tui;
mod tui_ui;

pub(crate) use app::run_interactive_tui;
pub(crate) use base::{UI_REGISTRY, Ui};
pub(crate) use prompt::prompt_from_schema;

/// Marker for models created by provider discovery (U+2315, magnifying glass).
pub(crate) const DISCOVERED_MARK: &str = "\u{2315}";
