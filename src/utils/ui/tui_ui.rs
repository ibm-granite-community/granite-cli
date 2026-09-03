/// A [`Ui`] implementation that drives interactive prompts through the ratatui
/// TUI rather than blocking the terminal directly.
///
/// Setup commands call the normal `Ui` methods (`select`, `text`, `confirm`,
/// …).  Instead of showing a dialoguer widget, each call:
///
/// 1. Sends a [`Prompt`] describing what to display over `prompt_tx`.
/// 2. Blocks on `answer_rx` until the TUI event loop delivers an [`Answer`].
///
/// The TUI side renders the current prompt inside the right pane, routes key
/// events to it, and sends back an `Answer` when the user confirms.
///
/// Non-interactive output methods (`info`, `warn`, `error`, …) append lines
/// to a shared `Vec<OutputLine>` that the TUI displays above the active prompt.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::utils::ui::base::{PullHandle, Ui};

/*-- public --*/

/// A single line of non-interactive output emitted during setup.
#[derive(Debug, Clone)]
pub struct OutputLine {
    pub level: OutputLevel,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutputLevel {
    Info,
    Warn,
    Error,
}

/// Live state for one active or completed pull/download.
#[derive(Debug, Clone)]
pub struct PullState {
    pub label: String,
    /// `None` means total is not yet known (spinner mode).
    pub total: Option<u64>,
    pub downloaded: u64,
    pub done: bool,
    pub error: Option<String>,
}

/// A prompt the setup wizard needs answered.
#[derive(Debug, Clone)]
pub enum Prompt {
    /// Pick one item from a list.  Returns the chosen index.
    Select {
        message: String,
        items: Vec<String>,
        default: usize,
    },
    /// Pick zero or more items from a list.  Returns the chosen indices.
    MultiSelect {
        message: String,
        items: Vec<String>,
        defaults: Vec<bool>,
    },
    /// Yes/no question.  Returns `true`/`false`.
    Confirm { message: String, default: bool },
    /// Free-text entry.  Returns the entered string.
    Text {
        message: String,
        default: String,
        allow_empty: bool,
        /// When true the pane renders typed characters as `*`.
        password: bool,
    },
}

/// The answer the TUI sends back for one [`Prompt`].
#[derive(Debug, Clone)]
pub enum Answer {
    Index(usize),
    Indices(Vec<usize>),
    Bool(bool),
    Text(String),
}

pub struct TuiUi {
    /// Prompts sent to the TUI render loop.
    pub prompt_tx: std::sync::mpsc::SyncSender<Prompt>,
    /// Answers received from the TUI render loop.
    answer_rx: Mutex<std::sync::mpsc::Receiver<Answer>>,
    /// Lines of non-interactive output accumulated so far.
    pub output: Arc<Mutex<Vec<OutputLine>>>,
    /// Live pull/download states, keyed by handle id.
    pub pulls: Arc<Mutex<HashMap<u64, PullState>>>,
    /// Counter for allocating pull handles.
    next_handle: Mutex<u64>,
}

impl TuiUi {
    pub fn new(
        prompt_tx: std::sync::mpsc::SyncSender<Prompt>,
        answer_rx: std::sync::mpsc::Receiver<Answer>,
        output: Arc<Mutex<Vec<OutputLine>>>,
        pulls: Arc<Mutex<HashMap<u64, PullState>>>,
    ) -> Self {
        Self {
            prompt_tx,
            answer_rx: Mutex::new(answer_rx),
            output,
            pulls,
            next_handle: Mutex::new(0),
        }
    }

    fn ask(&self, prompt: Prompt) -> anyhow::Result<Answer> {
        self.prompt_tx
            .send(prompt)
            .map_err(|_| anyhow::anyhow!("TUI setup pane closed unexpectedly"))?;
        self.answer_rx
            .lock()
            .unwrap()
            .recv()
            .map_err(|_| anyhow::anyhow!("TUI setup pane closed unexpectedly"))
    }

    fn push_line(&self, level: OutputLevel, text: &str) {
        if let Ok(mut guard) = self.output.lock() {
            // Split on embedded newlines so each ratatui `Line` stays single-line.
            for part in text.split('\n') {
                guard.push(OutputLine {
                    level: level.clone(),
                    text: part.to_string(),
                });
            }
        }
    }
}

impl Ui for TuiUi {
    fn table(&self, _title: &str, _headers: &[&str], _rows: &[Vec<String>]) {}
    fn detail(&self, _title: &str, _fields: &[(&str, String)]) {}

