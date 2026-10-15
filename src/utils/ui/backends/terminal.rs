use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crossterm::style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor};
use indicatif::{ProgressBar, ProgressStyle};

use crate::registry::{ConfigConstructable, ConstructError, NoConfig};
use crate::utils::ui::backends::plain::PlainOutput;
use crate::utils::ui::base::{HasUiMetadata, PullHandle, Ui, UiMetadata};

/*-- public --*/

/// The default terminal backend. Renders coloured, column-aligned output using
/// direct crossterm ANSI codes alongside `println!`.
///
/// No ratatui widgets, no raw mode, no Terminal, no draw loop. Output prints
/// directly to the terminal and stays there like any other CLI tool.
///
/// If stdout is not a tty (pipe, file redirect, CI — detected via
/// `crossterm::terminal::size()`) every method delegates to `PlainOutput`.
pub struct TerminalOutput {
    is_tty: bool,
    terminal_width: Option<u16>,
    next_pull_handle: AtomicU64,
    active_pulls: Mutex<HashMap<u64, ProgressBar>>,
}

impl ConfigConstructable for TerminalOutput {
    type Config = NoConfig;

    fn new(_instance_id: &str, _cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        let (is_tty, width) = match crossterm::terminal::size() {
            Ok((cols, _rows)) => (true, Some(cols.max(20))),
            Err(_) => (false, None),
        };
        Ok(Self {
            is_tty,
            terminal_width: width,
            next_pull_handle: AtomicU64::new(0),
            active_pulls: Mutex::new(HashMap::new()),
        })
    }
}

impl Ui for TerminalOutput {
    fn table(&self, title: &str, headers: &[&str], rows: &[Vec<String>]) {
        if !self.is_tty {
            return PlainOutput.table(title, headers, rows);
        }

        let col_count = headers.len();

        // 1. Calculate natural column widths (visible characters only)
        let mut natural_widths: Vec<usize> = headers.iter().map(|h| visible_len(h)).collect();
        for row in rows {
            for (i, cell) in row.iter().enumerate() {
                if i < col_count {
                    natural_widths[i] = natural_widths[i].max(visible_len(cell));
                }
            }
        }

        // 2. Fit to terminal width
        let widths = self
            .terminal_width
            .map(|w| fit_widths(natural_widths.clone(), headers, rows, w))
            .unwrap_or(natural_widths);

        // 3. Render title (unchanged)
        println!(
            "\n{}{}{}",
            SetAttribute(Attribute::Bold),
            title,
            SetAttribute(Attribute::Reset)
        );

        // 4. Render header (unchanged)
        let header_line: String = headers
            .iter()
            .zip(widths.iter())
            .map(|(h, w)| format!("{h:<w$}"))
            .collect::<Vec<_>>()
            .join("  ");
        println!(
            "{}{}{}{}",
            SetForegroundColor(Color::Cyan),
            SetAttribute(Attribute::Bold),
            header_line,
            ResetColor,
        );

        // 5. Render separator (unchanged)
        let sep: String = widths
            .iter()
            .map(|w| "─".repeat(*w))
            .collect::<Vec<_>>()
            .join("  ");
        println!(
            "{}{}{}",
            SetForegroundColor(Color::DarkGrey),
            sep,
            ResetColor
        );

        // 6. Render rows with wrapping
        render_rows_with_wrapping(rows, &widths);
    }

    fn detail(&self, title: &str, fields: &[(&str, String)]) {
        if !self.is_tty {
            return PlainOutput.detail(title, fields);
        }

        println!(
            "\n{}{}{}",
            SetAttribute(Attribute::Bold),
            title,
            SetAttribute(Attribute::Reset)
        );

        let key_width = fields.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        for (k, v) in fields {
            println!(
                "  {}{:<width$}{}  {}",
                SetForegroundColor(Color::Cyan),
                k,
                ResetColor,
                v,
                width = key_width,
            );
        }
    }

    fn status(&self, label: &str, ok: bool, detail: &str) {
        if !self.is_tty {
            return PlainOutput.status(label, ok, detail);
        }

        let (mark, colour) = if ok {
            ("✓", Color::Green)
        } else {
            ("✗", Color::Red)
        };
        if detail.is_empty() {
            println!(
                "  {}{}{}  {}",
                SetForegroundColor(colour),
                mark,
                ResetColor,
                label
            );
        } else {
            println!(
                "  {}{}{}  {}  {}",
                SetForegroundColor(colour),
                mark,
                ResetColor,
                label,
                detail
            );
        }
    }

