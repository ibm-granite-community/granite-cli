/// Ratatui widget that renders the active setup prompt inside the right pane
/// and handles key events for each prompt type.
///
/// The TUI event loop holds an `Option<SetupPane>`.  When a setup task starts,
/// the loop creates a `SetupPane`, stores it, and from that point every key
/// event is routed here first.  When the task completes (or fails) the pane
/// is dropped and normal navigation resumes.
use std::collections::HashMap;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::utils::ui::tui_ui::{Answer, OutputLevel, OutputLine, Prompt, PullState};

/*-- public --*/

/// State for the currently-active prompt widget.
#[derive(Debug)]
enum PromptState {
    Select {
        message: String,
        items: Vec<String>,
        cursor: usize,
        list_state: ListState,
    },
    MultiSelect {
        message: String,
        items: Vec<String>,
        checked: Vec<bool>,
        cursor: usize,
        list_state: ListState,
    },
    Confirm {
        message: String,
        /// `true` = Yes highlighted, `false` = No highlighted.
        yes: bool,
    },
    Text {
        message: String,
        buf: String,
        allow_empty: bool,
        password: bool,
    },
}

impl PromptState {
    fn from_prompt(prompt: Prompt) -> Self {
        match prompt {
            Prompt::Select {
                message,
                items,
                default,
            } => {
                let cursor = default.min(items.len().saturating_sub(1));
                let mut list_state = ListState::default();
                list_state.select(Some(cursor));
                Self::Select {
                    message,
                    cursor,
                    items,
                    list_state,
                }
            }
            Prompt::MultiSelect {
                message,
                items,
                defaults,
            } => {
                let len = items.len();
                let checked = if defaults.len() == len {
                    defaults
                } else {
                    vec![false; len]
                };
                let mut list_state = ListState::default();
                list_state.select(Some(0));
                Self::MultiSelect {
                    message,
                    items,
                    checked,
                    cursor: 0,
                    list_state,
                }
            }
            Prompt::Confirm { message, default } => Self::Confirm {
                message,
                yes: default,
            },
            Prompt::Text {
                message,
                default,
                allow_empty,
                password,
            } => Self::Text {
                message,
                buf: default,
                allow_empty,
                password,
            },
        }
    }

    /// Footer hint string for this prompt type.
    fn hint(&self) -> &'static str {
        match self {
            PromptState::Select { .. } => "[↑↓/jk] Move  [Enter] Confirm  [Esc] Cancel",
            PromptState::MultiSelect { .. } => {
                "[↑↓/jk] Move  [Space] Toggle  [Enter] Confirm  [Esc] Cancel"
            }
            PromptState::Confirm { .. } => "[←→/hl] Toggle  [Enter] Confirm  [Esc] Cancel",
            PromptState::Text { .. } => "[typing] Edit  [Enter] Confirm  [Esc] Cancel",
        }
    }
}

pub struct SetupPane {
    /// The title shown in the pane border (e.g. "model setup — granite-3.3-8b-instruct").
    pub title: String,
    /// Lines of info/warn/error output accumulated so far.
    pub output: std::sync::Arc<std::sync::Mutex<Vec<OutputLine>>>,
    /// Live pull/download states shared with the `TuiUi`.
    pulls: std::sync::Arc<std::sync::Mutex<HashMap<u64, PullState>>>,
    /// How many output lines to scroll (↑↓ when no prompt is active).
    output_scroll: usize,
    /// Receives new [`Prompt`]s from the setup task.
    prompt_rx: std::sync::mpsc::Receiver<Prompt>,
    /// Sends [`Answer`]s back to the setup task.
    answer_tx: std::sync::mpsc::SyncSender<Answer>,
    /// The prompt currently being answered, or `None` while waiting for the
    /// next one (i.e. the setup task is computing between prompts).
    active: Option<PromptState>,
    /// Set to `true` once `prompt_rx` is disconnected (task finished).
    pub finished: bool,
}

impl SetupPane {
    pub fn new(
        title: String,
        output: std::sync::Arc<std::sync::Mutex<Vec<OutputLine>>>,
        pulls: std::sync::Arc<std::sync::Mutex<HashMap<u64, PullState>>>,
        prompt_rx: std::sync::mpsc::Receiver<Prompt>,
        answer_tx: std::sync::mpsc::SyncSender<Answer>,
    ) -> Self {
        Self {
            title,
            output,
            pulls,
            output_scroll: 0,
            prompt_rx,
            answer_tx,
            active: None,
            finished: false,
        }
    }

    /// Poll for a new prompt from the setup task (non-blocking).
    /// Returns `true` if a new prompt arrived and the pane needs a redraw.
    pub fn poll(&mut self) -> bool {
        match self.prompt_rx.try_recv() {
            Ok(prompt) => {
                self.active = Some(PromptState::from_prompt(prompt));
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.finished = true;
                self.active = None;
                true
            }
        }
    }