    fn status(&self, label: &str, ok: bool, detail: &str) {
        let mark = if ok { "✓" } else { "✗" };
        self.push_line(
            if ok {
                OutputLevel::Info
            } else {
                OutputLevel::Warn
            },
            &format!("{mark} {label}  {detail}"),
        );
    }

    fn info(&self, msg: &str) {
        self.push_line(OutputLevel::Info, msg);
    }
    fn warn(&self, msg: &str) {
        self.push_line(OutputLevel::Warn, msg);
    }
    fn error(&self, msg: &str) {
        self.push_line(OutputLevel::Error, msg);
    }

    fn pull_start(&self, label: &str, total_bytes: Option<u64>) -> PullHandle {
        let id = {
            let mut n = self.next_handle.lock().unwrap();
            let id = *n;
            *n += 1;
            id
        };
        self.pulls.lock().unwrap().insert(
            id,
            PullState {
                label: label.to_string(),
                total: total_bytes,
                downloaded: 0,
                done: false,
                error: None,
            },
        );
        PullHandle(id)
    }

    fn pull_progress(&self, handle: PullHandle, downloaded: u64, total: Option<u64>) {
        if let Ok(mut guard) = self.pulls.lock()
            && let Some(state) = guard.get_mut(&handle.0)
        {
            state.downloaded = downloaded;
            if total.is_some() {
                state.total = total;
            }
        }
    }

    fn pull_finish(&self, handle: PullHandle, label: &str, error: Option<&str>) {
        if let Ok(mut guard) = self.pulls.lock()
            && let Some(state) = guard.get_mut(&handle.0)
        {
            state.done = true;
            state.error = error.map(|e| e.to_string());
        }
        // Also push a summary line to the output log.
        match error {
            None => self.push_line(OutputLevel::Info, &format!("✓ {label} — done")),
            Some(e) => self.push_line(OutputLevel::Error, &format!("✗ {label}: {e}")),
        }
    }

    fn select(&self, prompt: &str, items: &[String], default: usize) -> anyhow::Result<usize> {
        match self.ask(Prompt::Select {
            message: prompt.to_string(),
            items: items.to_vec(),
            default,
        })? {
            Answer::Index(i) => Ok(i),
            _ => anyhow::bail!("unexpected answer type for select"),
        }
    }

    fn multi_select(
        &self,
        prompt: &str,
        items: &[String],
        defaults: &[bool],
    ) -> anyhow::Result<Vec<usize>> {
        match self.ask(Prompt::MultiSelect {
            message: prompt.to_string(),
            items: items.to_vec(),
            defaults: defaults.to_vec(),
        })? {
            Answer::Indices(v) => Ok(v),
            _ => anyhow::bail!("unexpected answer type for multi_select"),
        }
    }

    fn confirm(&self, prompt: &str, default: bool) -> anyhow::Result<bool> {
        match self.ask(Prompt::Confirm {
            message: prompt.to_string(),
            default,
        })? {
            Answer::Bool(b) => Ok(b),
            _ => anyhow::bail!("unexpected answer type for confirm"),
        }
    }

    fn text(&self, prompt: &str, default: &str) -> anyhow::Result<String> {
        match self.ask(Prompt::Text {
            message: prompt.to_string(),
            default: default.to_string(),
            allow_empty: false,
            password: false,
        })? {
            Answer::Text(s) => Ok(s),
            _ => anyhow::bail!("unexpected answer type for text"),
        }
    }

    fn text_optional(&self, prompt: &str, default: &str) -> anyhow::Result<String> {
        match self.ask(Prompt::Text {
            message: prompt.to_string(),
            default: default.to_string(),
            allow_empty: true,
            password: false,
        })? {
            Answer::Text(s) => Ok(s),
            _ => anyhow::bail!("unexpected answer type for text_optional"),
        }
    }

    fn password(&self, prompt: &str) -> anyhow::Result<String> {
        match self.ask(Prompt::Text {
            message: prompt.to_string(),
            default: String::new(),
            allow_empty: true,
            password: true,
        })? {
            Answer::Text(s) => Ok(s),
            _ => anyhow::bail!("unexpected answer type for password"),
        }
    }
}

// Setup tasks run on a blocking thread; Ui requires Send + Sync.
unsafe impl Send for TuiUi {}
unsafe impl Sync for TuiUi {}
