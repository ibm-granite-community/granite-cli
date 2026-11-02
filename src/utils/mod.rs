pub(crate) mod capabilities;
pub(crate) mod hardware;
pub(crate) mod shell;
pub(crate) mod subserver;
mod traits;
pub(crate) mod ui;

#[cfg(test)]
pub(crate) mod test_support;

pub(crate) use capabilities::capability_model_ids;
pub(crate) use shell::resolve_shell_command;
pub(crate) use traits::Searchable;
pub(crate) use ui::prompt_from_schema;
