// Standard
use std::collections::HashMap;
use std::sync::Mutex;

// Third Party
use serde::{Deserialize, Serialize};

/*-- public --*/

/// Token usage recorded for one proxied binding (model or, in the future,
/// MCP server). Cache fields are populated on a best-effort basis -- not
/// every `ApiType` exposes cache accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageStats {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
}

impl UsageStats {
    fn add(&mut self, other: &UsageStats) {
        self.requests += other.requests;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
    }

    /// Elementwise max, used while accumulating usage across a streaming
    /// response: providers report token counts as running totals per event,
    /// not per-event deltas, so summing would over-count.
    pub(crate) fn merge_max(&mut self, other: &UsageStats) {
        self.input_tokens = self.input_tokens.max(other.input_tokens);
        self.output_tokens = self.output_tokens.max(other.output_tokens);
        self.cache_creation_tokens = self.cache_creation_tokens.max(other.cache_creation_tokens);
        self.cache_read_tokens = self.cache_read_tokens.max(other.cache_read_tokens);
    }
}

/// Shared, thread-safe accumulator of `UsageStats` keyed by binding label
/// (e.g. the capability id). One `UsageTracker` is shared by every
/// `ProxyServer` started for a given launch session.
///
/// An optional [`tokio::sync::watch`] notifier can be attached via
/// [`UsageTracker::set_notifier`]. When set, every call to [`record`] sends
/// a non-blocking notification so a background writer task can react to new
/// usage immediately
pub struct UsageTracker {
    stats: Mutex<HashMap<String, UsageStats>>,
    /// Fires `()` on every [`record`] call when set. The `watch` channel
    /// stores only the latest value, so rapid successive records coalesce into
    /// a single wake-up for the writer — exactly the overwrite-queue semantic
    /// we want. Non-blocking: if the receiver is lagging the send silently
    /// overwrites the queued notification.
    notifier: Mutex<Option<tokio::sync::watch::Sender<()>>>,
}

impl Default for UsageTracker {
    fn default() -> Self {
        Self {
            stats: Mutex::new(HashMap::new()),
            notifier: Mutex::new(None),
        }
    }
}

impl UsageTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a [`tokio::sync::watch::Sender`] that will be notified on
    /// every [`record`] call. Replaces any previously set notifier.
    pub fn set_notifier(&self, sender: tokio::sync::watch::Sender<()>) {
        *self.notifier.lock().unwrap() = Some(sender);
    }

    /// Fold `delta` into the running totals for `label`, plus one request.
    /// Fires the attached notifier (if any) after updating, without blocking.
    pub fn record(&self, label: &str, delta: UsageStats) {
        let mut stats = self.stats.lock().unwrap();
        let entry = stats.entry(label.to_string()).or_default();
        entry.add(&UsageStats {
            requests: 1,
            ..delta
        });

        // Non-blocking notify: send() on a watch channel overwrites the stored
        // value regardless of whether the receiver has read the previous one.
        // Errors only if all receivers have been dropped, which is fine.
        if let Some(tx) = self.notifier.lock().unwrap().as_ref() {
            let _ = tx.send(());
        }
    }

    /// Replace the totals for `label` with `stats` (not an accumulate — the
    /// caller already has the authoritative absolute total, e.g. read from an
    /// external store like bob.db). Fires the attached notifier (if any) after
    /// updating, without blocking.
    pub fn set(&self, label: &str, stats: UsageStats) {
        self.stats.lock().unwrap().insert(label.to_string(), stats);
        if let Some(tx) = self.notifier.lock().unwrap().as_ref() {
            let _ = tx.send(());
        }
    }

    /// A point-in-time copy of all recorded totals, keyed by label.
    pub fn snapshot(&self) -> HashMap<String, UsageStats> {
        self.stats.lock().unwrap().clone()
    }
}