    fn info(&self, msg: &str) {
        if !self.is_tty {
            return PlainOutput.info(msg);
        }
        println!("{msg}");
    }

    fn warn(&self, msg: &str) {
        if !self.is_tty {
            return PlainOutput.warn(msg);
        }
        eprintln!(
            "{}{}Warning:{} {}",
            SetForegroundColor(Color::Yellow),
            SetAttribute(Attribute::Bold),
            ResetColor,
            msg
        );
    }

    fn error(&self, msg: &str) {
        if !self.is_tty {
            return PlainOutput.error(msg);
        }
        eprintln!(
            "{}{}Error:{} {}",
            SetForegroundColor(Color::Red),
            SetAttribute(Attribute::Bold),
            ResetColor,
            msg
        );
    }

    fn ok(&self, msg: &str) -> String {
        if !self.is_tty {
            return PlainOutput.ok(msg);
        }
        format!("{}{}{}", SetForegroundColor(Color::Green), msg, ResetColor)
    }

    fn warn_mark(&self, msg: &str) -> String {
        if !self.is_tty {
            return PlainOutput.warn_mark(msg);
        }
        format!(
            "{}{}{}{}",
            SetForegroundColor(Color::Yellow),
            SetAttribute(Attribute::Bold),
            msg,
            ResetColor
        )
    }

    fn error_mark(&self, msg: &str) -> String {
        if !self.is_tty {
            return PlainOutput.error_mark(msg);
        }
        format!(
            "{}{}{}{}",
            SetForegroundColor(Color::Red),
            SetAttribute(Attribute::Bold),
            msg,
            ResetColor
        )
    }

    fn detail_mark(&self, msg: &str) -> String {
        if !self.is_tty {
            return PlainOutput.detail_mark(msg);
        }
        format!("{}{}{}", SetForegroundColor(Color::Grey), msg, ResetColor)
    }

    fn pull_start(&self, label: &str, total_bytes: Option<u64>) -> PullHandle {
        if !self.is_tty {
            return PlainOutput.pull_start(label, total_bytes);
        }

        let bar = match total_bytes {
            Some(total) => {
                let bar = ProgressBar::new(total);
                if let Ok(style) = ProgressStyle::with_template(
                    "{msg}  [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({percent}%)",
                ) {
                    bar.set_style(style);
                }
                bar
            }
            None => {
                let bar = ProgressBar::new_spinner();
                if let Ok(style) =
                    ProgressStyle::with_template("{msg}  {spinner} {bytes} downloaded")
                {
                    bar.set_style(style);
                }
                bar.enable_steady_tick(std::time::Duration::from_millis(120));
                bar
            }
        };
        bar.set_message(label.to_string());

        let id = self.next_pull_handle.fetch_add(1, Ordering::SeqCst);
        self.active_pulls.lock().unwrap().insert(id, bar);
        PullHandle(id)
    }

    fn pull_progress(&self, handle: PullHandle, downloaded_bytes: u64, total_bytes: Option<u64>) {
        if !self.is_tty {
            return PlainOutput.pull_progress(handle, downloaded_bytes, total_bytes);
        }

        let pulls = self.active_pulls.lock().unwrap();
        if let Some(bar) = pulls.get(&handle.0) {
            if let Some(total) = total_bytes {
                // The bar may have started as a spinner (total unknown at
                // `pull_start` time, e.g. llama.cpp doesn't know the size
                // until the first SSE progress frame). Upgrade it to a
                // percentage-capable style now that a total is known.
                if bar.length().is_none() {
                    bar.disable_steady_tick();
                    if let Ok(style) = ProgressStyle::with_template(
                        "{msg}  [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({percent}%)",
                    ) {
                        bar.set_style(style);
                    }
                }
                bar.set_length(total);
            }
            bar.set_position(downloaded_bytes);
        }
    }

    fn pull_finish(&self, handle: PullHandle, label: &str, error: Option<&str>) {
        if !self.is_tty {
            return PlainOutput.pull_finish(handle, label, error);
        }

        match self.active_pulls.lock().unwrap().remove(&handle.0) {
            Some(bar) => match error {
                Some(e) => bar.abandon_with_message(format!("{label}: failed: {e}")),
                None => bar.finish_with_message(format!("{label}: done")),
            },
            None => match error {
                Some(e) => self.error(&format!("{label}: failed: {e}")),
                None => self.info(&format!("{label}: done")),
            },
        }
    }
}

