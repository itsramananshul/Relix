//! `/v1/provenance/{trace_id}` and `/v1/provenance/diff` —
//! operator surface for the snapshot registry.
//!
//! - `GET /v1/provenance/{trace_id}` returns the
//!   [`ProvenanceSnapshot`] recorded for one trace.
//! - `GET /v1/provenance/diff?a=&b=` returns the
//!   [`ProvenanceDiff`] between two traces — the field-level
//!   change report operators read when a regression lands.

use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use relix_runtime::observability::{
    ObservabilityContext, ProvenanceDiff, ProvenanceError, ProvenanceSnapshot,
};
use serde::{Deserialize, Serialize};

use crate::config::AppState;

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize)]
pub struct DiffQuery {
    pub a: String,
    pub b: String,
}

type HandlerError = (StatusCode, Json<ApiError>);

pub(crate) fn show_logic(
    ctx: &ObservabilityContext,
    trace_id: &str,
) -> Result<ProvenanceSnapshot, HandlerError> {
    match ctx.provenance.get(trace_id).map_err(prov_err)? {
        Some(s) => Ok(s),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: format!("no provenance snapshot for trace {trace_id}"),
            }),
        )),
    }
}

pub(crate) fn diff_logic(
    ctx: &ObservabilityContext,
    q: &DiffQuery,
) -> Result<ProvenanceDiff, HandlerError> {
    ctx.provenance.diff(&q.a, &q.b).map_err(prov_err)
}

fn prov_err(e: ProvenanceError) -> HandlerError {
    match e {
        ProvenanceError::NotFound(_) => (
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: e.to_string(),
            }),
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: format!("provenance: {e}"),
            }),
        ),
    }
}

pub async fn show(
    State(state): State<AppState>,
    AxumPath(trace_id): AxumPath<String>,
) -> Result<Json<ProvenanceSnapshot>, HandlerError> {
    show_logic(&state.observability, &trace_id).map(Json)
}

pub async fn diff(
    State(state): State<AppState>,
    Query(q): Query<DiffQuery>,
) -> Result<Json<ProvenanceDiff>, HandlerError> {
    diff_logic(&state.observability, &q).map(Json)
}

#[derive(Debug, Deserialize)]
pub struct RecentQuery {
    #[serde(default = "default_recent_limit")]
    pub limit: usize,
}

fn default_recent_limit() -> usize {
    200
}

#[derive(Debug, Serialize)]
pub struct RecentResponse {
    pub snapshots: Vec<ProvenanceSnapshot>,
    pub count: usize,
}

/// GAP 13 — `GET /v1/provenance/recent?limit=200` returns the
/// newest N snapshots. Powers the `relix provenance history`
/// and `relix provenance audit` CLI subcommands.
pub async fn recent(
    State(state): State<AppState>,
    Query(q): Query<RecentQuery>,
) -> Result<Json<RecentResponse>, HandlerError> {
    let limit = q.limit.clamp(1, 1000);
    match state.observability.provenance.list_recent(limit) {
        Ok(snapshots) => {
            let count = snapshots.len();
            Ok(Json(RecentResponse { snapshots, count }))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: format!("provenance: {e}"),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_runtime::observability::ProvenanceSnapshot;
    use std::collections::BTreeMap;

    fn make_ctx() -> ObservabilityContext {
        ObservabilityContext::in_memory()
    }

    fn snap(trace: &str, model: &str, policy: &str) -> ProvenanceSnapshot {
        ProvenanceSnapshot {
            trace_id: trace.into(),
            timestamp_unix: 0,
            model_id: model.into(),
            policy_version: policy.into(),
            skill_versions: BTreeMap::new(),
            tool_versions: BTreeMap::new(),
        }
    }

    #[test]
    fn show_returns_recorded_snapshot() {
        let ctx = make_ctx();
        ctx.provenance.record(&snap("t1", "m1", "p1")).unwrap();
        let s = show_logic(&ctx, "t1").unwrap();
        assert_eq!(s.trace_id, "t1");
        assert_eq!(s.model_id, "m1");
    }

    #[test]
    fn show_404_when_missing() {
        let ctx = make_ctx();
        let err = show_logic(&ctx, "missing").unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn diff_endpoint_returns_changes() {
        let ctx = make_ctx();
        ctx.provenance.record(&snap("a", "m1", "p1")).unwrap();
        ctx.provenance.record(&snap("b", "m2", "p1")).unwrap();
        let d = diff_logic(
            &ctx,
            &DiffQuery {
                a: "a".into(),
                b: "b".into(),
            },
        )
        .unwrap();
        assert_eq!(d.changes.len(), 1);
    }

    #[test]
    fn diff_endpoint_404_when_either_trace_missing() {
        let ctx = make_ctx();
        ctx.provenance.record(&snap("a", "m1", "p1")).unwrap();
        let err = diff_logic(
            &ctx,
            &DiffQuery {
                a: "a".into(),
                b: "nope".into(),
            },
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }
}