    /// Current footer hint.
    pub fn hint(&self) -> &'static str {
        match &self.active {
            Some(p) => p.hint(),
            None if self.finished => "[Enter/Esc] Close",
            None => "[↑↓/jk] Scroll output  [Esc] Cancel",
        }
    }

    /// Handle a key event.  Returns `true` if the pane consumed the key.
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::KeyCode;

        // Esc always cancels setup from any prompt state.
        if key.code == KeyCode::Esc {
            // Disconnect the answer channel — the setup task will get an Err
            // on its next `ask()` call and propagate it up as a cancellation.
            self.finished = true;
            self.active = None;
            return true;
        }

        match &mut self.active {
            None => {
                // No active prompt: scroll the output log.
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.output_scroll += 1;
                        true
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.output_scroll = self.output_scroll.saturating_sub(1);
                        true
                    }
                    _ => false,
                }
            }
            Some(PromptState::Select {
                items,
                cursor,
                list_state,
                ..
            }) => {
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        *cursor = (*cursor + 1).min(items.len().saturating_sub(1));
                        list_state.select(Some(*cursor));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *cursor = cursor.saturating_sub(1);
                        list_state.select(Some(*cursor));
                    }
                    KeyCode::Enter => {
                        let idx = *cursor;
                        self.active = None;
                        let _ = self.answer_tx.send(Answer::Index(idx));
                    }
                    _ => return false,
                }
                true
            }
            Some(PromptState::MultiSelect {
                items,
                checked,
                cursor,
                list_state,
                ..
            }) => {
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        *cursor = (*cursor + 1).min(items.len().saturating_sub(1));
                        list_state.select(Some(*cursor));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        *cursor = cursor.saturating_sub(1);
                        list_state.select(Some(*cursor));
                    }
                    KeyCode::Char(' ') => {
                        if let Some(c) = checked.get_mut(*cursor) {
                            *c = !*c;
                        }
                    }
                    KeyCode::Enter => {
                        let indices: Vec<usize> = checked
                            .iter()
                            .enumerate()
                            .filter_map(|(i, &c)| c.then_some(i))
                            .collect();
                        self.active = None;
                        let _ = self.answer_tx.send(Answer::Indices(indices));
                    }
                    _ => return false,
                }
                true
            }
            Some(PromptState::Confirm { yes, .. }) => {
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') | KeyCode::Right | KeyCode::Char('l') => {
                        *yes = !*yes;
                    }
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        self.active = None;
                        let _ = self.answer_tx.send(Answer::Bool(true));
                        return true;
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') => {
                        self.active = None;
                        let _ = self.answer_tx.send(Answer::Bool(false));
                        return true;
                    }
                    KeyCode::Enter => {
                        let answer = *yes;
                        self.active = None;
                        let _ = self.answer_tx.send(Answer::Bool(answer));
                    }
                    _ => return false,
                }
                true
            }
            Some(PromptState::Text {
                buf, allow_empty, ..
            }) => {
                match key.code {
                    KeyCode::Char(c) => {
                        buf.push(c);
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Enter => {
                        if buf.is_empty() && !*allow_empty {
                            // Reject — re-prompt in place (do nothing, user must type).
                            return true;
                        }
                        let text = buf.clone();
                        self.active = None;
                        let _ = self.answer_tx.send(Answer::Text(text));
                    }
                    _ => return false,
                }
                true
            }
        }
    }

    /// Render the setup pane into `area`.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // Collect active (not-yet-done) pulls to size the bars region.
        let active_pulls: Vec<PullState> = self
            .pulls
            .lock()
            .unwrap()
            .values()
            .filter(|p| !p.done)
            .cloned()
            .collect();

        let pull_height = if active_pulls.is_empty() {
            0u16
        } else {
            // 1 row per bar + 2 for the block border
            active_pulls.len() as u16 + 2
        };
        let prompt_h = prompt_height(&self.active);

        // Build vertical layout: log | [pull bars] | [prompt]
        let constraints: Vec<Constraint> = {
            let mut c = vec![Constraint::Min(3)];
            if pull_height > 0 {
                c.push(Constraint::Length(pull_height));
            }
            if prompt_h > 0 {
                c.push(Constraint::Length(prompt_h));
            }
            c
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let mut idx = 0;
        self.render_output_log(frame, chunks[idx]);
        idx += 1;

        if pull_height > 0 {
            render_pull_bars(frame, chunks[idx], &active_pulls);
            idx += 1;
        }

        if prompt_h > 0 {
            self.render_prompt(frame, chunks[idx]);
        }
    }

    /*-- private --*/

    fn render_output_log(&mut self, frame: &mut Frame, area: Rect) {
        let lines: Vec<Line> = {
            let guard = self.output.lock().unwrap();
            guard
                .iter()
                .map(|ol| {
                    let style = match ol.level {
                        OutputLevel::Info => Style::default(),
                        OutputLevel::Warn => Style::default().fg(Color::Yellow),
                        OutputLevel::Error => Style::default().fg(Color::Red),
                    };
                    Line::from(Span::styled(ol.text.clone(), style))
                })
                .collect()
        };

        // Auto-scroll to bottom when no active prompt keeps the user busy.
        let total = lines.len();
        let visible = area.height.saturating_sub(2) as usize; // subtract borders
        if self.active.is_none() && !self.finished {
            self.output_scroll = total.saturating_sub(visible);
        }
        let scroll = self.output_scroll.min(total.saturating_sub(visible));

        let status = if self.finished {
            " — done"
        } else {
            " — running…"
        };
        let title = format!(" {} {status} ", self.title);

        let para = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0));
        frame.render_widget(para, area);
    }

    fn render_prompt(&mut self, frame: &mut Frame, area: Rect) {
        match &mut self.active {
            None => {}
            Some(PromptState::Select {
                message,
                items,
                cursor,
                list_state,
            }) => {
                let list_items: Vec<ListItem> = items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let prefix = if i == *cursor { "▶ " } else { "  " };
                        let style = if i == *cursor {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        ListItem::new(Line::from(Span::styled(format!("{prefix}{item}"), style)))
                    })
                    .collect();

                let list = List::new(list_items).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {message} ")),
                );
                frame.render_stateful_widget(list, area, list_state);
            }
            Some(PromptState::MultiSelect {
                message,
                items,
                checked,
                cursor,
                list_state,
            }) => {
                let list_items: Vec<ListItem> = items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let check = if checked[i] { "[x]" } else { "[ ]" };
                        let prefix = if i == *cursor { "▶ " } else { "  " };
                        let style = if i == *cursor {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        ListItem::new(Line::from(Span::styled(
                            format!("{prefix}{check} {item}"),
                            style,
                        )))
                    })
                    .collect();

                let list = List::new(list_items).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {message} ")),
                );
                frame.render_stateful_widget(list, area, list_state);
            }
            Some(PromptState::Confirm { message, yes }) => {
                let yes_style = if *yes {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let no_style = if !*yes {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let line = Line::from(vec![
                    Span::styled(" Yes ", yes_style),
                    Span::raw("   "),
                    Span::styled(" No ", no_style),
                ]);
                let para = Paragraph::new(line).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {message} ")),
                );
                frame.render_widget(para, area);
            }
            Some(PromptState::Text {
                message,
                buf,
                password,
                ..
            }) => {
                let display = if *password {
                    "*".repeat(buf.len())
                } else {
                    format!("{buf}█") // block cursor
                };
                let para = Paragraph::new(display).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow))
                        .title(format!(" {message} ")),
                );
                frame.render_widget(para, area);
            }
        }
    }
}

