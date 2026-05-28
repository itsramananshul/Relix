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

use std::collections::HashSet;
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
    #[serde(default)]
    pub api_key: Option<String>,
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

/// GAP 23: derive the Qdrant collection name for a request.
///
/// - When `tenant_isolation = false`, returns `cfg.collection`
///   verbatim. Single-tenant deployments are byte-identical to
///   the pre-GAP-23 behaviour.
/// - When `tenant_isolation = true`, returns
///   `format!("{prefix}_{sanitized_tenant_id}")` where the
///   tenant id has every non-`[A-Za-z0-9_]` character replaced
///   by `_`. Empty / `None` tenant ids resolve to the literal
///   `"default"` sanitisation key.
///
/// Pure function — exported for tests of the sanitiser.
pub fn resolve_collection_name(cfg: &QdrantConfig, tenant_id: Option<&str>) -> String {
    if !cfg.tenant_isolation {
        return cfg.collection.clone();
    }
    let raw = tenant_id.unwrap_or("default");
    let sanitised: String = if raw.is_empty() {
        "default".to_string()
    } else {
        raw.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    format!("{}_{}", cfg.collection_prefix, sanitised)
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
        }
    }

    /// Borrow the underlying config — used by tests + by the
    /// tenant-aware memory caps that need to resolve a
    /// collection name themselves.
    pub fn config(&self) -> &QdrantConfig {
        &self.cfg
    }

    /// GAP 23: collection name for `tenant_id` after consulting
    /// [`QdrantConfig::tenant_isolation`]. Thin wrapper around
    /// [`resolve_collection_name`].
    pub fn collection_for_tenant(&self, tenant_id: Option<&str>) -> String {
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

    /// GAP 23: ensure-collection against an explicit name. The
    /// `ensured` cache short-circuits repeat creates for
    /// collections this client has already created during the
    /// current process.
    pub async fn ensure_collection_in(&self, name: &str) -> Result<(), QdrantError> {
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
        match self.cfg.api_key.as_deref() {
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
        assert_eq!(cfg.api_key.as_deref(), Some("secret"));
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
        cfg.api_key = Some("topsecret".into());
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
        assert_eq!(resolve_collection_name(&cfg, Some("acme")), "relix_memory");
        assert_eq!(resolve_collection_name(&cfg, None), "relix_memory");
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
        assert_eq!(resolve_collection_name(&cfg, Some("acme")), "relix_acme");
        assert_eq!(
            resolve_collection_name(&cfg, Some("globex")),
            "relix_globex"
        );
        // Different tenants → different collections.
        assert_ne!(
            resolve_collection_name(&cfg, Some("acme")),
            resolve_collection_name(&cfg, Some("globex"))
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
            resolve_collection_name(&cfg, Some("acme/tenant-1.dev")),
            "relix_acme_tenant_1_dev"
        );
        // Empty tenant id falls back to "default".
        assert_eq!(resolve_collection_name(&cfg, Some("")), "relix_default");
        // None tenant resolves to "default" too.
        assert_eq!(resolve_collection_name(&cfg, None), "relix_default");
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
        assert_eq!(resolve_collection_name(&cfg, Some("acme")), "saas_acme");
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
        let coll = client.collection_for_tenant(Some("tenant_x"));
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
        let coll_a = client.collection_for_tenant(Some("alpha"));
        let coll_b = client.collection_for_tenant(Some("beta"));
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
