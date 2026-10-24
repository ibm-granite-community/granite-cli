pub mod capabilities;
pub mod hardware;
// Unit tests use the fake install environment, never the real one.
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod install;
pub mod shell;
pub mod subserver;
pub mod traits;
pub mod ui;

#[cfg(test)]
pub(crate) mod test_support;

pub use capabilities::capability_model_ids;
pub use hardware::{HardwareProfile, detect_hardware};
pub use shell::resolve_shell_command;
pub use traits::Searchable;
pub use ui::prompt_from_schema;
