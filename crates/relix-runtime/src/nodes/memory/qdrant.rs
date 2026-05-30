//! Qdrant HTTP client used by the memory node's vector-search
//! path.
//!
//! Direct reqwest client against Qdrant's REST surface. We
//! deliberately do NOT pull the upstream `qdrant-client` crate
//! — it carries a tonic / prost transitive that would balloon
//! the dependency closure for a few endpoints' worth of JSON.
//!
//! ## Endpoints
//!
//! - `PUT  /collections/{name}` — create-or-confirm a
//!   collection with the configured dimensionality. Idempotent
//!   per Qdrant's response shape: creating a collection that
//!   already exists with the same vector params returns 200.
//! - `PUT  /collections/{name}/points` — upsert points (the
//!   `wait=true` query string makes the call synchronous so a
//!   subsequent search sees the new vectors).
//! - `POST /collections/{name}/points/search` — nearest-neighbor
//!   query with an optional filter clause.
//! - `POST /collections/{name}/points/delete` — delete-by-filter.
//!
//! All four endpoints return Qdrant's standard envelope
//! `{ "status": "ok" | { "error": "..." }, "result": ..., "time": ... }`.
//! We surface non-2xx status codes + the body as
//! [`QdrantError::Api`]; transport errors as
//! [`QdrantError::Http`].
//!
//! ## Honest scope
//!
//! - No retries. The memory node's embedding pipeline already
//!   tolerates partial failure — a failed upsert is logged and
//!   the next loop iteration tries again. Adding a second
//!   retry layer here would just double-count failures.
//! - No streaming search. Memory's RAG path always wants a
//!   bounded top-K, never a cursor.
//! - Bearer auth only. Qdrant's HTTP surface also accepts
//!   `api-key` as a header; both work, and `Bearer` matches the
//!   project's other auth wiring (OpenAI, Anthropic).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// `[memory.qdrant]` config section. Absent / `url` empty
/// means the memory node runs without Qdrant — semantic search
/// falls back to SQLite text search.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct QdrantConfig {
    /// Base URL of the Qdrant server, e.g.
    /// `http://localhost:6333`. Empty value disables Qdrant.
    #[serde(default)]
    pub url: String,
    /// Collection name. Defaults to `relix_memory`. When
    /// [`Self::tenant_isolation`] is enabled this acts as a
    /// FALLBACK for callers that don't supply a tenant id;
    /// every tenant-scoped call uses
    /// `format!("{collection_prefix}_{sanitized_tenant_id}")`.
    #[serde(default = "default_collection")]
    pub collection: String,
    /// Vector dimensionality. Must match the embedding model's
    /// output dimension. Default 1536 (OpenAI
    /// `text-embedding-3-small`).
    #[serde(default = "default_dim", alias = "embedding_dim")]
    pub dim: usize,
    /// Optional API key. Empty string treated as `None`.
    ///
    /// SEC PART 2: stored as `SecretString` so the key bytes
    /// are zeroized on drop. The newtype carries its own
    /// `Serialize` / `Deserialize` impls so the TOML config
    /// path keeps working without macro contortions.
    #[serde(default)]
    pub api_key: Option<crate::credentials::SecretString>,
    /// GAP 23: per-tenant collection isolation. When `false`
    /// (the default), every read / write goes to
    /// [`Self::collection`] regardless of the request's
    /// tenant id — backwards-compatible behaviour. When
    /// `true`, the client derives a per-tenant collection
    /// name from the request's `tenant_id` and the
    /// [`Self::collection_prefix`] and auto-creates it on
    /// first write.
    #[serde(default)]
    pub tenant_isolation: bool,
    /// GAP 23: prefix used when deriving the per-tenant
    /// collection name. Defaults to `relix`. The resolved
    /// collection name is `format!("{prefix}_{tenant_id}")`
    /// where `tenant_id` is sanitised to ASCII alphanumeric +
    /// underscore.
    #[serde(default = "default_collection_prefix")]
    pub collection_prefix: String,
}

fn default_collection() -> String {
    "relix_memory".to_string()
}

fn default_dim() -> usize {
    1536
}

fn default_collection_prefix() -> String {
    "relix".to_string()
}

/// Maximum collection name length. Qdrant enforces a 63-char
/// limit on collection names so the resolver truncates
/// `<prefix>_<sanitized_tenant>` past this boundary. The
/// truncation is deterministic — same `(prefix, tenant)` always
/// produces the same collection — so a tenant cannot bypass
/// isolation by submitting a long id that collides with another
/// after truncation (the tenant_id is sanitised + truncated
/// before the prefix is prepended, and the prefix is part of
/// the static config, so collisions are operator-detectable).
pub const QDRANT_COLLECTION_NAME_MAX: usize = 63;

