//! RELIX-7.16 — coordinator-side dispatch handlers.
//!
//! Five unary capabilities, all JSON-encoded:
//!
//! - `knowledge.share`
//! - `knowledge.list_shared`
//! - `knowledge.group_broadcast`
//! - `knowledge.groups`
//! - `knowledge.revoke`
//!
//! Every handler is a thin wrapper around the
//! [`KnowledgeService`]. Error mapping mirrors the rest of
//! the coordinator caps:
//! - `InvalidArgs` → `error_kinds::INVALID_ARGS` (400 on the bridge).
//! - Store / mesh failures → `error_kinds::RESPONDER_INTERNAL`.

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::Deserialize;

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

use super::config::sharing_group_descriptors;
use super::remote::SignedSharePayload;
use super::service::{KnowledgeService, ListSharedFilter, ShareError, ShareRequest};

/// Wire every `knowledge.*` cap onto `bridge`.
pub fn register(bridge: &mut DispatchBridge, service: Arc<KnowledgeService>) {
    {
        let svc = service.clone();
        bridge.register(
            "knowledge.share",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = svc.clone();
                async move { handle_share(&svc, &ctx).await }
            })),
        );
    }
    {
        let svc = service.clone();
        bridge.register(
            "knowledge.list_shared",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = svc.clone();
                async move { handle_list_shared(&svc, &ctx) }
            })),
        );
    }
    {
        let svc = service.clone();
        bridge.register(
            "knowledge.group_broadcast",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = svc.clone();
                async move { handle_group_broadcast(&svc, &ctx).await }
            })),
        );
    }
    {
        let svc = service.clone();
        bridge.register(
            "knowledge.groups",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let svc = svc.clone();
                async move { handle_groups(&svc) }
            })),
        );
    }
    {
        let svc = service.clone();
        bridge.register(
            "knowledge.revoke",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = svc.clone();
                async move { handle_revoke(&svc, &ctx) }
            })),
        );
    }
    {
        let svc = service.clone();
        bridge.register(
            "knowledge.recall",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = svc.clone();
                async move { handle_recall(&svc, &ctx) }
            })),
        );
    }
    {
        let svc = service.clone();
        bridge.register(
            "knowledge.accept_shared",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = svc.clone();
                async move { handle_accept_shared(&svc, &ctx) }
            })),
        );
    }
    {
        let svc = service;
        bridge.register(
            "knowledge.autoshare_stats",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let svc = svc.clone();
                async move { handle_autoshare_stats(&svc).await }
            })),
        );
    }
}

/// Static descriptor list re-exported here so the
/// controller-runtime can build manifest entries from one
/// place. Mirrors the existing `metrics_capability_descriptors`
/// / `training_capability_descriptors` pattern.
pub fn knowledge_capability_descriptors() -> &'static [(&'static str, &'static str)] {
    sharing_group_descriptors()
}

async fn handle_share(svc: &KnowledgeService, ctx: &InvocationCtx) -> HandlerOutcome {
    let req: ShareRequest = match decode(ctx) {
        Ok(r) => r,
        Err(out) => return out,
    };
    match svc.share(&req).await {
        Ok(res) => ok_json(&res),
        Err(ShareError::InvalidArgs(m)) => invalid(&m),
        Err(e) => internal(&e),
    }
}

fn handle_list_shared(svc: &KnowledgeService, ctx: &InvocationCtx) -> HandlerOutcome {
    let filter: ListSharedFilter = match decode(ctx) {
        Ok(f) => f,
        Err(out) => return out,
    };
    match svc.list_shared(&filter) {
        Ok(rows) => ok_json(&rows),
        Err(ShareError::InvalidArgs(m)) => invalid(&m),
        Err(e) => internal(&e),
    }
}

#[derive(Debug, Deserialize, Default)]
struct BroadcastArgs {
    #[serde(default)]
    caller_agent: String,
    #[serde(default)]
    group: String,
    #[serde(default)]
    observation_ids: Vec<String>,
    #[serde(default)]
    message: Option<String>,
}

async fn handle_group_broadcast(svc: &KnowledgeService, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: BroadcastArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.caller_agent.trim().is_empty() {
        return invalid("caller_agent is required");
    }
    if args.group.trim().is_empty() {
        return invalid("group is required");
    }
    if args.observation_ids.is_empty() {
        return invalid("observation_ids must list at least one id");
    }
    match svc
        .group_broadcast(
            &args.caller_agent,
            &args.group,
            &args.observation_ids,
            args.message.as_deref(),
        )
        .await
    {
        Ok(res) => ok_json(&res),
        Err(ShareError::InvalidArgs(m)) => invalid(&m),
        Err(e) => internal(&e),
    }
}

