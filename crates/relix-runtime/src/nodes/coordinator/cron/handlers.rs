//! Capability handlers for `cron.*`.
//!
//! Wire formats (all string-typed per SIMP-016):
//!
//! | Method | Arg | Return |
//! |---|---|---|
//! | `cron.create` | `name\|schedule\|flow_template\|prompt\|subject_id` | `<job_id>\n` |
//! | `cron.list`   | `<subject_id>` (empty = all)                       | `<job_id>\t<name>\t<schedule>\t<next>\t<last>\t<enabled>\t<run_count>\n` per row, then `count=N\n` |
//! | `cron.get`    | `<job_id>`                                         | `job_id=…\|name=…\|schedule=…\|flow_template=…\|prompt=…\|subject_id=…\|enabled=…\|created_at=…\|updated_at=…\|last_run_at=…\|next_run_at=…\|run_count=…\|last_task_id=…\|last_status=…\n` |
//! | `cron.update` | `<job_id>\|<field>\|<value>`                       | `ok\n` |
//! | `cron.delete` | `<job_id>`                                         | `ok\n` |
//! | `cron.trigger` | `<job_id>`                                        | `<task_id>\n` (lands with the scheduler — see scheduler.rs) |
//!
//! Empty timestamp fields render as `-1`; empty text fields as the empty
//! string.

use relix_core::types::error_kinds;

use crate::dispatch::{HandlerOutcome, InvocationCtx};
use crate::nodes::coordinator::cron::store::{CronJob, CronStore, CronStoreError};

/// `cron.create` handler.
pub fn handle_create(store: &CronStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("cron.create utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(5, '|').collect();
    if parts.len() != 5 {
        return invalid(
            "cron.create: expected `name|schedule|flow_template|prompt|subject_id`".into(),
        );
    }
    let (name, schedule, flow_template, prompt, subject_id) =
        (parts[0], parts[1], parts[2], parts[3], parts[4]);
    match store.create(
        name,
        schedule,
        flow_template,
        prompt,
        subject_id,
        ctx.tenant_id_or_default(),
    ) {
        Ok(id) => HandlerOutcome::Ok(format!("{id}\n").into_bytes()),
        Err(CronStoreError::BadInput(m)) => invalid(m),
        Err(CronStoreError::Schedule(e)) => invalid(format!("cron.create: {e}")),
        Err(e) => internal(format!("cron.create: {e}")),
    }
}

/// `cron.list` handler.
pub fn handle_list(store: &CronStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("cron.list utf8: {e}")),
    };
    let subject = if s.is_empty() { None } else { Some(s) };
    match store.list(ctx.tenant_id_or_default(), subject) {
        Ok(rows) => {
            let mut out = String::new();
            for r in &rows {
                let last_at = r.last_run_at.unwrap_or(-1);
                let enabled = if r.enabled { 1 } else { 0 };
                out.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    r.job_id, r.name, r.schedule, r.next_run_at, last_at, enabled, r.run_count
                ));
            }
            out.push_str(&format!("count={}\n", rows.len()));
            HandlerOutcome::Ok(out.into_bytes())
        }
        Err(e) => internal(format!("cron.list: {e}")),
    }
}

/// `cron.get` handler.
pub fn handle_get(store: &CronStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let job_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("cron.get utf8: {e}")),
    };
    if job_id.is_empty() {
        return invalid("cron.get: job_id required".into());
    }
    match store.get_for_tenant(job_id, ctx.tenant_id_or_default()) {
        Ok(Some(j)) => HandlerOutcome::Ok(render_job_body(&j).into_bytes()),
        Ok(None) => invalid(format!("cron.get: not found: {job_id}")),
        Err(e) => internal(format!("cron.get: {e}")),
    }
}

