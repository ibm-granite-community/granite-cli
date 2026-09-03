use std::sync::{Arc, Mutex};

use alog::Filters as AlogFilters;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState},
};

use crate::commands::{
    CapabilityCommands, HardwareCommands, LauncherCommands, ModelCommands, ProviderCommands,
};
use crate::dependency::Configured;
use crate::models::MODEL_REGISTRY;
use crate::providers::PROVIDER_REGISTRY;
use crate::utils::ui::setup_pane::SetupPane;
use crate::utils::ui::tui::{restore_terminal, setup_terminal};
use crate::utils::ui::tui_ui::{Answer, OutputLine, TuiUi};

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
    /// Intermediate mode: user picks an existing instance (or "New instance")
    /// before the setup wizard starts.
    InstancePick {
        /// The catalog type id (provider type, launcher type, capability type).
        type_id: String,
        /// Sorted list of existing instance ids for this type.
        instances: Vec<String>,
        /// Currently highlighted item index (0 = "New instance", 1..N = existing).
        cursor: usize,
    },
}

/// Action returned by [`App::handle_key`] to drive the event loop in
/// [`run_interactive_tui`].
#[derive(Debug, Clone, PartialEq)]
pub enum AppAction {
    /// No special action; keep running the TUI.
    None,
    /// User requested quit.
    Quit,
    /// Open the in-pane setup wizard for the given section + type id.
    /// `instance_id` is `Some(id)` when editing an existing instance or
    /// `None` when creating a new one.
    StartSetup(Section, String, Option<String>),
}

pub struct App {
    pub ctx: crate::AppContext,
    pub section: Section,
    pub row: usize,
    pub mode: AppMode,
    table_state: TableState,
    pub detail_scroll: usize,
    /// Cached once at startup — recomputing this requires `detect_hardware()`
    /// (DXGI/NVML/Metal calls) on every render, which is the main cause of
    /// sluggish arrow-key navigation on Windows.
    recommend_rows_cache: Vec<Vec<String>>,
    /// Active in-pane setup wizard, if one is running.
    pub setup_pane: Option<SetupPane>,
    /// Per-section toggle: when true, only rows whose catalog type has at least
    /// one configured instance are shown.  Defaults to true for a section when
    /// it has any configured instances at startup, false otherwise.
    /// Indices: 0=Models, 1=Providers, 2=Launchers, 3=Capabilities.
    pub configured_only: [bool; 4],
}

