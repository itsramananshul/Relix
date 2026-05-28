//! GAP 6 — quarantine capabilities.
//!
//! Three JSON-wire caps operators (or trusted scripts) call when
//! an observation has been scored as anomalous and parked in
//! `memory_quarantine`:
//!
//! - `memory.quarantine_list`    — page through pending rows.
//! - `memory.quarantine_approve` — promote a quarantined row to
//!   a real Layer-3 record, applying the same anonymizer pass
//!   regular writes get.
//! - `memory.quarantine_reject`  — drop a quarantined row on the
//!   floor.
//!
//! The quarantine schema lives in `schema.rs`: the
//! [`super::schema::QuarantineRow`] stores the candidate as a
//! JSON-encoded [`super::schema::MemoryRecord`] plus a
//! human-readable `reason` and trust tier. This module is the
//! dispatch surface — it never holds state.

use serde::{Deserialize, Serialize};

use crate::dispatch::{HandlerOutcome, InvocationCtx};
use crate::nodes::memory::LayeredContext;
use crate::nodes::memory::schema::{MemoryLayer, MemoryRecord};

use super::{internal, invalid_args};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct ListArgs {
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional source filter — applied in-memory after the
    /// SQL list (the underlying table doesn't index on source).
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ListRowJson {
    pub id: String,
    pub source: String,
    pub text: String,
    pub reason: String,
    pub source_trust: String,
    pub queued_at_ms: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ListResponse {
    pub rows: Vec<ListRowJson>,
    pub count: usize,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct ApproveArgs {
    #[serde(default)]
    pub id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ApproveResponse {
    pub ok: bool,
    pub observation_id: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct RejectArgs {
    #[serde(default)]
    pub id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RejectResponse {
    pub ok: bool,
}

pub async fn handle_list(layered: &LayeredContext, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: ListArgs = if ctx.args.is_empty() {
        ListArgs::default()
    } else {
        match serde_json::from_slice(&ctx.args) {
            Ok(a) => a,
            Err(e) => return invalid_args(format!("memory.quarantine_list: decode args: {e}")),
        }
    };
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let rows = match layered.store.quarantine_list(limit) {
        Ok(r) => r,
        Err(e) => {
            return internal(format!("memory.quarantine_list: store error: {e}"));
        }
    };
    let mut out = Vec::with_capacity(rows.len());
    for q in rows {
        let candidate: MemoryRecord = match serde_json::from_str(&q.record_json) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(id = %q.id, error = %e, "memory.quarantine_list: decode candidate failed");
                continue;
            }
        };
        if let Some(src_filter) = args.source.as_deref()
            && candidate.source != src_filter
        {
            continue;
        }
        out.push(ListRowJson {
            id: q.id,
            source: candidate.source,
            text: candidate.text,
            reason: q.reason,
            source_trust: q.source_trust.as_str().to_string(),
            queued_at_ms: q.queued_at_ms,
        });
    }
    let count = out.len();
    let body = ListResponse { rows: out, count };
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("memory.quarantine_list: encode response: {e}")),
    }
}

pub async fn handle_approve(layered: &LayeredContext, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: ApproveArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.quarantine_approve: decode args: {e}")),
    };
    if args.id.trim().is_empty() {
        return invalid_args("memory.quarantine_approve: id required".into());
    }
    let row = match layered.store.quarantine_take(&args.id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return invalid_args(format!(
                "memory.quarantine_approve: no quarantined row {}",
                args.id
            ));
        }
        Err(e) => return internal(format!("memory.quarantine_approve: store error: {e}")),
    };
    let mut candidate: MemoryRecord = match serde_json::from_str(&row.record_json) {
        Ok(c) => c,
        Err(e) => {
            return internal(format!("memory.quarantine_approve: decode candidate: {e}"));
        }
    };
    let now = unix_secs();
    candidate.text = layered.anonymizer.anonymize(&candidate.text);
    candidate.layer = MemoryLayer::Observation;
    candidate.id = format!("obs.from_quarantine.{}", row.id);
    candidate.created_at = now;
    candidate.observed_at = now;
    candidate.valid_from = now;
    candidate.valid_to = None;
    candidate.source_trust = row.source_trust;
    let mut new_tags = vec!["origin:quarantine_approved".to_string()];
    for t in candidate.tags.iter() {
        if !new_tags.contains(t) {
            new_tags.push(t.clone());
        }
    }
    candidate.tags = new_tags;
    if let Err(e) = layered.store.insert(&candidate) {
        return internal(format!("memory.quarantine_approve: insert: {e}"));
    }
    let body = ApproveResponse {
        ok: true,
        observation_id: candidate.id.clone(),
    };
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("memory.quarantine_approve: encode response: {e}")),
    }
}

