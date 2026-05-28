//! Capability handlers for `delegate.*`.
//!
//! Wire formats (all string-typed per SIMP-016):
//!
//! | Method | Arg | Return |
//! |---|---|---|
//! | `delegate.spawn`  | `parent_task_id\|goal\|context\|target_subject_id\|depth` | `<child_task_id>\n` |
//! | `delegate.result` | `<child_task_id>` | `status\|result_preview\|completed_at\n` (-1 if not terminal) |
//! | `delegate.cancel` | `<child_task_id>\|<reason>` | `ok\n` |
//! | `delegate.list`   | `<parent_task_id>` | `<child_task_id>\t<goal_preview>\t<status>\t<created_at>\n` per row + `count=N\n` |
//!
//! Failure paths surface as `INVALID_ARGS` (caller-fixable) or
//! `RESPONDER_INTERNAL` (storage hiccup).

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{HandlerOutcome, InvocationCtx};
use crate::nodes::coordinator::{CoordinatorError, RetryPolicy, TaskStore, is_allowed_transition};

/// `delegate.spawn` — create a child task delegated from
/// `parent_task_id`. The wire arg carries the goal, optional
/// context, optional target_subject_id, and the caller's
/// claimed depth.
///
/// Enforces the depth cap two ways: the caller's `depth`
/// integer must be `< max_depth`, AND an independent walk of
/// the `delegated_to` ancestor chain must report a depth `<
/// max_depth`. A caller that under-reports `depth` still gets
/// caught by the second check.
pub fn handle_spawn(store: &TaskStore, ctx: &InvocationCtx, max_depth: usize) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("delegate.spawn utf8: {e}")),
    };
    // `parent_task_id|goal|context|target_subject_id|depth`
    let parts: Vec<&str> = s.splitn(5, '|').collect();
    if parts.len() != 5 {
        return invalid(
            "delegate.spawn: expected `parent_task_id|goal|context|target_subject_id|depth`".into(),
        );
    }
    let parent_task_id = parts[0].trim();
    let goal = parts[1].trim();
    let context = parts[2];
    let target_subject_id_raw = parts[3].trim();
    let claimed_depth: usize = match parts[4].trim().parse() {
        Ok(n) => n,
        Err(_) => return invalid(format!("delegate.spawn: bad depth: {:?}", parts[4])),
    };

    if parent_task_id.is_empty() {
        return invalid("delegate.spawn: parent_task_id required".into());
    }
    if goal.is_empty() {
        return invalid("delegate.spawn: goal required".into());
    }
    if claimed_depth >= max_depth {
        return invalid(format!(
            "delegate.spawn: delegation depth limit reached (max {max_depth}; claimed {claimed_depth})"
        ));
    }

    // Independently walk the chain. The caller could be honest
    // and reasoning from its own state, but the coordinator is
    // the source of truth.
    let chain_depth = match store.delegation_chain_depth(parent_task_id, max_depth + 1) {
        Ok(d) => d,
        Err(e) => return internal(format!("delegate.spawn: chain walk: {e}")),
    };
    // chain_depth counts the parent's ancestors; the child we're
    // about to spawn would be at chain_depth + 1 (it has the
    // parent + every existing ancestor above it). Compare
    // chain_depth + 1 to max_depth.
    if chain_depth + 1 >= max_depth {
        return invalid(format!(
            "delegate.spawn: delegation depth limit reached (max {max_depth}; chain depth {chain_depth} + this would be {})",
            chain_depth + 1
        ));
    }

    // Look up the parent task to inherit owner_subject_id when
    // target_subject_id is unset.
    let parent = match store.get(parent_task_id) {
        Ok(Some(v)) => v,
        Ok(None) => {
            return invalid(format!(
                "delegate.spawn: parent not found: {parent_task_id}"
            ));
        }
        Err(e) => return internal(format!("delegate.spawn: parent lookup: {e}")),
    };
    let owner = if target_subject_id_raw.is_empty() {
        parent.owner_subject_id.clone()
    } else {
        target_subject_id_raw.to_string()
    };

    let title = format!("delegate: {}", preview(goal, 64));
    let flow_template = "delegation".to_string();
    let params_json = build_params_json(goal, context, claimed_depth + 1);

    // Create the child task.
    let child_id = match store.create(
        &title,
        &flow_template,
        &params_json,
        &owner,
        RetryPolicy::None,
        0,
        None,
        Some("delegation"),
    ) {
        Ok(id) => id,
        Err(e) => return internal(format!("delegate.spawn: create child: {e}")),
    };

    // Edge + chronicle event recording the delegation. The
    // existing `record_delegated` writes both atomically.
    if let Err(e) = store.record_delegated(
        parent_task_id,
        &child_id,
        Some(&preview(goal, 200)),
        &ctx.caller.subject_id.to_string(),
    ) {
        return internal(format!("delegate.spawn: record_delegated: {e}"));
    }

    // Flip the parent to awaiting_input. Skip when it isn't a
    // legal transition (a paused parent stays paused — we still
    // record the delegation; the executor will respect the
    // parent's state on the resume path).
    if is_allowed_transition(&parent.status, "awaiting_input")
        && let Err(e) = store.update(
            parent_task_id,
            Some("awaiting_input"),
            None,
            None,
            None,
            None,
            None,
            None,
        )
    {
        tracing::warn!(
            parent = %parent_task_id,
            error = %e,
            "delegate.spawn: parent status flip to awaiting_input failed"
        );
    }
    // Chronicle event on the parent so SOL flows / dashboards
    // know which child the parent is blocked on.
    let payload = format!("child_task_id={child_id}");
    if let Err(e) = store.append_event(parent_task_id, "task.awaiting", &payload) {
        tracing::warn!(error = %e, "delegate.spawn: task.awaiting event failed");
    }

    HandlerOutcome::Ok(format!("{child_id}\n").into_bytes())
}