impl App {
    pub fn new(ctx: crate::AppContext) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        let recommend_rows_cache = {
            let source = crate::providers::ProviderSource::from_config(&ctx.config);
            let instances = source.instances();
            let providers: Vec<&dyn crate::providers::Provider> =
                instances.iter().map(|(_, p)| *p).collect();
            ModelCommands::recommend_rows(
                None,
                Some(&providers),
                &instances,
                false,
                ctx.ui.as_ref(),
            )
        };
        let configured_only = [
            !ctx.config.models.is_empty(),       // Models
            !ctx.config.providers.is_empty(),    // Providers
            !ctx.config.launchers.is_empty(),    // Launchers
            !ctx.config.capabilities.is_empty(), // Capabilities
        ];
        Self {
            ctx,
            section: Section::Models,
            row: 0,
            mode: AppMode::Browse,
            table_state,
            detail_scroll: 0,
            recommend_rows_cache,
            setup_pane: None,
            configured_only,
        }
    }

    /// Handle a key event and return an [`AppAction`].
    pub fn handle_key(&mut self, key: KeyEvent) -> AppAction {
        // Ctrl-C always quits from any mode
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return AppAction::Quit;
        }

        // For InstancePick we need mutable access to the cursor field
        // without borrowing self entirely, so handle it before the clone-based
        // match that drives the other modes.
        if let AppMode::InstancePick {
            ref type_id,
            ref instances,
            ref mut cursor,
        } = self.mode
        {
            match key.code {
                KeyCode::Esc => {
                    self.mode = AppMode::Browse;
                    return AppAction::None;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let max = instances.len();
                    *cursor = (*cursor + 1).min(max);
                    return AppAction::None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *cursor = cursor.saturating_sub(1);
                    return AppAction::None;
                }
                KeyCode::Enter => {
                    let instance_id = if *cursor == 0 {
                        None
                    } else {
                        Some(instances[*cursor - 1].clone())
                    };
                    let section = self.section.clone();
                    let type_id = type_id.clone();
                    self.mode = AppMode::Browse;
                    return AppAction::StartSetup(section, type_id, instance_id);
                }
                _ => return AppAction::None,
            }
        }

        match self.mode.clone() {
            AppMode::Browse => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return AppAction::Quit,
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
                KeyCode::Char('s') => {
                    // Show catalog (only when configured_only is true)
                    if let Some(idx) = Self::configured_only_idx(&self.section)
                        && self.configured_only[idx]
                    {
                        self.configured_only[idx] = false;
                        let max = self.row_count().saturating_sub(1);
                        self.row = self.row.min(max);
                        self.sync_table_state();
                    }
                }
                KeyCode::Char('h') => {
                    // Hide catalog (only when configured_only is false)
                    if let Some(idx) = Self::configured_only_idx(&self.section)
                        && !self.configured_only[idx]
                    {
                        self.configured_only[idx] = true;
                        let max = self.row_count().saturating_sub(1);
                        self.row = self.row.min(max);
                        self.sync_table_state();
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
            AppMode::Detail(ref id) => match key.code {
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
                KeyCode::Enter => {
                    let id = id.clone();
                    if let Some(instances) = self.existing_instances(&id) {
                        self.mode = AppMode::InstancePick {
                            type_id: id,
                            instances,
                            cursor: 0,
                        };
                    } else {
                        return AppAction::StartSetup(self.section.clone(), id, None);
                    }
                }
                _ => {}
            },
            // InstancePick is handled above before the clone-based match.
            AppMode::InstancePick { .. } => {}
        }
        AppAction::None
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

        // When a setup pane is active it replaces the normal content area.
        if let Some(pane) = &mut self.setup_pane {
            pane.render(frame, inner[1]);
        } else {
            let mode = self.mode.clone();
            match mode {
                AppMode::Browse => self.render_browse(frame, inner[1], ""),
                AppMode::Search(ref q) => self.render_browse(frame, inner[1], q),
                AppMode::Detail(ref id) => self.render_detail(frame, inner[1], id),
                AppMode::InstancePick {
                    ref type_id,
                    ref instances,
                    cursor,
                } => self.render_instance_pick(frame, inner[1], type_id, instances, cursor),
            }
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

    /// Index into `self.configured_only` for sections that support the toggle,
    /// or `None` for sections that don't (Models, Recommend, Hardware).
    fn configured_only_idx(section: &Section) -> Option<usize> {
        match section {
            Section::Models => Some(0),
            Section::Providers => Some(1),
            Section::Launchers => Some(2),
            Section::Capabilities => Some(3),
            _ => None,
        }
    }

    fn filtered_ids(&self, query: &str) -> Vec<String> {
        let q = query.to_lowercase();
        // IDs must be returned in the same order as the browse table renders
        // them so that self.row correctly indexes the highlighted entry.
        //
        // Models: catalog_rows uses sort_enriched_rows (family/version/size),
        //         not alphabetical — derive IDs from there.
        // Recommend: recommend_rows_cache is sorted by variant size; preserve
        //            that order instead of re-sorting alphabetically.
        // Providers/Launchers/Capabilities: render code sorts by key, so
        //                                   alphabetical sort here is correct.
        // Hardware: no selectable rows.
        let ids: Vec<String> = match self.section {
            Section::Models => {
                let only = self.configured_only[0];
                let configured_ids: std::collections::HashSet<&str> =
                    self.ctx.config.models.keys().map(|k| k.as_str()).collect();
                ModelCommands::catalog_rows(None)
                    .into_iter()
                    .filter(|r| !only || configured_ids.contains(r[0].as_str()))
                    .map(|r| r[0].clone())
                    .collect()
            }
            Section::Providers => {
                let only = self.configured_only[1];
                let configured_types: std::collections::HashSet<String> = self
                    .ctx
                    .config
                    .providers
                    .values()
                    .map(|c| c.provider_type.clone())
                    .collect();
                let mut v: Vec<String> = PROVIDER_REGISTRY
                    .entries()
                    .keys()
                    .filter(|k| !only || configured_types.contains(**k))
                    .map(|k| k.to_string())
                    .collect();
                v.sort();
                v
            }
            Section::Launchers => {
                let only = self.configured_only[2];
                let configured_types: std::collections::HashSet<String> = self
                    .ctx
                    .config
                    .launchers
                    .values()
                    .map(|c| c.launcher_type.clone())
                    .collect();
                let mut v: Vec<String> = crate::launchers::LAUNCHER_REGISTRY
                    .entries()
                    .keys()
                    .filter(|k| !only || configured_types.contains(**k))
                    .map(|k| k.to_string())
                    .collect();
                v.sort();
                v
            }
            Section::Capabilities => {
                let only = self.configured_only[3];
                let configured_types: std::collections::HashSet<String> = self
                    .ctx
                    .config
                    .capabilities
                    .values()
                    .map(|c| c.capability_type.clone())
                    .collect();
                let mut v: Vec<String> = crate::capabilities::CAPABILITY_REGISTRY
                    .entries()
                    .keys()
                    .filter(|k| !only || configured_types.contains(**k))
                    .map(|k| k.to_string())
                    .collect();
                v.sort();
                v
            }
            Section::Recommend => self
                .recommend_rows_cache
                .iter()
                .map(|r| r[0].clone())
                .collect(),
            Section::Hardware => vec![],
        };
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
                // For sections with a configured_only toggle, show the
                // filtered count (matching `filtered_ids`) rather than
                // the total registry size.
                let count = match s {
                    Section::Models => {
                        let only = self.configured_only[0];
                        if only {
                            self.ctx.config.models.len()
                        } else {
                            MODEL_REGISTRY.entries().len()
                        }
                    }
                    Section::Providers => {
                        let only = self.configured_only[1];
                        if only {
                            let configured_types: std::collections::HashSet<String> = self
                                .ctx
                                .config
                                .providers
                                .values()
                                .map(|c| c.provider_type.clone())
                                .collect();
                            PROVIDER_REGISTRY
                                .entries()
                                .keys()
                                .filter(|k| configured_types.contains(**k))
                                .count()
                        } else {
                            PROVIDER_REGISTRY.entries().len()
                        }
                    }
                    Section::Launchers => {
                        let only = self.configured_only[2];
                        if only {
                            let configured_types: std::collections::HashSet<String> = self
                                .ctx
                                .config
                                .launchers
                                .values()
                                .map(|c| c.launcher_type.clone())
                                .collect();
                            crate::launchers::LAUNCHER_REGISTRY
                                .entries()
                                .keys()
                                .filter(|k| configured_types.contains(**k))
                                .count()
                        } else {
                            crate::launchers::LAUNCHER_REGISTRY.entries().len()
                        }
                    }
                    Section::Capabilities => {
                        let only = self.configured_only[3];
                        if only {
                            let configured_types: std::collections::HashSet<String> = self
                                .ctx
                                .config
                                .capabilities
                                .values()
                                .map(|c| c.capability_type.clone())
                                .collect();
                            crate::capabilities::CAPABILITY_REGISTRY
                                .entries()
                                .keys()
                                .filter(|k| configured_types.contains(**k))
                                .count()
                        } else {
                            crate::capabilities::CAPABILITY_REGISTRY.entries().len()
                        }
                    }
                    Section::Recommend => self.recommend_rows_cache.len(),
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

                // Model instances are keyed by model ID directly
                let configured_ids: std::collections::HashSet<&str> =
                    self.ctx.config.models.keys().map(|k| k.as_str()).collect();

                let header = Row::new(vec!["", "ID", "FAMILY", "SIZE", "TYPE"]).style(
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
                        let marker = if configured_ids.contains(r[0].as_str()) {
                            Cell::from("✓").style(Style::default().fg(Color::Green))
                        } else {
                            Cell::from("")
                        };
                        // columns: [0]=id [1]=family [2]=size [3]=context [4]=type
                        Row::new(vec![
                            marker,
                            Cell::from(r[0].clone()),
                            Cell::from(r[1].clone()),
                            Cell::from(r[2].clone()),
                            Cell::from(r[4].clone()),
                        ])
                        .style(style)
                    })
                    .collect();

                let table =
                    Table::new(
                        rows,
                        [
                            Constraint::Length(2),
                            Constraint::Percentage(43),
                            Constraint::Percentage(25),
                            Constraint::Percentage(10),
                            Constraint::Percentage(20),
                        ],
                    )
                    .header(header)
                    .block(Block::default().borders(Borders::ALL).title(
                        if self.configured_only[0] {
                            " Models [s: show catalog] "
                        } else {
                            " Models [h: hide catalog] "
                        },
                    ));

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

                // Collect configured types for marker column
                let configured_types: std::collections::HashSet<String> = self
                    .ctx
                    .config
                    .providers
                    .values()
                    .map(|c| c.provider_type.clone())
                    .collect();

                let header = Row::new(vec!["", "ID", "DEFAULT URL"]).style(
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
                        let marker = if configured_types.contains(*id) {
                            Cell::from("✓").style(Style::default().fg(Color::Green))
                        } else {
                            Cell::from("")
                        };
                        Row::new(vec![
                            marker,
                            Cell::from(id.to_string()),
                            Cell::from(p.default_endpoint.clone()),
                        ])
                        .style(style)
                    })
                    .collect();

                let table =
                    Table::new(
                        rows,
                        [
                            Constraint::Length(2),
                            Constraint::Percentage(30),
                            Constraint::Percentage(68),
                        ],
                    )
                    .header(header)
                    .block(Block::default().borders(Borders::ALL).title(
                        if self.configured_only[1] {
                            " Providers [s: show catalog] "
                        } else {
                            " Providers [h: hide catalog] "
                        },
                    ));

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

                // Collect configured types for marker column
                let configured_types: std::collections::HashSet<String> = self
                    .ctx
                    .config
                    .launchers
                    .values()
                    .map(|c| c.launcher_type.clone())
                    .collect();

                let header = Row::new(vec!["", "ID", "DEFAULT COMMAND"]).style(
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
                        let marker = if configured_types.contains(*id) {
                            Cell::from("✓").style(Style::default().fg(Color::Green))
                        } else {
                            Cell::from("")
                        };
                        Row::new(vec![
                            marker,
                            Cell::from(id.to_string()),
                            Cell::from(l.default_command.clone()),
                        ])
                        .style(style)
                    })
                    .collect();

                let table =
                    Table::new(
                        rows,
                        [
                            Constraint::Length(2),
                            Constraint::Percentage(30),
                            Constraint::Percentage(68),
                        ],
                    )
                    .header(header)
                    .block(Block::default().borders(Borders::ALL).title(
                        if self.configured_only[2] {
                            " Launchers [s: show catalog] "
                        } else {
                            " Launchers [h: hide catalog] "
                        },
                    ));

                frame.render_stateful_widget(table, table_area, &mut self.table_state);
            }
            Section::Capabilities => {
                let all_entries: Vec<_> = {
                    let mut v: Vec<_> = crate::capabilities::CAPABILITY_REGISTRY
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

                // Collect configured types for marker column
                let configured_types: std::collections::HashSet<String> = self
                    .ctx
                    .config
                    .capabilities
                    .values()
                    .map(|c| c.capability_type.clone())
                    .collect();

                let header = Row::new(vec!["", "ID", "DESCRIPTION"]).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );

                let rows: Vec<Row> = entries
                    .iter()
                    .enumerate()
                    .map(|(i, (id, c))| {
                        let style = if i == self.row {
                            Style::default().bg(Color::DarkGray)
                        } else {
                            Style::default()
                        };
                        let marker = if configured_types.contains(*id) {
                            Cell::from("✓").style(Style::default().fg(Color::Green))
                        } else {
                            Cell::from("")
                        };
                        Row::new(vec![
                            marker,
                            Cell::from(id.to_string()),
                            Cell::from(c.description.clone()),
                        ])
                        .style(style)
                    })
                    .collect();

                let table =
                    Table::new(
                        rows,
                        [
                            Constraint::Length(2),
                            Constraint::Percentage(30),
                            Constraint::Percentage(68),
                        ],
                    )
                    .header(header)
                    .block(Block::default().borders(Borders::ALL).title(
                        if self.configured_only[3] {
                            " Capabilities [s: show catalog] "
                        } else {
                            " Capabilities [h: hide catalog] "
                        },
                    ));

                frame.render_stateful_widget(table, table_area, &mut self.table_state);
            }
            Section::Recommend => {
                let all_rows = &self.recommend_rows_cache;

                let configured_ids: std::collections::HashSet<&str> =
                    self.ctx.config.models.keys().map(|k| k.as_str()).collect();

                // columns: [0]=id [1]=size [2]=variant [3]=type [4]=fit [5]=providers
                let header = Row::new(vec![
                    "",
                    "ID",
                    "SIZE",
                    "VARIANT",
                    "TYPE",
                    "FIT",
                    "PROVIDERS",
                ])
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
                        let marker = if configured_ids.contains(r[0].as_str()) {
                            Cell::from("✓").style(Style::default().fg(Color::Green))
                        } else {
                            Cell::from("")
                        };
                        let fit_display = strip_ansi(&r[4]);
                        let fit_style = if fit_display.starts_with("Partial") {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default()
                        };
                        Row::new(vec![
                            marker,
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
                        Constraint::Length(2),
                        Constraint::Percentage(21),
                        Constraint::Percentage(10),
                        Constraint::Percentage(24),
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
                let label_style = Style::default().fg(Color::Cyan);
                let lines: Vec<Line> = HardwareCommands::hardware_fields()
                    .iter()
                    .map(|(k, v)| {
                        Line::from(vec![
                            Span::styled(format!("{k}: "), label_style),
                            Span::raw(v.clone()),
                        ])
                    })
                    .collect();
                let para = Paragraph::new(lines)
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
        let bold = Style::default().fg(Color::Cyan);
        let mut lines: Vec<Line> = match self.section {
            Section::Models | Section::Recommend => match ModelCommands::info_fields(id) {
                None => vec![Line::from(format!("Model '{id}' not found."))],
                Some(fields) => {
                    let mut lines: Vec<Line> = fields
                        .iter()
                        .map(|(k, v)| {
                            Line::from(vec![
                                Span::styled(format!("{k}: "), bold),
                                Span::raw(v.clone()),
                            ])
                        })
                        .collect();
                    lines.push(Line::from(""));
                    if let Some(mc) = self.ctx.config.get_model(id) {
                        let provider_val = match &mc.provider_id {
                            None => "(not set)".to_string(),
                            Some(pid) => match self.ctx.config.get_provider(pid) {
                                None => pid.clone(),
                                Some(pc) => format!("{pid} ({})", pc.provider_type),
                            },
                        };
                        let variant_val = match &mc.variant {
                            Some(v) => v.clone(),
                            None => "(not set)".to_string(),
                        };
                        lines.push(Line::from("── Configured ──"));
                        lines.push(Line::from(vec![
                            Span::styled("Provider: ", bold),
                            Span::raw(provider_val),
                        ]));
                        lines.push(Line::from(vec![
                            Span::styled("Variant: ", bold),
                            Span::raw(variant_val),
                        ]));
                    } else {
                        lines.push(Line::from("── Not configured ──"));
                    }
                    lines
                }
            },
            Section::Providers => {
                if let Some(p) = PROVIDER_REGISTRY.get(id) {
                    let api_types = p
                        .supported_api_types
                        .iter()
                        .map(|t| t.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let formats = p
                        .supported_formats
                        .iter()
                        .map(|f| f.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let mut endpoints_lines: Vec<(String, String)> = p
                        .default_function_endpoints
                        .iter()
                        .map(|(func, eps)| {
                            let ep_strs = eps
                                .iter()
                                .map(|ep| format!("{} ({})", ep.api_type(), ep.path()))
                                .collect::<Vec<_>>()
                                .join(", ");
                            (func.to_string(), ep_strs)
                        })
                        .collect();
                    endpoints_lines.sort_by(|a, b| a.0.cmp(&b.0));
                    let mut instances: Vec<String> = self
                        .ctx
                        .config
                        .providers
                        .iter()
                        .filter(|(_, c)| c.provider_type == id)
                        .map(|(k, _)| k.clone())
                        .collect();
                    instances.sort();
                    let instances_str = if instances.is_empty() {
                        "(none)".to_string()
                    } else {
                        instances.join(", ")
                    };
                    let mut lines = vec![
                        Line::from(vec![
                            Span::styled("Name: ", bold),
                            Span::raw(p.name.clone()),
                        ]),
                        Line::from(vec![
                            Span::styled("Type: ", bold),
                            Span::raw(p.provider_type.to_string()),
                        ]),
                        Line::from(vec![
                            Span::styled("Default URL: ", bold),
                            Span::raw(p.default_endpoint.clone()),
                        ]),
                        Line::from(vec![
                            Span::styled("API Types: ", bold),
                            Span::raw(api_types),
                        ]),
                        Line::from(vec![Span::styled("Formats: ", bold), Span::raw(formats)]),
                        Line::from(""),
                        Line::from(Span::styled("Endpoints:", bold)),
                    ];
                    for (func, ep_strs) in &endpoints_lines {
                        lines.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled(format!("{func}: "), bold),
                            Span::raw(ep_strs.clone()),
                        ]));
                    }
                    lines.push(Line::from(""));
                    lines.push(Line::from(vec![
                        Span::styled("Configured instances: ", bold),
                        Span::raw(instances_str),
                    ]));
                    lines.push(Line::from(""));
                    lines.push(Line::from(vec![
                        Span::styled("Description: ", bold),
                        Span::raw(p.description.clone()),
                    ]));
                    lines
                } else {
                    vec![Line::from(format!("Provider '{id}' not found."))]
                }
            }
            Section::Launchers => {
                if let Some(l) = crate::launchers::LAUNCHER_REGISTRY.get(id) {
                    let mut configured: Vec<&crate::config::LauncherConfig> = self
                        .ctx
                        .config
                        .launchers
                        .values()
                        .filter(|c| c.launcher_type == id)
                        .collect();
                    configured.sort_by(|a, b| a.launcher_id.cmp(&b.launcher_id));
                    let supported_caps = if l.supported_capabilities.is_empty() {
                        "(none yet)".to_string()
                    } else {
                        let mut caps: Vec<String> = l
                            .supported_capabilities
                            .iter()
                            .map(|c| c.to_string())
                            .collect();
                        caps.sort();
                        caps.join(", ")
                    };
                    let mut lines = vec![
                        Line::from(vec![
                            Span::styled("Default command: ", bold),
                            Span::raw(l.default_command.clone()),
                        ]),
                        Line::from(vec![
                            Span::styled("Description: ", bold),
                            Span::raw(l.description.clone()),
                        ]),
                        Line::from(""),
                        Line::from(vec![
                            Span::styled("Supported capabilities: ", bold),
                            Span::raw(supported_caps),
                        ]),
                        Line::from(""),
                        Line::from(Span::styled("Configured instances:", bold)),
                    ];
                    if configured.is_empty() {
                        lines.push(Line::from("  (none)"));
                    } else {
                        for c in &configured {
                            let caps = if c.enabled_capabilities.is_empty() {
                                "(none)".to_string()
                            } else {
                                c.enabled_capabilities.join(", ")
                            };
                            lines.push(Line::from(format!("  {}", c.launcher_id)));
                            lines.push(Line::from(vec![
                                Span::raw("    "),
                                Span::styled("Capabilities: ", bold),
                                Span::raw(caps),
                            ]));
                        }
                    }
                    lines
                } else {
                    vec![Line::from(format!("Launcher '{id}' not found."))]
                }
            }
            Section::Capabilities => {
                if let Some(c) = crate::capabilities::CAPABILITY_REGISTRY.get(id) {
                    let binding_types = if c.supported_binding_types.is_empty() {
                        "(none)".to_string()
                    } else {
                        let mut types: Vec<String> = c
                            .supported_binding_types
                            .iter()
                            .map(|t| t.to_string())
                            .collect();
                        types.sort();
                        types.join(", ")
                    };
                    let tags = if c.tags.is_empty() {
                        "(none)".to_string()
                    } else {
                        c.tags.join(", ")
                    };
                    let deps = if c.dependencies.is_empty() {
                        "(none)".to_string()
                    } else {
                        c.dependencies
                            .iter()
                            .map(|d| d.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    let mut instances: Vec<String> = self
                        .ctx
                        .config
                        .capabilities
                        .iter()
                        .filter(|(_, cfg)| cfg.capability_type == id)
                        .map(|(k, _)| k.clone())
                        .collect();
                    instances.sort();
                    let instances_str = if instances.is_empty() {
                        "(none)".to_string()
                    } else {
                        instances.join(", ")
                    };
                    vec![
                        Line::from(vec![
                            Span::styled("Name: ", bold),
                            Span::raw(c.name.clone()),
                        ]),
                        Line::from(vec![
                            Span::styled("Description: ", bold),
                            Span::raw(c.description.clone()),
                        ]),
                        Line::from(vec![
                            Span::styled("Binding Types: ", bold),
                            Span::raw(binding_types),
                        ]),
                        Line::from(vec![Span::styled("Tags: ", bold), Span::raw(tags)]),
                        Line::from(vec![Span::styled("Dependencies: ", bold), Span::raw(deps)]),
                        Line::from(""),
                        Line::from(vec![
                            Span::styled("Configured instances: ", bold),
                            Span::raw(instances_str),
                        ]),
                    ]
                } else {
                    vec![Line::from(format!("Capability '{id}' not found."))]
                }
            }
            Section::Hardware => HardwareCommands::hardware_fields()
                .iter()
                .map(|(k, v)| {
                    Line::from(vec![
                        Span::styled(format!("{k}: "), bold),
                        Span::raw(v.clone()),
                    ])
                })
                .collect(),
        };

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Press [Enter] to enter setup",
            Style::default().fg(Color::DarkGray),
        )));

        let para = Paragraph::new(lines)
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
        let hints = if self.setup_pane.is_some() {
            // Delegate hint rendering to the setup pane.
            match &self.setup_pane {
                Some(pane) => pane.hint(),
                None => "",
            }
        } else {
            match &self.mode {
                AppMode::Browse if self.section == Section::Hardware => {
                    "[↑↓/jk] Scroll  [Tab] Section  [q] Quit"
                }
                AppMode::Browse if Self::configured_only_idx(&self.section).is_some() => {
                    if self.configured_only[Self::configured_only_idx(&self.section).unwrap()] {
                        "[↑↓/jk] Navigate  [Tab] Section  [Enter] Detail/Setup  [/] Search  [s] Show catalog  [q] Quit  ✓ = configured"
                    } else {
                        "[↑↓/jk] Navigate  [Tab] Section  [Enter] Detail/Setup  [/] Search  [h] Hide catalog  [q] Quit  ✓ = configured"
                    }
                }
                AppMode::Browse => {
                    "[↑↓/jk] Navigate  [Tab] Section  [Enter] Detail/Setup  [/] Search  [q] Quit  ✓ = configured"
                }
                AppMode::Search(_) => "[typing] Filter  [Enter] Confirm  [Esc] Cancel",
                AppMode::Detail(_) => "[↑↓/jk] Scroll  [Enter] Setup  [Backspace/Esc/q] Back",
                AppMode::InstancePick { .. } => "[↑↓/jk] Move  [Enter] Select  [Esc] Cancel",
            }
        };
        let para = Paragraph::new(Span::styled(hints, Style::default().fg(Color::DarkGray)));
        frame.render_widget(para, area);
    }

    /// Returns existing instance ids for `type_id` in the current section,
    /// sorted.  Returns `None` for sections where the picker is not applicable
    /// (Models / Recommend / Hardware) or when there are no existing instances.
    fn existing_instances(&self, type_id: &str) -> Option<Vec<String>> {
        let mut instances: Vec<String> = match self.section {
            Section::Providers => self
                .ctx
                .config
                .providers
                .values()
                .filter(|c| c.provider_type == type_id)
                .map(|c| c.provider_id.clone())
                .collect(),
            Section::Launchers => self
                .ctx
                .config
                .launchers
                .values()
                .filter(|c| c.launcher_type == type_id)
                .map(|c| c.launcher_id.clone())
                .collect(),
            Section::Capabilities => self
                .ctx
                .config
                .capabilities
                .values()
                .filter(|c| c.capability_type == type_id)
                .map(|c| c.capability_id.clone())
                .collect(),
            // Models are keyed by model id directly — no picker needed.
            // Recommend / Hardware also bypass the picker.
            _ => return None,
        };
        if instances.is_empty() {
            return None;
        }
        instances.sort();
        Some(instances)
    }

    /// Render the instance picker list for `InstancePick` mode.
    fn render_instance_pick(
        &self,
        frame: &mut Frame,
        area: Rect,
        type_id: &str,
        instances: &[String],
        cursor: usize,
    ) {
        // Build list items: "New instance" first, then each existing instance.
        let mut all_items: Vec<String> = vec!["✦ New instance".to_string()];
        all_items.extend(instances.iter().cloned());

        let list_items: Vec<ListItem> = all_items
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let prefix = if i == cursor { "▶ " } else { "  " };
                let style = if i == cursor {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(format!("{prefix}{label}"), style)))
            })
            .collect();

        let list = List::new(list_items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {type_id} — Select instance ")),
        );
        let mut list_state = ListState::default();
        list_state.select(Some(cursor));
        frame.render_stateful_widget(list, area, &mut list_state);
    }
}