impl HasUiMetadata for TerminalOutput {
    fn metadata() -> UiMetadata {
        UiMetadata {
            name: "terminal".to_string(),
            description: "Coloured ANSI terminal output; falls back to plain when not a tty"
                .to_string(),
        }
    }
}

/*-- private --*/

/// Count visible characters in a string, skipping ANSI escape sequences.
/// Every character counts as one column, including multi-byte ones such as
/// `⚠`.
fn visible_len(s: &str) -> usize {
    let mut count = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            // Skip the entire escape sequence: ESC + introducer (e.g. '[') + args + final byte
            i += 1; // skip ESC
            if i < bytes.len() {
                i += 1; // skip introducer
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
            // UTF-8 continuation bytes belong to the character before them.
            if bytes[i] & 0xC0 != 0x80 {
                count += 1;
            }
            i += 1;
        }
    }
    count
}

/// The number of characters in `s`, which is its width in columns.
fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Split a cell string into (prefix_ansi, content, suffix_ansi).
/// Example: "\x1b[33m\x1b[1mPartial\x1b[0m" → ("\x1b[33m\x1b[1m", "Partial", "\x1b[0m")
/// Plain text: "hello" → ("", "hello", "")
fn split_cell_ansi(s: &str) -> (&str, &str, &str) {
    let bytes = s.as_bytes();
    let mut prefix_end = 0;

    // Collect CSI sequences at the start
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1; // skip ESC
            if i < bytes.len() {
                i += 1; // skip introducer
            }
            while i < bytes.len() {
                let b = bytes[i];
                if (0x41..=0x5A).contains(&b) || (0x61..=0x7A).contains(&b) {
                    i += 1;
                    break;
                }
                i += 1;
            }
            prefix_end = i;
        } else {
            break;
        }
    }

    // Check for trailing reset sequence
    let mut suffix_start = s.len();
    if suffix_start > prefix_end && s[prefix_end..].ends_with("\x1b[0m") {
        suffix_start = prefix_end + s[prefix_end..].len() - 4;
    }

    (
        &s[..prefix_end],
        &s[prefix_end..suffix_start],
        &s[suffix_start..],
    )
}

/// Wrap a cell string with ANSI awareness, optionally applying grey row styling.
fn wrap_ansi_cell(s: &str, max_width: usize, is_grey_row: bool) -> Vec<String> {
    let (prefix, content, suffix) = split_cell_ansi(s);
    let lines = wrap_text(content, max_width);

    lines
        .into_iter()
        .map(|line| {
            let padding = max_width.saturating_sub(char_len(&line));
            let padded = format!("{}{}", line, " ".repeat(padding));
            if is_grey_row {
                format!(
                    "{}{}{}{}{}",
                    SetForegroundColor(Color::Grey),
                    prefix,
                    padded,
                    suffix,
                    ResetColor
                )
            } else {
                format!("{prefix}{padded}{suffix}")
            }
        })
        .collect()
}

/// Minimum readable column width for short columns.
const MIN_WIDTH: usize = 8;

/// Minimum terminal width to enforce (treat narrower terminals as this wide).
const MIN_TERMINAL_WIDTH: u16 = 20;

/// Extra lines counted by [`fit_widths`] for a cell that starts to wrap.
const STARTS_TO_WRAP: usize = 3;

/// Number of separator characters between columns.
const SEPARATOR_WIDTH: usize = 2;