/// `delegate.result` — read a child task's current state.
/// Returns `status|preview|completed_at`. `completed_at` is
/// the task's `updated_at` when the status is terminal
/// (completed / failed / cancelled), -1 otherwise.
pub fn handle_result(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("delegate.result utf8: {e}")),
    };
    if id.is_empty() {
        return invalid("delegate.result: child_task_id required".into());
    }
    let view = match store.get(id) {
        Ok(Some(v)) => v,
        Ok(None) => return invalid(format!("delegate.result: not found: {id}")),
        Err(e) => return internal(format!("delegate.result: {e}")),
    };
    let status = view.status;
    let result_preview = view
        .latest_result
        .as_deref()
        .map(|s| preview(s, 500))
        .unwrap_or_default();
    let completed_at = if is_terminal(&status) {
        view.updated_at
    } else {
        -1
    };
    let body = format!(
        "{status}|{}|{completed_at}\n",
        result_preview.replace('|', " ")
    );
    HandlerOutcome::Ok(body.into_bytes())
}

/// `delegate.cancel` — terminal-cancel a delegated child task.
/// Refuses if the task is already in a terminal state.
pub fn handle_cancel(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("delegate.cancel utf8: {e}")),
    };
    // `<child_task_id>|<reason>` — reason is optional.
    let (id, reason) = match s.split_once('|') {
        Some((a, b)) => (a.trim(), b),
        None => (s.trim(), ""),
    };
    if id.is_empty() {
        return invalid("delegate.cancel: child_task_id required".into());
    }
    let view = match store.get(id) {
        Ok(Some(v)) => v,
        Ok(None) => return invalid(format!("delegate.cancel: not found: {id}")),
        Err(e) => return internal(format!("delegate.cancel: {e}")),
    };
    if is_terminal(&view.status) {
        return invalid(format!("delegate.cancel: already {}: {id}", view.status));
    }
    // Append the chronicle event first so a status-flip race
    // can't leave us with a state change that's missing its
    // explanation.
    let payload = if reason.trim().is_empty() {
        String::new()
    } else {
        format!("reason={}", reason.replace('|', " "))
    };
    if let Err(e) = store.append_event(id, "delegate.cancelled", &payload) {
        tracing::warn!(error = %e, "delegate.cancel: chronicle event failed");
    }
    let result_text = if reason.trim().is_empty() {
        "cancelled via delegate.cancel".to_string()
    } else {
        format!("cancelled: {}", preview(reason.trim(), 200))
    };
    if let Err(e) = store.update(
        id,
        Some("cancelled"),
        Some(&result_text),
        None,
        None,
        None,
        None,
        None,
    ) {
        return internal(format!("delegate.cancel: update: {e}"));
    }
    HandlerOutcome::Ok(b"ok\n".to_vec())
}