/// Launch the interactive TUI application. Runs until the user quits.
pub async fn run_interactive_tui(ctx: crate::AppContext) -> anyhow::Result<()> {
    // Silence the global logger for the entire TUI session.  The logger is
    // wired to TerminalOutput which calls println! directly — any log line
    // emitted from a setup task thread would write to stdout while ratatui
    // owns the alternate screen, shifting the display up one line per call.
    alog::adjust_levels(alog::Level::Off, AlogFilters::None);

    let mut terminal = setup_terminal()?;
    let mut app = App::new(ctx);

    loop {
        // When a setup pane is active, poll for new prompts before drawing.
        // This must happen before draw so the pane renders the latest state.
        if let Some(pane) = &mut app.setup_pane {
            pane.poll();
            if pane.finished {
                if let Ok(fresh) = crate::config::Config::new() {
                    app.ctx.config = fresh;
                }
                app.setup_pane = None;
            }
        }

        terminal.draw(|frame| app.render(frame))?;

        // When setup is active use a short timeout so we wake up to redraw
        // whenever the task sends a new prompt — without waiting for a key.
        // When idle, block indefinitely (no timeout needed, saves CPU).
        let has_event = if app.setup_pane.is_some() {
            event::poll(std::time::Duration::from_millis(16))?
        } else {
            event::poll(std::time::Duration::from_secs(3600))?
        };

        if !has_event {
            continue;
        }

        let action = if let Event::Key(key) = event::read()?
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            // Route keys to the setup pane first when it is active.
            if let Some(pane) = &mut app.setup_pane {
                pane.handle_key(key);
                if pane.finished {
                    if let Ok(fresh) = crate::config::Config::new() {
                        app.ctx.config = fresh;
                    }
                    app.setup_pane = None;
                }
                AppAction::None
            } else {
                app.handle_key(key)
            }
        } else {
            AppAction::None
        };

        match action {
            AppAction::Quit => break,
            AppAction::StartSetup(section, id, instance_id) => {
                app.setup_pane = Some(spawn_setup(&app.ctx, &section, &id, instance_id.as_deref()));
            }
            AppAction::None => {}
        }
    }

    restore_terminal(terminal)?;
    Ok(())
}

