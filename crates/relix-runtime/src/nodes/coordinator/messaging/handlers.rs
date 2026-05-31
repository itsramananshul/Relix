//! Capability handlers for `msg.*`.
//!
//! Wire formats (all string-typed per SIMP-016):
//!
//! | Method | Arg | Return |
//! |---|---|---|
//! | `msg.send`   | `from\|to\|subject\|body\|thread_id\|reply_to\|ttl_secs\|origin_surface` | `<message_id>\n` |
//! | `msg.inbox`  | `subject_id\|limit\|include_read\|since_message_id` | tab rows + `count=N\n` |
//! | `msg.read`   | `message_id\|reader_subject_id` | `ok\n` |
//! | `msg.thread` | `thread_id\|subject_id`  | tab rows (oldest-first) + `count=N\n` |
//! | `msg.delete` | `message_id\|subject_id` | `ok\n` |

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{HandlerOutcome, InvocationCtx};
use crate::nodes::coordinator::messaging::store::{
    BODY_PREVIEW_CHARS, MessageRecord, MessageStore, MessageStoreError,
};

// ── msg.send ─────────────────────────────────────────────

pub fn handle_send(store: &MessageStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("msg.send utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(8, '|').collect();
    if parts.len() != 8 {
        return invalid(
            "msg.send: expected `from|to|subject|body|thread_id|reply_to|ttl_secs|origin_surface`"
                .into(),
        );
    }
    let from = parts[0];
    let to = parts[1];
    let subject = parts[2];
    let body = parts[3];
    let thread_id = parts[4].trim();
    let reply_to = parts[5].trim();
    let ttl_raw = parts[6].trim();
    let origin = parts[7];
    let ttl_secs: i64 = if ttl_raw.is_empty() {
        0
    } else {
        match ttl_raw.parse() {
            Ok(n) => n,
            Err(_) => return invalid(format!("msg.send: bad ttl_secs: {ttl_raw}")),
        }
    };
    let thread_opt = if thread_id.is_empty() {
        None
    } else {
        Some(thread_id)
    };
    let reply_opt = if reply_to.is_empty() {
        None
    } else {
        Some(reply_to)
    };
    match store.send(
        from,
        to,
        subject,
        body,
        thread_opt,
        reply_opt,
        ttl_secs,
        origin,
        ctx.tenant_id_or_default(),
    ) {
        Ok(id) => HandlerOutcome::Ok(format!("{id}\n").into_bytes()),
        Err(MessageStoreError::BadInput(m)) => invalid(m),
        Err(e) => internal(format!("msg.send: {e}")),
    }
}

// ── msg.inbox ────────────────────────────────────────────

pub fn handle_inbox(store: &MessageStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("msg.inbox utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(4, '|').collect();
    if parts.is_empty() || parts[0].trim().is_empty() {
        return invalid("msg.inbox: subject_id required".into());
    }
    let subject_id = parts[0].trim();
    let limit: usize = parts
        .get(1)
        .map(|x| x.trim())
        .filter(|x| !x.is_empty())
        .and_then(|x| x.parse().ok())
        .unwrap_or(20);
    let include_read = parts.get(2).map(|x| x.trim() == "1").unwrap_or(false);
    let since = parts.get(3).map(|x| x.trim()).filter(|x| !x.is_empty());
    match store.inbox(subject_id, limit, include_read, since) {
        Ok(rows) => HandlerOutcome::Ok(render_rows(&rows).into_bytes()),
        Err(e) => internal(format!("msg.inbox: {e}")),
    }
}

// ── msg.read ─────────────────────────────────────────────

pub fn handle_read(store: &MessageStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("msg.read utf8: {e}")),
    };
    let (message_id, reader) = match s.split_once('|') {
        Some((a, b)) => (a.trim(), b.trim()),
        None => return invalid("msg.read: expected `message_id|reader_subject_id`".into()),
    };
    if message_id.is_empty() || reader.is_empty() {
        return invalid("msg.read: message_id and reader_subject_id required".into());
    }
    match store.mark_read(message_id, reader) {
        Ok(()) => HandlerOutcome::Ok(b"ok\n".to_vec()),
        Err(MessageStoreError::NotFound(_)) => {
            invalid(format!("msg.read: not found: {message_id}"))
        }
        Err(MessageStoreError::Forbidden(m)) => invalid(m),
        Err(e) => internal(format!("msg.read: {e}")),
    }
}

// ── msg.thread ───────────────────────────────────────────