pub async fn handle_reject(layered: &LayeredContext, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: RejectArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.quarantine_reject: decode args: {e}")),
    };
    if args.id.trim().is_empty() {
        return invalid_args("memory.quarantine_reject: id required".into());
    }
    let ok = match layered.store.quarantine_delete(&args.id) {
        Ok(b) => b,
        Err(e) => return internal(format!("memory.quarantine_reject: store error: {e}")),
    };
    if !ok {
        return invalid_args(format!(
            "memory.quarantine_reject: no quarantined row {}",
            args.id
        ));
    }
    let body = RejectResponse { ok: true };
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("memory.quarantine_reject: encode response: {e}")),
    }
}

/// Convenience wrapper used by the promoter and the ingest
/// path. Serialises a candidate observation to JSON and parks it
/// in the quarantine table with the supplied reason.
pub fn record_anomaly_quarantine(
    layered: &LayeredContext,
    candidate: &MemoryRecord,
    reason: &str,
) -> Result<String, String> {
    let id = mint_quarantine_id(candidate);
    let json = serde_json::to_string(candidate)
        .map_err(|e| format!("encode quarantine candidate: {e}"))?;
    let now_ms = unix_millis();
    layered
        .store
        .quarantine_insert(&id, &json, reason, now_ms, candidate.source_trust)
        .map_err(|e| format!("quarantine insert: {e}"))?;
    Ok(id)
}