/// `cron.update` handler.
pub fn handle_update(store: &CronStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("cron.update utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(3, '|').collect();
    if parts.len() != 3 {
        return invalid("cron.update: expected `job_id|field|value`".into());
    }
    let (job_id, field, value) = (parts[0], parts[1], parts[2]);
    match store.update_field(job_id, field, value) {
        Ok(()) => HandlerOutcome::Ok(b"ok\n".to_vec()),
        Err(CronStoreError::NotFound(_)) => invalid(format!("cron.update: not found: {job_id}")),
        Err(CronStoreError::BadInput(m)) => invalid(m),
        Err(CronStoreError::Schedule(e)) => invalid(format!("cron.update: {e}")),
        Err(e) => internal(format!("cron.update: {e}")),
    }
}

/// `cron.delete` handler.
pub fn handle_delete(store: &CronStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let job_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("cron.delete utf8: {e}")),
    };
    if job_id.is_empty() {
        return invalid("cron.delete: job_id required".into());
    }
    match store.delete(job_id) {
        Ok(()) => HandlerOutcome::Ok(b"ok\n".to_vec()),
        Err(CronStoreError::NotFound(_)) => invalid(format!("cron.delete: not found: {job_id}")),
        Err(e) => internal(format!("cron.delete: {e}")),
    }
}

/// Render one job to the `cron.get` wire body. Stable
/// pipe-delimited `key=value` shape consumed by the bridge
/// proxy and by SOL flows that read the body directly.
pub fn render_job_body(j: &CronJob) -> String {
    let last_at = j.last_run_at.unwrap_or(-1);
    let last_task = j.last_task_id.as_deref().unwrap_or("");
    let last_status = j.last_status.as_deref().unwrap_or("");
    let enabled = if j.enabled { 1 } else { 0 };
    // Sanitise the prompt minimally so `|` inside the prompt
    // doesn't break the wire format; tabs / newlines stay
    // intact since the format is `|`-delimited.
    let prompt_clean = j.prompt.replace('|', " ");
    let flow_clean = j.flow_template.replace('|', " ");
    format!(
        "job_id={}|name={}|schedule={}|flow_template={}|prompt={}|subject_id={}|enabled={}|created_at={}|updated_at={}|last_run_at={}|next_run_at={}|run_count={}|last_task_id={}|last_status={}\n",
        j.job_id,
        j.name,
        j.schedule,
        flow_clean,
        prompt_clean,
        j.subject_id,
        enabled,
        j.created_at,
        j.updated_at,
        last_at,
        j.next_run_at,
        j.run_count,
        last_task,
        last_status,
    )
}

fn invalid(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

fn internal(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause,
        retry_hint: 1,
        retry_after: None,
    })
}

