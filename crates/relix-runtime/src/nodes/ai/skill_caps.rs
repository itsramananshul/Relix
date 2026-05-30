//! GAP 4 — coordinator capabilities for the SkillStore.
//!
//! Six JSON-wire caps register on the same DispatchBridge that
//! holds the coordinator's `task.*` caps. They share the AI
//! controller's [`SkillStore`] handle (same SQLite file; the
//! WAL'd connection is safe for multi-reader / single-writer).
//!
//! - `memory.skill_search`    { query, limit?, agent?, min_confidence? }
//! - `memory.skill_get`       { id } → full StoredSkill + version history
//! - `memory.skill_store`     { name, description, steps, tags, source_agent }
//! - `memory.skill_update`    { id, steps?, description?, tags?, status? }
//! - `memory.skill_deprecate` { id, reason? }
//! - `memory.skill_stats`     {} → SkillStats

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use crate::nodes::ai::skill_store::{
    SkillFilter, SkillStatus, SkillStep, SkillStore, SkillVersionRow, StoredSkill, mint_skill_id,
};

const DEFAULT_SEARCH_LIMIT: usize = 20;
const MAX_SEARCH_LIMIT: usize = 200;

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct SearchArgs {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub min_confidence: Option<f32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SkillSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source_agent: String,
    pub confidence: f32,
    pub usage_count: i64,
    pub version: i64,
    pub tags: Vec<String>,
    pub status: SkillStatus,
}

