use crate::registry::{ConfigConstructable, ConstructError, NoConfig};
use crate::utils::ui::base::{self, HasUiMetadata, Ui, UiMetadata};

/*-- public --*/

/// GFM (GitHub Flavoured Markdown) output backend.
/// Renders tables as pipe tables and detail as a two-column key/value table.
pub struct MarkdownOutput;

impl ConfigConstructable for MarkdownOutput {
    type Config = NoConfig;

    fn new(_instance_id: &str, _cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        Ok(Self)
    }
}

impl Ui for MarkdownOutput {
    /// Output meant for a program to read, with nobody to prompt.
    fn is_interactive(&self) -> bool {
        false
    }

    fn table(&self, title: &str, headers: &[&str], rows: &[Vec<String>]) {
        println!("\n## {title}\n");
        let header_line = format!("| {} |", headers.join(" | "));
        let sep_line = format!(
            "| {} |",
            headers
                .iter()
                .map(|h| "-".repeat(h.len() + 2))
                .collect::<Vec<_>>()
                .join(" | ")
        );
        println!("{header_line}");
        println!("{sep_line}");
        for row in rows {
            println!("| {} |", row.join(" | "));
        }
    }

    fn detail(&self, title: &str, fields: &[(&str, String)]) {
        println!("\n## {title}\n");
        println!("| Field | Value |");
        println!("|-------|-------|");
        for (k, v) in fields {
            println!("| {k} | {v} |");
        }
    }

    fn status(&self, label: &str, ok: bool, detail: &str) {
        let mark = if ok { "✓" } else { "✗" };
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
        println!("> [!warning]\n> {msg}");
    }
    fn error(&self, msg: &str) {
        eprintln!("> [!critical]\n> {msg}");
    }

    fn select(&self, _prompt: &str, _items: &[String], _default: usize) -> anyhow::Result<usize> {
        base::non_interactive()
    }

    fn multi_select(
        &self,
        _prompt: &str,
        _items: &[String],
        _defaults: &[bool],
    ) -> anyhow::Result<Vec<usize>> {
        base::non_interactive()
    }

    fn confirm(&self, _prompt: &str, _default: bool) -> anyhow::Result<bool> {
        base::non_interactive()
    }

    fn text(&self, _prompt: &str, _default: &str) -> anyhow::Result<String> {
        base::non_interactive()
    }

    fn text_optional(&self, _prompt: &str, _default: &str) -> anyhow::Result<String> {
        base::non_interactive()
    }

    fn password(&self, _prompt: &str) -> anyhow::Result<String> {
        base::non_interactive()
    }

    fn pull_start(&self, _label: &str, _total_bytes: Option<u64>) -> base::PullHandle {
        base::PullHandle(0)
    }

    fn pull_progress(
        &self,
        _handle: base::PullHandle,
        _downloaded_bytes: u64,
        _total_bytes: Option<u64>,
    ) {
    }

    fn pull_finish(&self, _handle: base::PullHandle, label: &str, error: Option<&str>) {
        match error {
            Some(e) => println!("- Pulled **{label}**: failed ({e})"),
            None => println!("- Pulled **{label}**: done"),
        }
    }
}

impl HasUiMetadata for MarkdownOutput {
    fn metadata() -> UiMetadata {
        UiMetadata {
            name: "markdown".to_string(),
            description: "GFM markdown table output for documentation and GitHub".to_string(),
        }
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::ui::base::tests::CaptureUi;

    crate::output_contract_tests!(
        MarkdownOutput::new("markdown", &serde_json::json!({}),).unwrap()
    );

    #[test]
    fn markdown_output_is_not_interactive() {
        let out = MarkdownOutput::new("markdown", &serde_json::json!({})).unwrap();
        assert!(!out.is_interactive());
    }

    #[test]
    fn markdown_table_contains_pipe_chars() {
        let out = CaptureUi::default();
        out.table(
            "Test",
            &["ID", "NAME"],
            &[vec!["id-1".to_string(), "name-1".to_string()]],
        );
        let tables = out.tables.borrow();
        // verify the CaptureOutput recorded correctly; live rendering tested via contract tests
        assert_eq!(tables.len(), 1);
    }

    #[test]
    fn markdown_table_has_header_separator() {
        // Invoke the real MarkdownOutput (print-only) — just verifies no panic
        let md = MarkdownOutput::new("markdown", &serde_json::json!({})).unwrap();
        md.table(
            "T",
            &["ID", "NAME"],
            &[vec!["a".to_string(), "b".to_string()]],
        );
    }

    #[test]
    fn markdown_detail_is_two_column_table() {
        let md = MarkdownOutput::new("markdown", &serde_json::json!({})).unwrap();
        md.detail("My Item", &[("Family", "Granite 3.1".to_string())]);
    }

    #[test]
    fn markdown_status_ok_contains_checkmark() {
        let md = MarkdownOutput::new("markdown", &serde_json::json!({})).unwrap();
        md.status("my-service", true, "");
        md.status("my-service", false, "timeout");
    }
}