/// Spawn a setup task for `id` in `section` on a blocking thread and return
/// a [`SetupPane`] wired to it via channels.  `instance_id` is forwarded to
/// the setup commands that support per-instance configuration (Providers,
/// Launchers, Capabilities); `None` means "new instance".
fn spawn_setup(
    ctx: &crate::AppContext,
    section: &Section,
    id: &str,
    instance_id: Option<&str>,
) -> SetupPane {
    // Channels: task → TUI (prompts), TUI → task (answers), shared output log.
    let (prompt_tx, prompt_rx) =
        std::sync::mpsc::sync_channel::<crate::utils::ui::tui_ui::Prompt>(0);
    let (answer_tx, answer_rx) = std::sync::mpsc::sync_channel::<Answer>(0);
    let output: Arc<Mutex<Vec<OutputLine>>> = Arc::new(Mutex::new(Vec::new()));
    let pulls: Arc<Mutex<std::collections::HashMap<u64, crate::utils::ui::tui_ui::PullState>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));

    let tui_ui = Arc::new(TuiUi::new(
        prompt_tx,
        answer_rx,
        Arc::clone(&output),
        Arc::clone(&pulls),
    ));

    // Build the pane title before the closure captures section/id/instance_id.
    let section = section.clone();
    let id = id.to_string();
    let instance_id: Option<String> = instance_id.map(|s| s.to_string());
    let title = match &instance_id {
        Some(iid) => format!("{} — {iid}", section.label().to_lowercase()),
        None => format!("{} — {id}", section.label().to_lowercase()),
    };

    let mut task_ctx = crate::AppContext {
        config: ctx.config.clone(),
        ui: tui_ui,
    };

    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async move {
            let iid = instance_id.as_deref();
            let result = match &section {
                Section::Models | Section::Recommend => {
                    ModelCommands::setup(&mut task_ctx, &id, iid).await
                }
                Section::Providers => ProviderCommands::setup(&mut task_ctx, &id, iid).await,
                Section::Launchers => LauncherCommands::setup(&mut task_ctx, &id, iid).await,
                Section::Capabilities => CapabilityCommands::setup(&mut task_ctx, &id, iid).await,
                Section::Hardware => Ok(()),
            };
            if let Err(e) = result {
                task_ctx.ui.error(&format!("Setup failed: {e}"));
            }
            // Write config back from the task context so changes persist.
            // (The task_ctx.config was cloned before the task started; the
            // setup commands call ctx.config.insert_* which already persist
            // each entry to disk, so the main ctx just needs a reload.)
        });
    });

    SetupPane::new(title, output, pulls, prompt_rx, answer_tx)
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
    fn detail_enter_emits_start_setup() {
        let mut a = app();
        let detail_id = "granite-3.1-8b-instruct".to_string();
        a.mode = AppMode::Detail(detail_id.clone());
        let action = a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            action,
            AppAction::StartSetup(Section::Models, detail_id, None)
        );
    }

    #[test]
    fn app_q_returns_quit_action() {
        let mut a = app();
        let action = a.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(action, AppAction::Quit);
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
        let action = a.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_ne!(action, AppAction::Quit);
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

    // -- Enter triggers StartSetup --------------------------------------------

    #[test]
    fn browse_enter_on_hardware_section_does_not_emit_start_setup() {
        // Hardware has no rows, so selected_id() returns None and no action is emitted
        let mut a = app();
        a.section = Section::Hardware;
        let action = a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, AppAction::None);
    }

    #[test]
    fn browse_enter_goes_to_detail() {
        // Enter from Browse now navigates to Detail for the selected item.
        let mut a = app();
        a.section = Section::Providers;
        let action = a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, AppAction::None);
        assert!(matches!(a.mode, AppMode::Detail(_)));
    }

    #[test]
    fn browse_enter_models_goes_to_detail() {
        let mut a = app();
        a.section = Section::Models;
        a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(a.mode, AppMode::Detail(_)));
    }

    #[test]
    fn detail_enter_uses_current_section() {
        let mut a = app();
        a.section = Section::Providers;
        a.mode = AppMode::Detail("ollama".to_string());
        let action = a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // No configured instances → directly emits StartSetup (or enters
        // InstancePick if there were instances, but config is empty here).
        match action {
            AppAction::StartSetup(section, id, _) => {
                assert_eq!(section, Section::Providers);
                assert_eq!(id, "ollama");
            }
            AppAction::None => {
                // InstancePick mode entered instead — also acceptable.
                assert!(matches!(a.mode, AppMode::InstancePick { .. }));
            }
            _ => panic!("unexpected action"),
        }
    }

    // -- InstancePick mode ----------------------------------------------------

    #[test]
    fn enter_on_provider_with_existing_instance_goes_to_detail_first() {
        let _home = crate::config::TestConfigHome::new();
        let mut cfg = crate::config::Config::new().unwrap();
        cfg.insert_provider(
            "my-ollama",
            crate::config::ProviderConfig {
                provider_id: "my-ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({}),
            },
        )
        .unwrap();
        let mut a = App::new(crate::AppContext {
            config: cfg,
            ui: Arc::new(CaptureUi::default()),
        });
        a.section = Section::Providers;
        let ids = a.filtered_ids("");
        a.row = ids.iter().position(|id| id == "ollama").unwrap_or(0);
        a.sync_table_state();

        // Enter from Browse now goes to Detail, not straight to InstancePick.
        let action = a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, AppAction::None);
        assert!(matches!(&a.mode, AppMode::Detail(id) if id == "ollama"));

        // A second Enter from Detail triggers InstancePick (existing instance exists).
        let action2 = a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action2, AppAction::None);
        match &a.mode {
            AppMode::InstancePick {
                type_id,
                instances,
                cursor,
            } => {
                assert_eq!(type_id, "ollama");
                assert!(instances.contains(&"my-ollama".to_string()));
                assert_eq!(*cursor, 0);
            }
            _ => panic!("expected InstancePick mode, got {:?}", a.mode),
        }
    }

    #[test]
    fn instance_pick_enter_on_new_emits_start_setup_with_none_instance() {
        let mut a = app();
        a.mode = AppMode::InstancePick {
            type_id: "ollama".to_string(),
            instances: vec!["my-ollama".to_string()],
            cursor: 0,
        };
        a.section = Section::Providers;
        let action = a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            action,
            AppAction::StartSetup(Section::Providers, "ollama".to_string(), None)
        );
        assert_eq!(a.mode, AppMode::Browse);
    }

    #[test]
    fn instance_pick_enter_on_existing_emits_start_setup_with_instance_id() {
        let mut a = app();
        a.mode = AppMode::InstancePick {
            type_id: "ollama".to_string(),
            instances: vec!["my-ollama".to_string()],
            cursor: 1,
        };
        a.section = Section::Providers;
        let action = a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            action,
            AppAction::StartSetup(
                Section::Providers,
                "ollama".to_string(),
                Some("my-ollama".to_string())
            )
        );
        assert_eq!(a.mode, AppMode::Browse);
    }

    #[test]
    fn instance_pick_esc_returns_to_browse() {
        let mut a = app();
        a.mode = AppMode::InstancePick {
            type_id: "ollama".to_string(),
            instances: vec!["my-ollama".to_string()],
            cursor: 0,
        };
        a.section = Section::Providers;
        let action = a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(action, AppAction::None);
        assert_eq!(a.mode, AppMode::Browse);
    }

    #[test]
    fn instance_pick_down_increments_cursor() {
        let mut a = app();
        a.mode = AppMode::InstancePick {
            type_id: "ollama".to_string(),
            instances: vec!["my-ollama".to_string()],
            cursor: 0,
        };
        a.section = Section::Providers;
        a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            a.mode,
            AppMode::InstancePick {
                type_id: "ollama".to_string(),
                instances: vec!["my-ollama".to_string()],
                cursor: 1,
            }
        );
    }

    #[test]
    fn existing_instances_returns_none_for_models_section() {
        let a = app();
        // Models section never uses the picker
        assert!(a.existing_instances("granite-3.1-8b-instruct").is_none());
    }

    #[test]
    fn existing_instances_returns_none_when_no_instances_configured() {
        let mut a = app();
        a.section = Section::Providers;
        // Empty config — no instances for any provider type
        assert!(a.existing_instances("ollama").is_none());
    }
}
