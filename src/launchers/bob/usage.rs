// Standard
use std::path::Path;

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use rusqlite::{Connection, OpenFlags, params};

// Local
use crate::proxy::UsageStats;

use_channel!("BOB");

/// Collect cumulative token usage from Bob's SQLite database for a task tree
/// rooted at `root_task_id`.
///
/// Reads `~/.bob/db/bob.db` (or `db_path` if provided), walks the full
/// recursive descendant tree, and sums every row's JSON `costs` payload into
/// a single [`UsageStats`].
///
/// This function is intentionally panic-free and error-free: any failure
/// (missing file, corrupt DB, malformed JSON, etc.) degrades gracefully so
/// the launcher can continue even with zero usage data.
pub(crate) fn collect_bob_usage(db_path: &Path, root_task_id: &str) -> UsageStats {
    // Attempt to open the DB read-only.  If we can't, return default.
    let conn = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(e) => {
            alog_channel!(
                MessageLevel::Debug,
                "collect_bob_usage: failed to open DB at {:?}: {}",
                db_path,
                e
            );
            return UsageStats::default();
        }
    };

    // Recursive CTE that pulls the entire descendant tree rooted at root_task_id.
    let sql = "
        WITH RECURSIVE task_tree(id) AS (
            SELECT ?1
            UNION ALL
            SELECT tasks.id FROM tasks JOIN task_tree ON tasks.parent_id = task_tree.id
        )
        SELECT costs FROM tasks WHERE id IN (SELECT id FROM task_tree);
    ";

    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            alog_channel!(
                MessageLevel::Debug,
                "collect_bob_usage: failed to prepare query for task {}: {}",
                root_task_id,
                e
            );
            return UsageStats::default();
        }
    };

    let mut rows = match stmt.query(params![root_task_id]) {
        Ok(r) => r,
        Err(e) => {
            alog_channel!(
                MessageLevel::Debug,
                "collect_bob_usage: recursive query failed for task {}: {}",
                root_task_id,
                e
            );
            return UsageStats::default();
        }
    };

    // Accumulate stats across every row in the tree.
    let mut total = UsageStats::default();
    let mut row_count: u64 = 0;

    loop {
        let row = match rows.next() {
            Ok(Some(r)) => r,
            Ok(None) => break,
            Err(e) => {
                alog_channel!(
                    MessageLevel::Debug,
                    "collect_bob_usage: error iterating rows: {}",
                    e
                );
                break;
            }
        };

        row_count += 1;
        match row.get::<_, Option<String>>(0) {
            Ok(Some(json_str)) => {
                let row_stats = parse_costs_json(&json_str);
                total.input_tokens += row_stats.input_tokens;
                total.output_tokens += row_stats.output_tokens;
                total.cache_creation_tokens += row_stats.cache_creation_tokens;
                total.cache_read_tokens += row_stats.cache_read_tokens;
            }
            Ok(None) => {
                // NULL / empty costs: treat as zero, still counted
            }
            Err(e) => {
                alog_channel!(
                    MessageLevel::Debug,
                    "collect_bob_usage: error reading row costs: {}",
                    e
                );
            }
        }
    }

    if row_count == 0 {
        alog_channel!(
            MessageLevel::Debug,
            "collect_bob_usage: task {} not found in database",
            root_task_id
        );
        return UsageStats::default();
    }

    total.requests = row_count;
    total
}