/// Convenience: build a fake invocation context with the
/// given bytes. Lets unit tests call the handlers without
/// reaching for the full dispatch wiring.
#[cfg(test)]
pub(crate) fn fake_ctx(args: &[u8]) -> InvocationCtx {
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};
    InvocationCtx {
        caller: VerifiedIdentity {
            subject_id: NodeId::from_pubkey(b"x"),
            name: "x".into(),
            org_id: NodeId::from_pubkey(b"o"),
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

    fn store_with_jobs() -> (Arc<CronStore>, String, String) {
        let s = Arc::new(CronStore::in_memory().unwrap());
        let a = s
            .create("daily", "1d", "f.sol", "p", "subj-1", "default")
            .unwrap();
        let b = s
            .create("weekly", "7d", "f.sol", "p2", "subj-2", "default")
            .unwrap();
        (s, a, b)
    }

    fn ok_body(outcome: HandlerOutcome) -> String {
        match outcome {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got Err: {} / {}", e.kind, e.cause),
        }
    }

    fn err_kind(outcome: HandlerOutcome) -> u32 {
        match outcome {
            HandlerOutcome::Ok(_) => panic!("expected Err"),
            HandlerOutcome::Err(e) => e.kind,
        }
    }

    #[test]
    fn create_handler_returns_valid_job_id() {
        let s = CronStore::in_memory().unwrap();
        let out = handle_create(&s, &fake_ctx(b"daily|1d|f.sol|summarise|subj-1"));
        let body = ok_body(out);
        let id = body.trim();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        // And the row landed in the store.
        let j = s.get(id).unwrap().unwrap();
        assert_eq!(j.name, "daily");
        assert_eq!(j.schedule, "1d");
    }

    #[test]
    fn create_handler_rejects_missing_pipes() {
        let s = CronStore::in_memory().unwrap();
        let out = handle_create(&s, &fake_ctx(b"only-three|fields|here"));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn create_handler_surfaces_bad_schedule_as_invalid_argument() {
        let s = CronStore::in_memory().unwrap();
        let out = handle_create(&s, &fake_ctx(b"daily|garbage|f.sol|p|subj-1"));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn list_handler_returns_correct_row_count() {
        let (s, _a, _b) = store_with_jobs();
        let out = handle_list(&s, &fake_ctx(b""));
        let body = ok_body(out);
        // Two rows + one trailing count line.
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2], "count=2");
    }

    #[test]
    fn list_handler_filters_by_subject_id() {
        let (s, _a, _b) = store_with_jobs();
        let out = handle_list(&s, &fake_ctx(b"subj-1"));
        let body = ok_body(out);
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1], "count=1");
    }

    #[test]
    fn list_handler_row_columns_match_spec() {
        let (s, a, _b) = store_with_jobs();
        let out = handle_list(&s, &fake_ctx(b"subj-1"));
        let body = ok_body(out);
        let line = body.lines().next().unwrap();
        let cols: Vec<&str> = line.split('\t').collect();
        // job_id, name, schedule, next_run_at, last_run_at, enabled, run_count
        assert_eq!(cols.len(), 7);
        assert_eq!(cols[0], a);
        assert_eq!(cols[1], "daily");
        assert_eq!(cols[2], "1d");
        assert_eq!(cols[4], "-1", "last_run_at sentinel for a fresh job");
        assert_eq!(cols[5], "1", "enabled");
        assert_eq!(cols[6], "0", "run_count");
    }

    #[test]
    fn get_handler_returns_every_field() {
        let (s, a, _) = store_with_jobs();
        let out = handle_get(&s, &fake_ctx(a.as_bytes()));
        let body = ok_body(out);
        for needle in [
            "job_id=",
            "name=daily",
            "schedule=1d",
            "flow_template=f.sol",
            "prompt=p",
            "subject_id=subj-1",
            "enabled=1",
            "created_at=",
            "next_run_at=",
            "run_count=0",
            "last_run_at=-1",
            "last_task_id=",
            "last_status=",
        ] {
            assert!(body.contains(needle), "missing {needle:?} in {body:?}");
        }
    }

    #[test]
    fn get_handler_returns_invalid_argument_when_missing() {
        let s = CronStore::in_memory().unwrap();
        let out = handle_get(&s, &fake_ctx(b"nope"));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn get_handler_rejects_empty_job_id() {
        let s = CronStore::in_memory().unwrap();
        let out = handle_get(&s, &fake_ctx(b""));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn update_handler_toggles_enabled() {
        let (s, a, _) = store_with_jobs();
        let arg = format!("{a}|enabled|0");
        let out = handle_update(&s, &fake_ctx(arg.as_bytes()));
        assert!(matches!(ok_body(out).as_str(), "ok\n"));
        assert!(!s.get(&a).unwrap().unwrap().enabled);
    }

    #[test]
    fn update_handler_rejects_unknown_field() {
        let (s, a, _) = store_with_jobs();
        let arg = format!("{a}|name|new-name");
        let out = handle_update(&s, &fake_ctx(arg.as_bytes()));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn update_handler_returns_invalid_argument_when_job_missing() {
        let s = CronStore::in_memory().unwrap();
        let out = handle_update(&s, &fake_ctx(b"nope|enabled|0"));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn delete_handler_removes_job() {
        let (s, a, _) = store_with_jobs();
        let out = handle_delete(&s, &fake_ctx(a.as_bytes()));
        assert!(matches!(ok_body(out).as_str(), "ok\n"));
        assert!(s.get(&a).unwrap().is_none());
    }

    #[test]
    fn delete_handler_returns_invalid_argument_when_missing() {
        let s = CronStore::in_memory().unwrap();
        let out = handle_delete(&s, &fake_ctx(b"nope"));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }
}