/// Replace every character that is not ASCII alphanumeric or
/// `_` with `_`, then truncate to at most
/// [`QDRANT_COLLECTION_NAME_MAX`] characters. Empty / pre-empty
/// input becomes the literal `"default"`. Pure function —
/// exported so tests + Part 5 callers can use the same
/// canonical sanitiser.
pub fn sanitize_tenant_id(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    let canonical = if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    };
    if canonical.len() > QDRANT_COLLECTION_NAME_MAX {
        canonical.chars().take(QDRANT_COLLECTION_NAME_MAX).collect()
    } else {
        canonical
    }
}

/// GAP 23 / PART 4: derive the Qdrant collection name for a
/// request. Fails closed when tenant isolation is enabled and
/// no tenant id is supplied.
///
/// - When `tenant_isolation = false`, returns `cfg.collection`
///   verbatim. Single-tenant deployments are byte-identical to
///   the pre-GAP-23 behaviour.
/// - When `tenant_isolation = true` AND `tenant_id` is
///   `Some(non_empty)`, returns
///   `format!("{prefix}_{sanitized_tenant_id}")`.
/// - When `tenant_isolation = true` AND `tenant_id` is `None` /
///   empty, returns `Err(QdrantError::MissingTenant)`. The
///   pre-PART-4 silent fallback to `"default"` was a tenant
///   isolation bug — every tenant whose id wasn't propagated
///   shared one collection.
pub fn resolve_collection_name(
    cfg: &QdrantConfig,
    tenant_id: Option<&str>,
) -> Result<String, QdrantError> {
    if !cfg.tenant_isolation {
        return Ok(cfg.collection.clone());
    }
    let raw = match tenant_id {
        Some(t) if !t.trim().is_empty() => t,
        _ => return Err(QdrantError::MissingTenant),
    };
    let sanitised = sanitize_tenant_id(raw);
    Ok(format!("{}_{}", cfg.collection_prefix, sanitised))
}

