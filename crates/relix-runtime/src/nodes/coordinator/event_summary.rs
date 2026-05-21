//! Hermes-style one-line summary projection for [`TaskEvent`].
//!
//! Pure deterministic function. No LLM, no I/O, no DB lookup. Given a
//! single chronicle event the projection produces a short line suitable
//! for:
//!
//! - Dashboard timeline rendering (instead of dumping the full payload).
//! - Future chronicle compaction (D-003 deferred): an archival pass
//!   could store only the summary rather than the raw event row.
//! - Replay UX where the operator scrubs the timeline and needs a
//!   "what happened here?" label at a glance.
//!
//! The projection is intentionally lossy: each summary captures the
//! *intent* of the event, not its full payload. The chronicle remains
//! the source of truth — the summary is a UX projection. This mirrors
//! Hermes's "pass-2 tool-result pruning" pattern (per `docs/hermes-deep-dive.md`
//! §"Three-Pass Tool Result Pruning"): replace verbose tool outputs
//! with a one-line description that captures what was attempted +
//! the resulting state delta.
//!
//! ## Output shape
//!
//! `[<category>] <short body>`
//!
//! Examples:
//!
//! | event_type | summary |
//! |---|---|
//! | `task.create` | `[create] new task` |
//! | `task.update` | `[update] → completed` |
//! | `task.attempt_started` | `[attempt] started a#3` |
//! | `task.attempt_finished` | `[attempt] finished a#3 → success` |
//! | `task.retry_requested` | `[retry] requested (#2/5)` |
//! | `task.retry_suppressed` | `[retry] suppressed (paused)` |
//! | `task.pause_requested` | `[pause] requested gen=2 (reason)` |
//! | `task.spawned_child` | `[lineage] spawned → child-task-id` |
//! | `task.operator_note` | `[note] "first 60 chars of text…"` |
//! | `task.interrupted` | `[interrupt] pause gen=2` |
//!
//! Unknown event_types fall through to `[event] <event_type>`.
//!
//! ## What this does NOT do
//!
//! - **No DB joins.** The summary only sees the event row + its
//!   `payload_json`. Looking up the originating task to render its
//!   title is out of scope.
//! - **No LLM.** Hermes uses an LLM only when *cross-event* synthesis
//!   is needed (its `compress()` step); the per-event summary is
//!   pure pattern matching.
//! - **No truncation policy.** Caller-defined max line length (the
//!   summarizer respects an operator-bounded cap on the `[note]`
//!   body but does not enforce a hard line cap).

use super::TaskEvent;
use serde_json::Value;

/// Max chars of a `task.operator_note` text body that the summary
/// echoes verbatim. Matches the dashboard treatment (longer notes
/// land in the per-task drill-in).
const NOTE_BODY_CAP: usize = 60;

/// Thin adapter for callers that only carry the parts of a chronicle
/// event needed by the summarizer. The bridge's `GlobalEventRow` and
/// the dashboard timeline both fit this shape — they don't always
/// have a full [`TaskEvent`] handy.
pub fn summarize_event_parts(
    event_type: &str,
    payload: &str,
    attempt_id: Option<i64>,
    payload_json: Option<&str>,
) -> String {
    let ev = TaskEvent {
        event_id: 0,
        ts: 0,
        event_type: event_type.to_string(),
        payload: payload.to_string(),
        schema_version: 1,
        attempt_id,
        trace_id: None,
        payload_json: payload_json.map(str::to_string),
    };
    summarize_event(&ev)
}

