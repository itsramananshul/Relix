//! GAP 7 — Memory Inspector editing surface.
//!
//! Five JSON-wire caps operators call to mutate the four-layer
//! store directly (rather than going through the curator):
//!
//! - `memory.edit_record`           — replace one record's text.
//! - `memory.freeze_record`         — flip `frozen = true`.
//! - `memory.unfreeze_record`       — flip `frozen = false`.
//! - `memory.bulk_export`           — export every record for
//!   one `source` as JSONL.
//! - `memory.request_model_refresh` — force the next promoter
//!   tick to regenerate the Layer 4 model for one `source`
//!   (by aging the existing model past the throttle window).
//!
//! The cap layer is intentionally thin — every schema method it
//! needs already lives on [`super::schema::LayeredMemoryStore`].
//! Re-embedding of edited rows is delegated to the background
//! embedding pipeline; this module just clears the embedding
//! pointer so the pipeline notices on its next tick.

use serde::{Deserialize, Serialize};

use crate::dispatch::{HandlerOutcome, InvocationCtx};
use crate::nodes::memory::LayeredContext;
use crate::nodes::memory::schema::{MemoryLayer, MemoryRecord};

use super::{internal, invalid_args};

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct EditArgs {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct EditResponse {
    pub ok: bool,
    pub id: String,
    pub last_edited_ms: i64,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct FreezeArgs {
    #[serde(default)]
    pub id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct FreezeResponse {
    pub ok: bool,
    pub id: String,
    pub frozen: bool,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct BulkExportArgs {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub layer: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BulkExportResponse {
    pub source: String,
    pub records: Vec<MemoryRecord>,
    pub count: usize,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct ModelRefreshArgs {
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ModelRefreshResponse {
    pub ok: bool,
    pub source: String,
    /// `true` iff a Layer 4 model existed and was aged so the
    /// promoter regenerates it on its next tick. `false` when
    /// no model existed yet — the first promoter pass will
    /// build one without any intervention.
    pub aged_existing_model: bool,
}

pub async fn handle_edit(layered: &LayeredContext, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: EditArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.edit_record: decode args: {e}")),
    };
    if args.id.trim().is_empty() {
        return invalid_args("memory.edit_record: id required".into());
    }
    if args.text.is_empty() {
        return invalid_args("memory.edit_record: text required".into());
    }
    // Apply the same anonymizer pass regular writes get so the
    // operator can't accidentally insert PII via the edit path.
    let scrubbed = layered.anonymizer.anonymize(&args.text);
    let edited_at_ms = unix_millis();
    if let Err(e) = layered
        .store
        .edit_record_text(&args.id, &scrubbed, edited_at_ms)
    {
        return invalid_args(format!("memory.edit_record: {e}"));
    }
    let body = EditResponse {
        ok: true,
        id: args.id,
        last_edited_ms: edited_at_ms,
    };
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("memory.edit_record: encode response: {e}")),
    }
}

pub async fn handle_freeze(layered: &LayeredContext, ctx: &InvocationCtx) -> HandlerOutcome {
    apply_freeze(layered, ctx, true, "memory.freeze_record").await
}

pub async fn handle_unfreeze(layered: &LayeredContext, ctx: &InvocationCtx) -> HandlerOutcome {
    apply_freeze(layered, ctx, false, "memory.unfreeze_record").await
}

async fn apply_freeze(
    layered: &LayeredContext,
    ctx: &InvocationCtx,
    frozen: bool,
    cap_name: &str,
) -> HandlerOutcome {
    let args: FreezeArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("{cap_name}: decode args: {e}")),
    };
    if args.id.trim().is_empty() {
        return invalid_args(format!("{cap_name}: id required"));
    }
    if let Err(e) = layered.store.set_frozen(&args.id, frozen) {
        return invalid_args(format!("{cap_name}: {e}"));
    }
    let body = FreezeResponse {
        ok: true,
        id: args.id,
        frozen,
    };
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("{cap_name}: encode response: {e}")),
    }
}