/// Parse a single costs JSON string into UsageStats.
///
/// Keys `input`, `output`, `cacheRead`, `cacheWrite` are looked up and
/// treated as integers (default 0 if missing or not a valid integer).
fn parse_costs_json(json_str: &str) -> UsageStats {
    let value = match serde_json::from_str::<serde_json::Value>(json_str) {
        Ok(v) => v,
        Err(e) => {
            alog_channel!(
                MessageLevel::Debug2,
                "collect_bob_usage: failed to parse costs JSON: {}: {}",
                e,
                json_str
            );
            return UsageStats::default();
        }
    };

    let obj = match value.as_object() {
        Some(o) => o,
        None => {
            alog_channel!(
                MessageLevel::Debug2,
                "collect_bob_usage: costs JSON is not an object"
            );
            return UsageStats::default();
        }
    };

    let get_int = |key: &str| -> u64 { obj.get(key).and_then(|v| v.as_u64()).unwrap_or(0) };

    UsageStats {
        requests: 0,
        input_tokens: get_int("input"),
        output_tokens: get_int("output"),
        cache_creation_tokens: get_int("cacheWrite"),
        cache_read_tokens: get_int("cacheRead"),
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Helper: create a temp dir and return the path to a bob.db inside it.
    fn make_temp_db() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("bob.db");
        (tmp, db_path)
    }

    /// Helper: create a DB and insert the minimal `tasks` table.
    fn create_tasks_table(db_path: &Path) {
        let conn = Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE tasks (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                costs TEXT
            )",
        )
        .unwrap();
    }

    fn insert_task(conn: &Connection, id: &str, parent_id: Option<&str>, costs: Option<&str>) {
        conn.execute(
            "INSERT INTO tasks (id, parent_id, costs) VALUES (?1, ?2, ?3)",
            params![id, parent_id, costs],
        )
        .unwrap();
    }

    #[test]
    fn root_with_subagent_child_summation() {
        let (_tmp, db_path) = make_temp_db();
        create_tasks_table(&db_path);

        let conn = Connection::open(&db_path).unwrap();
        insert_task(
            &conn,
            "root-task",
            None,
            Some(r#"{"input":100, "output":50, "cacheRead":20, "cacheWrite":10}"#),
        );
        insert_task(
            &conn,
            "child-task",
            Some("root-task"),
            Some(r#"{"input":30, "output":15, "cacheRead":5, "cacheWrite":2}"#),
        );
        drop(conn);

        let result = collect_bob_usage(&db_path, "root-task");
        assert_eq!(result.requests, 2);
        assert_eq!(result.input_tokens, 130);
        assert_eq!(result.output_tokens, 65);
        assert_eq!(result.cache_read_tokens, 25);
        assert_eq!(result.cache_creation_tokens, 12);
    }

    #[test]
    fn malformed_costs_row_treated_as_zero() {
        let (_tmp, db_path) = make_temp_db();
        create_tasks_table(&db_path);

        let conn = Connection::open(&db_path).unwrap();
        insert_task(
            &conn,
            "root-task",
            None,
            Some(r#"{"input":100, "output":50}"#),
        );
        insert_task(
            &conn,
            "bad-child",
            Some("root-task"),
            Some("this is not json at all"),
        );
        drop(conn);

        let result = collect_bob_usage(&db_path, "root-task");
        assert_eq!(result.requests, 2);
        // Root's costs still count; bad row contributes zero
        assert_eq!(result.input_tokens, 100);
        assert_eq!(result.output_tokens, 50);
    }

    #[test]
    fn nonexistent_root_returns_default() {
        let (_tmp, db_path) = make_temp_db();
        create_tasks_table(&db_path);

        // Insert a task but with a different ID
        let conn = Connection::open(&db_path).unwrap();
        insert_task(&conn, "other-task", None, Some(r#"{"input":999}"#));
        drop(conn);

        let result = collect_bob_usage(&db_path, "does-not-exist");
        assert_eq!(result, UsageStats::default());
    }

    #[test]
    fn nonexistent_db_path_returns_default() {
        let result = collect_bob_usage(std::path::Path::new("/no/such/path/bob.db"), "any-id");
        assert_eq!(result, UsageStats::default());
    }

    #[test]
    fn null_costs_row_treated_as_zero() {
        let (_tmp, db_path) = make_temp_db();
        create_tasks_table(&db_path);

        let conn = Connection::open(&db_path).unwrap();
        insert_task(
            &conn,
            "root-task",
            None,
            Some(r#"{"input":100, "output":50}"#),
        );
        // Insert child with costs = NULL
        insert_task(&conn, "null-child", Some("root-task"), None);
        drop(conn);

        let result = collect_bob_usage(&db_path, "root-task");
        assert_eq!(result.requests, 2);
        assert_eq!(result.input_tokens, 100);
        assert_eq!(result.output_tokens, 50);
    }
}