/// Produce the one-line projection for a single chronicle event.
///
/// Always returns a non-empty string. Best-effort over `payload_json`
/// — missing or malformed payload still yields a valid summary line
/// (the projection just degrades gracefully to "[event] <event_type>"
/// for unknown types or "[<category>]" for known types without
/// payload).
pub fn summarize_event(ev: &TaskEvent) -> String {
    let payload = ev
        .payload_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok());

    match ev.event_type.as_str() {
        "task.create" => summarize_create(payload.as_ref()),
        "task.update" => summarize_update(payload.as_ref(), &ev.payload),
        "task.attempt_started" => summarize_attempt_started(ev.attempt_id),
        "task.attempt_finished" => summarize_attempt_finished(ev.attempt_id, payload.as_ref()),
        "task.retry_requested" => summarize_retry_requested(payload.as_ref()),
        "task.retry_exhausted" => summarize_retry_exhausted(payload.as_ref()),
        "task.retry_suppressed" => summarize_retry_suppressed(payload.as_ref()),
        "task.pause_requested" => summarize_pause_requested(payload.as_ref()),
        "task.resume_requested" => summarize_resume_requested(payload.as_ref()),
        "task.freeze_requested" => summarize_freeze_requested(payload.as_ref()),
        "task.unfreeze_requested" => summarize_unfreeze_requested(payload.as_ref()),
        "task.pause_observed" => summarize_observation("pause", payload.as_ref()),
        "task.resume_observed" => summarize_observation("resume", payload.as_ref()),
        "task.freeze_propagated" => summarize_observation("freeze", payload.as_ref()),
        "task.spawned_child" => summarize_lineage("spawned →", payload.as_ref(), "child_id"),
        "task.delegated_to" => summarize_lineage("delegated →", payload.as_ref(), "target_id"),
        "task.awaiting" => summarize_lineage("awaiting", payload.as_ref(), "awaited_id"),
        "task.investigation_marked" => summarize_investigation_marked(payload.as_ref()),
        "task.investigation_cleared" => "[investigation] cleared".to_string(),
        "task.operator_note" => summarize_operator_note(payload.as_ref(), &ev.payload),
        "task.interrupted" => summarize_interrupted(payload.as_ref()),
        "task.thrash_detected" => summarize_thrash(payload.as_ref()),
        "task.terminal_summary" => summarize_terminal(payload.as_ref()),
        "task.attempt_orphan_closed" => summarize_orphan_close(payload.as_ref()),
        other => format!("[event] {other}"),
    }
}

// ─────────────────────────── Per-type renderers ───────────────────────────

fn summarize_create(payload: Option<&Value>) -> String {
    if let Some(p) = payload
        && let Some(s) = p.get("status").and_then(Value::as_str)
    {
        return format!("[create] new task ({s})");
    }
    "[create] new task".to_string()
}

fn summarize_update(payload: Option<&Value>, legacy: &str) -> String {
    if let Some(p) = payload {
        if let Some(s) = p.get("status").and_then(Value::as_str) {
            return format!("[update] → {s}");
        }
        // No status delta — try to surface the field set changed.
        if let Some(obj) = p.as_object() {
            let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
            keys.sort();
            if !keys.is_empty() {
                let joined = keys.join(",");
                return format!("[update] {joined}");
            }
        }
    }
    if !legacy.is_empty() {
        let trimmed = clip_oneline(legacy, NOTE_BODY_CAP);
        return format!("[update] {trimmed}");
    }
    "[update]".to_string()
}

fn summarize_attempt_started(attempt_id: Option<i64>) -> String {
    match attempt_id {
        Some(id) => format!("[attempt] started a#{id}"),
        None => "[attempt] started".to_string(),
    }
}

fn summarize_attempt_finished(attempt_id: Option<i64>, payload: Option<&Value>) -> String {
    let outcome = payload
        .and_then(|p| {
            p.get("outcome")
                .or_else(|| p.get("status"))
                .or_else(|| p.get("result"))
                .and_then(Value::as_str)
        })
        .unwrap_or("");
    let prefix = match attempt_id {
        Some(id) => format!("[attempt] finished a#{id}"),
        None => "[attempt] finished".to_string(),
    };
    if outcome.is_empty() {
        prefix
    } else {
        format!("{prefix} → {outcome}")
    }
}

fn summarize_retry_requested(payload: Option<&Value>) -> String {
    let count = payload
        .and_then(|p| p.get("retry_count").and_then(Value::as_i64))
        .unwrap_or(-1);
    let budget = payload
        .and_then(|p| {
            p.get("retry_budget")
                .or_else(|| p.get("budget"))
                .and_then(Value::as_i64)
        })
        .unwrap_or(-1);
    match (count, budget) {
        (c, b) if c >= 0 && b >= 0 => format!("[retry] requested (#{c}/{b})"),
        (c, _) if c >= 0 => format!("[retry] requested (#{c})"),
        _ => "[retry] requested".to_string(),
    }
}

fn summarize_retry_exhausted(payload: Option<&Value>) -> String {
    let count = payload
        .and_then(|p| p.get("retry_count").and_then(Value::as_i64))
        .unwrap_or(-1);
    if count >= 0 {
        format!("[retry] exhausted at #{count}")
    } else {
        "[retry] exhausted".to_string()
    }
}