/// Parse token usage out of one decoded JSON value, regardless of which
/// provider produced it or whether it's a streaming event, an NDJSON line,
/// or a whole non-streaming body. Anthropic and OpenAI usage objects use
/// disjoint key names, so trying both in turn is unambiguous; Ollama never
/// nests its counts under a `usage` key, so falling through to it last is
/// safe.
///
/// - Anthropic: `message_start` nests usage under `.message.usage`;
///   `message_delta` and non-streaming bodies have it at the top level under
///   `.usage` -- check both.
/// - OpenAI: `.usage.{prompt,completion}_tokens`, only present on the final
///   streaming chunk when the client requested
///   `stream_options: {include_usage: true}`, or always on a non-streaming
///   body.
/// - Ollama: top-level `prompt_eval_count`/`eval_count`, present only on the
///   NDJSON line with `done: true`, or on a non-streaming body.
pub fn parse_usage(value: &serde_json::Value) -> Option<UsageStats> {
    let usage_obj = value
        .get("message")
        .and_then(|m| m.get("usage"))
        .or_else(|| value.get("usage"));
    if let Some(usage_obj) = usage_obj
        && let Some(stats) =
            parse_anthropic_usage(usage_obj).or_else(|| parse_openai_usage(usage_obj))
    {
        return Some(stats);
    }
    parse_ollama_usage(value)
}

/*-- private --*/

fn parse_anthropic_usage(usage: &serde_json::Value) -> Option<UsageStats> {
    let get = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    if usage.get("input_tokens").is_none() && usage.get("output_tokens").is_none() {
        return None;
    }
    Some(UsageStats {
        requests: 0,
        input_tokens: get("input_tokens"),
        output_tokens: get("output_tokens"),
        cache_creation_tokens: get("cache_creation_input_tokens"),
        cache_read_tokens: get("cache_read_input_tokens"),
    })
}

fn parse_openai_usage(usage: &serde_json::Value) -> Option<UsageStats> {
    let get = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    if usage.get("prompt_tokens").is_none() && usage.get("completion_tokens").is_none() {
        return None;
    }
    let cache_read_tokens = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some(UsageStats {
        requests: 0,
        input_tokens: get("prompt_tokens"),
        output_tokens: get("completion_tokens"),
        cache_creation_tokens: 0,
        cache_read_tokens,
    })
}

