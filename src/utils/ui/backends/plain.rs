use crate::registry::{ConfigConstructable, ConstructError, NoConfig};
use crate::utils::ui::base::{HasUiMetadata, Ui, UiMetadata};

/*-- public --*/

/// Plain text backend with no ANSI codes.
/// Suitable for CI, piped output, and non-ANSI terminals.
pub struct PlainOutput;

impl ConfigConstructable for PlainOutput {
    type Config = NoConfig;

    fn new(_instance_id: &str, _cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        Ok(Self)
    }
}

impl Ui for PlainOutput {
    fn table(&self, title: &str, headers: &[&str], rows: &[Vec<String>]) {
        println!("\n{title}");
        let col_count = headers.len();
        let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
        for row in rows {
            for (i, cell) in row.iter().enumerate() {
                if i < col_count {
                    widths[i] = widths[i].max(cell.len());
                }
            }
        }
        let header_line: String = headers
            .iter()
            .zip(widths.iter())
            .map(|(h, w)| format!("{h:<w$}"))
            .collect::<Vec<_>>()
            .join("  ");
        println!("{header_line}");
        let sep: String = widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  ");
        println!("{sep}");
        for row in rows {
            let line: String = row
                .iter()
                .zip(widths.iter())
                .map(|(c, w)| format!("{c:<w$}"))
                .collect::<Vec<_>>()
                .join("  ");
            println!("{line}");
        }
    }

    fn detail(&self, title: &str, fields: &[(&str, String)]) {
        println!("\n{title}");
        let key_width = fields.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        for (k, v) in fields {
            println!("  {k:<key_width$}  {v}");
        }
    }

    fn status(&self, label: &str, ok: bool, detail: &str) {
        let mark = if ok { "[OK]  " } else { "[FAIL]" };
        if detail.is_empty() {
            println!("{mark} {label}");
        } else {
            println!("{mark} {label}  {detail}");
        }
    }

    fn info(&self, msg: &str) {
        println!("{msg}");
    }
    fn warn(&self, msg: &str) {
        eprintln!("Warning: {msg}");
    }
    fn error(&self, msg: &str) {
        eprintln!("Error: {msg}");
    }

    fn pull_start(
        &self,
        label: &str,
        total_bytes: Option<u64>,
    ) -> crate::utils::ui::base::PullHandle {
        match total_bytes {
            Some(total) => println!("Pulling {label} ({total} bytes total)..."),
            None => println!("Pulling {label}..."),
        }
        crate::utils::ui::base::PullHandle(0)
    }

    fn pull_progress(
        &self,
        _handle: crate::utils::ui::base::PullHandle,
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
    ) {
        match total_bytes {
            Some(total) if total > 0 => {
                let pct =
                    ((downloaded_bytes as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as u32;
                println!("  {pct}%");
            }
            _ => println!("  {downloaded_bytes} bytes"),
        }
    }

    fn pull_finish(
        &self,
        _handle: crate::utils::ui::base::PullHandle,
        label: &str,
        error: Option<&str>,
    ) {
        match error {
            Some(e) => println!("{label}: failed: {e}"),
            None => println!("{label}: done"),
        }
    }
}

impl HasUiMetadata for PlainOutput {
    fn metadata() -> UiMetadata {
        UiMetadata {
            name: "plain".to_string(),
            description: "Plain text output without ANSI codes".to_string(),
        }
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;

    crate::output_contract_tests!(PlainOutput::new("plain", &serde_json::json!({}),).unwrap());
}
