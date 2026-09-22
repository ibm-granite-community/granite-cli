use std::any::Any;
use std::sync::LazyLock;

use crate::registry::ConfigConstructable;

/*-- public --*/

pub type TableRow = Vec<String>;
pub type TableEntry = (String, Vec<String>, Vec<TableRow>);
pub type DetailField = (String, String);
pub type DetailEntry = (String, Vec<DetailField>);
pub type SelectEntry = (String, Vec<String>, usize);
pub type MultiSelectEntry = (String, Vec<String>, Vec<bool>);

/// Metadata describing a registered UI backend.
#[derive(Debug, Clone)]
pub struct UiMetadata {
    pub name: String,
    pub description: String,
}

/// Opaque handle identifying an in-flight pull/download for progress reporting.
/// Allocated by [`Ui::pull_start`] and passed back into [`Ui::pull_progress`]/[`Ui::pull_finish`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PullHandle(pub u64);

// Generate UiFactory and HasUiMetadata via the existing macro.
use crate::define_factory;
define_factory!(Ui, UiMetadata, UiFactory);

/// Global registry of UI backends.
/// Backends are registered by name and constructed on demand via --output flag.
pub static UI_REGISTRY: LazyLock<UiFactory> = LazyLock::new(|| {
    let mut f = UiFactory::new();
    f.register::<crate::utils::ui::backends::terminal::TerminalOutput>("terminal");
    f.register::<crate::utils::ui::backends::plain::PlainOutput>("plain");
    f.register::<crate::utils::ui::backends::json::JsonOutput>("json");
    f.register::<crate::utils::ui::backends::markdown::MarkdownOutput>("markdown");
    f
});

/// Shared error for backends (json, markdown) that render for scripting/docs
/// rather than a live session, and so can't sensibly block on interactive input.
pub(crate) fn non_interactive<T>() -> anyhow::Result<T> {
    anyhow::bail!(
        "interactive prompts are not supported with this output format; rerun with --output=terminal or --output=plain"
    )
}

/// Generates panic-safety contract tests for any [`Ui`] implementation.
/// Invoke with the constructor expression as argument:
///
/// ```ignore
/// output_contract_tests!(PlainOutput::new("plain", &serde_json::json!({})).unwrap());
/// ```
#[macro_export]
macro_rules! output_contract_tests {
    ($make:expr) => {
        #[test]
        fn contract_empty_table_does_not_panic() {
            $make.table("T", &["A"], &[]);
        }
        #[test]
        fn contract_single_row_table_does_not_panic() {
            $make.table("T", &["A"], &[vec!["x".to_string()]]);
        }
        #[test]
        fn contract_hundred_row_table_does_not_panic() {
            let rows: Vec<Vec<String>> = (0..100)
                .map(|i| vec![format!("id-{}", i), format!("val-{}", i)])
                .collect();
            $make.table("Big", &["ID", "VAL"], &rows);
        }
        #[test]
        fn contract_table_with_empty_cell_does_not_panic() {
            $make.table("T", &["A"], &[vec!["".to_string()]]);
        }
        #[test]
        fn contract_detail_with_no_fields_does_not_panic() {
            $make.detail("Empty", &[]);
        }
        #[test]
        fn contract_status_ok_does_not_panic() {
            $make.status("svc", true, "");
        }
        #[test]
        fn contract_status_fail_does_not_panic() {
            $make.status("svc", false, "timed out");
        }
        #[test]
        fn contract_info_empty_string_does_not_panic() {
            $make.info("");
            $make.warn("");
            $make.error("");
        }
        #[test]
        fn contract_mark_does_not_panic() {
            $make.ok("");
            $make.warn_mark("");
            $make.error_mark("");
            $make.detail_mark("");
        }
        #[test]
        fn contract_pull_lifecycle_does_not_panic() {
            let handle = $make.pull_start("model", Some(100));
            $make.pull_progress(handle, 50, Some(100));
            $make.pull_finish(handle, "model", None);
            let handle2 = $make.pull_start("model2", None);
            $make.pull_progress(handle2, 0, None);
            $make.pull_finish(handle2, "model2", Some("failed"));
        }
    };
}