fn parse_ollama_usage(body: &serde_json::Value) -> Option<UsageStats> {
    let prompt_eval_count = body.get("prompt_eval_count").and_then(|v| v.as_u64());
    let eval_count = body.get("eval_count").and_then(|v| v.as_u64());
    if prompt_eval_count.is_none() && eval_count.is_none() {
        return None;
    }
    Some(UsageStats {
        requests: 0,
        input_tokens: prompt_eval_count.unwrap_or(0),
        output_tokens: eval_count.unwrap_or(0),
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
    })
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tracker_records_and_accumulates_by_label() {
        let tracker = UsageTracker::new();
        tracker.record(
            "chat",
            UsageStats {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        );
        tracker.record(
            "chat",
            UsageStats {
                input_tokens: 3,
                output_tokens: 7,
                ..Default::default()
            },
        );
        tracker.record(
            "other",
            UsageStats {
                input_tokens: 100,
                ..Default::default()
            },
        );

        let snapshot = tracker.snapshot();
        let chat = snapshot.get("chat").unwrap();
        assert_eq!(chat.requests, 2);
        assert_eq!(chat.input_tokens, 13);
        assert_eq!(chat.output_tokens, 12);
        assert_eq!(snapshot.get("other").unwrap().input_tokens, 100);
    }

    #[test]
    fn merge_max_takes_running_totals_not_sums() {
        let mut running = UsageStats {
            input_tokens: 10,
            output_tokens: 1,
            ..Default::default()
        };
        running.merge_max(&UsageStats {
            input_tokens: 10,
            output_tokens: 4,
            ..Default::default()
        });
        assert_eq!(running.input_tokens, 10);
        assert_eq!(running.output_tokens, 4);
    }

    #[test]
    fn parse_usage_anthropic_non_streaming() {
        let body = json!({
            "usage": {
                "input_tokens": 12,
                "output_tokens": 34,
                "cache_creation_input_tokens": 5,
                "cache_read_input_tokens": 6
            }
        });
        let usage = parse_usage(&body).unwrap();
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 34);
        assert_eq!(usage.cache_creation_tokens, 5);
        assert_eq!(usage.cache_read_tokens, 6);
    }

    #[test]
    fn parse_usage_missing_usage_returns_none() {
        let body = json!({"content": []});
        assert!(parse_usage(&body).is_none());
    }

    #[test]
    fn parse_usage_anthropic_message_start() {
        let event = json!({
            "type": "message_start",
            "message": {
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 1,
                    "cache_read_input_tokens": 20
                }
            }
        });
        let usage = parse_usage(&event).unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.cache_read_tokens, 20);
    }

    #[test]
    fn parse_usage_anthropic_message_delta() {
        let event = json!({
            "type": "message_delta",
            "usage": {
                "output_tokens": 42
            }
        });
        let usage = parse_usage(&event).unwrap();
        assert_eq!(usage.output_tokens, 42);
    }

    #[test]
    fn parse_usage_openai_with_cached_tokens() {
        let body = json!({
            "usage": {
                "prompt_tokens": 50,
                "completion_tokens": 25,
                "prompt_tokens_details": {"cached_tokens": 10}
            }
        });
        let usage = parse_usage(&body).unwrap();
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.output_tokens, 25);
        assert_eq!(usage.cache_read_tokens, 10);
    }

    #[test]
    fn parse_usage_ollama_done() {
        let body = json!({
            "done": true,
            "prompt_eval_count": 8,
            "eval_count": 16
        });
        let usage = parse_usage(&body).unwrap();
        assert_eq!(usage.input_tokens, 8);
        assert_eq!(usage.output_tokens, 16);
    }

    #[test]
    fn parse_usage_ollama_non_done_line_without_counts_is_none() {
        let body = json!({"done": false, "response": "partial"});
        assert!(parse_usage(&body).is_none());
    }

    #[test]
    fn tracker_set_replaces_not_accumulates() {
        let tracker = UsageTracker::new();
        let stats_a = UsageStats {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        };
        let stats_b = UsageStats {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        };
        tracker.set("bob", stats_a);
        tracker.set("bob", stats_b);
        let snapshot = tracker.snapshot();
        let bob = snapshot.get("bob").unwrap();
        assert_eq!(bob.input_tokens, 100);
        assert_eq!(bob.output_tokens, 50);
        // Verify old value is gone (10 + 100 = 110 would mean accumulation)
        assert_ne!(bob.input_tokens, 110);
    }

    #[test]
    fn tracker_set_does_not_modify_requests_beyond_passed_value() {
        let tracker = UsageTracker::new();
        tracker.set(
            "bob",
            UsageStats {
                requests: 42,
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        );
        let snapshot = tracker.snapshot();
        let bob = snapshot.get("bob").unwrap();
        assert_eq!(bob.requests, 42);
        assert_eq!(bob.input_tokens, 10);
        assert_eq!(bob.output_tokens, 5);
        // record() adds +1 to requests, but set() should not
        tracker.record("bob", UsageStats::default());
        let snapshot2 = tracker.snapshot();
        let bob2 = snapshot2.get("bob").unwrap();
        assert_eq!(bob2.requests, 43);
    }

    #[tokio::test]
    async fn tracker_set_fires_notifier() {
        let tracker = UsageTracker::new();
        let (tx, mut rx) = tokio::sync::watch::channel(());
        tracker.set_notifier(tx);
        let stats = UsageStats {
            input_tokens: 7,
            ..Default::default()
        };
        tracker.set("bob", stats);
        // Verify the notifier fires — changed() returns Ok(()) once a new
        // value has been sent since the receiver last consumed it.
        rx.changed().await.unwrap();
    }

    // ── parse_usage: OpenAI ───────────────────────────────────────────────

    #[test]
    fn parse_usage_openai_without_cache_details() {
        // OpenAI response with prompt_tokens and completion_tokens but no
        // prompt_tokens_details field at all. The absent field must default
        // to 0, not cause a panic or return None.
        let body = json!({
            "usage": {
                "prompt_tokens": 30,
                "completion_tokens": 15
            }
        });
        let usage = parse_usage(&body).unwrap();
        assert_eq!(usage.input_tokens, 30);
        assert_eq!(usage.output_tokens, 15);
        assert_eq!(usage.cache_read_tokens, 0);
        assert_eq!(usage.cache_creation_tokens, 0);
    }

    #[test]
    fn parse_usage_openai_only_prompt_tokens() {
        // OpenAI body that has prompt_tokens but no completion_tokens.
        // The guard is (prompt.is_none() && completion.is_none()), so one
        // key present is enough to proceed; the absent key defaults to 0.
        let body = json!({
            "usage": {
                "prompt_tokens": 20
            }
        });
        let usage = parse_usage(&body).unwrap();
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.output_tokens, 0);
    }

    #[test]
    fn parse_usage_openai_only_completion_tokens() {
        // Mirror of the above: completion_tokens present, prompt_tokens absent.
        let body = json!({
            "usage": {
                "completion_tokens": 10
            }
        });
        let usage = parse_usage(&body).unwrap();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 10);
    }

    // ── parse_usage: Anthropic ────────────────────────────────────────────

    #[test]
    fn parse_usage_anthropic_only_input_tokens() {
        // The Anthropic guard is (input.is_none() && output.is_none()), so a
        // body with only input_tokens must parse and default output to 0.
        let body = json!({
            "usage": {
                "input_tokens": 50
            }
        });
        let usage = parse_usage(&body).unwrap();
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.output_tokens, 0);
    }

    // ── parse_usage: Ollama ───────────────────────────────────────────────

    #[test]
    fn parse_usage_ollama_only_prompt_eval_count() {
        // Body with prompt_eval_count but no eval_count. The guard is
        // (prompt.is_none() && eval.is_none()), so one field is enough;
        // the absent field defaults to 0.
        let body = json!({
            "prompt_eval_count": 12
        });
        let usage = parse_usage(&body).unwrap();
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 0);
    }

    #[test]
    fn parse_usage_ollama_only_eval_count() {
        // Mirror of the above: eval_count present, prompt_eval_count absent.
        let body = json!({
            "eval_count": 7
        });
        let usage = parse_usage(&body).unwrap();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 7);
    }

    // ── UsageTracker ──────────────────────────────────────────────────────

    #[test]
    fn tracker_snapshot_is_empty_before_any_records() {
        // A brand-new tracker must return an empty map — nothing recorded yet.
        let tracker = UsageTracker::new();
        assert!(tracker.snapshot().is_empty());
    }

    #[test]
    fn tracker_record_fires_notifier() {
        // record() must fire the notifier just like set() does.
        // Attach a watch notifier, call record(), then confirm both that the
        // stats were stored and that the notifier received a notification.
        let tracker = UsageTracker::new();
        let (watch_tx, watch_rx) = tokio::sync::watch::channel(());
        tracker.set_notifier(watch_tx);
        tracker.record(
            "chat",
            UsageStats {
                input_tokens: 5,
                ..Default::default()
            },
        );
        let snapshot = tracker.snapshot();
        let chat = snapshot.get("chat").unwrap();
        assert_eq!(chat.requests, 1);
        assert_eq!(chat.input_tokens, 5);
        // has_changed() returns true when send() was called since the
        // receiver last observed the value — i.e. the notifier fired.
        assert!(watch_rx.has_changed().unwrap());
    }

    #[test]
    fn merge_max_updates_cache_fields() {
        // The existing merge_max test only exercises input_tokens and
        // output_tokens. This test confirms cache_creation_tokens and
        // cache_read_tokens are also updated by merge_max, and that
        // requests is intentionally left untouched.
        let mut running = UsageStats {
            requests: 3,
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_tokens: 2,
            cache_read_tokens: 1,
        };
        running.merge_max(&UsageStats {
            requests: 99,
            input_tokens: 8,
            output_tokens: 6,
            cache_creation_tokens: 4,
            cache_read_tokens: 0,
        });
        // max wins for token fields
        assert_eq!(running.input_tokens, 10);
        assert_eq!(running.output_tokens, 6);
        assert_eq!(running.cache_creation_tokens, 4);
        assert_eq!(running.cache_read_tokens, 1);
        // requests is not touched by merge_max
        assert_eq!(running.requests, 3);
    }
}
