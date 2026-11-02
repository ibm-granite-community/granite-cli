use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io::Stdout;

/*-- public --*/

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Enter raw mode + alternate screen and return a ready-to-use Terminal.
/// Used exclusively by the interactive TUI (`run_interactive_tui` in `app.rs`).
pub(crate) fn setup_terminal() -> anyhow::Result<Term> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(std::io::stdout()))?)
}

/// Restore the terminal after `setup_terminal`.
pub(crate) fn restore_terminal(mut terminal: Term) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
