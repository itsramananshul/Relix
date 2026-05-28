//! `memory.context_flush` — move all-but-N raw turns from the
//! live SQLite context window into Qdrant-backed Layer 2.
//!
//! The handler:
//!
//! 1. Reads every unflushed `turn` for `session_id` from the
//!    `turns` table (oldest first).
//! 2. Slices off the most recent `keep_recent_n` entries so
//!    they remain in the live context window.
//! 3. Embeds the rest via the configured
//!    [`super::curator::EmbeddingDispatcher`]; when the
//!    dispatcher is missing OR returns an error the records
//!    still land in SQLite with a NULL embedding and the
//!    background pipeline picks them up on its next tick.
//! 4. Upserts one Layer 2 `Semantic` record per flushed turn
//!    into the layered store.
//! 5. Marks each flushed turn `flushed = 1` in the SQLite
//!    `turns` table (preserved forever; just excluded from the
//!    next flush pass).
//!
//! Wire format (JSON):
//!
//! ```json
//! { "session_id": "...", "agent_name": "...", "keep_recent_n": 5 }
//! ```

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::dispatch::{HandlerOutcome, InvocationCtx};

use super::curator::EmbeddingDispatcher;
use super::schema::{MemoryLayer, MemoryRecord, SourceTrust};
use super::{LayeredContext, MemoryStore, internal, invalid_args};

#[derive(Debug, Deserialize, Default)]
pub(crate) struct ContextFlushArgs {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub agent_name: String,
    #[serde(default = "default_keep_recent_n")]
    pub keep_recent_n: usize,
}

fn default_keep_recent_n() -> usize {
    5
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ContextFlushResponse {
    pub flushed_count: usize,
    pub remaining_in_context: usize,
    pub session_id: String,
    pub embedded: usize,
    pub deferred_embeddings: usize,
}

pub async fn handle_context_flush(
    store: &MemoryStore,
    layered: &LayeredContext,
    embed_cell: &tokio::sync::OnceCell<Arc<dyn EmbeddingDispatcher>>,
    embedding_model: &str,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let args: ContextFlushArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.context_flush: decode args: {e}")),
    };
    if args.session_id.trim().is_empty() {
        return invalid_args("memory.context_flush: session_id required".into());
    }
    let keep = args.keep_recent_n;

    let turns = match store.unflushed_turns_for_session(&args.session_id) {
        Ok(t) => t,
        Err(e) => return internal(format!("memory.context_flush: read turns: {e}")),
    };

    // Slice: everything except the most recent `keep` is flushed.
    let total = turns.len();
    let to_flush: Vec<(i64, String, String)> = if total > keep {
        turns.into_iter().take(total - keep).collect()
    } else {
        // Nothing to flush — every unflushed row is within the
        // keep window. Return a clean response noting it.
        let remaining = store
            .unflushed_turn_count(&args.session_id)
            .unwrap_or(total);
        let body = ContextFlushResponse {
            flushed_count: 0,
            remaining_in_context: remaining,
            session_id: args.session_id,
            embedded: 0,
            deferred_embeddings: 0,
        };
        return match serde_json::to_vec(&body) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("memory.context_flush: encode response: {e}")),
        };
    };

    // Embed the batch when we can; otherwise defer.
    let dispatcher = embed_cell.get().cloned();
    let bodies: Vec<&str> = to_flush.iter().map(|(_, _, body)| body.as_str()).collect();
    let vectors: Vec<Option<Vec<f32>>> = match dispatcher {
        Some(d) => match d.embed(embedding_model, &bodies).await {
            Ok(v) if v.len() == to_flush.len() => v.into_iter().map(Some).collect(),
            Ok(v) => {
                tracing::warn!(
                    got = v.len(),
                    want = to_flush.len(),
                    "memory.context_flush: embed returned wrong count; deferring all"
                );
                to_flush.iter().map(|_| None).collect()
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "memory.context_flush: embed failed; deferring all"
                );
                to_flush.iter().map(|_| None).collect()
            }
        },
        None => to_flush.iter().map(|_| None).collect(),
    };

    let mut flushed_ids: Vec<i64> = Vec::with_capacity(to_flush.len());
    let mut embedded = 0usize;
    // GAP 23: stamp the caller's tenant on every flushed
    // record so the embedder pipeline routes the resulting
    // vector into the right Qdrant collection.
    let tenant_for_records = ctx.tenant_id.clone();
    for ((turn_id, role, body), vec_opt) in to_flush.iter().zip(vectors.iter()) {
        let id = mint_flush_id(&args.session_id, *turn_id, role, body);
        let mut record = MemoryRecord::new_raw(id, body.clone(), args.session_id.clone());
        record.layer = MemoryLayer::Semantic;
        record.source_trust = SourceTrust::Internal;
        record.tenant_id = tenant_for_records.clone();
        record.tags = vec![
            "ingest:context_flush".to_string(),
            "type:chat".to_string(),
            format!("role:{role}"),
            format!("session_id:{}", args.session_id),
            format!("turn_id:{turn_id}"),
        ];
        if !args.agent_name.is_empty() {
            record.tags.push(format!("agent:{}", args.agent_name));
        }
        if layered.anonymizer.enabled() {
            record.text = layered.anonymizer.anonymize(&record.text);
        }
        if let Some(v) = vec_opt {
            record.embedding = Some(v.clone());
            embedded += 1;
        }
        if let Err(e) = layered.store.insert(&record) {
            return internal(format!("memory.context_flush: store insert: {e}"));
        }
        flushed_ids.push(*turn_id);
    }

    let flushed_count = match store.mark_turns_flushed(&flushed_ids) {
        Ok(n) => n,
        Err(e) => return internal(format!("memory.context_flush: mark flushed: {e}")),
    };
    let remaining = store
        .unflushed_turn_count(&args.session_id)
        .unwrap_or(total - flushed_count);

    let response = ContextFlushResponse {
        flushed_count,
        remaining_in_context: remaining,
        session_id: args.session_id,
        embedded,
        deferred_embeddings: flushed_count - embedded,
    };
    match serde_json::to_vec(&response) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("memory.context_flush: encode response: {e}")),
    }
}

