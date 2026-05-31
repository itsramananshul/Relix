//! Per-principal, per-route-class rate limiting for the bridge.
//!
//! A single runaway client can otherwise:
//! - exhaust the operator's provider quota by hammering `/chat` /
//!   `/v1/chat/completions`,
//! - saturate the dashboard's polling routes (every dashboard
//!   refresh fans out to many GETs),
//! - or open thousands of WebSocket sessions and pin the bridge's
//!   process file-descriptor budget.
//!
//! This module owns four limits applied AFTER the auth middleware:
//!
//! | Class            | Default budget                | What it covers                                       |
//! |------------------|-------------------------------|------------------------------------------------------|
//! | `ai`             | 60 req / minute  / principal  | `POST /chat`, `POST /v1/chat/completions`, `GET /ws/chat` |
//! | `dashboard_poll` | 120 req / minute / principal  | dashboard GETs (`/v1/tasks`, `/v1/topology`, `/v1/health`, `/v1/capabilities`) |
//! | `task_mutation`  | 30 req / minute  / principal  | mutating task verbs (`POST/PUT/PATCH/DELETE /v1/tasks/*`) |
//! | `ws_concurrent`  | 5 open sockets / principal    | `GET /ws/chat` upgrade lifetime                      |
//!
//! Anything that doesn't classify into one of the above passes
//! through. The HTTP-side limits live in
//! [`rate_limit_middleware`], the concurrent-WS gate lives in
//! [`RateLimits::ws_acquire`] and is taken explicitly from the
//! WebSocket handler.
//!
//! Principal identity is the bearer token the auth middleware
//! already validated; we use a SHA-256 hex prefix as the bucket
//! key so debug-log dumps never carry a raw token.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

/// Default per-minute / concurrent limits — applied when
/// `[mesh.rate_limits]` is absent from the operator config.
pub const DEFAULT_AI_PER_MIN: u32 = 60;
pub const DEFAULT_DASHBOARD_PER_MIN: u32 = 120;
pub const DEFAULT_TASK_MUT_PER_MIN: u32 = 30;
pub const DEFAULT_WS_MAX_CONCURRENT: u32 = 5;

/// Operator-supplied limits. Every field has a default so a
/// partial `[mesh.rate_limits]` table still deserialises cleanly.
#[derive(Clone, Debug, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default = "default_ai_per_min")]
    pub ai_calls_per_min: u32,
    #[serde(default = "default_dashboard_per_min")]
    pub dashboard_polls_per_min: u32,
    #[serde(default = "default_task_mut_per_min")]
    pub task_mutations_per_min: u32,
    #[serde(default = "default_ws_max_concurrent")]
    pub ws_max_concurrent: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            ai_calls_per_min: DEFAULT_AI_PER_MIN,
            dashboard_polls_per_min: DEFAULT_DASHBOARD_PER_MIN,
            task_mutations_per_min: DEFAULT_TASK_MUT_PER_MIN,
            ws_max_concurrent: DEFAULT_WS_MAX_CONCURRENT,
        }
    }
}

fn default_ai_per_min() -> u32 {
    DEFAULT_AI_PER_MIN
}
fn default_dashboard_per_min() -> u32 {
    DEFAULT_DASHBOARD_PER_MIN
}
fn default_task_mut_per_min() -> u32 {
    DEFAULT_TASK_MUT_PER_MIN
}
fn default_ws_max_concurrent() -> u32 {
    DEFAULT_WS_MAX_CONCURRENT
}

/// Route classes the limiter knows about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RouteClass {
    Ai,
    DashboardPoll,
    TaskMutation,
    /// Anything that doesn't match a known class — middleware
    /// short-circuits straight to the inner handler.
    Other,
}

/// Token bucket. `take(1)` either succeeds (and consumes one
/// token) or returns the wall-clock duration the caller would
/// have to wait for the next token to mature.
#[derive(Debug)]
struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(capacity_per_min: u32) -> Self {
        let capacity = capacity_per_min as f64;
        Self {
            capacity,
            // Continuous refill: `capacity` tokens per 60 seconds.
            refill_per_sec: capacity / 60.0,
            tokens: capacity,
            last_refill: Instant::now(),
        }
    }

    fn take_one(&mut self) -> Result<(), Duration> {
        let now = Instant::now();
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            let needed = 1.0 - self.tokens;
            let secs = needed / self.refill_per_sec.max(f64::MIN_POSITIVE);
            // Round up — 0.4s → "wait 1s", which is what a
            // Retry-After header expresses honestly.
            let rounded = secs.ceil().max(1.0);
            Err(Duration::from_secs(rounded as u64))
        }
    }
}

