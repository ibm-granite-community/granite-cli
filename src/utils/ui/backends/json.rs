use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::registry::{ConfigConstructable, ConstructError, NoConfig};
use crate::utils::ui::base::{self, HasUiMetadata, Ui, UiMetadata};

/*-- public --*/

/// JSON backend: emits one JSON object per `Output` call, newline-delimited.
///
/// The `buf` field is only `Some` in test builds (constructed via
/// `Jsonbase::with_capture()`). In production the backend writes
/// directly to stdout.
pub struct JsonOutput {
    writer: Mutex<Box<dyn Write + Send>>,
    /// Shared capture buffer — `Some` only when constructed via `with_capture`.
    buf: Option<Arc<Mutex<Vec<u8>>>>,
}

impl JsonOutput {
    /// Construct a capturing instance whose output can be read back via
    /// `captured()`. Used in tests only.
    pub fn with_capture() -> Self {
        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writer: Box<dyn Write + Send> = Box::new(SharedWriter(Arc::clone(&buf)));
        Self {
            writer: Mutex::new(writer),
            buf: Some(buf),
        }
    }

    /// Return all emitted JSON objects in emission order.
    /// Panics if called on a non-capturing instance.
    pub fn captured(&self) -> Vec<serde_json::Value> {
        let buf = self
            .buf
            .as_ref()
            .expect("captured() called on stdout JsonOutput")
            .lock()
            .unwrap();
        String::from_utf8_lossy(&buf)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).expect("JsonOutput emitted invalid JSON"))
            .collect()
    }

    fn emit(&self, value: serde_json::Value) {
        let mut w = self.writer.lock().unwrap();
        let _ = writeln!(w, "{value}");
    }
}

/// A `Write` impl backed by a shared `Arc<Mutex<Vec<u8>>>` so the buffer
/// can be read back without downcasting.
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl ConfigConstructable for JsonOutput {
    type Config = NoConfig;

    fn new(_instance_id: &str, _cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        Ok(Self {
            writer: Mutex::new(Box::new(std::io::stdout())),
            buf: None,
        })
    }
}

impl Ui for JsonOutput {
    /// Output meant for a program to read, with nobody to prompt.
    fn is_interactive(&self) -> bool {
        false
    }

    fn table(&self, title: &str, headers: &[&str], rows: &[Vec<String>]) {
        self.emit(serde_json::json!({
            "type": "table",
            "title": title,
            "headers": headers,
            "rows": rows,
        }));
    }

    fn detail(&self, title: &str, fields: &[(&str, String)]) {
        let obj: serde_json::Map<String, serde_json::Value> = fields
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.clone())))
            .collect();
        self.emit(serde_json::json!({
            "type": "detail",
            "title": title,
            "fields": obj,
        }));
    }

    fn status(&self, label: &str, ok: bool, detail: &str) {
        self.emit(serde_json::json!({
            "type": "status",
            "label": label,
            "ok": ok,
            "detail": detail,
        }));
    }

    fn info(&self, msg: &str) {
        self.emit(serde_json::json!({ "type": "info", "message": msg }));
    }

    fn warn(&self, msg: &str) {
        self.emit(serde_json::json!({ "type": "warn", "message": msg }));
    }

    fn error(&self, msg: &str) {
        self.emit(serde_json::json!({ "type": "error", "message": msg }));
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
        self.emit(serde_json::json!({
            "type": "pull",
            "label": label,
            "success": error.is_none(),
            "error": error,
        }));
    }
}

impl HasUiMetadata for JsonOutput {
    fn metadata() -> UiMetadata {
        UiMetadata {
            name: "json".to_string(),
            description: "Newline-delimited JSON output for scripting".to_string(),
        }
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;

    fn make() -> JsonOutput {
        JsonOutput::with_capture()
    }

    crate::output_contract_tests!(make());

    #[test]
    fn json_output_is_not_interactive() {
        // Machine-readable output has nobody to prompt, so commands fall
        // back to leaving a broken reference alone rather than asking.
        assert!(!make().is_interactive());
    }

    #[test]
    fn json_table_output_is_valid_json_with_correct_type() {
        let out = make();
        out.table(
            "T",
            &["A", "B"],
            &[vec!["r1a".to_string(), "r1b".to_string()]],
        );
        let vals = out.captured();
        assert_eq!(vals.len(), 1);
        assert_eq!(vals[0]["type"], "table");
    }

    #[test]
    fn json_table_contains_title_headers_rows_keys() {
        let out = make();
        out.table("My Table", &["X"], &[vec!["cell".to_string()]]);
        let vals = out.captured();
        assert_eq!(vals[0]["title"], "My Table");
        assert!(vals[0]["headers"].is_array());
        assert!(vals[0]["rows"].is_array());
    }

    #[test]
    fn json_detail_contains_title_and_fields_keys() {
        let out = make();
        out.detail("Item", &[("key", "value".to_string())]);
        let vals = out.captured();
        assert_eq!(vals[0]["type"], "detail");
        assert_eq!(vals[0]["title"], "Item");
        assert!(vals[0]["fields"].is_object());
        assert_eq!(vals[0]["fields"]["key"], "value");
    }

    #[test]
    fn json_info_has_type_and_message() {
        let out = make();
        out.info("hello world");
        let vals = out.captured();
        assert_eq!(vals[0]["type"], "info");
        assert_eq!(vals[0]["message"], "hello world");
    }

    #[test]
    fn json_status_has_ok_flag() {
        let out = make();
        out.status("svc", true, "fast");
        let vals = out.captured();
        assert_eq!(vals[0]["type"], "status");
        assert_eq!(vals[0]["ok"], true);
    }
}