/// How many terminal rows the active prompt widget should occupy.
fn prompt_height(active: &Option<PromptState>) -> u16 {
    match active {
        None => 0,
        Some(PromptState::Select { items, .. }) => (items.len() as u16 + 2).min(14),
        Some(PromptState::MultiSelect { items, .. }) => (items.len() as u16 + 2).min(14),
        Some(PromptState::Confirm { .. }) => 3,
        Some(PromptState::Text { .. }) => 3,
    }
}

/// Render one progress bar row per active pull inside a bordered block.
fn render_pull_bars(frame: &mut Frame, area: Rect, pulls: &[PullState]) {
    // Inner area (subtract the 1-cell border on each side).
    let inner_width = area.width.saturating_sub(2) as usize;

    let lines: Vec<Line> = pulls
        .iter()
        .map(|p| {
            match p.total {
                Some(total) if total > 0 => {
                    // Filled progress bar.
                    let pct = (p.downloaded as f64 / total as f64).clamp(0.0, 1.0);
                    // Reserve space for label and percentage text on the sides.
                    // Format: "label [====----] xx%"
                    let label_max = 20usize;
                    let label = if p.label.len() > label_max {
                        format!("{}…", &p.label[..label_max - 1])
                    } else {
                        p.label.clone()
                    };
                    let pct_text = format!(" {:3.0}%", pct * 100.0);
                    let bar_width = inner_width.saturating_sub(label.len() + pct_text.len() + 3); // 3 = " [" + "]"
                    let filled = ((bar_width as f64) * pct) as usize;
                    let empty = bar_width.saturating_sub(filled);
                    Line::from(vec![
                        Span::styled(format!("{label} ["), Style::default().fg(Color::DarkGray)),
                        Span::styled("█".repeat(filled), Style::default().fg(Color::Cyan)),
                        Span::styled("░".repeat(empty), Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            format!("]{}  {}", pct_text, format_bytes(p.downloaded)),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ])
                }
                _ => {
                    // Spinner / unknown total — show downloaded bytes and a
                    // simple animated indicator using the downloaded count.
                    let spinner_chars = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
                    let spin = spinner_chars[(p.downloaded as usize / 65536) % spinner_chars.len()];
                    Line::from(vec![
                        Span::styled(
                            format!("{} {} ", p.label, spin),
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(
                            format!("{} downloaded", format_bytes(p.downloaded)),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ])
                }
            }
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Downloading ");
    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, area);
}

fn format_bytes(b: u64) -> String {
    if b >= 1_073_741_824 {
        format!("{:.1} GB", b as f64 / 1_073_741_824.0)
    } else if b >= 1_048_576 {
        format!("{:.1} MB", b as f64 / 1_048_576.0)
    } else if b >= 1_024 {
        format!("{:.1} KB", b as f64 / 1_024.0)
    } else {
        format!("{b} B")
    }
}
