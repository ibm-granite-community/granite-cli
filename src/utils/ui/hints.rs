//! Footer hint tokens for the interactive TUI.
//!
//! Each variant represents one key-binding hint shown in the footer bar.
//! [`render_hints`] joins a slice of hints into the final footer string.

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Hint {
    /// `[↑↓/jk] Navigate` — browse-mode row movement.
    Navigate,
    /// `[↑↓/jk] Scroll` — detail/hardware pane scrolling.
    Scroll,
    /// `[↑↓/jk] Move` — cursor movement inside a picker or prompt list.
    Move,
    /// `[↑↓/jk] Scroll output` — setup pane output scrolling while waiting.
    ScrollOutput,
    /// `[Tab/⇧Tab] Section` — cycle between nav sections.
    Section,
    /// `[Enter] <label>` — context-dependent confirm/open action.
    Open(&'static str),
    /// `[/] Search` — enter search mode.
    Search,
    /// `[Esc] Clear filter` — clear the active committed search filter.
    ClearFilter,
    /// `[c] Show catalog` / `[c] Hide catalog` — toggle configured-only view.
    ToggleCatalog { hidden: bool },
    /// `[i] Show inactive` / `[i] Hide inactive` — toggle inactive sessions.
    ToggleInactive { hidden: bool },
    /// `[h] Next profile` — show Recommend for the next hardware profile.
    NextHardware,
    /// `[Space] Toggle` — toggle a multi-select item.
    Toggle,
    /// `[typing] <label>` — free-text input; label is e.g. `"Filter"` or `"Edit"`.
    Typing(&'static str),
    /// `[y/n/←→/hl] Select` — yes/no confirm prompt navigation.
    ConfirmYN,
    /// `[Enter] Confirm` — submit the current prompt answer.
    Confirm,
    /// `[Esc] Cancel` — cancel the current operation.
    Cancel,
    /// `[Backspace/Esc/q] Back` — exit detail view.
    Back,
    /// `[q] Quit` — exit the TUI.
    Quit,
    /// `✓ = configured` — legend for the configured marker column.
    ConfiguredLegend,
    /// `[Enter/Esc] Close` — dismiss a finished setup pane.
    Close,
    /// `[l] Launch` — launch a configured launcher instance.
    Launch,
}

impl std::fmt::Display for Hint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Hint::Navigate => write!(f, "[↑↓/jk] Navigate"),
            Hint::Scroll => write!(f, "[↑↓/jk] Scroll"),
            Hint::Move => write!(f, "[↑↓/jk] Move"),
            Hint::ScrollOutput => write!(f, "[↑↓/jk] Scroll output"),
            Hint::Section => write!(f, "[Tab/⇧Tab] Section"),
            Hint::Open(label) => write!(f, "[Enter] {label}"),
            Hint::Search => write!(f, "[/] Search"),
            Hint::ClearFilter => write!(f, "[Esc] Clear filter"),
            Hint::ToggleCatalog { hidden: true } => write!(f, "[c] Show catalog"),
            Hint::ToggleCatalog { hidden: false } => write!(f, "[c] Hide catalog"),
            Hint::ToggleInactive { hidden: true } => write!(f, "[i] Show inactive"),
            Hint::ToggleInactive { hidden: false } => write!(f, "[i] Hide inactive"),
            Hint::NextHardware => write!(f, "[h] Next profile"),
            Hint::Toggle => write!(f, "[Space] Toggle"),
            Hint::Typing(label) => write!(f, "[typing] {label}"),
            Hint::ConfirmYN => write!(f, "[y/n/←→/hl] Select"),
            Hint::Confirm => write!(f, "[Enter] Confirm"),
            Hint::Cancel => write!(f, "[Esc] Cancel"),
            Hint::Back => write!(f, "[Backspace/Esc/q] Back"),
            Hint::Quit => write!(f, "[q] Quit"),
            Hint::ConfiguredLegend => write!(f, "✓ = configured"),
            Hint::Close => write!(f, "[Enter/Esc] Close"),
            Hint::Launch => write!(f, "[l] Launch"),
        }
    }
}