impl From<&StoredSkill> for SkillSummary {
    fn from(s: &StoredSkill) -> Self {
        SkillSummary {
            id: s.id.clone(),
            name: s.name.clone(),
            description: s.description.clone(),
            source_agent: s.source_agent.clone(),
            confidence: s.confidence,
            usage_count: s.usage_count,
            version: s.version,
            tags: s.tags.clone(),
            status: s.status,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SearchResponse {
    pub results: Vec<SkillSummary>,
    pub count: usize,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct GetArgs {
    #[serde(default)]
    pub id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct GetResponse {
    pub skill: StoredSkill,
    pub versions: Vec<SkillVersionRow>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct StoreArgs {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub steps: Vec<SkillStep>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub source_agent: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct UpdateArgs {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub steps: Option<Vec<SkillStep>>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub change_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct DeprecateArgs {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Register the six caps on `bridge` against the shared
/// `store`. Idempotent at the bridge level — the bridge rejects
/// duplicate method registrations.
pub fn register(bridge: &mut DispatchBridge, store: Arc<SkillStore>) {
    {
        let s = store.clone();
        bridge.register(
            "memory.skill_search",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_search(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "memory.skill_get",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_get(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "memory.skill_store",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_store(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "memory.skill_update",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_update(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "memory.skill_deprecate",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_deprecate(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "memory.skill_stats",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_stats(&s, &ctx) }
            })),
        );
    }
}

pub fn handle_search(store: &SkillStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: SearchArgs = if ctx.args.is_empty() {
        SearchArgs::default()
    } else {
        match serde_json::from_slice(&ctx.args) {
            Ok(a) => a,
            Err(e) => return invalid_args(format!("memory.skill_search: decode args: {e}")),
        }
    };
    let limit = args
        .limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);
    let results = if args.query.trim().is_empty() {
        let filter = SkillFilter {
            agent: args.agent.clone(),
            min_confidence: args.min_confidence,
            status: Some(SkillStatus::Active),
            tag: None,
            limit: Some(limit),
        };
        let r = if store.tenant_isolation_enabled() {
            store.list_for_tenant(&filter, ctx.tenant_id.as_deref())
        } else {
            store.list(&filter)
        };
        match r {
            Ok(v) => v,
            Err(e) => return internal(format!("memory.skill_search: list: {e}")),
        }
    } else {
        let r = if store.tenant_isolation_enabled() {
            store.search_for_tenant(
                &args.query,
                limit,
                args.min_confidence,
                args.agent.as_deref(),
                ctx.tenant_id.as_deref(),
            )
        } else {
            store.search(
                &args.query,
                limit,
                args.min_confidence,
                args.agent.as_deref(),
            )
        };
        match r {
            Ok(v) => v,
            Err(e) => return internal(format!("memory.skill_search: search: {e}")),
        }
    };
    let summaries: Vec<SkillSummary> = results.iter().map(SkillSummary::from).collect();
    let count = summaries.len();
    let body = SearchResponse {
        results: summaries,
        count,
    };
    encode(&body, "memory.skill_search")
}

pub fn handle_get(store: &SkillStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: GetArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.skill_get: decode args: {e}")),
    };
    if args.id.trim().is_empty() {
        return invalid_args("memory.skill_get: id required".into());
    }
    let lookup = if store.tenant_isolation_enabled() {
        store.get_for_tenant(&args.id, ctx.tenant_id.as_deref())
    } else {
        store.get(&args.id)
    };
    let skill = match lookup {
        Ok(Some(s)) => s,
        Ok(None) => {
            return invalid_args(format!("memory.skill_get: no skill `{}`", args.id));
        }
        Err(e) => return internal(format!("memory.skill_get: store: {e}")),
    };
    let versions = match store.versions(&args.id) {
        Ok(v) => v,
        Err(e) => return internal(format!("memory.skill_get: versions: {e}")),
    };
    encode(&GetResponse { skill, versions }, "memory.skill_get")
}

pub fn handle_store(store: &SkillStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: StoreArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.skill_store: decode args: {e}")),
    };
    if args.name.trim().is_empty() || args.description.trim().is_empty() {
        return invalid_args("memory.skill_store: name and description are required".into());
    }
    if args.steps.is_empty() {
        return invalid_args("memory.skill_store: steps must be non-empty".into());
    }
    if args.source_agent.trim().is_empty() {
        return invalid_args("memory.skill_store: source_agent required".into());
    }
    let now = unix_millis();
    let id = mint_skill_id(&args.source_agent, &args.name);
    let skill = StoredSkill {
        id: id.clone(),
        name: args.name,
        description: args.description,
        source_agent: args.source_agent,
        version: 1,
        confidence: 0.5,
        usage_count: 0,
        last_used_ms: None,
        created_at_ms: now,
        updated_at_ms: now,
        tags: args.tags,
        steps: args.steps,
        example_inputs: Vec::new(),
        example_outputs: Vec::new(),
        status: SkillStatus::Active,
        tenant_id: ctx.tenant_id.clone(),
    };
    if let Err(e) = store.insert(&skill) {
        return internal(format!("memory.skill_store: insert: {e}"));
    }
    encode(&skill, "memory.skill_store")
}

pub fn handle_update(store: &SkillStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: UpdateArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.skill_update: decode args: {e}")),
    };
    if args.id.trim().is_empty() {
        return invalid_args("memory.skill_update: id required".into());
    }
    let status = match args.status.as_deref() {
        Some(s) => match SkillStatus::parse(s) {
            Some(st) => Some(st),
            None => return invalid_args(format!("memory.skill_update: unknown status `{s}`")),
        },
        None => None,
    };
    if let Err(e) = store.update(
        &args.id,
        args.description.as_deref(),
        args.tags.as_deref(),
        args.steps.as_deref(),
        status,
        args.change_reason.as_deref(),
    ) {
        return match e {
            crate::nodes::ai::skill_store::SkillStoreError::NotFound(_) => {
                invalid_args(format!("memory.skill_update: {e}"))
            }
            other => internal(format!("memory.skill_update: {other}")),
        };
    }
    let skill = match store.get(&args.id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return internal("memory.skill_update: skill disappeared during update".into());
        }
        Err(e) => return internal(format!("memory.skill_update: get: {e}")),
    };
    encode(&skill, "memory.skill_update")
}

pub fn handle_deprecate(store: &SkillStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: DeprecateArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.skill_deprecate: decode args: {e}")),
    };
    if args.id.trim().is_empty() {
        return invalid_args("memory.skill_deprecate: id required".into());
    }
    if let Err(e) = store.update(
        &args.id,
        None,
        None,
        None,
        Some(SkillStatus::Deprecated),
        args.reason.as_deref(),
    ) {
        return match e {
            crate::nodes::ai::skill_store::SkillStoreError::NotFound(_) => {
                invalid_args(format!("memory.skill_deprecate: {e}"))
            }
            other => internal(format!("memory.skill_deprecate: {other}")),
        };
    }
    let skill = match store.get(&args.id) {
        Ok(Some(s)) => s,
        Ok(None) => return internal("memory.skill_deprecate: skill disappeared".into()),
        Err(e) => return internal(format!("memory.skill_deprecate: get: {e}")),
    };
    encode(&skill, "memory.skill_deprecate")
}

pub fn handle_stats(store: &SkillStore, _ctx: &InvocationCtx) -> HandlerOutcome {
    let stats = match store.stats() {
        Ok(s) => s,
        Err(e) => return internal(format!("memory.skill_stats: store: {e}")),
    };
    encode(&stats, "memory.skill_stats")
}

fn encode<T: serde::Serialize>(body: &T, cap: &str) -> HandlerOutcome {
    match serde_json::to_vec(body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("{cap}: encode: {e}")),
    }
}

fn invalid_args(cause: String) -> HandlerOutcome {
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

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn ctx_empty() -> InvocationCtx {
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
            args: vec![],
            tenant_id: None,
        }
    }

    fn store() -> SkillStore {
        SkillStore::open_in_memory().unwrap()
    }

    fn seed(store: &SkillStore, name: &str, agent: &str, conf: f32, usage: i64) -> String {
        let id = mint_skill_id(agent, name);
        let s = StoredSkill {
            id: id.clone(),
            name: name.to_string(),
            description: format!("desc for {name}"),
            source_agent: agent.to_string(),
            version: 1,
            confidence: conf,
            usage_count: usage,
            last_used_ms: None,
            created_at_ms: 1,
            updated_at_ms: 1,
            tags: vec!["tagA".into(), "tagB".into()],
            steps: vec![
                SkillStep {
                    step: "step one".into(),
                    tool: None,
                    prompt: None,
                },
                SkillStep {
                    step: "step two".into(),
                    tool: None,
                    prompt: None,
                },
            ],
            example_inputs: vec![],
            example_outputs: vec![],
            status: SkillStatus::Active,
            tenant_id: None,
        };
        store.insert(&s).unwrap();
        id
    }

    #[test]
    fn search_with_empty_query_returns_active_list() {
        let s = store();
        seed(&s, "alpha", "agent.x", 0.6, 5);
        seed(&s, "beta", "agent.x", 0.9, 10);
        let out = handle_search(&s, &ctx_for(&SearchArgs::default()));
        let body = match out {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        let v: SearchResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.count, 2);
    }

    #[test]
    fn search_with_query_matches_substrings() {
        let s = store();
        seed(&s, "deploy_to_prod", "agent.x", 0.8, 3);
        seed(&s, "fetch_data", "agent.x", 0.8, 3);
        let args = SearchArgs {
            query: "deploy".into(),
            ..Default::default()
        };
        let out = handle_search(&s, &ctx_for(&args));
        let body = match out {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        let v: SearchResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.count, 1);
        assert_eq!(v.results[0].name, "deploy_to_prod");
    }

    #[test]
    fn search_filters_by_min_confidence_and_agent() {
        let s = store();
        seed(&s, "alpha", "agent.x", 0.4, 1);
        seed(&s, "beta", "agent.x", 0.9, 1);
        seed(&s, "gamma", "agent.y", 0.9, 1);
        let args = SearchArgs {
            query: String::new(),
            limit: Some(50),
            agent: Some("agent.x".into()),
            min_confidence: Some(0.7),
        };
        let out = handle_search(&s, &ctx_for(&args));
        let body = match out {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        let v: SearchResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.count, 1);
        assert_eq!(v.results[0].name, "beta");
    }

    #[test]
    fn get_returns_skill_plus_versions() {
        let s = store();
        let id = seed(&s, "alpha", "agent.x", 0.5, 0);
        let args = GetArgs { id: id.clone() };
        let out = handle_get(&s, &ctx_for(&args));
        let body = match out {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        let v: GetResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.skill.id, id);
        assert_eq!(v.versions.len(), 1);
        assert_eq!(v.versions[0].version, 1);
    }

    #[test]
    fn get_missing_id_returns_invalid_args() {
        let s = store();
        let out = handle_get(&s, &ctx_for(&GetArgs::default()));
        let HandlerOutcome::Err(env) = out else {
            panic!("expected err");
        };
        assert_eq!(env.kind, error_kinds::INVALID_ARGS);
    }

    #[test]
    fn store_writes_and_returns_skill() {
        let s = store();
        let args = StoreArgs {
            name: "deploy_to_staging".into(),
            description: "Deploys current branch to staging.".into(),
            steps: vec![
                SkillStep {
                    step: "build".into(),
                    tool: None,
                    prompt: None,
                },
                SkillStep {
                    step: "push".into(),
                    tool: None,
                    prompt: None,
                },
            ],
            tags: vec!["deploy".into(), "staging".into()],
            source_agent: "agent.alpha".into(),
        };
        let out = handle_store(&s, &ctx_for(&args));
        let body = match out {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        let v: StoredSkill = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.name, "deploy_to_staging");
        // Confirm the SQL row is real, not just the response.
        let row = s.get(&v.id).unwrap().unwrap();
        assert_eq!(row.name, "deploy_to_staging");
    }

    #[test]
    fn store_rejects_empty_required_fields() {
        let s = store();
        let args = StoreArgs::default();
        let out = handle_store(&s, &ctx_for(&args));
        let HandlerOutcome::Err(env) = out else {
            panic!("expected err");
        };
        assert_eq!(env.kind, error_kinds::INVALID_ARGS);
    }

    #[test]
    fn update_changes_description_and_returns_updated_skill() {
        let s = store();
        let id = seed(&s, "alpha", "agent.x", 0.5, 0);
        let args = UpdateArgs {
            id: id.clone(),
            description: Some("brand new description".into()),
            ..Default::default()
        };
        let out = handle_update(&s, &ctx_for(&args));
        let body = match out {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        let v: StoredSkill = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.description, "brand new description");
    }

    #[test]
    fn update_with_steps_creates_version() {
        let s = store();
        let id = seed(&s, "alpha", "agent.x", 0.5, 0);
        let args = UpdateArgs {
            id: id.clone(),
            steps: Some(vec![SkillStep {
                step: "refined".into(),
                tool: None,
                prompt: None,
            }]),
            change_reason: Some("operator fix".into()),
            ..Default::default()
        };
        let out = handle_update(&s, &ctx_for(&args));
        let HandlerOutcome::Ok(_) = out else {
            panic!("expected ok");
        };
        let versions = s.versions(&id).unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[1].change_reason.as_deref(), Some("operator fix"));
    }

    #[test]
    fn update_with_unknown_status_returns_invalid_args() {
        let s = store();
        let id = seed(&s, "alpha", "agent.x", 0.5, 0);
        let args = UpdateArgs {
            id,
            status: Some("frozen".into()),
            ..Default::default()
        };
        let out = handle_update(&s, &ctx_for(&args));
        let HandlerOutcome::Err(env) = out else {
            panic!("expected err");
        };
        assert_eq!(env.kind, error_kinds::INVALID_ARGS);
    }

    #[test]
    fn deprecate_flips_status_to_deprecated() {
        let s = store();
        let id = seed(&s, "alpha", "agent.x", 0.5, 0);
        let args = DeprecateArgs {
            id: id.clone(),
            reason: Some("outdated".into()),
        };
        let out = handle_deprecate(&s, &ctx_for(&args));
        let HandlerOutcome::Ok(_) = out else {
            panic!("expected ok");
        };
        let row = s.get(&id).unwrap().unwrap();
        assert_eq!(row.status, SkillStatus::Deprecated);
    }

    #[test]
    fn stats_returns_aggregate_counts() {
        let s = store();
        seed(&s, "alpha", "agent.x", 0.6, 5);
        seed(&s, "beta", "agent.x", 0.9, 10);
        let out = handle_stats(&s, &ctx_empty());
        let body = match out {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        let v: crate::nodes::ai::skill_store::SkillStats = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.total_skills, 2);
        assert_eq!(v.active_skills, 2);
    }
}