/// `delegate.list` — child tasks delegated from `parent_task_id`.
pub fn handle_list(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let parent = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("delegate.list utf8: {e}")),
    };
    if parent.is_empty() {
        return invalid("delegate.list: parent_task_id required".into());
    }
    let edges = match store.list_edges_for_task(parent) {
        Ok(v) => v,
        Err(e) => return internal(format!("delegate.list: {e}")),
    };
    // Filter edges to those where `parent` is the producer
    // (task_id) and edge_type is `delegated_to`.
    let mut out = String::new();
    let mut count = 0usize;
    for e in edges {
        if e.edge_type != "delegated_to" || e.task_id != parent {
            continue;
        }
        let Some(child_id) = e.related_task_id else {
            continue;
        };
        let (goal_preview, status) = match store.get(&child_id) {
            Ok(Some(v)) => (extract_goal_from_params(&v.params_json), v.status),
            Ok(None) => (String::new(), "missing".to_string()),
            Err(_) => (String::new(), "error".to_string()),
        };
        out.push_str(&format!(
            "{child_id}\t{}\t{status}\t{}\n",
            goal_preview.replace(['\t', '\n'], " "),
            e.created_at
        ));
        count += 1;
    }
    out.push_str(&format!("count={count}\n"));
    HandlerOutcome::Ok(out.into_bytes())
}

// ── helpers ──────────────────────────────────────────────

/// Render `params_json` for a delegated child. Stores the
/// goal, optional context, and the depth so the executor can
/// reconstruct the call without re-walking the chain. JSON-
/// escaped on both fields.
pub fn build_params_json(goal: &str, context: &str, depth: usize) -> String {
    format!(
        "{{\"goal\":\"{}\",\"context\":\"{}\",\"depth\":{depth}}}",
        json_escape(goal),
        json_escape(context)
    )
}

/// Pull the `goal` value out of a delegated child's
/// `params_json` for the list view. Hand-rolled rather than
/// pulling in serde_json here: the format is fixed and
/// internal.
pub fn extract_goal_from_params(params_json: &str) -> String {
    extract_json_string_field(params_json, "goal").unwrap_or_default()
}

/// Pull the `context` value out of a delegated child's
/// `params_json` for the executor.
pub fn extract_context_from_params(params_json: &str) -> String {
    extract_json_string_field(params_json, "context").unwrap_or_default()
}

fn extract_json_string_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = json.find(&needle)? + needle.len();
    // Walk forward respecting `\"` escapes until an unescaped `"`.
    let mut out = String::new();
    let mut escaped = false;
    for c in json[start..].chars() {
        if escaped {
            out.push(match c {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '"' => '"',
                '\\' => '\\',
                other => other,
            });
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if c == '"' {
            return Some(out);
        }
        out.push(c);
    }
    Some(out)
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn preview(s: &str, max_chars: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect();
    cleaned.chars().take(max_chars).collect()
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled" | "interrupted")
}

fn invalid(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

fn internal(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause,
        retry_hint: 1,
        retry_after: None,
    })
}

#[allow(dead_code)]
fn coordinator_err_to_handler(e: CoordinatorError) -> HandlerOutcome {
    internal(format!("{e}"))
}