fn summarize_retry_suppressed(payload: Option<&Value>) -> String {
    let by = payload
        .and_then(|p| p.get("suppressed_by").and_then(Value::as_str))
        .unwrap_or("");
    if by.is_empty() {
        "[retry] suppressed".to_string()
    } else {
        format!("[retry] suppressed ({by})")
    }
}

fn summarize_pause_requested(payload: Option<&Value>) -> String {
    let g = payload.and_then(|p| p.get("pause_generation").and_then(Value::as_i64));
    let reason = payload
        .and_then(|p| p.get("reason").and_then(Value::as_str))
        .filter(|s| !s.is_empty());
    match (g, reason) {
        (Some(g), Some(r)) => format!("[pause] requested gen={g} ({})", clip_oneline(r, 40)),
        (Some(g), None) => format!("[pause] requested gen={g}"),
        (None, Some(r)) => format!("[pause] requested ({})", clip_oneline(r, 40)),
        (None, None) => "[pause] requested".to_string(),
    }
}

fn summarize_resume_requested(payload: Option<&Value>) -> String {
    let g = payload.and_then(|p| p.get("pause_generation").and_then(Value::as_i64));
    match g {
        Some(g) => format!("[resume] requested gen={g}"),
        None => "[resume] requested".to_string(),
    }
}

fn summarize_freeze_requested(payload: Option<&Value>) -> String {
    let g = payload.and_then(|p| p.get("freeze_generation").and_then(Value::as_i64));
    let reason = payload
        .and_then(|p| p.get("reason").and_then(Value::as_str))
        .filter(|s| !s.is_empty());
    match (g, reason) {
        (Some(g), Some(r)) => format!("[freeze] requested gen={g} ({})", clip_oneline(r, 40)),
        (Some(g), None) => format!("[freeze] requested gen={g}"),
        (None, Some(r)) => format!("[freeze] requested ({})", clip_oneline(r, 40)),
        (None, None) => "[freeze] requested".to_string(),
    }
}

fn summarize_unfreeze_requested(payload: Option<&Value>) -> String {
    let g = payload.and_then(|p| p.get("freeze_generation").and_then(Value::as_i64));
    match g {
        Some(g) => format!("[unfreeze] requested gen={g}"),
        None => "[unfreeze] requested".to_string(),
    }
}

fn summarize_observation(kind: &str, payload: Option<&Value>) -> String {
    let observed = payload
        .and_then(|p| p.get("generation_observed").and_then(Value::as_i64))
        .or_else(|| payload.and_then(|p| p.get("generation").and_then(Value::as_i64)));
    match observed {
        Some(g) => format!("[{kind}] observed gen={g}"),
        None => format!("[{kind}] observed"),
    }
}

fn summarize_lineage(verb: &str, payload: Option<&Value>, target_key: &str) -> String {
    let target = payload
        .and_then(|p| {
            p.get(target_key)
                .or_else(|| p.get("task_id"))
                .or_else(|| p.get("id"))
                .and_then(Value::as_str)
        })
        .unwrap_or("");
    if target.is_empty() {
        format!("[lineage] {verb}")
    } else {
        format!("[lineage] {verb} {target}")
    }
}

fn summarize_investigation_marked(payload: Option<&Value>) -> String {
    let reason = payload
        .and_then(|p| p.get("reason").and_then(Value::as_str))
        .filter(|s| !s.is_empty());
    match reason {
        Some(r) => format!("[investigation] marked ({})", clip_oneline(r, 40)),
        None => "[investigation] marked".to_string(),
    }
}