pub fn handle_thread(store: &MessageStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("msg.thread utf8: {e}")),
    };
    let (thread_id, subject_id) = match s.split_once('|') {
        Some((a, b)) => (a.trim(), b.trim()),
        None => return invalid("msg.thread: expected `thread_id|subject_id`".into()),
    };
    if thread_id.is_empty() || subject_id.is_empty() {
        return invalid("msg.thread: thread_id and subject_id required".into());
    }
    match store.thread(thread_id, subject_id) {
        Ok(rows) => HandlerOutcome::Ok(render_rows(&rows).into_bytes()),
        Err(MessageStoreError::Forbidden(m)) => invalid(m),
        Err(MessageStoreError::NotFound(_)) => {
            invalid(format!("msg.thread: not found: {thread_id}"))
        }
        Err(e) => internal(format!("msg.thread: {e}")),
    }
}

// ── msg.delete ───────────────────────────────────────────

pub fn handle_delete(store: &MessageStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("msg.delete utf8: {e}")),
    };
    let (message_id, subject_id) = match s.split_once('|') {
        Some((a, b)) => (a.trim(), b.trim()),
        None => return invalid("msg.delete: expected `message_id|subject_id`".into()),
    };
    if message_id.is_empty() || subject_id.is_empty() {
        return invalid("msg.delete: message_id and subject_id required".into());
    }
    match store.delete(message_id, subject_id) {
        Ok(()) => HandlerOutcome::Ok(b"ok\n".to_vec()),
        Err(MessageStoreError::NotFound(_)) => {
            invalid(format!("msg.delete: not found: {message_id}"))
        }
        Err(MessageStoreError::Forbidden(m)) => invalid(m),
        Err(e) => internal(format!("msg.delete: {e}")),
    }
}

// ── Shared rendering ─────────────────────────────────────

/// Render rows as
/// `message_id\tthread_id\tfrom\tsubject\tbody_preview\tsent_at\tread_at\tstatus\n`
/// plus a trailing `count=N\n`. Inbox uses this newest-first
/// and thread uses it oldest-first — the order is controlled
/// by the caller, not the renderer.
pub fn render_rows(rows: &[MessageRecord]) -> String {
    let mut out = String::new();
    for m in rows {
        let preview = preview(&m.body, BODY_PREVIEW_CHARS);
        let read_at = m.read_at.unwrap_or(-1);
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            m.message_id,
            m.thread_id,
            m.from_subject_id,
            sanitize(&m.subject),
            sanitize(&preview),
            m.sent_at,
            read_at,
            m.status,
        ));
    }
    out.push_str(&format!("count={}\n", rows.len()));
    out
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