/// Shared rate-limit state. Cheap to clone (everything behind
/// an Arc). One instance lives on `AppState` and is also passed
/// directly to the axum middleware layer.
#[derive(Clone)]
pub struct RateLimits {
    inner: Arc<Inner>,
}

struct Inner {
    cfg: RateLimitConfig,
    ai_buckets: Mutex<HashMap<String, TokenBucket>>,
    dashboard_buckets: Mutex<HashMap<String, TokenBucket>>,
    task_buckets: Mutex<HashMap<String, TokenBucket>>,
    ws_inflight: Mutex<HashMap<String, u32>>,
}

impl RateLimits {
    pub fn new(cfg: RateLimitConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                cfg,
                ai_buckets: Mutex::new(HashMap::new()),
                dashboard_buckets: Mutex::new(HashMap::new()),
                task_buckets: Mutex::new(HashMap::new()),
                ws_inflight: Mutex::new(HashMap::new()),
            }),
        }
    }

    #[allow(dead_code)]
    pub fn config(&self) -> &RateLimitConfig {
        &self.inner.cfg
    }

    /// Take one token from the bucket for `class`/`principal`.
    /// `Ok(())` → the request may proceed; `Err(retry_after)` →
    /// the middleware must return 429 and stamp `Retry-After`.
    /// `Other` class always passes (no limit configured).
    pub fn try_acquire_http(&self, class: RouteClass, principal: &str) -> Result<(), Duration> {
        let (cap, map) = match class {
            RouteClass::Ai => (self.inner.cfg.ai_calls_per_min, &self.inner.ai_buckets),
            RouteClass::DashboardPoll => (
                self.inner.cfg.dashboard_polls_per_min,
                &self.inner.dashboard_buckets,
            ),
            RouteClass::TaskMutation => (
                self.inner.cfg.task_mutations_per_min,
                &self.inner.task_buckets,
            ),
            RouteClass::Other => return Ok(()),
        };
        if cap == 0 {
            // Operator-disabled (e.g. `ai_calls_per_min = 0`).
            // Refuse outright so a misconfiguration is loud.
            return Err(Duration::from_secs(60));
        }
        let mut guard = map.lock().unwrap_or_else(|e| {
            tracing::warn!("rate-limit map lock poisoned; recovering inner state");
            e.into_inner()
        });
        let entry = guard
            .entry(principal.to_string())
            .or_insert_with(|| TokenBucket::new(cap));
        entry.take_one()
    }

    /// Increment the concurrent-WebSocket counter for `principal`.
    /// Returns `Ok(WsGuard)` when below the limit (the guard
    /// decrements on drop); `Err(limit)` when the principal
    /// already holds `ws_max_concurrent` sockets. The WS handler
    /// holds the guard for the lifetime of the upgrade.
    pub fn ws_acquire(&self, principal: &str) -> Result<WsGuard, u32> {
        let limit = self.inner.cfg.ws_max_concurrent;
        let mut guard = self.inner.ws_inflight.lock().unwrap_or_else(|e| {
            tracing::warn!("ws map lock poisoned; recovering inner state");
            e.into_inner()
        });
        let count = guard.entry(principal.to_string()).or_insert(0);
        if *count >= limit {
            return Err(limit);
        }
        *count += 1;
        Ok(WsGuard {
            state: self.clone(),
            principal: principal.to_string(),
        })
    }
}

/// RAII handle returned by [`RateLimits::ws_acquire`]. Dropping
/// the guard decrements the per-principal counter so a graceful
/// close, crash, or task cancellation all return capacity.
pub struct WsGuard {
    state: RateLimits,
    principal: String,
}

impl Drop for WsGuard {
    fn drop(&mut self) {
        let mut guard = self.state.inner.ws_inflight.lock().unwrap_or_else(|e| {
            tracing::warn!("ws map lock poisoned during WsGuard drop; recovering inner state");
            e.into_inner()
        });
        if let Some(count) = guard.get_mut(&self.principal) {
            if *count > 0 {
                *count -= 1;
            }
            if *count == 0 {
                guard.remove(&self.principal);
            }
        }
    }
}