fn mint_flush_id(session_id: &str, turn_id: i64, role: &str, body: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"context_flush|");
    hasher.update(session_id.as_bytes());
    hasher.update(b"|");
    hasher.update(turn_id.to_le_bytes().as_ref());
    hasher.update(b"|");
    hasher.update(role.as_bytes());
    hasher.update(b"|");
    hasher.update(body.as_bytes());
    hasher.finalize().to_hex().as_str()[..24].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::memory::curator::EmbeddingError;
    use crate::nodes::memory::schema::LayeredMemoryStore;
    use async_trait::async_trait;

    fn layered_ctx() -> LayeredContext {
        LayeredContext::new(
            Arc::new(LayeredMemoryStore::in_memory().unwrap()),
            None,
            0.5,
        )
    }

    struct StubEmbed;

    #[async_trait]
    impl EmbeddingDispatcher for StubEmbed {
        async fn embed(
            &self,
            _model: &str,
            texts: &[&str],
        ) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            Ok(texts
                .iter()
                .enumerate()
                .map(|(i, _)| vec![(i as f32) + 1.0, 0.0, 0.0, 0.0])
                .collect())
        }
    }

    fn embed_cell_populated() -> Arc<tokio::sync::OnceCell<Arc<dyn EmbeddingDispatcher>>> {
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn EmbeddingDispatcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        cell.set(Arc::new(StubEmbed) as Arc<dyn EmbeddingDispatcher>)
            .ok();
        cell
    }

    fn ctx_for(args: serde_json::Value) -> InvocationCtx {
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
            args: serde_json::to_vec(&args).unwrap(),
            tenant_id: None,
        }
    }

    #[tokio::test]
    async fn flush_moves_old_turns_into_layer_2_and_keeps_recent() {
        let store = Arc::new(MemoryStore::in_memory().unwrap());
        let layered = layered_ctx();
        for i in 0..7 {
            store
                .write_turn(
                    "sess",
                    if i % 2 == 0 { "user" } else { "assistant" },
                    &format!("msg-{i}"),
                )
                .unwrap();
        }
        let embed_cell = embed_cell_populated();
        let outcome = handle_context_flush(
            &store,
            &layered,
            &embed_cell,
            "mock",
            &ctx_for(serde_json::json!({
                "session_id": "sess",
                "agent_name": "alice",
                "keep_recent_n": 3,
            })),
        )
        .await;
        let resp: ContextFlushResponse = match outcome {
            HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        assert_eq!(resp.flushed_count, 4);
        assert_eq!(resp.remaining_in_context, 3);
        // Layer 2 received four records under source = session id.
        let recs = layered
            .store
            .list(Some(MemoryLayer::Semantic), Some("sess"), 100, 0)
            .unwrap();
        assert_eq!(recs.len(), 4);
        for r in &recs {
            assert!(r.tags.iter().any(|t| t == "ingest:context_flush"));
            assert!(r.tags.iter().any(|t| t == "type:chat"));
            assert!(r.tags.iter().any(|t| t.starts_with("role:")));
            assert!(r.embedding.is_some());
        }
    }

    #[tokio::test]
    async fn second_flush_is_noop_on_already_flushed_turns() {
        let store = Arc::new(MemoryStore::in_memory().unwrap());
        let layered = layered_ctx();
        for i in 0..6 {
            store
                .write_turn("sess", "user", &format!("msg-{i}"))
                .unwrap();
        }
        let embed_cell = embed_cell_populated();
        // First flush.
        let _ = handle_context_flush(
            &store,
            &layered,
            &embed_cell,
            "mock",
            &ctx_for(serde_json::json!({"session_id":"sess","agent_name":"a","keep_recent_n":2})),
        )
        .await;
        // Second flush — only 2 turns remain in context, so 0
        // are flushed.
        let outcome = handle_context_flush(
            &store,
            &layered,
            &embed_cell,
            "mock",
            &ctx_for(serde_json::json!({"session_id":"sess","agent_name":"a","keep_recent_n":2})),
        )
        .await;
        let resp: ContextFlushResponse = match outcome {
            HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        assert_eq!(resp.flushed_count, 0);
        assert_eq!(resp.remaining_in_context, 2);
    }

    #[tokio::test]
    async fn keep_recent_n_controls_how_many_remain_in_context() {
        let store = Arc::new(MemoryStore::in_memory().unwrap());
        let layered = layered_ctx();
        for i in 0..10 {
            store
                .write_turn("sess", "user", &format!("msg-{i}"))
                .unwrap();
        }
        let embed_cell = embed_cell_populated();
        let outcome = handle_context_flush(
            &store,
            &layered,
            &embed_cell,
            "mock",
            &ctx_for(serde_json::json!({"session_id":"sess","agent_name":"a","keep_recent_n":7})),
        )
        .await;
        let resp: ContextFlushResponse = match outcome {
            HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        assert_eq!(resp.flushed_count, 3);
        assert_eq!(resp.remaining_in_context, 7);
    }

    #[tokio::test]
    async fn flushed_turns_become_searchable_via_layered_store() {
        let store = Arc::new(MemoryStore::in_memory().unwrap());
        let layered = layered_ctx();
        store
            .write_turn("sess", "user", "deploy staging tomorrow")
            .unwrap();
        store.write_turn("sess", "user", "newer message").unwrap();
        let embed_cell = embed_cell_populated();
        let _ = handle_context_flush(
            &store,
            &layered,
            &embed_cell,
            "mock",
            &ctx_for(serde_json::json!({"session_id":"sess","agent_name":"a","keep_recent_n":1})),
        )
        .await;
        let hits = layered.store.text_search("deploy", 10).unwrap();
        assert!(
            hits.iter().any(|r| r.text.contains("deploy staging")),
            "expected flushed text to be searchable"
        );
    }

    #[tokio::test]
    async fn flush_with_no_dispatcher_defers_embeddings() {
        let store = Arc::new(MemoryStore::in_memory().unwrap());
        let layered = layered_ctx();
        store.write_turn("sess", "user", "one").unwrap();
        store.write_turn("sess", "user", "two").unwrap();
        store.write_turn("sess", "user", "three").unwrap();
        let empty_cell: Arc<tokio::sync::OnceCell<Arc<dyn EmbeddingDispatcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let outcome = handle_context_flush(
            &store,
            &layered,
            &empty_cell,
            "mock",
            &ctx_for(serde_json::json!({"session_id":"sess","agent_name":"a","keep_recent_n":1})),
        )
        .await;
        let resp: ContextFlushResponse = match outcome {
            HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        assert_eq!(resp.flushed_count, 2);
        assert_eq!(resp.embedded, 0);
        assert_eq!(resp.deferred_embeddings, 2);
    }
}