fn sanitize(s: &str) -> String {
    s.replace(['\n', '\r', '\t'], " ")
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

#[cfg(test)]
pub(crate) fn fake_ctx(args: &[u8]) -> InvocationCtx {
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};
    InvocationCtx {
        caller: VerifiedIdentity {
            subject_id: NodeId::from_pubkey(b"caller"),
            name: "test".into(),
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

    fn store() -> Arc<MessageStore> {
        Arc::new(MessageStore::in_memory().unwrap())
    }

    fn ok_body(o: HandlerOutcome) -> String {
        match o {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok: {} {}", e.kind, e.cause),
        }
    }
    fn err_kind(o: HandlerOutcome) -> u32 {
        match o {
            HandlerOutcome::Ok(_) => panic!("expected Err"),
            HandlerOutcome::Err(e) => e.kind,
        }
    }

    #[test]
    fn send_returns_valid_message_id() {
        let s = store();
        let body = ok_body(handle_send(
            &s,
            &fake_ctx(b"alice|bob|hi|how's it going||||api"),
        ));
        let id = body.trim();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        // Row landed in the store.
        let m = s.get(id).unwrap().unwrap();
        assert_eq!(m.from_subject_id, "alice");
        assert_eq!(m.to_subject_id, "bob");
    }

    #[test]
    fn send_rejects_wrong_field_count() {
        let s = store();
        let out = handle_send(&s, &fake_ctx(b"too|few"));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn send_rejects_empty_required_fields() {
        let s = store();
        // empty `from`.
        let out = handle_send(&s, &fake_ctx(b"|bob|s|b||||api"));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
        // empty `body`.
        let out = handle_send(&s, &fake_ctx(b"alice|bob|s|||||api"));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn inbox_returns_correct_row_count_and_count_line() {
        let s = store();
        ok_body(handle_send(&s, &fake_ctx(b"alice|bob|s|first||||api")));
        ok_body(handle_send(&s, &fake_ctx(b"alice|bob|s|second||||api")));
        let body = ok_body(handle_inbox(&s, &fake_ctx(b"bob")));
        let lines: Vec<&str> = body.lines().collect();
        // 2 row lines + 1 count line.
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2], "count=2");
        // Each row has 8 columns.
        for row in &lines[..2] {
            assert_eq!(row.split('\t').count(), 8);
        }
    }

    #[test]
    fn inbox_default_excludes_read_messages() {
        let s = store();
        let id = ok_body(handle_send(&s, &fake_ctx(b"alice|bob|s|hi||||api")))
            .trim()
            .to_string();
        let arg = format!("{id}|bob");
        ok_body(handle_read(&s, &fake_ctx(arg.as_bytes())));
        let body = ok_body(handle_inbox(&s, &fake_ctx(b"bob")));
        assert!(body.contains("count=0"));
        // With include_read=1 the read row shows up.
        let body = ok_body(handle_inbox(&s, &fake_ctx(b"bob|20|1")));
        assert!(body.contains("count=1"));
        assert!(body.contains("\tread\n"));
    }

    #[test]
    fn read_handler_marks_correct_message() {
        let s = store();
        let id = ok_body(handle_send(&s, &fake_ctx(b"alice|bob|s|hi||||api")))
            .trim()
            .to_string();
        let arg = format!("{id}|bob");
        let body = ok_body(handle_read(&s, &fake_ctx(arg.as_bytes())));
        assert_eq!(body, "ok\n");
        let m = s.get(&id).unwrap().unwrap();
        assert_eq!(m.status, "read");
        assert!(m.read_at.is_some());
    }

    #[test]
    fn read_handler_rejects_non_recipient() {
        let s = store();
        let id = ok_body(handle_send(&s, &fake_ctx(b"alice|bob|s|hi||||api")))
            .trim()
            .to_string();
        let arg = format!("{id}|carol");
        let out = handle_read(&s, &fake_ctx(arg.as_bytes()));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn read_handler_returns_invalid_args_for_unknown_id() {
        let s = store();
        let out = handle_read(&s, &fake_ctx(b"nope|bob"));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn thread_handler_returns_every_message_in_thread() {
        let s = store();
        let m1 = ok_body(handle_send(&s, &fake_ctx(b"alice|bob|s|one||||api")))
            .trim()
            .to_string();
        let reply_arg = format!("bob|alice|s|two|{m1}|{m1}||api");
        let m2 = ok_body(handle_send(&s, &fake_ctx(reply_arg.as_bytes())))
            .trim()
            .to_string();
        let q = format!("{m1}|alice");
        let body = ok_body(handle_thread(&s, &fake_ctx(q.as_bytes())));
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.last().copied(), Some("count=2"));
        // Both ids present, regardless of intra-second tie order.
        let row_ids: Vec<&str> = lines[..2]
            .iter()
            .map(|l| l.split('\t').next().unwrap())
            .collect();
        assert!(row_ids.contains(&m1.as_str()));
        assert!(row_ids.contains(&m2.as_str()));
    }

    #[test]
    fn thread_handler_denies_non_participants() {
        let s = store();
        let m1 = ok_body(handle_send(&s, &fake_ctx(b"alice|bob|s|one||||api")))
            .trim()
            .to_string();
        let q = format!("{m1}|carol");
        let out = handle_thread(&s, &fake_ctx(q.as_bytes()));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn delete_handler_soft_deletes_for_sender_and_recipient() {
        let s = store();
        let id = ok_body(handle_send(&s, &fake_ctx(b"alice|bob|s|hi||||api")))
            .trim()
            .to_string();
        // Sender deletes.
        let arg = format!("{id}|alice");
        assert_eq!(
            ok_body(handle_delete(&s, &fake_ctx(arg.as_bytes()))),
            "ok\n"
        );
        assert_eq!(s.get(&id).unwrap().unwrap().status, "expired");
    }

    #[test]
    fn delete_handler_denies_third_party() {
        let s = store();
        let id = ok_body(handle_send(&s, &fake_ctx(b"alice|bob|s|hi||||api")))
            .trim()
            .to_string();
        let arg = format!("{id}|carol");
        let out = handle_delete(&s, &fake_ctx(arg.as_bytes()));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn delete_handler_returns_invalid_args_for_unknown_id() {
        let s = store();
        let out = handle_delete(&s, &fake_ctx(b"nope|alice"));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn inbox_body_preview_truncated_to_80_chars_and_no_tabs() {
        let s = store();
        let long: String = "x".repeat(200);
        let arg = format!("alice|bob|s|{long}||||api");
        ok_body(handle_send(&s, &fake_ctx(arg.as_bytes())));
        let body = ok_body(handle_inbox(&s, &fake_ctx(b"bob")));
        let line = body.lines().next().unwrap();
        let preview = line.split('\t').nth(4).unwrap();
        assert!(preview.chars().count() <= BODY_PREVIEW_CHARS);
        // No tabs or newlines inside the preview column.
        assert!(!preview.contains('\t'));
        assert!(!preview.contains('\n'));
    }
}