/// Fit natural column widths within the terminal width.
///
/// When the natural widths (plus separators) exceed the terminal width,
/// columns shrink in steps. Each step shrinks the column, and picks the
/// width, that adds the fewest lines for each character it frees. Lines
/// added are those added to the table's height plus those added to the
/// column's own cells, and a cell that starts to wrap counts two more, so
/// a long text that already wraps takes more lines before an id or a URL
/// wraps. A column whose one long cell is in a row that
/// another column already wraps frees its extra width for one line, so it
/// shrinks before a column that would wrap every row. On a tie, the step
/// that frees more characters wins, then the widest column. Columns shrink
/// to their
/// longest word first, and below that, splitting words, only when the table
/// still does not fit. A column never shrinks below its header, or below 8
/// characters unless it is narrower than that.
///
/// If the natural widths fit, they are returned unchanged.
fn fit_widths(
    natural: Vec<usize>,
    headers: &[&str],
    rows: &[Vec<String>],
    terminal_width: u16,
) -> Vec<usize> {
    let col_count = natural.len();
    let term_width = terminal_width.max(MIN_TERMINAL_WIDTH) as usize;
    let separator_total = col_count.saturating_sub(1) * SEPARATOR_WIDTH;
    let available = term_width.saturating_sub(separator_total);

    if natural.iter().sum::<usize>() <= available {
        return natural;
    }

    // columns[c][r] is the text of row r in column c, without its styling.
    let columns: Vec<Vec<&str>> = (0..col_count)
        .map(|c| {
            rows.iter()
                .map(|row| row.get(c).map_or("", |cell| split_cell_ansi(cell).1))
                .collect()
        })
        .collect();
    let lines_at = |c: usize, width: usize| -> Vec<usize> {
        columns[c]
            .iter()
            .map(|text| wrap_text(text, width).len())
            .collect()
    };
    let floor: Vec<usize> = natural
        .iter()
        .zip(headers)
        .map(|(&w, h)| w.min(MIN_WIDTH).max(char_len(h)))
        .collect();
    let longest_word: Vec<usize> = columns
        .iter()
        .zip(&floor)
        .map(|(texts, &f)| {
            texts
                .iter()
                .flat_map(|text| text.split_inclusive(is_break))
                .map(|word| char_len(word.trim()))
                .max()
                .unwrap_or(0)
                .max(f)
        })
        .collect();

    let mut widths = natural;
    let mut lines: Vec<Vec<usize>> = (0..col_count).map(|c| lines_at(c, widths[c])).collect();
    for limit in [&longest_word, &floor] {
        loop {
            let excess = widths.iter().sum::<usize>().saturating_sub(available);
            if excess == 0 {
                break;
            }
            let mut best: Option<Step> = None;
            for c in 0..col_count {
                let lowest = limit[c].max(widths[c].saturating_sub(excess));
                for width in (lowest..widths[c]).rev() {
                    let shrunk = lines_at(c, width);
                    let step = Step {
                        column: c,
                        width,
                        freed: widths[c] - width,
                        cost: added_lines(&lines, c, &shrunk),
                        lines: shrunk,
                    };
                    if best.as_ref().is_none_or(|b| step.beats(b, &widths)) {
                        best = Some(step);
                    }
                }
            }
            let Some(step) = best else {
                break;
            };
            widths[step.column] = step.width;
            lines[step.column] = step.lines;
        }
    }
    widths
}

/// One candidate step of [`fit_widths`]: shrink `column` to `width`.
struct Step {
    column: usize,
    width: usize,
    freed: usize,
    cost: usize,
    lines: Vec<usize>,
}

impl Step {
    /// Fewer lines per character freed, then more characters freed, then the
    /// wider column.
    fn beats(&self, other: &Step, widths: &[usize]) -> bool {
        let (mine, theirs) = (self.cost * other.freed, other.cost * self.freed);
        mine < theirs
            || (mine == theirs
                && (self.freed > other.freed
                    || (self.freed == other.freed && widths[self.column] > widths[other.column])))
    }
}

/// Where [`wrap_text`] may break a line: after whitespace or punctuation.
fn is_break(c: char) -> bool {
    c.is_whitespace() || ",.;:!?-".contains(c)
}

/// The lines added when column `c` wraps to `shrunk` (lines per row): the
/// increase in the height of each row plus the increase in the column's own
/// cells, plus [`STARTS_TO_WRAP`] for each cell that starts to wrap.
fn added_lines(lines: &[Vec<usize>], c: usize, shrunk: &[usize]) -> usize {
    let mut added = 0;
    for (r, &new) in shrunk.iter().enumerate() {
        let others = lines
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != c)
            .map(|(_, col)| col[r])
            .max()
            .unwrap_or(0);
        let old_height = others.max(lines[c][r]);
        added += others.max(new) - old_height;
        added += new - lines[c][r];
        if lines[c][r] == 1 && new > 1 {
            added += STARTS_TO_WRAP;
        }
    }
    added
}