fn handle_groups(svc: &KnowledgeService) -> HandlerOutcome {
    let groups = svc.groups();
    ok_json(&groups)
}

#[derive(Debug, Deserialize, Default)]
struct RevokeArgs {
    #[serde(default)]
    observation_ids: Vec<String>,
}

fn handle_revoke(svc: &KnowledgeService, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: RevokeArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.observation_ids.is_empty() {
        return invalid("observation_ids must list at least one id");
    }
    match svc.revoke(&args.observation_ids) {
        Ok(res) => ok_json(&res),
        Err(ShareError::InvalidArgs(m)) => invalid(&m),
        Err(e) => internal(&e),
    }
}

#[derive(Debug, Deserialize, Default)]
struct RecallArgs {
    #[serde(default)]
    source_agent: String,
    #[serde(default)]
    source_observation_ids: Vec<String>,
}

fn handle_recall(svc: &KnowledgeService, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: RecallArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.source_agent.trim().is_empty() {
        return invalid("source_agent is required");
    }
    if args.source_observation_ids.is_empty() {
        return invalid("source_observation_ids must list at least one id");
    }
    match svc.recall(&args.source_agent, &args.source_observation_ids) {
        Ok(res) => ok_json(&res),
        Err(ShareError::InvalidArgs(m)) => invalid(&m),
        Err(e) => internal(&e),
    }
}

/// RELIX-7.16 GAP 4: snapshot the AutoShareTask's lifetime
/// counters. When the task wasn't spawned (no groups
/// configured) the cap returns a zero-filled snapshot so the
/// surface is stable for operator tooling.
async fn handle_autoshare_stats(svc: &KnowledgeService) -> HandlerOutcome {
    let snap = match svc.autoshare_stats() {
        Some(s) => s.snapshot().await,
        None => crate::knowledge::autoshare::LifetimeCounters::default(),
    };
    ok_json(&snap)
}

/// RELIX-7.16 GAP 3: handle inbound `knowledge.accept_shared`
/// from a remote memory node. Args is the JSON-encoded
/// [`SignedSharePayload`]. The receiver runs full signature
/// verification + the local `TrustChecker` before any row
/// touches SQLite. On success, returns the deterministic
/// receiver-side copy id so callers can correlate.
fn handle_accept_shared(svc: &KnowledgeService, ctx: &InvocationCtx) -> HandlerOutcome {
    if ctx.args.is_empty() {
        return invalid("knowledge.accept_shared: payload is required");
    }
    let payload: SignedSharePayload = match serde_json::from_slice(&ctx.args) {
        Ok(p) => p,
        Err(e) => return invalid(&format!("decode payload: {e}")),
    };
    let source_id = payload.record.id.clone();
    let target_agent = payload.target_agent.clone();
    match svc.accept_shared(payload) {
        Ok(()) => {
            let copy_id = crate::knowledge::service::mint_copy_id(&source_id, &target_agent);
            let body = serde_json::json!({
                "copy_id": copy_id,
                "target_agent": target_agent,
            });
            ok_json(&body)
        }
        Err(ShareError::InvalidArgs(m)) => invalid(&m),
        Err(ShareError::Rejected(reason)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!("knowledge.accept_shared: rejected: {reason:?}"),
            retry_hint: 0,
            retry_after: None,
        }),
        Err(e) => internal(&e),
    }
}

// ── shared helpers ────────────────────────────────────────

fn decode<T: serde::de::DeserializeOwned + Default>(
    ctx: &InvocationCtx,
) -> Result<T, HandlerOutcome> {
    if ctx.args.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(&ctx.args).map_err(|e| invalid(&format!("decode args: {e}")))
}

fn ok_json<T: serde::Serialize>(value: &T) -> HandlerOutcome {
    match serde_json::to_vec(value) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("knowledge: encode response: {e}"),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

fn invalid(msg: &str) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause: msg.to_string(),
        retry_hint: 0,
        retry_after: None,
    })
}