/// The core UI abstraction: rendering output and prompting for input.
///
/// All command methods receive `ui: &dyn Ui` as their final parameter.
/// Command code never calls `println!` or `dialoguer` directly — it calls
/// these methods and the registered backend decides how to render and
/// whether/how to prompt.
///
/// `Any` is included as a supertrait so that `&dyn Ui` can be downcast
/// to concrete types at runtime via `downcast_ref::<T>()`. This enables
/// test inspection of `CaptureUi` fields and allows prod code to
/// access type-specific methods when necessary.
pub trait Ui: Send + Sync + Any {
    /// Render a tabular result (catalog, list, health).
    fn table(&self, title: &str, headers: &[&str], rows: &[Vec<String>]);

    /// Render a key-value detail block (info commands).
    fn detail(&self, title: &str, fields: &[(&str, String)]);

    /// Render a single status row with pass/fail indicator (health checks).
    fn status(&self, label: &str, ok: bool, detail: &str);

    /// Plain informational message.
    fn info(&self, msg: &str);

    /// Warning message.
    fn warn(&self, msg: &str);

    /// Error message. Implementations should route this to stderr.
    fn error(&self, msg: &str);

    /// Begin tracking a long-running pull/download. `total_bytes` is `None`
    /// if the size isn't known upfront. Returns a handle for subsequent
    /// `pull_progress`/`pull_finish` calls.
    fn pull_start(&self, label: &str, total_bytes: Option<u64>) -> PullHandle;

    /// Report incremental progress for a handle returned by `pull_start`.
    /// Implementations may throttle how often this actually renders.
    fn pull_progress(&self, handle: PullHandle, downloaded_bytes: u64, total_bytes: Option<u64>);

    /// Mark a handle as finished, successfully or with an error message.
    fn pull_finish(&self, handle: PullHandle, label: &str, error: Option<&str>);

    /// Apply severity-based markings (e.g. green) to `msg`.
    fn ok(&self, msg: &str) -> String {
        msg.to_string()
    }

    /// Apply severity-based markings (e.g. yellow) to `msg`.
    fn warn_mark(&self, msg: &str) -> String {
        msg.to_string()
    }

    /// Apply severity-based markings (e.g. red) to `msg`.
    fn error_mark(&self, msg: &str) -> String {
        msg.to_string()
    }

    /// Apply severity-based markings (e.g. gray) to `msg`.
    fn detail_mark(&self, msg: &str) -> String {
        msg.to_string()
    }

    /// Whether this session can prompt at all. False for backends whose
    /// output is meant to be read by a program rather than a person, which
    /// have nobody to ask.
    fn is_interactive(&self) -> bool {
        true
    }

    /// Ask the user to pick one of `items`, returning the chosen index.
    fn select(&self, prompt: &str, items: &[String], default: usize) -> anyhow::Result<usize> {
        Ok(dialoguer::Select::new()
            .with_prompt(prompt)
            .items(items)
            .default(default)
            .interact()?)
    }

    /// Ask the user to pick zero or more of `items`, returning the chosen indices.
    /// `defaults` is a parallel slice of booleans — `true` means the item is
    /// pre-checked. Callers may pass an empty `defaults` slice; unchecked is the
    /// fallback for any item without a corresponding entry.
    fn multi_select(
        &self,
        prompt: &str,
        items: &[String],
        defaults: &[bool],
    ) -> anyhow::Result<Vec<usize>> {
        // Use interact_opt so that non-TTY environments (CI, tests, pipes) get
        // an Err instead of blocking — matching the behaviour of dialoguer's
        // Select/Confirm/Input which all error on non-TTY via interact().
        dialoguer::MultiSelect::new()
            .with_prompt(format!("{} {}", prompt, self.detail_mark("[SPACE]")))
            .items(items)
            .defaults(defaults)
            .interact_opt()?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "interactive prompts are not supported outside of a terminal; \
                 rerun with --output=terminal or --output=plain"
                )
            })
    }

    /// Ask a yes/no question.
    fn confirm(&self, prompt: &str, default: bool) -> anyhow::Result<bool> {
        Ok(dialoguer::Confirm::new()
            .with_prompt(prompt)
            .default(default)
            .interact()?)
    }

    /// Ask for a line of free text, pre-filled with `default`. Rejects a
    /// blank submission, re-prompting instead of returning "".
    fn text(&self, prompt: &str, default: &str) -> anyhow::Result<String> {
        Ok(dialoguer::Input::new()
            .with_prompt(prompt)
            .with_initial_text(default)
            .interact_text()?)
    }

    /// Like [`text`](Ui::text), but accepts a blank submission as a
    /// legitimate answer instead of re-prompting. For fields where empty
    /// means "unset" -- e.g. an optional `command_path` that falls back to
    /// PATH discovery -- rather than a value that's merely allowed to be
    /// the empty string.
    fn text_optional(&self, prompt: &str, default: &str) -> anyhow::Result<String> {
        Ok(dialoguer::Input::new()
            .with_prompt(prompt)
            .with_initial_text(default)
            .allow_empty(true)
            .interact_text()?)
    }

    /// Ask for a secret value (masked input, not echoed).
    fn password(&self, prompt: &str) -> anyhow::Result<String> {
        Ok(dialoguer::Password::new()
            .with_prompt(prompt)
            .allow_empty_password(true)
            .interact()?)
    }
}