fn mint_quarantine_id(candidate: &MemoryRecord) -> String {
    let now = unix_millis();
    let mut hasher = blake3::Hasher::new();
    hasher.update(candidate.source.as_bytes());
    hasher.update(b"|");
    hasher.update(candidate.text.as_bytes());
    hasher.update(b"|");
    hasher.update(now.to_le_bytes().as_ref());
    format!("q.{}", hex::encode(&hasher.finalize().as_bytes()[..8]))
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::memory::schema::{LayeredMemoryStore, SourceTrust};
    use relix_core::types::error_kinds;
    use std::sync::Arc;

    fn ctx_for<T: Serialize>(args: &T) -> InvocationCtx {
        use relix_core::types::{NodeId, RequestId, TraceId};
        InvocationCtx {
            caller: relix_core::identity::VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"caller"),
                name: "alice".into(),
                org_id: NodeId::from_pubkey(b"org"),
                groups: vec![],
                role: "agent".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: serde_json::to_vec(args).unwrap(),
            tenant_id: None,
        }
    }

    fn make_layered() -> LayeredContext {
        LayeredContext::new(
            Arc::new(LayeredMemoryStore::in_memory().unwrap()),
            None,
            0.0,
        )
    }

    fn candidate(source: &str, text: &str) -> MemoryRecord {
        let mut r = MemoryRecord::new_raw(format!("cand.{source}.{}", text.len()), text, source);
        r.layer = MemoryLayer::Observation;
        r.source_trust = SourceTrust::External;
        r
    }

    #[tokio::test]
    async fn list_returns_empty_when_store_empty() {
        let layered = make_layered();
        let args = ListArgs::default();
        let outcome = handle_list(&layered, &ctx_for(&args)).await;
        let body = match outcome {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        let v: ListResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.count, 0);
        assert!(v.rows.is_empty());
    }

    #[tokio::test]
    async fn list_surfaces_inserted_quarantine_rows() {
        let layered = make_layered();
        record_anomaly_quarantine(
            &layered,
            &candidate("alice", "User does X"),
            "short-message",
        )
        .unwrap();
        record_anomaly_quarantine(
            &layered,
            &candidate("alice", "User does Y"),
            "low-specificity",
        )
        .unwrap();
        let outcome = handle_list(&layered, &ctx_for(&ListArgs::default())).await;
        let body = match outcome {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        let v: ListResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.count, 2);
        assert!(v.rows.iter().all(|r| r.source == "alice"));
    }

    #[tokio::test]
    async fn list_filters_by_source_when_supplied() {
        let layered = make_layered();
        record_anomaly_quarantine(&layered, &candidate("alice", "obs A"), "reason").unwrap();
        record_anomaly_quarantine(&layered, &candidate("bob", "obs B"), "reason").unwrap();
        let args = ListArgs {
            limit: Some(50),
            source: Some("bob".into()),
        };
        let outcome = handle_list(&layered, &ctx_for(&args)).await;
        let body = match outcome {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        let v: ListResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.count, 1);
        assert_eq!(v.rows[0].source, "bob");
    }

    #[tokio::test]
    async fn approve_moves_row_to_observation_layer() {
        let layered = make_layered();
        let qid = record_anomaly_quarantine(
            &layered,
            &candidate("alice", "User prefers async chat"),
            "low-specificity",
        )
        .unwrap();
        let args = ApproveArgs { id: qid.clone() };
        let outcome = handle_approve(&layered, &ctx_for(&args)).await;
        let body = match outcome {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        let v: ApproveResponse = serde_json::from_slice(&body).unwrap();
        assert!(v.ok);
        // The quarantine row is gone.
        let list = layered.store.quarantine_list(100).unwrap();
        assert!(list.iter().all(|r| r.id != qid));
        // And there's a new observation.
        let obs = layered
            .store
            .list(Some(MemoryLayer::Observation), Some("alice"), 10, 0)
            .unwrap();
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].text, "User prefers async chat");
        assert_eq!(obs[0].source_trust, SourceTrust::External);
    }

    #[tokio::test]
    async fn reject_removes_row_without_creating_observation() {
        let layered = make_layered();
        let qid =
            record_anomaly_quarantine(&layered, &candidate("alice", "User dislikes X"), "filler")
                .unwrap();
        let args = RejectArgs { id: qid.clone() };
        let outcome = handle_reject(&layered, &ctx_for(&args)).await;
        let body = match outcome {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        let v: RejectResponse = serde_json::from_slice(&body).unwrap();
        assert!(v.ok);
        let list = layered.store.quarantine_list(100).unwrap();
        assert!(list.iter().all(|r| r.id != qid));
        let obs = layered
            .store
            .list(Some(MemoryLayer::Observation), Some("alice"), 10, 0)
            .unwrap();
        assert!(obs.is_empty());
    }

    #[tokio::test]
    async fn approve_unknown_id_is_invalid_args() {
        let layered = make_layered();
        let args = ApproveArgs {
            id: "q.nonexistent".into(),
        };
        let outcome = handle_approve(&layered, &ctx_for(&args)).await;
        let HandlerOutcome::Err(env) = outcome else {
            panic!("expected error");
        };
        assert_eq!(env.kind, error_kinds::INVALID_ARGS);
    }

    #[tokio::test]
    async fn approve_empty_id_is_invalid_args() {
        let layered = make_layered();
        let args = ApproveArgs::default();
        let outcome = handle_approve(&layered, &ctx_for(&args)).await;
        let HandlerOutcome::Err(env) = outcome else {
            panic!("expected error");
        };
        assert_eq!(env.kind, error_kinds::INVALID_ARGS);
    }

    #[test]
    fn mint_quarantine_id_starts_with_qdot() {
        let r = candidate("alice", "User does X");
        let id = mint_quarantine_id(&r);
        assert!(id.starts_with("q."));
    }
}