/// Errors raised by the Qdrant client. The memory pipeline
/// downgrades these to `tracing::warn!` logs — a Qdrant blip
/// must never destabilise the memory node.
#[derive(Debug, thiserror::Error)]
pub enum QdrantError {
    #[error("qdrant http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("qdrant api status={status} body={message}")]
    Api { status: u16, message: String },
    #[error("qdrant serialization: {0}")]
    Serialization(#[from] serde_json::Error),
    /// PART 4: returned by [`resolve_collection_name`] when
    /// tenant isolation is enabled but no tenant id was
    /// supplied. The bridge surfaces this as HTTP 401 so
    /// operators see the wire reason for a missing
    /// X-Relix-Tenant binding instead of a silent fallback
    /// to a shared collection.
    #[error("qdrant: tenant_id required in multi-tenant mode")]
    MissingTenant,
}

/// One point to upsert. `id` is a stable u64 derived from the
/// memory record id (blake3-hash truncation); `vector` is the
/// embedding; `payload` is arbitrary metadata Qdrant indexes
/// for filtering.
#[derive(Clone, Debug, Serialize)]
pub struct QdrantPoint {
    pub id: u64,
    pub vector: Vec<f32>,
    pub payload: serde_json::Value,
}

/// One result row from `search()`. Score is Qdrant's
/// configured distance metric (cosine by default).
#[derive(Clone, Debug, Deserialize)]
pub struct QdrantSearchResult {
    pub id: u64,
    pub score: f32,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Reqwest-backed Qdrant client. Cheap to clone (`reqwest::Client`
/// holds an `Arc` internally).
#[derive(Clone)]
pub struct QdrantClient {
    http: reqwest::Client,
    cfg: QdrantConfig,
    /// GAP 23: collections we've already auto-created during
    /// this process. Tracked so per-tenant writes / searches
    /// don't issue a `PUT /collections/<name>` on every hot-path
    /// call. The mutex is held only across the cache check;
    /// the ensure_collection RPC happens outside the lock.
    ensured: Arc<Mutex<HashSet<String>>>,
    /// PART 4: per-collection-name async serialisation for
    /// the FIRST `ensure_collection_in` call. Without this
    /// lock, two concurrent requests for a brand-new tenant
    /// would both observe `!was_ensured`, both fire the
    /// `PUT /collections/<name>` request, and both insert
    /// into `ensured`. Qdrant tolerates the duplicate PUT
    /// (the second returns 200 "already exists with matching
    /// params") but the duplicate request is wasted work +
    /// each PUT goes through the slow "create collection +
    /// allocate disk" path on Qdrant's side. The lock makes
    /// the FIRST observer of a new collection hold a
    /// per-name `tokio::sync::Mutex` while the PUT lands,
    /// then update the `ensured` cache, then drop the lock;
    /// subsequent observers see the cached-ensured marker
    /// and skip the PUT entirely.
    ensure_locks: Arc<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl QdrantClient {
    /// New client. The `reqwest::Client` is built with a 10s
    /// timeout so a wedged Qdrant doesn't pin a memory pipeline
    /// worker forever.
    pub fn new(cfg: QdrantConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest::Client::builder succeeds with default config");
        Self {
            http,
            cfg,
            ensured: Arc::new(Mutex::new(HashSet::new())),
            ensure_locks: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Borrow the underlying config — used by tests + by the
    /// tenant-aware memory caps that need to resolve a
    /// collection name themselves.
    pub fn config(&self) -> &QdrantConfig {
        &self.cfg
    }

    /// GAP 23 / PART 4: collection name for `tenant_id` after
    /// consulting [`QdrantConfig::tenant_isolation`]. Thin
    /// wrapper around [`resolve_collection_name`]. Returns
    /// `Err(QdrantError::MissingTenant)` when tenant isolation
    /// is enabled but no tenant id was supplied — callers must
    /// surface this rather than fall through to a shared
    /// collection.
    pub fn collection_for_tenant(&self, tenant_id: Option<&str>) -> Result<String, QdrantError> {
        resolve_collection_name(&self.cfg, tenant_id)
    }

    /// Idempotent collection create. Calls
    /// `PUT /collections/{name}` with the configured `dim` and
    /// cosine distance. A 200/2xx response is the success
    /// signal; Qdrant returns 200 both for "newly created" and
    /// "already exists with matching params."
    ///
    /// Operates against [`QdrantConfig::collection`].
    /// GAP 23 callers wanting a per-tenant collection use
    /// [`Self::ensure_collection_in`].
    pub async fn ensure_collection(&self) -> Result<(), QdrantError> {
        self.ensure_collection_in(&self.cfg.collection).await
    }

    /// GAP 23 / PART 4: ensure-collection against an explicit
    /// name. The `ensured` cache short-circuits repeat creates
    /// for collections this client has already created during
    /// the current process.
    ///
    /// PART 4: serialised on a per-collection-name async lock
    /// so two concurrent requests for a brand-new tenant
    /// produce ONE `PUT /collections/<name>` call, not two.
    /// Qdrant tolerates the duplicate PUT but the lock turns
    /// it into a cheap cache-hit on the second caller.
    pub async fn ensure_collection_in(&self, name: &str) -> Result<(), QdrantError> {
        if self.was_ensured(name) {
            return Ok(());
        }
        // Acquire (or create) the per-name async lock.
        let lock = {
            let mut map = self.ensure_locks.lock().await;
            map.entry(name.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        // Re-check under the lock — a concurrent caller may
        // have just finished the PUT and the `ensured` cache
        // is now warm.
        if self.was_ensured(name) {
            return Ok(());
        }
        let url = format!(
            "{}/collections/{}",
            self.cfg.url.trim_end_matches('/'),
            name
        );
        let body = serde_json::json!({
            "vectors": {
                "size": self.cfg.dim,
                "distance": "Cosine",
            },
        });
        let resp = self.auth(self.http.put(&url)).json(&body).send().await?;
        check_status(resp).await?;
        self.mark_ensured(name);
        Ok(())
    }

    /// Upsert one or more points. Uses `?wait=true` so a search
    /// issued immediately after sees the new vectors. Operates
    /// against [`QdrantConfig::collection`].
    pub async fn upsert(&self, points: Vec<QdrantPoint>) -> Result<(), QdrantError> {
        let coll = self.cfg.collection.clone();
        self.upsert_in(&coll, points).await
    }

    /// GAP 23: tenant-aware upsert. Auto-ensures the
    /// collection on first write per process so callers don't
    /// have to.
    pub async fn upsert_in(
        &self,
        collection: &str,
        points: Vec<QdrantPoint>,
    ) -> Result<(), QdrantError> {
        self.ensure_collection_in(collection).await?;
        let url = format!(
            "{}/collections/{}/points?wait=true",
            self.cfg.url.trim_end_matches('/'),
            collection
        );
        let body = serde_json::json!({ "points": points });
        let resp = self.auth(self.http.put(&url)).json(&body).send().await?;
        check_status(resp).await
    }

    /// Nearest-neighbor search. `score_threshold` filters out
    /// hits with cosine similarity below the floor;
    /// `filter` is Qdrant's standard filter clause (or `None`
    /// for no filter). Operates against
    /// [`QdrantConfig::collection`].
    pub async fn search(
        &self,
        vector: Vec<f32>,
        limit: usize,
        score_threshold: f32,
        filter: Option<serde_json::Value>,
    ) -> Result<Vec<QdrantSearchResult>, QdrantError> {
        let coll = self.cfg.collection.clone();
        self.search_in(&coll, vector, limit, score_threshold, filter)
            .await
    }

    /// GAP 23: tenant-aware search. Auto-ensures the collection
    /// so the first search after boot doesn't 404; on
    /// already-empty collections the search returns the empty
    /// result set as before.
    pub async fn search_in(
        &self,
        collection: &str,
        vector: Vec<f32>,
        limit: usize,
        score_threshold: f32,
        filter: Option<serde_json::Value>,
    ) -> Result<Vec<QdrantSearchResult>, QdrantError> {
        self.ensure_collection_in(collection).await?;
        let url = format!(
            "{}/collections/{}/points/search",
            self.cfg.url.trim_end_matches('/'),
            collection
        );
        let mut body = serde_json::json!({
            "vector": vector,
            "limit": limit,
            "with_payload": true,
            "score_threshold": score_threshold,
        });
        if let Some(f) = filter {
            body["filter"] = f;
        }
        let resp = self.auth(self.http.post(&url)).json(&body).send().await?;
        let env = decode_json::<SearchEnvelope>(resp).await?;
        Ok(env.result)
    }

    /// Delete points matching `filter`. Returns Qdrant's
    /// reported number of deleted points (0 when the filter
    /// matched nothing). Operates against
    /// [`QdrantConfig::collection`].
    pub async fn delete_by_filter(&self, filter: serde_json::Value) -> Result<u64, QdrantError> {
        let coll = self.cfg.collection.clone();
        self.delete_by_filter_in(&coll, filter).await
    }

    /// GAP 23: tenant-aware delete. Skips the ensure-collection
    /// step — a delete against a never-created collection is a
    /// no-op rather than an error since the alpha treats
    /// missing collections as empty.
    pub async fn delete_by_filter_in(
        &self,
        collection: &str,
        filter: serde_json::Value,
    ) -> Result<u64, QdrantError> {
        let url = format!(
            "{}/collections/{}/points/delete?wait=true",
            self.cfg.url.trim_end_matches('/'),
            collection
        );
        let body = serde_json::json!({ "filter": filter });
        let resp = self.auth(self.http.post(&url)).json(&body).send().await?;
        let env = decode_json::<DeleteEnvelope>(resp).await?;
        Ok(env.result.deleted.unwrap_or(0))
    }

    fn auth(&self, b: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.cfg.api_key.as_ref().map(|s| s.as_str()) {
            Some(k) if !k.is_empty() => b.header("api-key", k),
            _ => b,
        }
    }

    fn was_ensured(&self, name: &str) -> bool {
        self.ensured
            .lock()
            .map(|s| s.contains(name))
            .unwrap_or(false)
    }

    fn mark_ensured(&self, name: &str) {
        if let Ok(mut s) = self.ensured.lock() {
            s.insert(name.to_string());
        }
    }
}

#[derive(Debug, Deserialize)]
struct SearchEnvelope {
    #[serde(default)]
    result: Vec<QdrantSearchResult>,
}

#[derive(Debug, Deserialize)]
struct DeleteEnvelope {
    #[serde(default)]
    result: DeleteResult,
}

#[derive(Debug, Default, Deserialize)]
struct DeleteResult {
    /// Some Qdrant deployments include a `deleted` count under
    /// `result`; older versions don't. Optional + default-0 so
    /// the decoder tolerates either shape.
    #[serde(default)]
    deleted: Option<u64>,
}

async fn check_status(resp: reqwest::Response) -> Result<(), QdrantError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(QdrantError::Api {
        status: status.as_u16(),
        message: body,
    })
}

async fn decode_json<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
) -> Result<T, QdrantError> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(QdrantError::Api {
            status: status.as_u16(),
            message: body,
        });
    }
    let text = resp.text().await?;
    serde_json::from_str(&text).map_err(QdrantError::Serialization)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;

    fn cfg_for(server_url: &str) -> QdrantConfig {
        QdrantConfig {
            url: server_url.to_string(),
            collection: "test_coll".to_string(),
            dim: 4,
            api_key: None,
            tenant_isolation: false,
            collection_prefix: "relix".to_string(),
        }
    }

    #[test]
    fn config_deserializes_from_toml_section() {
        let s = r#"
            url = "http://localhost:6333"
            collection = "my_coll"
            dim = 768
            api_key = "secret"
        "#;
        let cfg: QdrantConfig = toml::from_str(s).unwrap();
        assert_eq!(cfg.url, "http://localhost:6333");
        assert_eq!(cfg.collection, "my_coll");
        assert_eq!(cfg.dim, 768);
        assert_eq!(cfg.api_key.as_ref().map(|s| s.as_str()), Some("secret"));
    }

    #[test]
    fn config_defaults_when_only_url_is_supplied() {
        let s = r#"url = "http://q:6333""#;
        let cfg: QdrantConfig = toml::from_str(s).unwrap();
        assert_eq!(cfg.collection, "relix_memory");
        assert_eq!(cfg.dim, 1536);
        assert!(cfg.api_key.is_none());
    }

    #[test]
    fn config_accepts_embedding_dim_alias() {
        // External docs sometimes name the field `embedding_dim`.
        // The serde alias keeps that wire-compatible.
        let s = r#"
            url = "http://q:6333"
            embedding_dim = 96
        "#;
        let cfg: QdrantConfig = toml::from_str(s).unwrap();
        assert_eq!(cfg.dim, 96);
    }

    /// Tiny axum test server that records the last request +
    /// returns a canned response. Use this to verify the
    /// client's request shape end-to-end.
    struct MockQdrant {
        addr: std::net::SocketAddr,
        captured: Arc<Mutex<Vec<CapturedReq>>>,
    }

    #[derive(Clone, Debug)]
    struct CapturedReq {
        method: String,
        path: String,
        body: serde_json::Value,
        api_key: Option<String>,
    }

    impl MockQdrant {
        async fn spawn(canned_search: serde_json::Value) -> Self {
            use axum::Router;
            use axum::extract::State;
            use axum::http::{HeaderMap, Method, Request};
            use axum::routing::any;

            let captured: Arc<Mutex<Vec<CapturedReq>>> = Arc::new(Mutex::new(Vec::new()));
            let canned = Arc::new(canned_search);
            let captured_clone = captured.clone();

            async fn record(
                State(state): State<MockState>,
                method: Method,
                headers: HeaderMap,
                req: Request<axum::body::Body>,
            ) -> axum::Json<serde_json::Value> {
                let path = req.uri().path().to_string();
                let api_key = headers
                    .get("api-key")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let bytes = axum::body::to_bytes(req.into_body(), 64 * 1024)
                    .await
                    .unwrap_or_default();
                let body: serde_json::Value = if bytes.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
                };
                state.captured.lock().unwrap().push(CapturedReq {
                    method: method.to_string(),
                    path: path.clone(),
                    body,
                    api_key,
                });
                // Return canned search payload for search calls,
                // an empty 'result' for everything else.
                if path.ends_with("/points/search") {
                    axum::Json((*state.canned).clone())
                } else if path.ends_with("/points/delete") {
                    axum::Json(serde_json::json!({
                        "result": { "deleted": 7, "status": "completed" },
                        "status": "ok",
                        "time": 0.001,
                    }))
                } else {
                    axum::Json(serde_json::json!({
                        "result": true,
                        "status": "ok",
                        "time": 0.001,
                    }))
                }
            }

            #[derive(Clone)]
            struct MockState {
                captured: Arc<Mutex<Vec<CapturedReq>>>,
                canned: Arc<serde_json::Value>,
            }
            let state = MockState {
                captured: captured_clone,
                canned: canned.clone(),
            };
            let app = Router::new().fallback(any(record)).with_state(state);

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            MockQdrant { addr, captured }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    #[tokio::test]
    async fn ensure_collection_sends_put_with_vector_dim() {
        let mock = MockQdrant::spawn(serde_json::Value::Null).await;
        let client = QdrantClient::new(cfg_for(&mock.url()));
        client.ensure_collection().await.unwrap();
        let cap = mock.captured.lock().unwrap();
        assert_eq!(cap.len(), 1);
        assert_eq!(cap[0].method, "PUT");
        assert_eq!(cap[0].path, "/collections/test_coll");
        assert_eq!(cap[0].body["vectors"]["size"], 4);
        assert_eq!(cap[0].body["vectors"]["distance"], "Cosine");
        // No api_key configured ⇒ header not sent.
        assert!(cap[0].api_key.is_none());
    }

    #[tokio::test]
    async fn upsert_sends_put_with_points_array() {
        let mock = MockQdrant::spawn(serde_json::Value::Null).await;
        let client = QdrantClient::new(cfg_for(&mock.url()));
        let pts = vec![QdrantPoint {
            id: 42,
            vector: vec![0.1, 0.2, 0.3, 0.4],
            payload: serde_json::json!({"layer": "raw", "text": "hi"}),
        }];
        client.upsert(pts).await.unwrap();
        let cap = mock.captured.lock().unwrap();
        // GAP 23: every write auto-ensures the collection
        // first, so there are now 2 calls (the PUT
        // /collections/test_coll ensure + the
        // PUT /collections/test_coll/points upsert).
        assert_eq!(cap.len(), 2);
        let upsert = cap
            .iter()
            .find(|r| r.method == "PUT" && r.path.starts_with("/collections/test_coll/points"))
            .expect("upsert call must land");
        let pts = &upsert.body["points"];
        assert!(pts.is_array());
        assert_eq!(pts[0]["id"], 42);
        assert_eq!(pts[0]["payload"]["layer"], "raw");
    }

    #[tokio::test]
    async fn search_round_trips_request_and_response() {
        let canned = serde_json::json!({
            "result": [
                {"id": 7, "score": 0.91, "payload": {"text": "abc"}},
                {"id": 9, "score": 0.83, "payload": {"text": "def"}},
            ],
            "status": "ok",
            "time": 0.002,
        });
        let mock = MockQdrant::spawn(canned).await;
        let client = QdrantClient::new(cfg_for(&mock.url()));
        let hits = client
            .search(
                vec![1.0, 0.0, 0.0, 0.0],
                10,
                0.75,
                Some(serde_json::json!({"must": [{"key": "layer", "match": {"value": "raw"}}]})),
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, 7);
        assert!((hits[0].score - 0.91).abs() < 1e-5);
        let cap = mock.captured.lock().unwrap();
        // GAP 23: search now also auto-ensures the collection
        // before issuing the POST; find the search call by
        // method+path rather than position.
        let search = cap
            .iter()
            .find(|r| r.method == "POST" && r.path == "/collections/test_coll/points/search")
            .expect("search call must land");
        assert_eq!(search.body["limit"], 10);
        assert!((search.body["score_threshold"].as_f64().unwrap() - 0.75).abs() < 1e-5);
        assert!(search.body["filter"].is_object());
    }

    #[tokio::test]
    async fn delete_by_filter_sends_post_with_filter_clause() {
        let mock = MockQdrant::spawn(serde_json::Value::Null).await;
        let client = QdrantClient::new(cfg_for(&mock.url()));
        let n = client
            .delete_by_filter(serde_json::json!({
                "must": [{"key": "id", "match": {"value": "abc"}}]
            }))
            .await
            .unwrap();
        // Mock returns deleted=7 for /points/delete.
        assert_eq!(n, 7);
        let cap = mock.captured.lock().unwrap();
        assert_eq!(cap[0].method, "POST");
        assert!(
            cap[0]
                .path
                .starts_with("/collections/test_coll/points/delete")
        );
        assert!(cap[0].body["filter"]["must"].is_array());
    }

    #[tokio::test]
    async fn api_key_is_passed_as_header_when_configured() {
        let mock = MockQdrant::spawn(serde_json::Value::Null).await;
        let mut cfg = cfg_for(&mock.url());
        cfg.api_key = Some(crate::credentials::SecretString::new("topsecret".into()));
        let client = QdrantClient::new(cfg);
        client.ensure_collection().await.unwrap();
        let cap = mock.captured.lock().unwrap();
        assert_eq!(cap[0].api_key.as_deref(), Some("topsecret"));
    }

    // ── GAP 23: per-tenant collection resolution ─────────

    #[test]
    fn resolve_collection_name_returns_default_when_isolation_off() {
        let cfg = QdrantConfig {
            url: String::new(),
            collection: "relix_memory".into(),
            dim: 1536,
            api_key: None,
            tenant_isolation: false,
            collection_prefix: "relix".into(),
        };
        assert_eq!(
            resolve_collection_name(&cfg, Some("acme")).unwrap(),
            "relix_memory"
        );
        assert_eq!(resolve_collection_name(&cfg, None).unwrap(), "relix_memory");
    }

    #[test]
    fn resolve_collection_name_isolates_per_tenant_when_enabled() {
        let cfg = QdrantConfig {
            url: String::new(),
            collection: "relix_memory".into(),
            dim: 1536,
            api_key: None,
            tenant_isolation: true,
            collection_prefix: "relix".into(),
        };
        assert_eq!(
            resolve_collection_name(&cfg, Some("acme")).unwrap(),
            "relix_acme"
        );
        assert_eq!(
            resolve_collection_name(&cfg, Some("globex")).unwrap(),
            "relix_globex"
        );
        // Different tenants → different collections.
        assert_ne!(
            resolve_collection_name(&cfg, Some("acme")).unwrap(),
            resolve_collection_name(&cfg, Some("globex")).unwrap()
        );
    }

    #[test]
    fn resolve_collection_name_sanitises_special_chars() {
        let cfg = QdrantConfig {
            url: String::new(),
            collection: "x".into(),
            dim: 1536,
            api_key: None,
            tenant_isolation: true,
            collection_prefix: "relix".into(),
        };
        // Slashes / dots / hyphens collapse to underscore so
        // Qdrant's collection naming rules are satisfied.
        assert_eq!(
            resolve_collection_name(&cfg, Some("acme/tenant-1.dev")).unwrap(),
            "relix_acme_tenant_1_dev"
        );
    }

    /// PART 4: tenant-isolation mode MUST fail closed when no
    /// tenant id is supplied. The pre-PART-4 silent fallback to
    /// `"default"` was a critical isolation bug — any handler
    /// that forgot to propagate the tenant header would route
    /// to a shared collection.
    #[test]
    fn fix_part4_resolve_collection_name_fails_closed_on_missing_tenant() {
        let cfg = QdrantConfig {
            url: String::new(),
            collection: "x".into(),
            dim: 1536,
            api_key: None,
            tenant_isolation: true,
            collection_prefix: "relix".into(),
        };
        // None tenant in isolation mode → MissingTenant.
        assert!(matches!(
            resolve_collection_name(&cfg, None),
            Err(QdrantError::MissingTenant)
        ));
        // Empty / whitespace-only tenant id also fails closed.
        assert!(matches!(
            resolve_collection_name(&cfg, Some("")),
            Err(QdrantError::MissingTenant)
        ));
        assert!(matches!(
            resolve_collection_name(&cfg, Some("   ")),
            Err(QdrantError::MissingTenant)
        ));
    }

    #[test]
    fn resolve_collection_name_uses_configured_prefix() {
        let cfg = QdrantConfig {
            url: String::new(),
            collection: "x".into(),
            dim: 1536,
            api_key: None,
            tenant_isolation: true,
            collection_prefix: "saas".into(),
        };
        assert_eq!(
            resolve_collection_name(&cfg, Some("acme")).unwrap(),
            "saas_acme"
        );
    }

    /// PART 4: sanitize_tenant_id replaces every non-alphanumeric
    /// + non-underscore character with `_`.
    #[test]
    fn fix_part4_sanitize_tenant_id_replaces_special_characters() {
        assert_eq!(sanitize_tenant_id("acme"), "acme");
        assert_eq!(sanitize_tenant_id("acme-corp"), "acme_corp");
        assert_eq!(sanitize_tenant_id("acme/tenant.1"), "acme_tenant_1");
        assert_eq!(sanitize_tenant_id("a:b@c#d$e%"), "a_b_c_d_e");
        // Multiple specials collapse but do not get
        // de-duplicated — Qdrant accepts repeated underscores.
        assert_eq!(sanitize_tenant_id("a..b"), "a__b");
    }

    /// PART 4: sanitize_tenant_id truncates at 63 chars (Qdrant
    /// collection name limit).
    #[test]
    fn fix_part4_sanitize_tenant_id_truncates_at_63_chars() {
        let long = "a".repeat(100);
        let out = sanitize_tenant_id(&long);
        assert_eq!(out.len(), 63);
        assert_eq!(out, "a".repeat(63));
        // Exactly 63 passes through.
        let exact = "b".repeat(63);
        assert_eq!(sanitize_tenant_id(&exact), exact);
        // 64 truncates.
        let one_over = "c".repeat(64);
        assert_eq!(sanitize_tenant_id(&one_over).len(), 63);
    }

    /// PART 4: sanitize_tenant_id falls back to `"default"` on
    /// empty / all-special input. Used by the audit_partition
    /// path that explicitly opts into the shared bucket.
    #[test]
    fn fix_part4_sanitize_tenant_id_defaults_on_empty_input() {
        assert_eq!(sanitize_tenant_id(""), "default");
        // All-special input → all underscores → trimmed →
        // empty → default.
        assert_eq!(sanitize_tenant_id("///"), "default");
    }

    #[tokio::test]
    async fn ensure_collection_in_is_idempotent_across_calls() {
        let mock = MockQdrant::spawn(serde_json::Value::Null).await;
        let client = QdrantClient::new(cfg_for(&mock.url()));
        // First call: real PUT.
        client.ensure_collection_in("acme_coll").await.unwrap();
        // Second call: cached, no extra request.
        client.ensure_collection_in("acme_coll").await.unwrap();
        let cap = mock.captured.lock().unwrap();
        let puts: Vec<_> = cap
            .iter()
            .filter(|r| r.method == "PUT" && r.path == "/collections/acme_coll")
            .collect();
        assert_eq!(puts.len(), 1, "ensure_collection_in should cache");
    }

    #[tokio::test]
    async fn upsert_in_targets_named_collection_and_auto_ensures() {
        let mock = MockQdrant::spawn(serde_json::Value::Null).await;
        let client = QdrantClient::new(cfg_for(&mock.url()));
        let pts = vec![QdrantPoint {
            id: 1,
            vector: vec![0.1, 0.2, 0.3, 0.4],
            payload: serde_json::json!({"k": "v"}),
        }];
        client.upsert_in("tenant_acme", pts).await.unwrap();
        let cap = mock.captured.lock().unwrap();
        // First a PUT /collections/tenant_acme (auto-ensure),
        // then a PUT /collections/tenant_acme/points?wait=true.
        let ensure_calls: Vec<_> = cap
            .iter()
            .filter(|r| r.method == "PUT" && r.path == "/collections/tenant_acme")
            .collect();
        assert_eq!(ensure_calls.len(), 1);
        let upsert_calls: Vec<_> = cap
            .iter()
            .filter(|r| r.method == "PUT" && r.path.starts_with("/collections/tenant_acme/points"))
            .collect();
        assert_eq!(upsert_calls.len(), 1);
    }

    #[tokio::test]
    async fn auto_create_per_tenant_collection_on_first_write() {
        let mock = MockQdrant::spawn(serde_json::Value::Null).await;
        let mut cfg = cfg_for(&mock.url());
        cfg.tenant_isolation = true;
        let client = QdrantClient::new(cfg);
        // Tenant "tenant_x" has never written before — the
        // first upsert should produce the ensure-collection
        // PUT.
        let pts = vec![QdrantPoint {
            id: 1,
            vector: vec![0.1, 0.2, 0.3, 0.4],
            payload: serde_json::Value::Null,
        }];
        let coll = client.collection_for_tenant(Some("tenant_x")).unwrap();
        client.upsert_in(&coll, pts).await.unwrap();
        let cap = mock.captured.lock().unwrap();
        assert!(
            cap.iter()
                .any(|r| r.method == "PUT" && r.path == "/collections/relix_tenant_x"),
            "first write to new tenant must auto-create its collection"
        );
    }

    #[tokio::test]
    async fn two_tenants_with_isolation_use_distinct_collections() {
        let mock = MockQdrant::spawn(serde_json::Value::Null).await;
        let mut cfg = cfg_for(&mock.url());
        cfg.tenant_isolation = true;
        cfg.collection_prefix = "relix".into();
        let client = QdrantClient::new(cfg);
        let coll_a = client.collection_for_tenant(Some("alpha")).unwrap();
        let coll_b = client.collection_for_tenant(Some("beta")).unwrap();
        assert_eq!(coll_a, "relix_alpha");
        assert_eq!(coll_b, "relix_beta");
        let pts = vec![QdrantPoint {
            id: 1,
            vector: vec![0.1, 0.2, 0.3, 0.4],
            payload: serde_json::Value::Null,
        }];
        client.upsert_in(&coll_a, pts.clone()).await.unwrap();
        client.upsert_in(&coll_b, pts).await.unwrap();
        let cap = mock.captured.lock().unwrap();
        assert!(cap.iter().any(|r| r.path == "/collections/relix_alpha"));
        assert!(cap.iter().any(|r| r.path == "/collections/relix_beta"));
    }

    /// PART 4: two concurrent ensure_collection_in calls on a
    /// brand-new tenant must produce EXACTLY ONE
    /// PUT /collections/<name> — the per-collection async lock
    /// serialises the first observer's work and subsequent
    /// observers see the cached-ensured marker.
    #[tokio::test]
    async fn fix_part4_concurrent_ensure_creates_collection_exactly_once() {
        let mock = MockQdrant::spawn(serde_json::Value::Null).await;
        let client = QdrantClient::new(cfg_for(&mock.url()));
        // 8 concurrent ensure calls on a brand-new collection.
        // Without the per-name lock, the race would produce up
        // to 8 PUT calls.
        let coll = "relix_concurrent_tenant";
        let mut handles = Vec::with_capacity(8);
        for _ in 0..8 {
            let c = client.clone();
            let n = coll.to_string();
            handles.push(tokio::spawn(
                async move { c.ensure_collection_in(&n).await },
            ));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
        let cap = mock.captured.lock().unwrap();
        let puts: Vec<_> = cap
            .iter()
            .filter(|r| r.method == "PUT" && r.path == format!("/collections/{coll}"))
            .collect();
        assert_eq!(
            puts.len(),
            1,
            "concurrent ensure_collection_in must produce exactly one PUT"
        );
    }

    #[test]
    fn config_deserialises_with_tenant_isolation_section() {
        let s = r#"
            url = "http://q:6333"
            tenant_isolation = true
            collection_prefix = "saas"
        "#;
        let cfg: QdrantConfig = toml::from_str(s).unwrap();
        assert!(cfg.tenant_isolation);
        assert_eq!(cfg.collection_prefix, "saas");
        assert_eq!(cfg.collection, "relix_memory");
    }

    #[tokio::test]
    async fn non_2xx_response_surfaces_as_api_error() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::any;
        let app: Router = Router::new().fallback(any(|| async {
            (StatusCode::BAD_REQUEST, "vector dim mismatch")
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = QdrantClient::new(cfg_for(&format!("http://{addr}")));
        let err = client.ensure_collection().await.unwrap_err();
        match err {
            QdrantError::Api { status, message } => {
                assert_eq!(status, 400);
                assert!(message.contains("dim mismatch"));
            }
            other => panic!("expected QdrantError::Api, got {other:?}"),
        }
    }
}