#[cfg(test)]
pub(crate) fn fake_ctx(args: &[u8]) -> InvocationCtx {
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};
    InvocationCtx {
        caller: VerifiedIdentity {
            subject_id: NodeId::from_pubkey(b"caller"),
            name: "agent-a".into(),
            org_id: NodeId::from_pubkey(b"org"),
            groups: vec![],
            role: "".into(),
            clearance: "".into(),
            bundle_id: [0; 32],
        },
        trace_id: TraceId::new(),
        request_id: RequestId::new(),
        args: args.to_vec(),
        tenant_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn mk_store_with_parent() -> (Arc<TaskStore>, String) {
        let s = Arc::new(TaskStore::in_memory().unwrap());
        let parent = s
            .create(
                "parent-task",
                "agent.sol",
                "{}",
                "subj-a",
                RetryPolicy::None,
                0,
                None,
                Some("dashboard"),
            )
            .unwrap();
        s.update(&parent, Some("running"), None, None, None, None, None, None)
            .unwrap();
        (s, parent)
    }

    fn ok_body(o: HandlerOutcome) -> String {
        match o {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got Err: {} {}", e.kind, e.cause),
        }
    }
    fn err_kind(o: HandlerOutcome) -> u32 {
        match o {
            HandlerOutcome::Ok(_) => panic!("expected Err"),
            HandlerOutcome::Err(e) => e.kind,
        }
    }

    #[test]
    fn spawn_creates_a_child_task_and_records_edge() {
        let (s, parent) = mk_store_with_parent();
        let arg = format!("{parent}|do the thing|ctx|subj-b|0");
        let out = handle_spawn(&s, &fake_ctx(arg.as_bytes()), 3);
        let body = ok_body(out);
        let child = body.trim();
        assert!(!child.is_empty());
        // Edge exists.
        let edges = s.list_edges_for_task(&parent).unwrap();
        assert!(edges.iter().any(|e| e.edge_type == "delegated_to"
            && e.task_id == parent
            && e.related_task_id.as_deref() == Some(child)));
        // Child exists with origin_surface=delegation.
        let view = s.get(child).unwrap().unwrap();
        assert_eq!(view.origin_surface.as_deref(), Some("delegation"));
        assert_eq!(view.flow_template, "delegation");
        // Parent flipped to awaiting_input.
        let p = s.get(&parent).unwrap().unwrap();
        assert_eq!(p.status, "awaiting_input");
    }

    #[test]
    fn spawn_inherits_owner_when_target_subject_empty() {
        let (s, parent) = mk_store_with_parent();
        let arg = format!("{parent}|goal|ctx||0");
        let out = handle_spawn(&s, &fake_ctx(arg.as_bytes()), 3);
        let child = ok_body(out).trim().to_string();
        let view = s.get(&child).unwrap().unwrap();
        assert_eq!(view.owner_subject_id, "subj-a");
    }

    #[test]
    fn spawn_uses_target_subject_id_when_provided() {
        let (s, parent) = mk_store_with_parent();
        let arg = format!("{parent}|goal|ctx|subj-target|0");
        let out = handle_spawn(&s, &fake_ctx(arg.as_bytes()), 3);
        let child = ok_body(out).trim().to_string();
        let view = s.get(&child).unwrap().unwrap();
        assert_eq!(view.owner_subject_id, "subj-target");
    }

    #[test]
    fn spawn_rejects_claimed_depth_at_or_above_max() {
        let (s, parent) = mk_store_with_parent();
        let arg = format!("{parent}|goal|ctx||3");
        let out = handle_spawn(&s, &fake_ctx(arg.as_bytes()), 3);
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn spawn_walks_chain_and_rejects_when_chain_too_deep() {
        // Build a chain parent → child1 → child2. Then spawn
        // a third from child2 with depth=0 (lied). The chain
        // walk should catch it.
        let (s, parent) = mk_store_with_parent();
        let mk_intermediate = |s: &Arc<TaskStore>, ancestor: &str| -> String {
            let arg = format!("{ancestor}|step|c||0");
            let out = handle_spawn(s, &fake_ctx(arg.as_bytes()), /* max_depth */ 99);
            ok_body(out).trim().to_string()
        };
        // Mark `parent` as running so it can flip to awaiting_input.
        let child1 = mk_intermediate(&s, &parent);
        // For child1 to be a delegating parent it has to be
        // running, not awaiting_input. Push it back.
        s.update(&child1, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let child2 = mk_intermediate(&s, &child1);
        s.update(&child2, Some("running"), None, None, None, None, None, None)
            .unwrap();
        // Chain depth at child2 = 2 (child1, parent). Spawning
        // a third from child2 would put it at depth 3.
        // max_depth = 3 should reject.
        let arg = format!("{child2}|goal|ctx||0"); // caller lies depth=0
        let out = handle_spawn(&s, &fake_ctx(arg.as_bytes()), 3);
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn spawn_with_unknown_parent_returns_invalid_args() {
        let s = Arc::new(TaskStore::in_memory().unwrap());
        let out = handle_spawn(&s, &fake_ctx(b"nope|goal|c||0"), 3);
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn result_returns_pending_for_a_new_child_task() {
        let (s, parent) = mk_store_with_parent();
        let arg = format!("{parent}|goal|ctx||0");
        let child = ok_body(handle_spawn(&s, &fake_ctx(arg.as_bytes()), 3))
            .trim()
            .to_string();
        let out = handle_result(&s, &fake_ctx(child.as_bytes()));
        let body = ok_body(out);
        assert!(body.starts_with("pending|"), "got {body:?}");
        // completed_at sentinel.
        assert!(body.trim_end_matches('\n').ends_with("|-1"));
    }

    #[test]
    fn result_returns_completed_with_preview_and_timestamp() {
        let (s, parent) = mk_store_with_parent();
        let arg = format!("{parent}|goal|ctx||0");
        let child = ok_body(handle_spawn(&s, &fake_ctx(arg.as_bytes()), 3))
            .trim()
            .to_string();
        // Simulate executor completion.
        let big = "a".repeat(800);
        s.update(
            &child,
            Some("completed"),
            Some(&big),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let body = ok_body(handle_result(&s, &fake_ctx(child.as_bytes())));
        assert!(body.starts_with("completed|"));
        // Preview cut to 500 chars.
        let preview_part = body.split('|').nth(1).unwrap();
        assert_eq!(preview_part.chars().count(), 500);
        let completed_at: i64 = body
            .trim_end_matches('\n')
            .rsplit('|')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(completed_at > 0);
    }

    #[test]
    fn cancel_flips_status_and_records_event() {
        let (s, parent) = mk_store_with_parent();
        let arg = format!("{parent}|goal|ctx||0");
        let child = ok_body(handle_spawn(&s, &fake_ctx(arg.as_bytes()), 3))
            .trim()
            .to_string();
        let cancel_arg = format!("{child}|user requested cancel");
        let out = handle_cancel(&s, &fake_ctx(cancel_arg.as_bytes()));
        assert_eq!(ok_body(out), "ok\n");
        let view = s.get(&child).unwrap().unwrap();
        assert_eq!(view.status, "cancelled");
        // Chronicle event landed.
        let events = s
            .query_events(
                &child,
                0,
                100,
                None,
                crate::nodes::coordinator::EventOrder::Asc,
            )
            .unwrap();
        assert!(events.iter().any(|e| e.event_type == "delegate.cancelled"));
    }

    #[test]
    fn cancel_rejects_already_terminal_task() {
        let (s, parent) = mk_store_with_parent();
        let arg = format!("{parent}|goal|ctx||0");
        let child = ok_body(handle_spawn(&s, &fake_ctx(arg.as_bytes()), 3))
            .trim()
            .to_string();
        // Pre-complete it.
        s.update(
            &child,
            Some("completed"),
            Some("ok"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let cancel_arg = format!("{child}|too late");
        let out = handle_cancel(&s, &fake_ctx(cancel_arg.as_bytes()));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn list_returns_each_delegated_child_for_a_parent() {
        let (s, parent) = mk_store_with_parent();
        for goal in ["first", "second", "third"] {
            let arg = format!("{parent}|{goal}|c||0");
            let _ = handle_spawn(&s, &fake_ctx(arg.as_bytes()), 99);
            // Allow next spawn — flip parent back to running.
            s.update(&parent, Some("running"), None, None, None, None, None, None)
                .unwrap();
        }
        let out = handle_list(&s, &fake_ctx(parent.as_bytes()));
        let body = ok_body(out);
        let lines: Vec<&str> = body.lines().collect();
        // 3 rows + count line.
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[3], "count=3");
        // Each row has 4 columns.
        for row in &lines[..3] {
            assert_eq!(row.split('\t').count(), 4);
        }
    }

    #[test]
    fn list_returns_count_zero_for_parent_without_delegations() {
        let s = Arc::new(TaskStore::in_memory().unwrap());
        let parent = s
            .create(
                "lonely",
                "f.sol",
                "{}",
                "subj",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        let body = ok_body(handle_list(&s, &fake_ctx(parent.as_bytes())));
        assert_eq!(body, "count=0\n");
    }

    #[test]
    fn extract_goal_round_trips_through_build_params_json() {
        let p = build_params_json("ship the thing", "with \"quotes\" and \\backslashes\n", 2);
        assert_eq!(extract_goal_from_params(&p), "ship the thing");
        assert_eq!(
            extract_context_from_params(&p),
            "with \"quotes\" and \\backslashes\n"
        );
    }
}