/// Join a slice of [`Hint`]s into a single footer string, separated by two spaces.
pub(crate) fn render_hints(hints: &[Hint]) -> String {
    hints
        .iter()
        .map(|h| h.to_string())
        .collect::<Vec<_>>()
        .join("  ")
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigate_display() {
        assert_eq!(Hint::Navigate.to_string(), "[↑↓/jk] Navigate");
    }

    #[test]
    fn scroll_display() {
        assert_eq!(Hint::Scroll.to_string(), "[↑↓/jk] Scroll");
    }

    #[test]
    fn move_display() {
        assert_eq!(Hint::Move.to_string(), "[↑↓/jk] Move");
    }

    #[test]
    fn scroll_output_display() {
        assert_eq!(Hint::ScrollOutput.to_string(), "[↑↓/jk] Scroll output");
    }

    #[test]
    fn section_display() {
        assert_eq!(Hint::Section.to_string(), "[Tab/⇧Tab] Section");
    }

    #[test]
    fn search_display() {
        assert_eq!(Hint::Search.to_string(), "[/] Search");
    }

    #[test]
    fn clear_filter_display() {
        assert_eq!(Hint::ClearFilter.to_string(), "[Esc] Clear filter");
    }

    #[test]
    fn toggle_display() {
        assert_eq!(Hint::Toggle.to_string(), "[Space] Toggle");
    }

    #[test]
    fn confirm_yn_display() {
        assert_eq!(Hint::ConfirmYN.to_string(), "[y/n/←→/hl] Select");
    }

    #[test]
    fn confirm_display() {
        assert_eq!(Hint::Confirm.to_string(), "[Enter] Confirm");
    }

    #[test]
    fn cancel_display() {
        assert_eq!(Hint::Cancel.to_string(), "[Esc] Cancel");
    }

    #[test]
    fn back_display() {
        assert_eq!(Hint::Back.to_string(), "[Backspace/Esc/q] Back");
    }

    #[test]
    fn quit_display() {
        assert_eq!(Hint::Quit.to_string(), "[q] Quit");
    }

    #[test]
    fn configured_legend_display() {
        assert_eq!(Hint::ConfiguredLegend.to_string(), "✓ = configured");
    }

    #[test]
    fn close_display() {
        assert_eq!(Hint::Close.to_string(), "[Enter/Esc] Close");
    }

    #[test]
    fn launch_display() {
        assert_eq!(Hint::Launch.to_string(), "[l] Launch");
    }

    #[test]
    fn toggle_catalog_hidden_true() {
        assert_eq!(
            Hint::ToggleCatalog { hidden: true }.to_string(),
            "[c] Show catalog"
        );
    }

    #[test]
    fn toggle_catalog_hidden_false() {
        assert_eq!(
            Hint::ToggleCatalog { hidden: false }.to_string(),
            "[c] Hide catalog"
        );
    }

    #[test]
    fn next_hardware() {
        assert_eq!(Hint::NextHardware.to_string(), "[h] Next profile");
    }

    #[test]
    fn toggle_inactive_hidden_true() {
        assert_eq!(
            Hint::ToggleInactive { hidden: true }.to_string(),
            "[i] Show inactive"
        );
    }

    #[test]
    fn toggle_inactive_hidden_false() {
        assert_eq!(
            Hint::ToggleInactive { hidden: false }.to_string(),
            "[i] Hide inactive"
        );
    }

    #[test]
    fn open_with_label() {
        assert_eq!(
            Hint::Open("Detail/Setup").to_string(),
            "[Enter] Detail/Setup"
        );
    }

    #[test]
    fn typing_with_label() {
        assert_eq!(Hint::Typing("Filter").to_string(), "[typing] Filter");
        assert_eq!(Hint::Typing("Edit").to_string(), "[typing] Edit");
    }

    #[test]
    fn render_hints_joins_with_two_spaces() {
        let hints = [Hint::Navigate, Hint::Section, Hint::Quit];
        assert_eq!(
            render_hints(&hints),
            "[↑↓/jk] Navigate  [Tab/⇧Tab] Section  [q] Quit"
        );
    }

    #[test]
    fn render_hints_empty() {
        assert_eq!(render_hints(&[]), "");
    }
}