fn summarize_operator_note(payload: Option<&Value>, legacy: &str) -> String {
    let text = payload
        .and_then(|p| {
            p.get("note")
                .or_else(|| p.get("text"))
                .or_else(|| p.get("body"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
        .or_else(|| (!legacy.is_empty()).then(|| legacy.to_string()))
        .unwrap_or_default();
    if text.is_empty() {
        "[note]".to_string()
    } else {
        format!("[note] \"{}\"", clip_oneline(&text, NOTE_BODY_CAP))
    }
}

fn summarize_interrupted(payload: Option<&Value>) -> String {
    let kind = payload
        .and_then(|p| {
            p.get("interruption_type")
                .or_else(|| p.get("kind"))
                .and_then(Value::as_str)
        })
        .unwrap_or("");
    let g = payload.and_then(|p| {
        p.get("generation")
            .or_else(|| p.get("pause_generation"))
            .or_else(|| p.get("freeze_generation"))
            .and_then(Value::as_i64)
    });
    match (kind.is_empty(), g) {
        (false, Some(g)) => format!("[interrupt] {kind} gen={g}"),
        (false, None) => format!("[interrupt] {kind}"),
        (true, Some(g)) => format!("[interrupt] gen={g}"),
        (true, None) => "[interrupt]".to_string(),
    }
}

fn summarize_orphan_close(payload: Option<&Value>) -> String {
    let aid = payload
        .and_then(|p| p.get("attempt_id").and_then(Value::as_i64))
        .unwrap_or(0);
    let st = payload
        .and_then(|p| p.get("task_status").and_then(Value::as_str))
        .unwrap_or("");
    match (aid, st.is_empty()) {
        (0, true) => "[orphan] attempt closed".to_string(),
        (a, true) => format!("[orphan] a#{a} closed"),
        (0, false) => format!("[orphan] attempt closed (task={st})"),
        (a, false) => format!("[orphan] a#{a} closed (task={st})"),
    }
}

fn summarize_terminal(payload: Option<&Value>) -> String {
    let reason = payload
        .and_then(|p| p.get("reason").and_then(Value::as_str))
        .unwrap_or("");
    let attempts = payload
        .and_then(|p| p.get("attempts").and_then(Value::as_i64))
        .unwrap_or(-1);
    let retries = payload
        .and_then(|p| p.get("retries").and_then(Value::as_i64))
        .unwrap_or(-1);
    let wall = payload
        .and_then(|p| p.get("wall_clock_secs").and_then(Value::as_i64))
        .unwrap_or(-1);
    let class = payload
        .and_then(|p| p.get("last_failure_class").and_then(Value::as_str))
        .unwrap_or("");
    let head = if reason.is_empty() {
        "[terminal]".to_string()
    } else {
        format!("[terminal] {reason}")
    };
    let mut parts: Vec<String> = Vec::new();
    if attempts >= 0 {
        parts.push(format!("attempts={attempts}"));
    }
    if retries >= 0 {
        parts.push(format!("retries={retries}"));
    }
    if wall >= 0 {
        parts.push(format!("wall={wall}s"));
    }
    if !class.is_empty() {
        parts.push(format!("class={class}"));
    }
    if parts.is_empty() {
        head
    } else {
        format!("{head} · {}", parts.join(" "))
    }
}

fn summarize_thrash(payload: Option<&Value>) -> String {
    let class = payload
        .and_then(|p| p.get("class").and_then(Value::as_str))
        .unwrap_or("");
    let count = payload
        .and_then(|p| p.get("count").and_then(Value::as_i64))
        .unwrap_or(0);
    let threshold = payload
        .and_then(|p| p.get("threshold").and_then(Value::as_i64))
        .unwrap_or(0);
    match (class.is_empty(), count, threshold) {
        (false, c, t) if c > 0 && t > 0 => format!("[thrash] {class} ×{c}/{t}"),
        (false, c, _) if c > 0 => format!("[thrash] {class} ×{c}"),
        (false, _, _) => format!("[thrash] {class}"),
        (true, c, _) if c > 0 => format!("[thrash] ×{c}"),
        (true, _, _) => "[thrash] detected".to_string(),
    }
}

// ─────────────────────────── Helpers ───────────────────────────

/// Replace whitespace/control chars with single spaces, collapse runs,
/// and cap at `cap` characters with a trailing `…` when clipped. Suitable
/// for putting arbitrary user text in a one-line dashboard cell.
fn clip_oneline(s: &str, cap: usize) -> String {
    let mut out = String::with_capacity(s.len().min(cap + 1));
    let mut last_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() || ch.is_control() {
            if !last_space && !out.is_empty() {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    let trimmed = out.trim_end().to_string();
    if trimmed.chars().count() <= cap {
        trimmed
    } else {
        let mut clipped: String = trimmed.chars().take(cap).collect();
        clipped.push('…');
        clipped
    }
}

// ─────────────────────────── Tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event_type: &str, payload_json: Option<&str>) -> TaskEvent {
        TaskEvent {
            event_id: 1,
            ts: 0,
            event_type: event_type.to_string(),
            payload: String::new(),
            schema_version: 1,
            attempt_id: None,
            trace_id: None,
            payload_json: payload_json.map(str::to_string),
        }
    }

    fn ev_with_attempt(event_type: &str, attempt_id: i64, payload_json: Option<&str>) -> TaskEvent {
        let mut e = ev(event_type, payload_json);
        e.attempt_id = Some(attempt_id);
        e
    }

    #[test]
    fn create_with_status() {
        let s = summarize_event(&ev("task.create", Some(r#"{"status":"new"}"#)));
        assert_eq!(s, "[create] new task (new)");
    }

    #[test]
    fn create_without_status() {
        let s = summarize_event(&ev("task.create", None));
        assert_eq!(s, "[create] new task");
    }

    #[test]
    fn update_with_status_delta() {
        let s = summarize_event(&ev("task.update", Some(r#"{"status":"completed"}"#)));
        assert_eq!(s, "[update] → completed");
    }

    #[test]
    fn update_without_status_lists_keys() {
        let s = summarize_event(&ev(
            "task.update",
            Some(r#"{"title":"new title","priority":3}"#),
        ));
        assert_eq!(s, "[update] priority,title");
    }

    #[test]
    fn attempt_started_with_id() {
        let s = summarize_event(&ev_with_attempt("task.attempt_started", 3, None));
        assert_eq!(s, "[attempt] started a#3");
    }

    #[test]
    fn attempt_finished_with_outcome() {
        let s = summarize_event(&ev_with_attempt(
            "task.attempt_finished",
            7,
            Some(r#"{"outcome":"success"}"#),
        ));
        assert_eq!(s, "[attempt] finished a#7 → success");
    }

    #[test]
    fn attempt_finished_without_outcome() {
        let s = summarize_event(&ev_with_attempt("task.attempt_finished", 7, None));
        assert_eq!(s, "[attempt] finished a#7");
    }

    #[test]
    fn retry_requested_with_count_and_budget() {
        let s = summarize_event(&ev(
            "task.retry_requested",
            Some(r#"{"retry_count":2,"retry_budget":5}"#),
        ));
        assert_eq!(s, "[retry] requested (#2/5)");
    }

    #[test]
    fn retry_requested_with_count_only() {
        let s = summarize_event(&ev("task.retry_requested", Some(r#"{"retry_count":3}"#)));
        assert_eq!(s, "[retry] requested (#3)");
    }

    #[test]
    fn retry_suppressed_with_reason() {
        let s = summarize_event(&ev(
            "task.retry_suppressed",
            Some(r#"{"suppressed_by":"paused"}"#),
        ));
        assert_eq!(s, "[retry] suppressed (paused)");
    }

    #[test]
    fn pause_requested_with_gen_and_reason() {
        let s = summarize_event(&ev(
            "task.pause_requested",
            Some(r#"{"pause_generation":2,"reason":"operator hold"}"#),
        ));
        assert_eq!(s, "[pause] requested gen=2 (operator hold)");
    }

    #[test]
    fn pause_requested_without_payload() {
        let s = summarize_event(&ev("task.pause_requested", None));
        assert_eq!(s, "[pause] requested");
    }

    #[test]
    fn freeze_requested_with_gen() {
        let s = summarize_event(&ev(
            "task.freeze_requested",
            Some(r#"{"freeze_generation":1}"#),
        ));
        assert_eq!(s, "[freeze] requested gen=1");
    }

    #[test]
    fn observation_pause() {
        let s = summarize_event(&ev(
            "task.pause_observed",
            Some(r#"{"generation_observed":2}"#),
        ));
        assert_eq!(s, "[pause] observed gen=2");
    }

    #[test]
    fn lineage_spawned() {
        let s = summarize_event(&ev("task.spawned_child", Some(r#"{"child_id":"abc-123"}"#)));
        assert_eq!(s, "[lineage] spawned → abc-123");
    }

    #[test]
    fn lineage_delegated() {
        let s = summarize_event(&ev("task.delegated_to", Some(r#"{"target_id":"node-7"}"#)));
        assert_eq!(s, "[lineage] delegated → node-7");
    }

    #[test]
    fn lineage_missing_target() {
        let s = summarize_event(&ev("task.spawned_child", None));
        assert_eq!(s, "[lineage] spawned →");
    }

    #[test]
    fn investigation_marked_with_reason() {
        let s = summarize_event(&ev(
            "task.investigation_marked",
            Some(r#"{"reason":"flaky test"}"#),
        ));
        assert_eq!(s, "[investigation] marked (flaky test)");
    }

    #[test]
    fn investigation_cleared() {
        let s = summarize_event(&ev("task.investigation_cleared", None));
        assert_eq!(s, "[investigation] cleared");
    }

    #[test]
    fn operator_note_with_text() {
        let s = summarize_event(&ev("task.operator_note", Some(r#"{"note":"deployed v2"}"#)));
        assert_eq!(s, "[note] \"deployed v2\"");
    }

    #[test]
    fn operator_note_long_text_clipped() {
        let long = "a".repeat(80);
        let payload = format!(r#"{{"note":"{long}"}}"#);
        let s = summarize_event(&ev("task.operator_note", Some(&payload)));
        // 60-char cap + ellipsis.
        let expected_chars: String = "a".repeat(60);
        assert_eq!(s, format!("[note] \"{expected_chars}…\""));
    }

    #[test]
    fn interrupted_with_kind_and_gen() {
        let s = summarize_event(&ev(
            "task.interrupted",
            Some(r#"{"interruption_type":"pause","generation":2}"#),
        ));
        assert_eq!(s, "[interrupt] pause gen=2");
    }

    #[test]
    fn thrash_detected_full_payload() {
        let s = summarize_event(&ev(
            "task.thrash_detected",
            Some(r#"{"class":"transport","count":3,"threshold":3}"#),
        ));
        assert_eq!(s, "[thrash] transport ×3/3");
    }

    #[test]
    fn thrash_detected_no_payload() {
        let s = summarize_event(&ev("task.thrash_detected", None));
        assert_eq!(s, "[thrash] detected");
    }

    #[test]
    fn terminal_summary_full_payload() {
        let s = summarize_event(&ev(
            "task.terminal_summary",
            Some(
                r#"{"reason":"deadline_exceeded","attempts":3,"retries":2,"wall_clock_secs":120,"last_failure_class":"timeout"}"#,
            ),
        ));
        assert_eq!(
            s,
            "[terminal] deadline_exceeded · attempts=3 retries=2 wall=120s class=timeout"
        );
    }

    #[test]
    fn terminal_summary_no_payload() {
        let s = summarize_event(&ev("task.terminal_summary", None));
        assert_eq!(s, "[terminal]");
    }

    #[test]
    fn orphan_close_full() {
        let s = summarize_event(&ev(
            "task.attempt_orphan_closed",
            Some(
                r#"{"attempt_id":7,"closed_as":"interrupted","reason":"orphan","task_status":"failed"}"#,
            ),
        ));
        assert_eq!(s, "[orphan] a#7 closed (task=failed)");
    }

    #[test]
    fn orphan_close_minimal() {
        let s = summarize_event(&ev("task.attempt_orphan_closed", None));
        assert_eq!(s, "[orphan] attempt closed");
    }

    #[test]
    fn unknown_event_falls_through() {
        let s = summarize_event(&ev("task.fancy_new_event_type", None));
        assert_eq!(s, "[event] task.fancy_new_event_type");
    }

    #[test]
    fn malformed_payload_does_not_panic() {
        // Garbage JSON falls back to no-payload behaviour.
        let s = summarize_event(&ev("task.update", Some("not json {{}}")));
        assert_eq!(s, "[update]");
    }

    #[test]
    fn clip_oneline_collapses_whitespace() {
        assert_eq!(clip_oneline("  hello  \n\tworld   ", 100), "hello world");
        assert_eq!(clip_oneline("abc", 100), "abc");
    }

    #[test]
    fn clip_oneline_truncates_with_ellipsis() {
        assert_eq!(clip_oneline("abcdefghij", 5), "abcde…");
    }

    #[test]
    fn clip_oneline_handles_multibyte() {
        let s = "héllo wörld";
        assert_eq!(clip_oneline(s, 100), s);
    }

    #[test]
    fn all_summaries_are_non_empty() {
        for event_type in [
            "task.create",
            "task.update",
            "task.attempt_started",
            "task.attempt_finished",
            "task.retry_requested",
            "task.retry_exhausted",
            "task.retry_suppressed",
            "task.pause_requested",
            "task.resume_requested",
            "task.freeze_requested",
            "task.unfreeze_requested",
            "task.pause_observed",
            "task.resume_observed",
            "task.freeze_propagated",
            "task.spawned_child",
            "task.delegated_to",
            "task.awaiting",
            "task.investigation_marked",
            "task.investigation_cleared",
            "task.operator_note",
            "task.interrupted",
            "task.thrash_detected",
            "task.terminal_summary",
            "task.attempt_orphan_closed",
            "task.someday_unknown",
        ] {
            let s = summarize_event(&ev(event_type, None));
            assert!(!s.is_empty(), "empty summary for {event_type}");
            assert!(
                !s.contains('\n'),
                "newline in summary for {event_type}: {s}"
            );
        }
    }
}
