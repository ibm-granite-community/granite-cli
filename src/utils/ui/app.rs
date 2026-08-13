use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState},
};

use crate::commands::{HardwareCommands, ModelCommands};
use crate::dependency::Configured;
use crate::models::MODEL_REGISTRY;
use crate::providers::PROVIDER_REGISTRY;
use crate::utils::ui::tui::{restore_terminal, setup_terminal};

/*-- private --*/

/// Strip ANSI escape sequences from a string.
fn strip_ansi(input: &str) -> String {
    let mut result = String::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            // Skip the entire escape sequence
            i += 1;
            if i < bytes.len() {
                i += 1; // skip introducer (e.g. '[')
            }
            while i < bytes.len() {
                let b = bytes[i];
                if (0x41..=0x5A).contains(&b) || (0x61..=0x7A).contains(&b) {
                    i += 1;
                    break;
                }
                i += 1;
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

/*-- public --*/

#[derive(Debug, Clone, PartialEq)]
pub enum Section {
    Models,
    Providers,
    Launchers,
    Capabilities,
    Recommend,
    Hardware,
}

impl Section {
    fn next(&self) -> Self {
        match self {
            Section::Models => Section::Providers,
            Section::Providers => Section::Launchers,
            Section::Launchers => Section::Capabilities,
            Section::Capabilities => Section::Recommend,
            Section::Recommend => Section::Hardware,
            Section::Hardware => Section::Models,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Section::Models => "Models",
            Section::Providers => "Providers",
            Section::Launchers => "Launchers",
            Section::Capabilities => "Capabilities",
            Section::Recommend => "Recommend",
            Section::Hardware => "Hardware",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Browse,
    Search(String),
    Detail(String),
}

pub struct App {
    pub ctx: crate::AppContext,
    pub section: Section,
    pub row: usize,
    pub mode: AppMode,
    table_state: TableState,
    pub detail_scroll: usize,
}

impl App {
    pub fn new(ctx: crate::AppContext) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        Self {
            ctx,
            section: Section::Models,
            row: 0,
            mode: AppMode::Browse,
            table_state,
            detail_scroll: 0,
        }
    }

    /// Handle a key event and return whether the app should quit.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Ctrl-C always quits from any mode
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }

        match self.mode.clone() {
            AppMode::Browse => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return true,
                KeyCode::Char('/') => {
                    self.mode = AppMode::Search(String::new());
                }
                KeyCode::Tab => {
                    self.section = self.section.next();
                    self.row = 0;
                    self.sync_table_state();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let max = self.row_count().saturating_sub(1);
                    self.row = (self.row + 1).min(max);
                    self.sync_table_state();
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.row = self.row.saturating_sub(1);
                    self.sync_table_state();
                }
                KeyCode::Enter => {
                    if let Some(id) = self.selected_id() {
                        self.mode = AppMode::Detail(id);
                    }
                }
                _ => {}
            },
            AppMode::Search(ref query) => {
                let mut q = query.clone();
                match key.code {
                    KeyCode::Esc => {
                        // Cancel: return to browse, leave row unchanged
                        self.mode = AppMode::Browse;
                    }
                    KeyCode::Enter => {
                        // Confirm: position cursor at first match, return to browse
                        self.row = 0;
                        self.sync_table_state();
                        self.mode = AppMode::Browse;
                    }
                    KeyCode::Backspace => {
                        q.pop();
                        // Clamp row to new filtered count
                        let max = self.filtered_ids(&q).len().saturating_sub(1);
                        self.row = self.row.min(max);
                        self.sync_table_state();
                        self.mode = AppMode::Search(q);
                    }
                    KeyCode::Char(c) => {
                        q.push(c);
                        // Reset row to 0 when query changes so cursor is at first result
                        self.row = 0;
                        self.sync_table_state();
                        self.mode = AppMode::Search(q);
                    }
                    _ => {}
                }
            }
            AppMode::Detail(_) => match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Backspace => {
                    self.mode = AppMode::Browse;
                    self.detail_scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.detail_scroll += 1;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
                _ => {}
            },
        }
        false
    }

    /// Render the full application layout into the given frame.
    pub fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();

        // Outer layout: content + footer
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);

        // Inner layout: nav panel (20%) + content (80%)
        let inner = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(20), Constraint::Percentage(80)])
            .split(outer[0]);

        self.render_nav(frame, inner[0]);
        let mode = self.mode.clone();
        match mode {
            AppMode::Browse => self.render_browse(frame, inner[1], ""),
            AppMode::Search(ref q) => self.render_browse(frame, inner[1], q),
            AppMode::Detail(ref id) => self.render_detail(frame, inner[1], id),
        }
        self.render_footer(frame, outer[1]);
    }

    /*-- private --*/

    fn sync_table_state(&mut self) {
        self.table_state.select(Some(self.row));
    }

    fn active_query(&self) -> &str {
        match &self.mode {
            AppMode::Search(q) => q.as_str(),
            _ => "",
        }
    }

    fn filtered_ids(&self, query: &str) -> Vec<String> {
        let q = query.to_lowercase();
        // Hardware has no selectable rows; Recommend uses its own row source
        let mut ids: Vec<String> = match self.section {
            Section::Models => MODEL_REGISTRY
                .entries()
                .keys()
                .map(|k| k.to_string())
                .collect(),
            Section::Providers => PROVIDER_REGISTRY
                .entries()
                .keys()
                .map(|k| k.to_string())
                .collect(),
            Section::Launchers => crate::launchers::LAUNCHER_REGISTRY
                .entries()
                .keys()
                .map(|k| k.to_string())
                .collect(),
            Section::Capabilities => crate::capabilities::CAPABILITY_REGISTRY
                .entries()
                .keys()
                .map(|k| k.to_string())
                .collect(),
            Section::Recommend => {
                let source = crate::providers::ProviderSource::from_config(&self.ctx.config);
                let instances = source.instances();
                let providers: Vec<std::sync::Arc<dyn crate::providers::Provider>> =
                    instances.iter().map(|(_, p)| std::sync::Arc::clone(p)).collect();
                ModelCommands::recommend_rows(
                    None,
                    Some(&providers),
                    &instances,
                    false,
                    self.ctx.ui.as_ref(),
                )
                .into_iter()
                .map(|r| r[0].clone())
                .collect()
            }
            Section::Hardware => vec![],
        };
        ids.sort();
        if q.is_empty() {
            ids
        } else {
            ids.into_iter()
                .filter(|id| id.to_lowercase().contains(&q))
                .collect()
        }
    }

    fn row_count(&self) -> usize {
        self.filtered_ids(self.active_query()).len()
    }

    fn selected_id(&self) -> Option<String> {
        self.filtered_ids(self.active_query())
            .into_iter()
            .nth(self.row)
    }

    fn render_nav(&self, frame: &mut Frame, area: Rect) {
        let sections = [
            Section::Models,
            Section::Providers,
            Section::Launchers,
            Section::Capabilities,
            Section::Recommend,
            Section::Hardware,
        ];
        let items: Vec<ListItem> = sections
            .iter()
            .map(|s| {
                let count = match s {
                    Section::Models => MODEL_REGISTRY.entries().len(),
                    Section::Providers => PROVIDER_REGISTRY.entries().len(),
                    Section::Launchers => crate::launchers::LAUNCHER_REGISTRY.entries().len(),
                    Section::Capabilities => {
                        crate::capabilities::CAPABILITY_REGISTRY.entries().len()
                    }
                    Section::Recommend => {
                        let source =
                            crate::providers::ProviderSource::from_config(&self.ctx.config);
                        let instances = source.instances();
                        let providers: Vec<std::sync::Arc<dyn crate::providers::Provider>> =
                            instances.iter().map(|(_, p)| std::sync::Arc::clone(p)).collect();
                        ModelCommands::recommend_rows(
                            None,
                            Some(&providers),
                            &instances,
                            false,
                            self.ctx.ui.as_ref(),
                        )
                        .len()
                    }
                    Section::Hardware => 0,
                };
                let style = if *s == self.section {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(
                    format!("  {} ({})", s.label(), count),
                    style,
                )))
            })
            .collect();

        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" granite-cli "),
        );
        let mut state = ListState::default();
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn render_browse(&mut self, frame: &mut Frame, area: Rect, query: &str) {
        // When searching, split the pane: table on top, search bar on bottom
        let (table_area, search_area) =
            if !query.is_empty() || matches!(self.mode, AppMode::Search(_)) {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(1), Constraint::Length(3)])
                    .split(area);
                (chunks[0], Some(chunks[1]))
            } else {
                (area, None)
            };

        match self.section {
            Section::Models => {
                let filtered_ids = self.filtered_ids(query);
                // Use the shared data layer — catalog_rows returns [id, family, size, context, type]
                let all_rows = ModelCommands::catalog_rows(None);
                let entries: Vec<Vec<String>> = all_rows
                    .into_iter()
                    .filter(|r| filtered_ids.contains(&r[0]))
                    .collect();

                let header = Row::new(vec!["ID", "FAMILY", "SIZE", "TYPE"]).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );

                let rows: Vec<Row> = entries
                    .iter()
                    .enumerate()
                    .map(|(i, r)| {
                        let style = if i == self.row {
                            Style::default().bg(Color::DarkGray)
                        } else if i % 2 == 0 {
                            Style::default()
                        } else {
                            Style::default().bg(Color::Rgb(20, 20, 20))
                        };
                        // columns: [0]=id [1]=family [2]=size [3]=context [4]=type
                        Row::new(vec![
                            Cell::from(r[0].clone()),
                            Cell::from(r[1].clone()),
                            Cell::from(r[2].clone()),
                            Cell::from(r[4].clone()),
                        ])
                        .style(style)
                    })
                    .collect();

                let table = Table::new(
                    rows,
                    [
                        Constraint::Percentage(45),
                        Constraint::Percentage(25),
                        Constraint::Percentage(10),
                        Constraint::Percentage(20),
                    ],
                )
                .header(header)
                .block(Block::default().borders(Borders::ALL).title(" Models "));

                frame.render_stateful_widget(table, table_area, &mut self.table_state);
            }
            Section::Providers => {
                let all_entries: Vec<_> = {
                    let mut v: Vec<_> = PROVIDER_REGISTRY.entries().into_iter().collect();
                    v.sort_by(|a, b| a.0.cmp(b.0));
                    v
                };

                let filtered_ids = self.filtered_ids(query);
                let entries: Vec<_> = all_entries
                    .into_iter()
                    .filter(|(id, _)| filtered_ids.contains(&id.to_string()))
                    .collect();

                let header = Row::new(vec!["ID", "ENDPOINT"]).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );

                let rows: Vec<Row> = entries
                    .iter()
                    .enumerate()
                    .map(|(i, (id, p))| {
                        let style = if i == self.row {
                            Style::default().bg(Color::DarkGray)
                        } else {
                            Style::default()
                        };
                        Row::new(vec![
                            Cell::from(id.to_string()),
                            Cell::from(p.default_endpoint.clone()),
                        ])
                        .style(style)
                    })
                    .collect();

                let table = Table::new(
                    rows,
                    [Constraint::Percentage(30), Constraint::Percentage(70)],
                )
                .header(header)
                .block(Block::default().borders(Borders::ALL).title(" Providers "));

                frame.render_stateful_widget(table, table_area, &mut self.table_state);
            }
            Section::Launchers => {
                let all_entries: Vec<_> = {
                    let mut v: Vec<_> = crate::launchers::LAUNCHER_REGISTRY
                        .entries()
                        .into_iter()
                        .collect();
                    v.sort_by(|a, b| a.0.cmp(b.0));
                    v
                };

                let filtered_ids = self.filtered_ids(query);
                let entries: Vec<_> = all_entries
                    .into_iter()
                    .filter(|(id, _)| filtered_ids.contains(&id.to_string()))
                    .collect();

                let header = Row::new(vec!["ID", "DEFAULT COMMAND"]).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );

                let rows: Vec<Row> = entries
                    .iter()
                    .enumerate()
                    .map(|(i, (id, l))| {
                        let style = if i == self.row {
                            Style::default().bg(Color::DarkGray)
                        } else {
                            Style::default()
                        };
                        Row::new(vec![
                            Cell::from(id.to_string()),
                            Cell::from(l.default_command.clone()),
                        ])
                        .style(style)
                    })
                    .collect();

                let table = Table::new(
                    rows,
                    [Constraint::Percentage(30), Constraint::Percentage(70)],
                )
                .header(header)
                .block(Block::default().borders(Borders::ALL).title(" Launchers "));

                frame.render_stateful_widget(table, table_area, &mut self.table_state);
            }
            Section::Capabilities => {
                let text = Paragraph::new("No capabilities registered yet.").block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Capabilities "),
                );
                frame.render_widget(text, table_area);
            }
            Section::Recommend => {
                let source = crate::providers::ProviderSource::from_config(&self.ctx.config);
                let instances = source.instances();
                let providers: Vec<std::sync::Arc<dyn crate::providers::Provider>> =
                    instances.iter().map(|(_, p)| std::sync::Arc::clone(p)).collect();
                let all_rows = ModelCommands::recommend_rows(
                    None,
                    Some(&providers),
                    &instances,
                    false,
                    self.ctx.ui.as_ref(),
                );

                // columns: [0]=id [1]=size [2]=variant [3]=type [4]=fit [5]=providers
                let header = Row::new(vec!["ID", "SIZE", "VARIANT", "TYPE", "FIT", "PROVIDERS"])
                    .style(
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    );

                let rows: Vec<Row> = all_rows
                    .iter()
                    .enumerate()
                    .map(|(i, r)| {
                        let style = if i == self.row {
                            Style::default().bg(Color::DarkGray)
                        } else if i % 2 == 0 {
                            Style::default()
                        } else {
                            Style::default().bg(Color::Rgb(20, 20, 20))
                        };
                        let fit_display = strip_ansi(&r[4]);
                        let fit_style = if fit_display.starts_with("Partial") {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default()
                        };
                        Row::new(vec![
                            Cell::from(r[0].clone()),
                            Cell::from(r[1].clone()),
                            Cell::from(r[2].clone()),
                            Cell::from(r[3].clone()),
                            Cell::from(fit_display).style(fit_style),
                            Cell::from(r[5].clone()),
                        ])
                        .style(style)
                    })
                    .collect();

                let table = Table::new(
                    rows,
                    [
                        Constraint::Percentage(22),
                        Constraint::Percentage(10),
                        Constraint::Percentage(25),
                        Constraint::Percentage(10),
                        Constraint::Percentage(13),
                        Constraint::Percentage(20),
                    ],
                )
                .header(header)
                .block(Block::default().borders(Borders::ALL).title(" Recommend "));

                frame.render_stateful_widget(table, table_area, &mut self.table_state);
            }
            Section::Hardware => {
                // Hardware is a single static detail pane — no rows to browse
                let content = HardwareCommands::hardware_fields()
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let para = Paragraph::new(content)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" Hardware Profile "),
                    )
                    .wrap(ratatui::widgets::Wrap { trim: false })
                    .scroll((self.detail_scroll as u16, 0));
                frame.render_widget(para, table_area);
            }
        }

        // Render the search bar when in search mode
        if let Some(bar_area) = search_area {
            let display = format!(" / {query}");
            let bar = Paragraph::new(display).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(" Search "),
            );
            frame.render_widget(bar, bar_area);
        }
    }

    fn render_detail(&self, frame: &mut Frame, area: Rect, id: &str) {
        let content = match self.section {
            Section::Models => {
                // Use the shared data layer — no registry access here
                match ModelCommands::info_fields(id) {
                    Some(fields) => fields
                        .iter()
                        .map(|(k, v)| format!("{k}: {v}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    None => format!("Model '{id}' not found."),
                }
            }
            Section::Providers => {
                if let Some(p) = PROVIDER_REGISTRY.get(id) {
                    format!(
                        "Provider: {}\n\nName: {}\nType: {}\nEndpoint: {}\n\nDescription: {}",
                        id, p.name, p.provider_type, p.default_endpoint, p.description
                    )
                } else {
                    format!("Provider '{id}' not found.")
                }
            }
            Section::Launchers => {
                if let Some(l) = crate::launchers::LAUNCHER_REGISTRY.get(id) {
                    format!(
                        "Launcher: {id}\n\nDefault command: {}\nDescription: {}\n\nSupported capabilities: {}",
                        l.default_command,
                        l.description,
                        if l.supported_capabilities.is_empty() {
                            "(none yet)".to_string()
                        } else {
                            let mut caps: Vec<String> = l
                                .supported_capabilities
                                .iter()
                                .map(|c| c.to_string())
                                .collect();
                            caps.sort();
                            caps.join(", ")
                        }
                    )
                } else {
                    format!("Launcher '{id}' not found.")
                }
            }
            Section::Capabilities => format!("Capability: {id}"),
            // Recommend detail reuses the Model info_fields
            Section::Recommend => match ModelCommands::info_fields(id) {
                Some(fields) => fields
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                None => format!("Model '{id}' not found."),
            },
            // Hardware has no per-row detail — render the full profile
            Section::Hardware => HardwareCommands::hardware_fields()
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect::<Vec<_>>()
                .join("\n"),
        };

        let para = Paragraph::new(content)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {id} — Detail ")),
            )
            .wrap(ratatui::widgets::Wrap { trim: false })
            .scroll((self.detail_scroll as u16, 0));
        frame.render_widget(para, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let hints = match &self.mode {
            AppMode::Browse if self.section == Section::Hardware => {
                "[↑↓/jk] Scroll  [Tab] Section  [q] Quit"
            }
            AppMode::Browse => {
                "[↑↓/jk] Navigate  [Tab] Section  [Enter] Detail  [/] Search  [q] Quit"
            }
            AppMode::Search(_) => "[typing] Filter  [Enter] Confirm  [Esc] Cancel",
            AppMode::Detail(_) => "[↑↓/jk] Scroll  [Backspace/Esc/q] Back",
        };
        let para = Paragraph::new(Span::styled(hints, Style::default().fg(Color::DarkGray)));
        frame.render_widget(para, area);
    }
}

/// Launch the interactive TUI application. Runs until the user quits.
pub async fn run_interactive_tui(ctx: crate::AppContext) -> anyhow::Result<()> {
    let mut terminal = setup_terminal()?;
    let mut app = App::new(ctx);

    loop {
        terminal.draw(|frame| app.render(frame))?;

        if let Event::Key(key) = event::read()?
            && app.handle_key(key)
        {
            break;
        }
    }

    restore_terminal(terminal)?;
    Ok(())
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::utils::ui::base::tests::CaptureUi;
    use std::sync::Arc;

    fn app() -> App {
        App::new(crate::AppContext {
            config: Config::default(),
            ui: Arc::new(CaptureUi::default()),
        })
    }

    // -- existing Browse tests ------------------------------------------------

    #[test]
    fn app_default_section_is_models() {
        let a = app();
        assert_eq!(a.section, Section::Models);
    }

    #[test]
    fn app_default_mode_is_browse() {
        let a = app();
        assert_eq!(a.mode, AppMode::Browse);
    }

    #[test]
    fn app_tab_cycles_models_to_providers() {
        let mut a = app();
        a.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(a.section, Section::Providers);
    }

    #[test]
    fn app_tab_reaches_launchers_on_second_press() {
        // Models →(1) Providers →(2) Launchers
        let mut a = app();
        for _ in 0..2 {
            a.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        }
        assert_eq!(a.section, Section::Launchers);
    }

    #[test]
    fn app_tab_cycles_through_all_six_sections() {
        // Six sections: Models → Providers → Launchers → Capabilities → Recommend → Hardware → Models
        let mut a = app();
        for _ in 0..6 {
            a.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        }
        assert_eq!(a.section, Section::Models);
    }

    #[test]
    fn app_tab_resets_row_to_zero() {
        let mut a = app();
        a.row = 3;
        a.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(a.row, 0);
    }

    #[test]
    fn app_down_increments_row() {
        let mut a = app();
        a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(a.row, 1);
    }

    #[test]
    fn app_down_does_not_exceed_row_count() {
        let mut a = app();
        let max = a.row_count();
        for _ in 0..max + 10 {
            a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        assert_eq!(a.row, max.saturating_sub(1));
    }

    #[test]
    fn app_up_at_zero_stays_at_zero() {
        let mut a = app();
        a.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(a.row, 0);
    }

    #[test]
    fn app_enter_sets_detail_mode_with_id() {
        let mut a = app();
        a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match &a.mode {
            AppMode::Detail(id) => assert!(!id.is_empty()),
            _ => panic!("expected Detail mode"),
        }
    }

    #[test]
    fn app_backspace_from_detail_returns_to_browse() {
        let mut a = app();
        a.mode = AppMode::Detail("granite-3.1-8b-instruct".to_string());
        a.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(a.mode, AppMode::Browse);
    }

    #[test]
    fn app_q_returns_true_to_signal_quit() {
        let mut a = app();
        let quit = a.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(quit);
    }

    // -- scroll: TableState stays in sync ------------------------------------

    #[test]
    fn table_state_syncs_to_row_on_down() {
        let mut a = app();
        a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(a.table_state.selected(), Some(1));
    }

    #[test]
    fn table_state_syncs_to_row_on_up() {
        let mut a = app();
        a.row = 3;
        a.table_state.select(Some(3));
        a.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(a.table_state.selected(), Some(2));
    }

    #[test]
    fn table_state_resets_on_tab() {
        let mut a = app();
        a.row = 5;
        a.table_state.select(Some(5));
        a.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(a.table_state.selected(), Some(0));
    }

    // -- search: mode transitions ---------------------------------------------

    #[test]
    fn search_slash_enters_search_mode() {
        let mut a = app();
        a.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert_eq!(a.mode, AppMode::Search(String::new()));
    }

    #[test]
    fn search_typing_appends_to_query() {
        let mut a = app();
        a.mode = AppMode::Search(String::new());
        a.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        a.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert_eq!(a.mode, AppMode::Search("gr".to_string()));
    }

    #[test]
    fn search_backspace_removes_last_char() {
        let mut a = app();
        a.mode = AppMode::Search("gran".to_string());
        a.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(a.mode, AppMode::Search("gra".to_string()));
    }

    #[test]
    fn search_esc_returns_to_browse_without_changing_row() {
        let mut a = app();
        a.row = 3;
        a.mode = AppMode::Search("gran".to_string());
        a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(a.mode, AppMode::Browse);
        assert_eq!(a.row, 3);
    }

    #[test]
    fn search_enter_returns_to_browse() {
        let mut a = app();
        a.mode = AppMode::Search("granite".to_string());
        a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(a.mode, AppMode::Browse);
    }

    #[test]
    fn search_enter_sets_row_to_zero_of_filtered_results() {
        let mut a = app();
        a.mode = AppMode::Search("3.1".to_string());
        a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(a.row, 0);
        assert_eq!(a.mode, AppMode::Browse);
    }

    #[test]
    fn search_q_does_not_quit() {
        let mut a = app();
        a.mode = AppMode::Search(String::new());
        let quit = a.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!quit);
        assert_eq!(a.mode, AppMode::Search("q".to_string()));
    }

    #[test]
    fn filtered_ids_empty_query_returns_all_models() {
        let a = app();
        let ids = a.filtered_ids("");
        let total = MODEL_REGISTRY.entries().len();
        assert_eq!(ids.len(), total);
    }

    #[test]
    fn filtered_ids_substring_filters_correctly() {
        let a = app();
        let ids = a.filtered_ids("3.1");
        assert!(!ids.is_empty());
        assert!(ids.iter().all(|id| id.contains("3.1")));
    }

    #[test]
    fn filtered_ids_no_match_returns_empty() {
        let a = app();
        let ids = a.filtered_ids("zzznomatch");
        assert!(ids.is_empty());
    }

    // -- detail scroll --------------------------------------------------------

    #[test]
    fn detail_scroll_default_is_zero() {
        let a = app();
        assert_eq!(a.detail_scroll, 0);
    }

    #[test]
    fn detail_down_increments_scroll() {
        let mut a = app();
        a.mode = AppMode::Detail("granite-3.1-8b-instruct".to_string());
        a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(a.detail_scroll, 1);
    }

    #[test]
    fn detail_up_at_zero_stays_zero() {
        let mut a = app();
        a.mode = AppMode::Detail("granite-3.1-8b-instruct".to_string());
        a.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(a.detail_scroll, 0);
    }

    #[test]
    fn detail_esc_resets_scroll_to_zero() {
        let mut a = app();
        a.mode = AppMode::Detail("granite-3.1-8b-instruct".to_string());
        a.detail_scroll = 5;
        a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(a.detail_scroll, 0);
        assert_eq!(a.mode, AppMode::Browse);
    }

    #[test]
    fn detail_scroll_does_not_affect_browse_row() {
        let mut a = app();
        a.row = 2;
        a.mode = AppMode::Detail("granite-3.1-8b-instruct".to_string());
        a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(a.row, 2);
    }

    // ── recommend + hardware sections ────────────────────────────────────────

    #[test]
    fn recommend_section_row_count_is_non_negative() {
        let mut a = app();
        a.section = Section::Recommend;
        // row_count must not panic and must return a valid usize
        let _ = a.row_count();
    }

    #[test]
    fn hardware_section_row_count_is_zero() {
        let mut a = app();
        a.section = Section::Hardware;
        assert_eq!(a.row_count(), 0);
    }

    #[test]
    fn recommend_rows_all_have_six_columns() {
        let ui: Box<dyn crate::utils::ui::base::Ui + Send + Sync> =
            Box::new(crate::utils::ui::base::tests::CaptureUi::default());
        for row in ModelCommands::recommend_rows(None, None, &[], false, &*ui) {
            assert_eq!(row.len(), 6, "each recommend row must have 6 columns");
        }
    }

    #[test]
    fn hardware_fields_has_all_expected_keys() {
        use crate::commands::HardwareCommands;
        let fields = HardwareCommands::hardware_fields();
        let keys: Vec<&str> = fields.iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&"CPU Cores"));
        assert!(keys.contains(&"RAM"));
    }
}
