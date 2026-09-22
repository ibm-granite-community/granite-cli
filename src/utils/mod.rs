pub mod capabilities;
pub mod hardware;
pub mod shell;
pub mod subserver;
#[cfg(test)]
pub(crate) mod test_support;
pub mod traits;
pub mod ui;

pub use capabilities::capability_model_ids;
pub use hardware::{HardwareProfile, detect_hardware};
pub use shell::resolve_shell_command;
pub use traits::Searchable;
pub use ui::prompt_from_schema;