/// Classify a request into one of the limiter's known route
/// classes. Pure function; only inspects method + path. WS
/// upgrades are classified as `Ai` for the per-minute budget
/// (the connection-attempt rate); the concurrent-socket gate is
/// taken separately inside the handler.
pub fn classify(method: &str, path: &str) -> RouteClass {
    if path == "/chat"
        || path == "/chat/stream"
        || path == "/chat_with_tool"
        || path == "/v1/chat/completions"
        || path == "/ws/chat"
    {
        return RouteClass::Ai;
    }

    if path.starts_with("/v1/tasks") {
        return match method {
            "POST" | "PUT" | "PATCH" | "DELETE" => RouteClass::TaskMutation,
            _ => RouteClass::DashboardPoll,
        };
    }

    if method == "GET"
        && (path == "/v1/health"
            || path == "/v1/topology"
            || path.starts_with("/v1/topology/")
            || path == "/v1/capabilities"
            || path.starts_with("/v1/capabilities/")
            || path == "/v1/streams"
            || path == "/v1/routing"
            || path == "/v1/dispatch/stats"
            || path == "/v1/intervention/recent")
    {
        return RouteClass::DashboardPoll;
    }

    RouteClass::Other
}

/// Hash the bearer token into a short hex string so map keys and
/// debug logs never carry the raw secret. SHA-256 is overkill for
/// a non-cryptographic key but consistent with the rest of the
/// codebase. Returns the first 16 hex chars (64 bits).
fn principal_from_bearer(bearer: &str) -> String {
    let h = blake3::hash(bearer.as_bytes());
    let hex = h.to_hex();
    hex.as_str()[..16].to_string()
}

/// Extract the principal key from a request. Prefers the
/// authenticated bearer (`Authorization: Bearer <token>`), then
/// the SSE `?token=` query fallback, and finally a synthetic
/// `anon:<source>` label for requests that reached the limiter
/// without auth (shouldn't happen in production — the auth
/// middleware runs first — but lets the limiter behave on its
/// own in tests).
fn principal_for(req: &Request) -> String {
    if let Some(v) = req.headers().get(header::AUTHORIZATION)
        && let Ok(s) = v.to_str()
    {
        let trimmed = s
            .strip_prefix("Bearer ")
            .or_else(|| s.strip_prefix("bearer "))
            .map(str::trim)
            .unwrap_or("");
        if !trimmed.is_empty() {
            return principal_from_bearer(trimmed);
        }
    }
    if let Some(q) = req.uri().query() {
        for pair in q.split('&') {
            if let Some(v) = pair.strip_prefix("token=") {
                let v = v.trim();
                if !v.is_empty() {
                    return principal_from_bearer(v);
                }
            }
        }
    }
    "anon".to_string()
}

#[derive(Serialize)]
struct LimitErrBody {
    error: &'static str,
    retry_after_secs: u64,
}

fn too_many(retry_after: Duration) -> Response {
    let secs = retry_after.as_secs().max(1);
    let body = LimitErrBody {
        error: "rate_limit_exceeded",
        retry_after_secs: secs,
    };
    let mut resp = (StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response();
    resp.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&secs.to_string()).unwrap_or(HeaderValue::from_static("60")),
    );
    resp
}