pub async fn handle_bulk_export(layered: &LayeredContext, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: BulkExportArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.bulk_export: decode args: {e}")),
    };
    if args.source.trim().is_empty() {
        return invalid_args("memory.bulk_export: source required".into());
    }
    let layer = match args.layer.as_deref() {
        None | Some("") => None,
        Some(s) => match MemoryLayer::parse(s) {
            Some(l) => Some(l),
            None => return invalid_args(format!("memory.bulk_export: unknown layer {s}")),
        },
    };
    let records = match layered.store.export_for_source(&args.source, layer) {
        Ok(r) => r,
        Err(e) => return internal(format!("memory.bulk_export: store error: {e}")),
    };
    let count = records.len();
    let body = BulkExportResponse {
        source: args.source,
        records,
        count,
    };
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("memory.bulk_export: encode response: {e}")),
    }
}

pub async fn handle_request_model_refresh(
    layered: &LayeredContext,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let args: ModelRefreshArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => {
            return invalid_args(format!("memory.request_model_refresh: decode args: {e}"));
        }
    };
    if args.source.trim().is_empty() {
        return invalid_args("memory.request_model_refresh: source required".into());
    }
    let now = unix_secs();
    let aged_existing_model = match layered
        .store
        .latest_by_layer_and_source(MemoryLayer::Model, &args.source)
    {
        Ok(Some(model)) => {
            // Age the model past the promoter's throttle so the
            // next tick regenerates. We do this by editing the
            // observed_at column directly.
            let stale = now - super::promoter::MODEL_THROTTLE_SECS - 1;
            if let Err(e) = layered.store.touch_observed_at(&model.id, stale) {
                return internal(format!("memory.request_model_refresh: touch model: {e}"));
            }
            true
        }
        Ok(None) => false,
        Err(e) => return internal(format!("memory.request_model_refresh: lookup: {e}")),
    };
    let body = ModelRefreshResponse {
        ok: true,
        source: args.source,
        aged_existing_model,
    };
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!(
            "memory.request_model_refresh: encode response: {e}"
        )),
    }
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
    use crate::nodes::memory::schema::{LayeredMemoryStore, MemoryLayer};
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

    fn seed_obs(layered: &LayeredContext, id: &str, source: &str, text: &str) {
        let mut r = MemoryRecord::new_raw(id, text, source);
        r.layer = MemoryLayer::Observation;
        layered.store.insert(&r).unwrap();
    }

    #[tokio::test]
    async fn edit_replaces_text_and_clears_embedding() {
        let layered = make_layered();
        let mut r = MemoryRecord::new_raw("o1", "old text", "alice");
        r.layer = MemoryLayer::Observation;
        r.embedding = Some(vec![1.0, 2.0]);
        layered.store.insert(&r).unwrap();
        let args = EditArgs {
            id: "o1".into(),
            text: "new text".into(),
        };
        let outcome = handle_edit(&layered, &ctx_for(&args)).await;
        let body = match outcome {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        let v: EditResponse = serde_json::from_slice(&body).unwrap();
        assert!(v.ok);
        assert_eq!(v.id, "o1");
        let after = layered.store.get("o1").unwrap().unwrap();
        assert_eq!(after.text, "new text");
        assert!(after.embedding.is_none(), "embedding cleared");
        assert!(after.last_edited_ms.is_some());
    }

    #[tokio::test]
    async fn edit_empty_id_is_invalid_args() {
        let layered = make_layered();
        let outcome = handle_edit(&layered, &ctx_for(&EditArgs::default())).await;
        let HandlerOutcome::Err(env) = outcome else {
            panic!("expected error");
        };
        assert_eq!(env.kind, error_kinds::INVALID_ARGS);
    }

    #[tokio::test]
    async fn edit_empty_text_is_invalid_args() {
        let layered = make_layered();
        let args = EditArgs {
            id: "o1".into(),
            text: String::new(),
        };
        let outcome = handle_edit(&layered, &ctx_for(&args)).await;
        let HandlerOutcome::Err(env) = outcome else {
            panic!("expected error");
        };
        assert_eq!(env.kind, error_kinds::INVALID_ARGS);
    }

    #[tokio::test]
    async fn freeze_and_unfreeze_flip_the_flag() {
        let layered = make_layered();
        seed_obs(&layered, "o1", "alice", "User likes Postgres");
        // Freeze.
        let outcome = handle_freeze(&layered, &ctx_for(&FreezeArgs { id: "o1".into() })).await;
        let HandlerOutcome::Ok(b) = outcome else {
            panic!("freeze err");
        };
        let v: FreezeResponse = serde_json::from_slice(&b).unwrap();
        assert!(v.frozen);
        let after = layered.store.get("o1").unwrap().unwrap();
        assert!(after.frozen);
        // Unfreeze.
        let outcome = handle_unfreeze(&layered, &ctx_for(&FreezeArgs { id: "o1".into() })).await;
        let HandlerOutcome::Ok(b) = outcome else {
            panic!("unfreeze err");
        };
        let v: FreezeResponse = serde_json::from_slice(&b).unwrap();
        assert!(!v.frozen);
        let after = layered.store.get("o1").unwrap().unwrap();
        assert!(!after.frozen);
    }

    #[tokio::test]
    async fn bulk_export_returns_every_record_for_source() {
        let layered = make_layered();
        seed_obs(&layered, "o1", "alice", "fact 1");
        seed_obs(&layered, "o2", "alice", "fact 2");
        seed_obs(&layered, "o3", "bob", "other");
        let args = BulkExportArgs {
            source: "alice".into(),
            layer: None,
        };
        let outcome = handle_bulk_export(&layered, &ctx_for(&args)).await;
        let HandlerOutcome::Ok(b) = outcome else {
            panic!("bulk_export err");
        };
        let v: BulkExportResponse = serde_json::from_slice(&b).unwrap();
        assert_eq!(v.count, 2);
        assert!(v.records.iter().all(|r| r.source == "alice"));
    }

    #[tokio::test]
    async fn bulk_export_filters_by_layer() {
        let layered = make_layered();
        let mut raw = MemoryRecord::new_raw("r1", "raw turn", "alice");
        raw.layer = MemoryLayer::Raw;
        layered.store.insert(&raw).unwrap();
        seed_obs(&layered, "o1", "alice", "User likes Postgres");
        let args = BulkExportArgs {
            source: "alice".into(),
            layer: Some("observation".into()),
        };
        let outcome = handle_bulk_export(&layered, &ctx_for(&args)).await;
        let HandlerOutcome::Ok(b) = outcome else {
            panic!("err");
        };
        let v: BulkExportResponse = serde_json::from_slice(&b).unwrap();
        assert_eq!(v.count, 1);
        assert_eq!(v.records[0].layer, MemoryLayer::Observation);
    }

    #[tokio::test]
    async fn request_model_refresh_with_no_existing_model_is_idempotent() {
        let layered = make_layered();
        let args = ModelRefreshArgs {
            source: "alice".into(),
        };
        let outcome = handle_request_model_refresh(&layered, &ctx_for(&args)).await;
        let HandlerOutcome::Ok(b) = outcome else {
            panic!("err");
        };
        let v: ModelRefreshResponse = serde_json::from_slice(&b).unwrap();
        assert!(v.ok);
        assert!(!v.aged_existing_model);
    }

    #[tokio::test]
    async fn request_model_refresh_ages_existing_model_past_throttle() {
        let layered = make_layered();
        let mut model = MemoryRecord::new_raw("m1", "STATE", "alice");
        model.layer = MemoryLayer::Model;
        // Fresh model — within throttle window.
        layered.store.insert(&model).unwrap();
        let args = ModelRefreshArgs {
            source: "alice".into(),
        };
        let outcome = handle_request_model_refresh(&layered, &ctx_for(&args)).await;
        let HandlerOutcome::Ok(b) = outcome else {
            panic!("err");
        };
        let v: ModelRefreshResponse = serde_json::from_slice(&b).unwrap();
        assert!(v.aged_existing_model);
        let after = layered.store.get("m1").unwrap().unwrap();
        // The throttle is 3600s. After aging, the observed_at
        // must be at least that far in the past.
        let now = unix_secs();
        assert!(now - after.observed_at > super::super::promoter::MODEL_THROTTLE_SECS);
    }
}