/*-- tests --*/

// NOTE: tests module is crate public for CaptureUi reuse
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// A test double that records every `Ui` call into inspectable `Vec`s,
    /// and answers `select`/`confirm`/`text`/`password` from canned queues
    /// (falling back to the method's own `default` when a queue is empty).
    ///
    /// Uses interior mutability (`RefCell`) because `Ui` methods take `&self`.
    /// Safe for single-threaded test code.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let ui = CaptureUi::default();
    /// ModelCommands::catalog(&ctx, None, &ui).unwrap();
    /// let (title, headers, rows) = &ui.tables.borrow()[0];
    /// assert!(headers.contains(&"FAMILY".to_string()));
    /// ```
    #[derive(Default)]
    pub struct CaptureUi {
        /// (title, headers, rows) for each table() call
        pub tables: RefCell<Vec<TableEntry>>,
        /// (title, fields) for each detail() call
        pub details: RefCell<Vec<DetailEntry>>,
        /// (label, ok, detail) for each status() call
        pub statuses: RefCell<Vec<(String, bool, String)>>,
        pub infos: RefCell<Vec<String>>,
        pub warns: RefCell<Vec<String>>,
        pub errors: RefCell<Vec<String>>,
        pub oks: RefCell<Vec<String>>,
        pub warn_marks: RefCell<Vec<String>>,
        pub error_marks: RefCell<Vec<String>>,
        pub detail_marks: RefCell<Vec<String>>,

        /// (prompt, items, default) for each select() call
        pub select_prompts: RefCell<Vec<SelectEntry>>,
        /// (prompt, items, defaults) for each multi_select() call
        pub multi_select_prompts: RefCell<Vec<MultiSelectEntry>>,
        /// (prompt, default) for each confirm() call
        pub confirm_prompts: RefCell<Vec<(String, bool)>>,
        /// (prompt, default) for each text() call
        pub text_prompts: RefCell<Vec<(String, String)>>,
        /// prompt for each password() call
        pub password_prompts: RefCell<Vec<String>>,

        /// Canned answers consumed in order by select(); falls back to `default` when empty.
        pub select_answers: RefCell<VecDeque<usize>>,
        /// Canned answers consumed in order by multi_select(); falls back to `vec![]` when empty.
        pub multi_select_answers: RefCell<VecDeque<Vec<usize>>>,
        /// Canned answers consumed in order by confirm(); falls back to `default` when empty.
        pub confirm_answers: RefCell<VecDeque<bool>>,
        /// Canned answers consumed in order by text(); falls back to `default` when empty.
        pub text_answers: RefCell<VecDeque<String>>,
        /// Canned answers consumed in order by password(); falls back to "" when empty.
        pub password_answers: RefCell<VecDeque<String>>,

        /// (label, total_bytes) for each pull_start() call, in handle-allocation order.
        pub pull_starts: RefCell<Vec<(String, Option<u64>)>>,
        /// (handle, downloaded_bytes, total_bytes) for each pull_progress() call.
        pub pull_progresses: RefCell<Vec<(PullHandle, u64, Option<u64>)>>,
        /// (handle, label, error) for each pull_finish() call.
        pub pull_finishes: RefCell<Vec<(PullHandle, String, Option<String>)>>,
        /// Counter used to allocate sequential PullHandles.
        pub next_pull_handle: RefCell<u64>,

        /// Answer for `is_interactive()`. `None`, the default, answers true,
        /// so a test only sets this when it is testing what happens without
        /// a person to ask.
        pub interactive: RefCell<Option<bool>>,
    }

    impl ConfigConstructable for CaptureUi {
        type Config = crate::registry::NoConfig;

        fn new(
            _instance_id: &str,
            _cfg: &serde_json::Value,
        ) -> Result<Self, crate::registry::ConstructError> {
            Ok(Self::default())
        }
    }

    impl Ui for CaptureUi {
        fn is_interactive(&self) -> bool {
            self.interactive.borrow().unwrap_or(true)
        }

        fn table(&self, title: &str, headers: &[&str], rows: &[Vec<String>]) {
            self.tables.borrow_mut().push((
                title.to_string(),
                headers.iter().map(|h| h.to_string()).collect(),
                rows.to_vec(),
            ));
        }

        fn detail(&self, title: &str, fields: &[(&str, String)]) {
            self.details.borrow_mut().push((
                title.to_string(),
                fields
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
            ));
        }

        fn status(&self, label: &str, ok: bool, detail: &str) {
            self.statuses
                .borrow_mut()
                .push((label.to_string(), ok, detail.to_string()));
        }

        fn info(&self, msg: &str) {
            self.infos.borrow_mut().push(msg.to_string());
        }

        fn warn(&self, msg: &str) {
            self.warns.borrow_mut().push(msg.to_string());
        }

        fn error(&self, msg: &str) {
            self.errors.borrow_mut().push(msg.to_string());
        }

        fn pull_start(&self, label: &str, total_bytes: Option<u64>) -> PullHandle {
            self.pull_starts
                .borrow_mut()
                .push((label.to_string(), total_bytes));
            let mut next = self.next_pull_handle.borrow_mut();
            let handle = PullHandle(*next);
            *next += 1;
            handle
        }

        fn pull_progress(
            &self,
            handle: PullHandle,
            downloaded_bytes: u64,
            total_bytes: Option<u64>,
        ) {
            self.pull_progresses
                .borrow_mut()
                .push((handle, downloaded_bytes, total_bytes));
        }

        fn pull_finish(&self, handle: PullHandle, label: &str, error: Option<&str>) {
            self.pull_finishes.borrow_mut().push((
                handle,
                label.to_string(),
                error.map(|e| e.to_string()),
            ));
        }

        fn ok(&self, msg: &str) -> String {
            self.oks.borrow_mut().push(msg.to_string());
            msg.to_string()
        }

        fn warn_mark(&self, msg: &str) -> String {
            self.warn_marks.borrow_mut().push(msg.to_string());
            msg.to_string()
        }

        fn error_mark(&self, msg: &str) -> String {
            self.error_marks.borrow_mut().push(msg.to_string());
            msg.to_string()
        }

        fn detail_mark(&self, msg: &str) -> String {
            self.detail_marks.borrow_mut().push(msg.to_string());
            msg.to_string()
        }

        fn select(&self, prompt: &str, items: &[String], default: usize) -> anyhow::Result<usize> {
            self.select_prompts
                .borrow_mut()
                .push((prompt.to_string(), items.to_vec(), default));
            Ok(self
                .select_answers
                .borrow_mut()
                .pop_front()
                .unwrap_or(default))
        }

        fn multi_select(
            &self,
            prompt: &str,
            items: &[String],
            defaults: &[bool],
        ) -> anyhow::Result<Vec<usize>> {
            self.multi_select_prompts.borrow_mut().push((
                prompt.to_string(),
                items.to_vec(),
                defaults.to_vec(),
            ));
            Ok(self
                .multi_select_answers
                .borrow_mut()
                .pop_front()
                .unwrap_or_default())
        }

        fn confirm(&self, prompt: &str, default: bool) -> anyhow::Result<bool> {
            self.confirm_prompts
                .borrow_mut()
                .push((prompt.to_string(), default));
            Ok(self
                .confirm_answers
                .borrow_mut()
                .pop_front()
                .unwrap_or(default))
        }

        fn text(&self, prompt: &str, default: &str) -> anyhow::Result<String> {
            self.text_prompts
                .borrow_mut()
                .push((prompt.to_string(), default.to_string()));
            Ok(self
                .text_answers
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| default.to_string()))
        }

        fn text_optional(&self, prompt: &str, default: &str) -> anyhow::Result<String> {
            self.text(prompt, default)
        }

        fn password(&self, prompt: &str) -> anyhow::Result<String> {
            self.password_prompts.borrow_mut().push(prompt.to_string());
            Ok(self
                .password_answers
                .borrow_mut()
                .pop_front()
                .unwrap_or_default())
        }
    }

    // CaptureUi is single-threaded test-only code, but the Ui trait
    // requires Send + Sync so it can be used as &dyn Ui in command signatures.
    // RefCell is not Send; these impls assert that test code won't share the
    // capture across threads, which is always true in practice.
    unsafe impl Send for CaptureUi {}
    unsafe impl Sync for CaptureUi {}

    // -- UiFactory registry ------------------------------------------------

    #[test]
    fn ui_registry_contains_all_backends() {
        assert!(UI_REGISTRY.get("terminal").is_some());
        assert!(UI_REGISTRY.get("plain").is_some());
        assert!(UI_REGISTRY.get("json").is_some());
        assert!(UI_REGISTRY.get("markdown").is_some());
    }

    #[test]
    fn ui_registry_construct_unknown_returns_err() {
        let result = UI_REGISTRY.construct("nonexistent", "nonexistent", &serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn ui_registry_has_exactly_four_backends() {
        assert_eq!(UI_REGISTRY.entries().len(), 4);
    }

    #[test]
    fn ui_metadata_has_non_empty_name_and_description() {
        for name in &["terminal", "plain", "json", "markdown"] {
            let meta = UI_REGISTRY.get(name).unwrap();
            assert!(!meta.name.is_empty(), "{name} name empty");
            assert!(!meta.description.is_empty(), "{name} description empty");
        }
    }

    // -- CaptureUi ---------------------------------------------------------

    fn make() -> CaptureUi {
        CaptureUi::default()
    }

    #[test]
    fn capture_default_starts_with_all_vecs_empty() {
        let out = make();
        assert!(out.tables.borrow().is_empty());
        assert!(out.details.borrow().is_empty());
        assert!(out.statuses.borrow().is_empty());
        assert!(out.infos.borrow().is_empty());
        assert!(out.warns.borrow().is_empty());
        assert!(out.errors.borrow().is_empty());
        assert!(out.oks.borrow().is_empty());
        assert!(out.warn_marks.borrow().is_empty());
        assert!(out.error_marks.borrow().is_empty());
        assert!(out.detail_marks.borrow().is_empty());
    }

    #[test]
    fn capture_records_table_title_headers_rows() {
        let out = make();
        out.table(
            "My Table",
            &["A", "B"],
            &[vec!["r1a".to_string(), "r1b".to_string()]],
        );
        let tables = out.tables.borrow();
        assert_eq!(tables.len(), 1);
        let (title, headers, rows) = &tables[0];
        assert_eq!(title, "My Table");
        assert_eq!(headers, &["A".to_string(), "B".to_string()]);
        assert_eq!(rows[0], vec!["r1a".to_string(), "r1b".to_string()]);
    }

    #[test]
    fn capture_records_multiple_tables_in_order() {
        let out = make();
        out.table("T1", &["X"], &[vec!["a".to_string()]]);
        out.table("T2", &["Y"], &[vec!["b".to_string()]]);
        let tables = out.tables.borrow();
        assert_eq!(tables.len(), 2);
        assert_eq!(tables[0].0, "T1");
        assert_eq!(tables[1].0, "T2");
    }

    #[test]
    fn capture_records_detail_title_and_field_pairs() {
        let out = make();
        out.detail(
            "Item",
            &[("Key", "Value".to_string()), ("Foo", "Bar".to_string())],
        );
        let details = out.details.borrow();
        assert_eq!(details.len(), 1);
        let (title, fields) = &details[0];
        assert_eq!(title, "Item");
        assert_eq!(fields[0], ("Key".to_string(), "Value".to_string()));
        assert_eq!(fields[1], ("Foo".to_string(), "Bar".to_string()));
    }

    #[test]
    fn capture_records_info_warn_error_to_separate_vecs() {
        let out = make();
        out.info("hello");
        out.warn("careful");
        out.error("boom");
        assert_eq!(*out.infos.borrow(), vec!["hello"]);
        assert_eq!(*out.warns.borrow(), vec!["careful"]);
        assert_eq!(*out.errors.borrow(), vec!["boom"]);
    }

    #[test]
    fn capture_records_mark_methods_to_separate_vecs() {
        let out = make();
        out.ok("okmsg");
        out.warn_mark("warnmsg");
        out.error_mark("errormsg");
        out.detail_mark("detailmsg");
        assert_eq!(*out.oks.borrow(), vec!["okmsg"]);
        assert_eq!(*out.warn_marks.borrow(), vec!["warnmsg"]);
        assert_eq!(*out.error_marks.borrow(), vec!["errormsg"]);
        assert_eq!(*out.detail_marks.borrow(), vec!["detailmsg"]);
    }

    #[test]
    fn capture_records_status_with_ok_flag_and_detail() {
        let out = make();
        out.status("provider-a", true, "");
        out.status("provider-b", false, "connection refused");
        let statuses = out.statuses.borrow();
        assert_eq!(
            statuses[0],
            ("provider-a".to_string(), true, "".to_string())
        );
        assert_eq!(
            statuses[1],
            (
                "provider-b".to_string(),
                false,
                "connection refused".to_string()
            )
        );
    }

    // -- CaptureUi input methods -------------------------------------------

    #[test]
    fn select_falls_back_to_default_when_no_canned_answer() {
        let ui = make();
        let choice = ui
            .select("Pick one", &["a".to_string(), "b".to_string()], 1)
            .unwrap();
        assert_eq!(choice, 1);
        assert_eq!(ui.select_prompts.borrow()[0].0, "Pick one");
    }

    #[test]
    fn select_consumes_canned_answers_in_order() {
        let ui = make();
        ui.select_answers.borrow_mut().extend([2, 0]);
        assert_eq!(ui.select("first", &["a".to_string()], 0).unwrap(), 2);
        assert_eq!(ui.select("second", &["a".to_string()], 0).unwrap(), 0);
    }

    #[test]
    fn confirm_falls_back_to_default_when_no_canned_answer() {
        let ui = make();
        assert!(ui.confirm("Sure?", true).unwrap());
    }

    #[test]
    fn confirm_consumes_canned_answer() {
        let ui = make();
        ui.confirm_answers.borrow_mut().push_back(false);
        assert!(!ui.confirm("Sure?", true).unwrap());
    }

    #[test]
    fn text_falls_back_to_default_when_no_canned_answer() {
        let ui = make();
        assert_eq!(ui.text("Name?", "bob").unwrap(), "bob");
    }

    #[test]
    fn text_consumes_canned_answer() {
        let ui = make();
        ui.text_answers.borrow_mut().push_back("alice".to_string());
        assert_eq!(ui.text("Name?", "bob").unwrap(), "alice");
    }

    #[test]
    fn password_falls_back_to_empty_string_when_no_canned_answer() {
        let ui = make();
        assert_eq!(ui.password("Secret?").unwrap(), "");
    }

    #[test]
    fn password_consumes_canned_answer() {
        let ui = make();
        ui.password_answers
            .borrow_mut()
            .push_back("s3cr3t".to_string());
        assert_eq!(ui.password("Secret?").unwrap(), "s3cr3t");
    }

    // -- CaptureUi pull lifecycle --------------------------------------------

    #[test]
    fn pull_lifecycle_records_calls_and_allocates_distinct_handles() {
        let ui = make();
        let h1 = ui.pull_start("model-a", Some(100));
        let h2 = ui.pull_start("model-b", None);
        assert_ne!(h1, h2);

        ui.pull_progress(h1, 50, Some(100));
        ui.pull_finish(h1, "model-a", None);
        ui.pull_finish(h2, "model-b", Some("boom"));

        assert_eq!(
            *ui.pull_starts.borrow(),
            vec![
                ("model-a".to_string(), Some(100)),
                ("model-b".to_string(), None),
            ]
        );
        assert_eq!(*ui.pull_progresses.borrow(), vec![(h1, 50, Some(100))]);
        assert_eq!(
            ui.pull_finishes.borrow()[0],
            (h1, "model-a".to_string(), None)
        );
        assert_eq!(
            ui.pull_finishes.borrow()[1],
            (h2, "model-b".to_string(), Some("boom".to_string()))
        );
    }
}