fn internal<E: std::fmt::Display>(e: &E) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause: format!("knowledge: {e}"),
        retry_hint: 0,
        retry_after: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::config::{KnowledgeConfig, SharingGroup};
    use crate::nodes::memory::schema::{LayeredMemoryStore, MemoryLayer, MemoryRecord};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use relix_core::policy::PolicyEngine;
    use tempfile::TempDir;

    fn fresh_bridge() -> (DispatchBridge, TempDir) {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::permissive();
        let bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        (bridge, dir)
    }

    fn observation(id: &str, owner: &str, shareable: bool) -> MemoryRecord {
        let mut r = MemoryRecord::new_raw(id, "fact", owner);
        r.layer = MemoryLayer::Observation;
        r.shareable = shareable;
        r
    }

    #[tokio::test]
    async fn caps_register_without_panic() {
        let (mut bridge, _dir) = fresh_bridge();
        let store = Arc::new(LayeredMemoryStore::in_memory().unwrap());
        let cfg = KnowledgeConfig {
            groups: vec![SharingGroup {
                name: "g".into(),
                members: vec!["alice".into(), "bob".into()],
                auto_share_layers: vec![],
                min_quality_score: None,
                member_nodes: Vec::new(),
            }],
            auto_share_interval_secs: 60,
            max_observations_per_agent: None,
            quality_scorer: Default::default(),
            auto_share_per_tick_budget: None,
            auto_share_per_agent_limit: None,
        };
        let svc = Arc::new(KnowledgeService::new(store.clone(), &cfg).unwrap());
        store.insert(&observation("a1", "alice", true)).unwrap();
        register(&mut bridge, svc);
        let _snapshot = bridge.capability_stats_snapshot();
    }

    #[test]
    fn descriptors_cover_every_capability() {
        let methods: Vec<&str> = knowledge_capability_descriptors()
            .iter()
            .map(|(m, _)| *m)
            .collect();
        for expected in [
            "knowledge.share",
            "knowledge.list_shared",
            "knowledge.group_broadcast",
            "knowledge.groups",
            "knowledge.revoke",
        ] {
            assert!(
                methods.contains(&expected),
                "missing descriptor: {expected}"
            );
        }
    }

    fn ctx_with(args: &[u8]) -> InvocationCtx {
        use relix_core::identity::VerifiedIdentity;
        use relix_core::types::{NodeId, RequestId, TraceId};
        InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"caller"),
                name: "alice".into(),
                org_id: NodeId::from_pubkey(b"org"),
                groups: vec!["operators".into()],
                role: "agent".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: args.to_vec(),
            tenant_id: None,
        }
    }

    #[tokio::test]
    async fn handle_share_returns_invalid_args_on_empty_targets() {
        let store = Arc::new(LayeredMemoryStore::in_memory().unwrap());
        let cfg = KnowledgeConfig {
            groups: vec![SharingGroup {
                name: "g".into(),
                members: vec!["alice".into(), "bob".into()],
                auto_share_layers: vec![],
                min_quality_score: None,
                member_nodes: Vec::new(),
            }],
            auto_share_interval_secs: 60,
            max_observations_per_agent: None,
            quality_scorer: Default::default(),
            auto_share_per_tick_budget: None,
            auto_share_per_agent_limit: None,
        };
        let svc = KnowledgeService::new(store, &cfg).unwrap();
        let ctx =
            ctx_with(br#"{"source_agent":"alice","target_agents":[],"observation_ids":["a"]}"#);
        match handle_share(&svc, &ctx).await {
            HandlerOutcome::Err(env) => assert_eq!(env.kind, error_kinds::INVALID_ARGS),
            _ => panic!("expected INVALID_ARGS"),
        }
    }

    #[test]
    fn handle_groups_returns_json_list_of_configured_groups() {
        let store = Arc::new(LayeredMemoryStore::in_memory().unwrap());
        let cfg = KnowledgeConfig {
            groups: vec![SharingGroup {
                name: "g".into(),
                members: vec!["alice".into(), "bob".into()],
                auto_share_layers: vec!["observation".into()],
                min_quality_score: Some(0.7),
                member_nodes: Vec::new(),
            }],
            auto_share_interval_secs: 60,
            max_observations_per_agent: None,
            quality_scorer: Default::default(),
            auto_share_per_tick_budget: None,
            auto_share_per_agent_limit: None,
        };
        let svc = KnowledgeService::new(store, &cfg).unwrap();
        let HandlerOutcome::Ok(body) = handle_groups(&svc) else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.is_array());
        assert_eq!(v.as_array().unwrap()[0]["name"], "g");
    }
}
