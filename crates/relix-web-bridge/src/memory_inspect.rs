//! `/v1/memory/records/*` and `/v1/memory/stats` — operator
//! inspector surface over the four-layer
//! [`LayeredMemoryStore`].
//!
//! The bridge reads the store directly (the store handle lives
//! on `AppState::layered_memory`) rather than calling a memory
//! peer over libp2p. The reasoning: the inspector is purely
//! operator-facing tooling — read-mostly, low traffic, must
//! work even when the memory controller is down. Direct SQLite
//! access (via WAL mode for concurrency with the memory
//! controller's writer) is the simplest reliable shape.
//!
//! When the operator did not wire `[bridge] memory_db_path`,
//! every handler returns `503 Service Unavailable` with a
//! body that names the missing config key — the dashboard's
//! Memory tab can render an explanatory call-to-action.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use relix_runtime::nodes::memory::schema::{LayeredMemoryStore, MemoryLayer, MemoryRecord};
use serde::{Deserialize, Serialize};

use crate::config::AppState;

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

/// JSON-friendly view of a [`MemoryRecord`]. Drops the
/// embedding vector itself (it's noisy and large) but reports
/// whether one is present.
#[derive(Debug, Clone, Serialize)]
pub struct RecordJson {
    pub id: String,
    pub layer: String,
    pub text: String,
    pub source: String,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub valid_from: i64,
    pub valid_to: Option<i64>,
    pub observed_at: i64,
    pub has_embedding: bool,
    pub embedding_dim: usize,
}

impl From<MemoryRecord> for RecordJson {
    fn from(r: MemoryRecord) -> Self {
        let (has_embedding, embedding_dim) = match &r.embedding {
            Some(v) => (true, v.len()),
            None => (false, 0),
        };
        Self {
            id: r.id,
            layer: r.layer.as_str().to_string(),
            text: r.text,
            source: r.source,
            tags: r.tags,
            created_at: r.created_at,
            valid_from: r.valid_from,
            valid_to: r.valid_to,
            observed_at: r.observed_at,
            has_embedding,
            embedding_dim,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub layer: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

fn default_limit() -> usize {
    50
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub records: Vec<RecordJson>,
    pub count: usize,
}

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub counts_per_layer: serde_json::Value,
    pub pending_embeddings_per_layer: serde_json::Value,
    pub most_recent_per_layer: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct InvalidateResponse {
    pub id: String,
    pub valid_to: i64,
}

type HandlerError = (StatusCode, Json<ApiError>);

fn store_or_503(
    store: &Option<Arc<LayeredMemoryStore>>,
) -> Result<Arc<LayeredMemoryStore>, HandlerError> {
    store.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError {
                error:
                    "layered memory store not configured; set [bridge] memory_db_path in bridge.toml"
                        .into(),
            }),
        )
    })
}

fn parse_layer(s: &str) -> Result<MemoryLayer, HandlerError> {
    MemoryLayer::parse(s).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: format!(
                    "unknown layer `{s}`; expected one of raw, semantic, observation, model"
                ),
            }),
        )
    })
}

fn db_err(e: relix_runtime::nodes::memory::schema::LayeredMemoryError) -> HandlerError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError {
            error: format!("layered memory: {e}"),
        }),
    )
}

// ── Logic functions ────────────────────────────────────────
//
// Pure-data signatures so tests can exercise them with just
// the store; the axum handlers below are thin wrappers that
// extract the store from `AppState`.

pub(crate) fn list_logic(
    store: &Option<Arc<LayeredMemoryStore>>,
    q: &ListQuery,
) -> Result<ListResponse, HandlerError> {
    let store = store_or_503(store)?;
    let layer = q.layer.as_deref().map(parse_layer).transpose()?;
    let rows = store
        .list(layer, q.source.as_deref(), q.limit, q.offset)
        .map_err(db_err)?;
    let records: Vec<RecordJson> = rows.into_iter().map(RecordJson::from).collect();
    Ok(ListResponse {
        count: records.len(),
        records,
    })
}

pub(crate) fn show_logic(
    store: &Option<Arc<LayeredMemoryStore>>,
    id: &str,
) -> Result<RecordJson, HandlerError> {
    let store = store_or_503(store)?;
    match store.get(id).map_err(db_err)? {
        Some(r) => Ok(RecordJson::from(r)),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: format!("memory record {id} not found"),
            }),
        )),
    }
}

pub(crate) fn search_logic(
    store: &Option<Arc<LayeredMemoryStore>>,
    req: &SearchRequest,
) -> Result<ListResponse, HandlerError> {
    let store = store_or_503(store)?;
    if req.query.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "query must be non-empty".into(),
            }),
        ));
    }
    let rows = store.text_search(&req.query, req.limit).map_err(db_err)?;
    let records: Vec<RecordJson> = rows.into_iter().map(RecordJson::from).collect();
    Ok(ListResponse {
        count: records.len(),
        records,
    })
}

pub(crate) fn invalidate_logic(
    store: &Option<Arc<LayeredMemoryStore>>,
    id: &str,
) -> Result<InvalidateResponse, HandlerError> {
    let store = store_or_503(store)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if store.get(id).map_err(db_err)?.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: format!("memory record {id} not found"),
            }),
        ));
    }
    store.invalidate(id, now).map_err(db_err)?;
    Ok(InvalidateResponse {
        id: id.to_string(),
        valid_to: now,
    })
}