/// Axum middleware. Drops `Body::empty()`-payload requests
/// straight through when their class is `Other`; otherwise calls
/// [`RateLimits::try_acquire_http`] and returns 429 on overflow.
pub async fn rate_limit_middleware(
    State(state): State<RateLimits>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let class = classify(&method, &path);
    if matches!(class, RouteClass::Other) {
        return next.run(req).await;
    }
    let principal = principal_for(&req);
    match state.try_acquire_http(class, &principal) {
        Ok(()) => next.run(req).await,
        Err(retry_after) => too_many(retry_after),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::{get, post};
    use tower::ServiceExt;

    #[test]
    fn token_bucket_starts_full_and_drains() {
        let mut b = TokenBucket::new(2);
        assert!(b.take_one().is_ok());
        assert!(b.take_one().is_ok());
        // Third should fail (no time elapsed).
        assert!(b.take_one().is_err());
    }

    #[test]
    fn token_bucket_refills_proportionally() {
        let mut b = TokenBucket::new(60);
        // Drain the bucket.
        for _ in 0..60 {
            assert!(b.take_one().is_ok());
        }
        // Synthesise elapsed time by manipulating last_refill.
        b.last_refill = Instant::now() - Duration::from_secs(2);
        // 60/min = 1 per second; after 2 seconds we have ~2.
        assert!(b.take_one().is_ok());
        assert!(b.take_one().is_ok());
        assert!(b.take_one().is_err());
    }

    #[test]
    fn classify_recognises_ai_routes() {
        assert_eq!(classify("POST", "/chat"), RouteClass::Ai);
        assert_eq!(classify("POST", "/v1/chat/completions"), RouteClass::Ai);
        assert_eq!(classify("GET", "/ws/chat"), RouteClass::Ai);
        assert_eq!(classify("POST", "/chat/stream"), RouteClass::Ai);
    }

    #[test]
    fn classify_recognises_dashboard_polls() {
        assert_eq!(classify("GET", "/v1/health"), RouteClass::DashboardPoll);
        assert_eq!(classify("GET", "/v1/topology"), RouteClass::DashboardPoll);
        assert_eq!(
            classify("GET", "/v1/capabilities"),
            RouteClass::DashboardPoll
        );
        assert_eq!(classify("GET", "/v1/tasks"), RouteClass::DashboardPoll);
        assert_eq!(classify("GET", "/v1/tasks/abc"), RouteClass::DashboardPoll);
    }

    #[test]
    fn classify_recognises_task_mutations() {
        assert_eq!(
            classify("POST", "/v1/tasks/abc/retry"),
            RouteClass::TaskMutation
        );
        assert_eq!(
            classify("PUT", "/v1/tasks/abc/todos"),
            RouteClass::TaskMutation
        );
        assert_eq!(
            classify("PATCH", "/v1/tasks/abc/todos/1"),
            RouteClass::TaskMutation
        );
        assert_eq!(
            classify("DELETE", "/v1/tasks/abc"),
            RouteClass::TaskMutation
        );
    }

    #[test]
    fn classify_passes_through_unrelated_routes() {
        assert_eq!(classify("GET", "/dashboard"), RouteClass::Other);
        assert_eq!(classify("GET", "/health"), RouteClass::Other);
        assert_eq!(classify("POST", "/v1/auth/token"), RouteClass::Other);
        assert_eq!(classify("GET", "/v1/mcp/audit"), RouteClass::Other);
    }

    #[test]
    fn principal_from_bearer_is_stable_and_hashes() {
        let p1 = principal_from_bearer("abc123");
        let p2 = principal_from_bearer("abc123");
        let p3 = principal_from_bearer("abc124");
        assert_eq!(p1, p2);
        assert_ne!(p1, p3);
        assert_eq!(p1.len(), 16);
        assert!(p1.chars().all(|c| c.is_ascii_hexdigit()));
        // Never echoes the raw bearer.
        assert!(!p1.contains("abc"));
    }

    fn cfg_strict() -> RateLimitConfig {
        RateLimitConfig {
            ai_calls_per_min: 2,
            dashboard_polls_per_min: 3,
            task_mutations_per_min: 2,
            ws_max_concurrent: 2,
        }
    }

    async fn handler_ok() -> &'static str {
        "ok"
    }

    fn router(state: RateLimits) -> Router {
        Router::new()
            .route("/chat", post(handler_ok))
            .route("/v1/tasks", get(handler_ok).post(handler_ok))
            .route("/v1/health", get(handler_ok))
            .layer(axum::middleware::from_fn_with_state(
                state,
                rate_limit_middleware,
            ))
    }

    fn req(method: &str, path: &str, bearer: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {bearer}"))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn under_limit_passes() {
        let state = RateLimits::new(cfg_strict());
        let app = router(state);
        let r = app.oneshot(req("POST", "/chat", "abc")).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn over_limit_returns_429_with_retry_after() {
        let state = RateLimits::new(cfg_strict());
        // 2-per-minute on `Ai`; third request must 429.
        let r1 = router(state.clone())
            .oneshot(req("POST", "/chat", "abc"))
            .await
            .unwrap();
        let r2 = router(state.clone())
            .oneshot(req("POST", "/chat", "abc"))
            .await
            .unwrap();
        let r3 = router(state.clone())
            .oneshot(req("POST", "/chat", "abc"))
            .await
            .unwrap();
        assert_eq!(r1.status(), StatusCode::OK);
        assert_eq!(r2.status(), StatusCode::OK);
        assert_eq!(r3.status(), StatusCode::TOO_MANY_REQUESTS);
        // Retry-After header present and parseable.
        let ra = r3
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .expect("Retry-After header");
        assert!(ra >= 1, "expected at least 1s, got {ra}");
        // Body matches the documented shape.
        let bytes = axum::body::to_bytes(r3.into_body(), 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "rate_limit_exceeded");
        assert!(body["retry_after_secs"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn different_principals_have_independent_buckets() {
        let state = RateLimits::new(cfg_strict());
        // alice exhausts her budget…
        let _ = router(state.clone())
            .oneshot(req("POST", "/chat", "alice"))
            .await;
        let _ = router(state.clone())
            .oneshot(req("POST", "/chat", "alice"))
            .await;
        let alice_3rd = router(state.clone())
            .oneshot(req("POST", "/chat", "alice"))
            .await
            .unwrap();
        assert_eq!(alice_3rd.status(), StatusCode::TOO_MANY_REQUESTS);
        // …bob is unaffected.
        let bob_1st = router(state.clone())
            .oneshot(req("POST", "/chat", "bob"))
            .await
            .unwrap();
        assert_eq!(bob_1st.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn task_mutation_and_dashboard_are_separate_buckets() {
        let state = RateLimits::new(cfg_strict());
        // Burn the task-mutation bucket.
        for _ in 0..2 {
            let r = router(state.clone())
                .oneshot(req("POST", "/v1/tasks", "x"))
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK);
        }
        let over = router(state.clone())
            .oneshot(req("POST", "/v1/tasks", "x"))
            .await
            .unwrap();
        assert_eq!(over.status(), StatusCode::TOO_MANY_REQUESTS);
        // Dashboard polls on the same principal still pass.
        let poll = router(state.clone())
            .oneshot(req("GET", "/v1/tasks", "x"))
            .await
            .unwrap();
        assert_eq!(poll.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn other_route_class_skips_limiter_entirely() {
        // Even an exhausted Ai bucket can't affect /dashboard
        // (which classifies as Other).
        let state = RateLimits::new(RateLimitConfig {
            ai_calls_per_min: 1,
            dashboard_polls_per_min: 1,
            task_mutations_per_min: 1,
            ws_max_concurrent: 1,
        });
        // Burn the bucket.
        let _ = state.try_acquire_http(RouteClass::Ai, "x");
        let _ = state.try_acquire_http(RouteClass::Ai, "x");
        // Other routes bypass the limit.
        let app = Router::new()
            .route("/v1/auth/token", get(handler_ok))
            .layer(axum::middleware::from_fn_with_state(
                state,
                rate_limit_middleware,
            ));
        let r = app
            .oneshot(req("GET", "/v1/auth/token", "x"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[test]
    fn ws_acquire_blocks_after_limit_and_releases_on_drop() {
        let state = RateLimits::new(cfg_strict());
        let g1 = state.ws_acquire("principal-x").expect("first");
        let g2 = state.ws_acquire("principal-x").expect("second");
        // Third would exceed the cap of 2.
        let err = state.ws_acquire("principal-x");
        assert!(matches!(err, Err(2)));
        // Drop one — capacity returns.
        drop(g1);
        let _g3 = state.ws_acquire("principal-x").expect("third after drop");
        drop(g2);
    }

    // ── CORR PART 1: lock poison recovery ─────────────────

    #[test]
    fn corr_p1_poisoned_lock_recovered_and_takes_token() {
        // Poison the inner Mutex by panicking inside a
        // lock guard, then assert the next `try_acquire_http`
        // call recovers via `unwrap_or_else(e.into_inner())`
        // instead of panicking.
        let state = RateLimits::new(cfg_strict());
        let state_clone = state.clone();
        let _ = std::thread::spawn(move || {
            let _g = state_clone.inner.ai_buckets.lock().unwrap();
            panic!("intentional poison");
        })
        .join();
        // Pre-fix path panicked here ("rate-limit map lock");
        // post-fix path recovers + serves the token.
        let res = state.try_acquire_http(RouteClass::Ai, "anyone");
        assert!(res.is_ok(), "poisoned-lock recovery must serve a token");
    }

    #[test]
    fn rate_limit_config_defaults_match_documented() {
        let c = RateLimitConfig::default();
        assert_eq!(c.ai_calls_per_min, 60);
        assert_eq!(c.dashboard_polls_per_min, 120);
        assert_eq!(c.task_mutations_per_min, 30);
        assert_eq!(c.ws_max_concurrent, 5);
    }
}