/// Word-wrap text to fit within `max_width` characters.
///
/// Splitting occurs at whitespace or punctuation characters (including
/// hyphens). Words longer than `max_width` are character-wrapped as a
/// fallback. The result is a vector of lines that, when joined,
/// reconstitute the original text (minus trailing spaces).
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if text.is_empty() || char_len(text) <= max_width {
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();

    for word in text.split_inclusive(is_break) {
        let word_trimmed = word.trim_start();
        // A word that follows punctuation keeps its leading space within a
        // line, and drops it at the start of one.
        let piece = if current_line.is_empty() {
            word_trimmed
        } else {
            word
        };

        if char_len(&current_line) + char_len(piece) <= max_width {
            current_line.push_str(piece);
        } else {
            // Flush current line if not empty
            if !current_line.is_empty() {
                lines.push(current_line.trim_end().to_string());
                current_line = String::new();
            }

            // Handle word longer than max_width (character-wrap fallback)
            if char_len(word_trimmed) > max_width {
                let mut remaining = word_trimmed;
                while char_len(remaining) > max_width {
                    let split = remaining
                        .char_indices()
                        .nth(max_width)
                        .map_or(remaining.len(), |(i, _)| i);
                    let (chunk, rest) = remaining.split_at(split);
                    lines.push(chunk.to_string());
                    remaining = rest;
                }
                if !remaining.is_empty() {
                    current_line.push_str(remaining);
                }
            } else {
                current_line.push_str(word_trimmed);
            }
        }
    }

    if !current_line.is_empty() {
        lines.push(current_line.trim_end().to_string());
    }

    // Ensure at least one line
    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}