pub(crate) fn stats_logic(
    store: &Option<Arc<LayeredMemoryStore>>,
) -> Result<StatsResponse, HandlerError> {
    let store = store_or_503(store)?;
    let mut counts = serde_json::Map::new();
    let mut most_recent = serde_json::Map::new();
    for layer in [
        MemoryLayer::Raw,
        MemoryLayer::Semantic,
        MemoryLayer::Observation,
        MemoryLayer::Model,
    ] {
        let rows = store.list(Some(layer), None, 1_000, 0).map_err(db_err)?;
        counts.insert(
            layer.as_str().to_string(),
            serde_json::Value::Number(rows.len().into()),
        );
        let latest = rows.into_iter().next().map(RecordJson::from);
        most_recent.insert(
            layer.as_str().to_string(),
            match latest {
                Some(r) => serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
                None => serde_json::Value::Null,
            },
        );
    }
    let pending = store.count_pending_embeddings().map_err(db_err)?;
    let mut pending_json = serde_json::Map::new();
    for (layer, count) in pending {
        pending_json.insert(
            layer.as_str().to_string(),
            serde_json::Value::Number(count.into()),
        );
    }
    Ok(StatsResponse {
        counts_per_layer: serde_json::Value::Object(counts),
        pending_embeddings_per_layer: serde_json::Value::Object(pending_json),
        most_recent_per_layer: serde_json::Value::Object(most_recent),
    })
}

// ── Axum handler wrappers ──────────────────────────────────

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ListResponse>, HandlerError> {
    list_logic(&state.layered_memory, &q).map(Json)
}

pub async fn show(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<RecordJson>, HandlerError> {
    show_logic(&state.layered_memory, &id).map(Json)
}

pub async fn search(
    State(state): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<ListResponse>, HandlerError> {
    search_logic(&state.layered_memory, &req).map(Json)
}

pub async fn invalidate(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<InvalidateResponse>, HandlerError> {
    invalidate_logic(&state.layered_memory, &id).map(Json)
}

pub async fn stats(State(state): State<AppState>) -> Result<Json<StatsResponse>, HandlerError> {
    stats_logic(&state.layered_memory).map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_runtime::nodes::memory::schema::MemoryRecord;

    fn make_store_with_rows() -> Arc<LayeredMemoryStore> {
        let store = LayeredMemoryStore::in_memory().unwrap();
        let mut a = MemoryRecord::new_raw("a", "deploy staging", "s1");
        a.embedding = Some(vec![0.1, 0.2]);
        store.insert(&a).unwrap();
        let b = MemoryRecord::new_raw("b", "weather report", "s1");
        store.insert(&b).unwrap();
        let mut c = MemoryRecord::new_raw("c-obs", "the user prefers Helvetica", "s1");
        c.layer = MemoryLayer::Observation;
        store.insert(&c).unwrap();
        Arc::new(store)
    }

    #[test]
    fn list_returns_records_json_with_layer_filter() {
        let store = Some(make_store_with_rows());
        let all = list_logic(
            &store,
            &ListQuery {
                layer: None,
                source: None,
                limit: 10,
                offset: 0,
            },
        )
        .unwrap();
        assert!(all.count >= 3);
        let raws = list_logic(
            &store,
            &ListQuery {
                layer: Some("raw".into()),
                source: None,
                limit: 10,
                offset: 0,
            },
        )
        .unwrap();
        for r in &raws.records {
            assert_eq!(r.layer, "raw");
        }
    }

    #[test]
    fn show_returns_full_record_and_404_on_miss() {
        let store = Some(make_store_with_rows());
        let r = show_logic(&store, "a").unwrap();
        assert_eq!(r.id, "a");
        assert!(r.has_embedding);
        assert_eq!(r.embedding_dim, 2);
        let err = show_logic(&store, "nope").unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn invalidate_sets_valid_to_and_404s_on_missing() {
        let store_arc = make_store_with_rows();
        let store = Some(store_arc.clone());
        let resp = invalidate_logic(&store, "a").unwrap();
        assert!(resp.valid_to > 0);
        let got = store_arc.get("a").unwrap().unwrap();
        assert_eq!(got.valid_to, Some(resp.valid_to));
        let err = invalidate_logic(&store, "missing").unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn stats_returns_counts_per_layer() {
        let store = Some(make_store_with_rows());
        let s = stats_logic(&store).unwrap();
        let raw_count = s.counts_per_layer.get("raw").unwrap().as_i64().unwrap();
        assert!(raw_count >= 1);
        // The observation insert above has no embedding ⇒ at
        // least one pending observation.
        let pending_obs = s
            .pending_embeddings_per_layer
            .get("observation")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        assert!(pending_obs >= 1, "expected pending observation, got {s:?}");
    }

    #[test]
    fn endpoints_return_503_when_store_unconfigured() {
        let store: Option<Arc<LayeredMemoryStore>> = None;
        let err = list_logic(
            &store,
            &ListQuery {
                layer: None,
                source: None,
                limit: 10,
                offset: 0,
            },
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(err.1.error.contains("memory_db_path"));
        let err = stats_logic(&store).unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn search_rejects_empty_query_and_returns_text_hits() {
        let store = Some(make_store_with_rows());
        let err = search_logic(
            &store,
            &SearchRequest {
                query: "  ".into(),
                limit: 10,
            },
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        let hits = search_logic(
            &store,
            &SearchRequest {
                query: "Helvetica".into(),
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(hits.count, 1);
        assert_eq!(hits.records[0].id, "c-obs");
    }

    #[test]
    fn list_rejects_unknown_layer() {
        let store = Some(make_store_with_rows());
        let err = list_logic(
            &store,
            &ListQuery {
                layer: Some("garbage".into()),
                source: None,
                limit: 10,
                offset: 0,
            },
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.error.contains("unknown layer"));
    }
}