/// Render data rows with word-wrapping support.
///
/// Each cell is wrapped independently to its column width. Grey row styling
/// is applied at the cell level so that ANSI-coloured cells (e.g. from
/// `warn_mark`) are not overridden by the grey background.
fn render_rows_with_wrapping(rows: &[Vec<String>], widths: &[usize]) {
    for (row_idx, row) in rows.iter().enumerate() {
        let is_grey = row_idx % 2 == 1;

        // Wrap each cell with ANSI awareness
        let wrapped_cells: Vec<Vec<String>> = row
            .iter()
            .zip(widths.iter())
            .map(|(cell, width)| wrap_ansi_cell(cell, *width, is_grey))
            .collect();

        let max_lines = wrapped_cells.iter().map(|l| l.len()).max().unwrap_or(1);

        for line_idx in 0..max_lines {
            let line_parts: Vec<String> = wrapped_cells
                .iter()
                .zip(widths.iter())
                .map(|(cell_lines, width)| {
                    cell_lines
                        .get(line_idx)
                        .cloned()
                        .unwrap_or_else(|| " ".repeat(*width))
                })
                .collect();
            let line = line_parts.join("  ");
            println!("{line}");
        }
    }
}
/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_text_short_no_wrapping() {
        assert_eq!(wrap_text("hello", 10), vec!["hello"]);
    }

    #[test]
    fn wrap_text_empty() {
        assert_eq!(wrap_text("", 10), vec![""]);
    }

    #[test]
    fn wrap_text_at_space() {
        assert_eq!(wrap_text("hello world", 8), vec!["hello", "world"]);
    }

    #[test]
    fn wrap_text_at_hyphen() {
        assert_eq!(wrap_text("granite-3.0-8b", 12), vec!["granite-3.0-", "8b"]);
    }

    #[test]
    fn wrap_text_at_comma() {
        assert_eq!(wrap_text("Hello, world", 8), vec!["Hello,", "world"]);
    }

    #[test]
    fn wrap_text_character_wrap_long_word() {
        assert_eq!(wrap_text("verylongword", 6), vec!["verylo", "ngword"]);
    }

    #[test]
    fn wrap_text_multiple_breaks() {
        let result = wrap_text("a b c d e", 4);
        assert_eq!(result, vec!["a b", "c d", "e"]);
    }

    #[test]
    fn wrap_text_multiple_words() {
        let result = wrap_text("hello world this is a test", 12);
        assert_eq!(result, vec!["hello world", "this is a", "test"]);
    }

    #[test]
    fn wrap_text_keeps_the_space_after_punctuation() {
        assert_eq!(
            wrap_text("legacy config: run setup to set", 20),
            vec!["legacy config: run", "setup to set"]
        );
        assert_eq!(
            wrap_text("legacy config: run setup", 14),
            vec!["legacy config:", "run setup"]
        );
    }

    #[test]
    fn wrap_text_counts_characters() {
        assert_eq!(
            wrap_text("⚠ local (not set)", 17),
            vec!["⚠ local (not set)"]
        );
        assert_eq!(wrap_text("⚠⚠⚠⚠⚠⚠", 4), vec!["⚠⚠⚠⚠", "⚠⚠"]);
    }

    #[test]
    fn visible_len_counts_characters_and_skips_ansi() {
        assert_eq!(visible_len("\x1b[33m\x1b[1m⚠ not specified\x1b[0m"), 15);
    }

    #[test]
    fn wrap_text_punctuation_chain() {
        let result = wrap_text("one.two,three.four", 8);
        assert_eq!(result, vec!["one.two,", "three.", "four"]);
    }

    fn cells(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|row| row.iter().map(|c| c.to_string()).collect())
            .collect()
    }

    #[test]
    fn fit_widths_unchanged_when_they_fit() {
        let rows = cells(&[&["a", "b", "c"]]);
        assert_eq!(
            fit_widths(vec![10, 10, 10], &["A", "B", "C"], &rows, 50),
            vec![10, 10, 10]
        );
        // Natural total (20) == available (22 - 2).
        assert_eq!(
            fit_widths(vec![10, 10], &["A", "B"], &rows, 22),
            vec![10, 10]
        );
    }

    #[test]
    fn fit_widths_shrinks_a_column_whose_long_cell_is_in_a_tall_row() {
        // `provider list` with legacy notes, in 100 columns.
        let headers = ["ID", "TYPE", "BASE URL", "HARDWARE", "NOTES"];
        let rows = cells(&[
            &[
                "ollama",
                "ollama",
                "http://localhost:11434",
                "⚠ local (not set)",
                "legacy config: run `granite-cli provider setup ollama` to set",
            ],
            &[
                "vllm-lab",
                "openai-compatible",
                "http://lab.example:8000",
                "⚠ local (not set)",
                "legacy config: run `granite-cli provider setup openai-compatible --id vllm-lab` to set",
            ],
            &[
                "openrouter",
                "openrouter",
                "https://openrouter.ai/api",
                "⚠ local (not set)",
                "legacy config: run `granite-cli provider setup openrouter` to set",
            ],
        ]);
        let natural = vec![10, 17, 25, 17, 86];

        let widths = fit_widths(natural, &headers, &rows, 100);

        // openai-compatible wraps in a row that NOTES already wraps, and
        // NOTES takes the rest; ids, URLs and hardware keep one line.
        assert_eq!(widths, vec![10, 10, 25, 17, 30]);
    }

    #[test]
    fn fit_widths_keeps_short_columns_when_one_cell_is_long() {
        // `hardware list` with a load warning in NOTES, in 100 columns.
        let headers = ["ID", "KIND", "MEMORY", "OS", "NOTES"];
        let long = "word ".repeat(14);
        let rows = cells(&[&["local", "unified 29.2 GB", "29.2 GB", "macos", long.trim()]]);
        let natural = vec![5, 15, 7, 5, 69];

        let widths = fit_widths(natural, &headers, &rows, 100);

        assert_eq!(widths, vec![5, 15, 7, 5, 60]);
    }

    #[test]
    fn fit_widths_never_cuts_a_header_or_widens_a_column() {
        let headers = ["IDENTIFIER", "X"];
        let rows = cells(&[&["a b c d e f g h i j k l", "y"]]);

        let widths = fit_widths(vec![23, 1], &headers, &rows, 20);

        assert_eq!(widths, vec![17, 1]);

        let widths = fit_widths(vec![23, 1], &headers, &rows, 10);
        // The terminal is treated as 20 columns wide.
        assert_eq!(widths, vec![17, 1]);
    }

    #[test]
    fn fit_widths_stops_at_the_floor() {
        let headers = ["A"; 10];
        let rows = cells(&[&["word word"; 10]]);

        let widths = fit_widths(vec![9; 10], &headers, &rows, 30);

        assert_eq!(widths, vec![8; 10]);
    }

    #[test]
    fn render_rows_with_wrapping_produces_no_panic() {
        let rows = vec![
            vec!["hello".to_string(), "world".to_string()],
            vec!["short".to_string(), "line".to_string()],
        ];
        let widths = vec![8, 8];
        render_rows_with_wrapping(&rows, &widths);
    }

    #[test]
    fn render_rows_with_wrapping_empty_rows() {
        let rows: Vec<Vec<String>> = vec![];
        let widths = vec![8, 8];
        render_rows_with_wrapping(&rows, &widths);
    }
}
