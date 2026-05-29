//! Integration tests for `LiveSlackApi`. Stands up a localhost
//! axum server that emulates Slack's Web API surface (auth.test
//! / conversations.history / chat.postMessage / chat.update) and
//! points `LiveSlackApi` at it via `with_base_url`. No network —
//! runs offline in CI.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::post,
};
use serde_json::json;
use tokio::net::TcpListener;

use relix_slack::{LiveSlackApi, OutgoingMessage, SlackApi, SlackApiError};

#[derive(Default)]
struct MockState {
    send_calls: AtomicU32,
    send_script: tokio::sync::Mutex<Vec<MockAction>>,
    last_auth: tokio::sync::Mutex<String>,
}

#[derive(Clone, Debug)]
enum MockAction {
    Ok,
    RateLimited { retry_after: u32 },
    ServerError,
    OkFalse(&'static str),
}

async fn start_mock_server(state: Arc<MockState>) -> String {
    let app = Router::new()
        .route("/auth.test", post(handle_auth_test))
        .route("/conversations.history", post(handle_history))
        .route("/chat.postMessage", post(handle_post_message))
        .route("/chat.update", post(handle_update))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn record_auth(state: &Arc<MockState>, headers: &HeaderMap) {
    let v = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    if let Ok(mut g) = state.last_auth.try_lock() {
        *g = v;
    }
}

async fn handle_auth_test(
    State(s): State<Arc<MockState>>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    record_auth(&s, &headers);
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if auth == "Bearer xoxb-bad-token" {
        return Json(json!({ "ok": false, "error": "invalid_auth" }));
    }
    Json(json!({
        "ok": true,
        "user_id": "U999",
        "team_id": "T999",
        "bot_id": "B999",
        "user": "relixbot"
    }))
}

async fn handle_history(
    State(s): State<Arc<MockState>>,
    headers: HeaderMap,
    Json(_body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    record_auth(&s, &headers);
    Json(json!({
        "ok": true,
        "messages": [
            { "ts": "1700000002.000100", "user": "U0", "text": "second" },
            { "ts": "1700000001.500200", "text": "system msg", "subtype": "channel_join" },
            { "ts": "1700000001.000100", "text": "other bot says hi", "bot_id": "B0123" },
            { "ts": "1700000000.000100", "user": "U0", "text": "first" }
        ]
    }))
}

async fn handle_post_message(
    State(s): State<Arc<MockState>>,
    headers: HeaderMap,
    Json(_body): Json<serde_json::Value>,
) -> impl IntoResponse {
    record_auth(&s, &headers);
    let n = s.send_calls.fetch_add(1, Ordering::SeqCst) as usize;
    let script = s.send_script.lock().await;
    let action = script.get(n).cloned().unwrap_or(MockAction::Ok);
    drop(script);
    match action {
        MockAction::Ok => (
            StatusCode::OK,
            Json(json!({ "ok": true, "ts": "1700000010.000100" })),
        )
            .into_response(),
        MockAction::RateLimited { retry_after } => {
            let mut h = HeaderMap::new();
            h.insert(
                axum::http::header::RETRY_AFTER,
                HeaderValue::from_str(&retry_after.to_string()).unwrap(),
            );
            (StatusCode::TOO_MANY_REQUESTS, h, Json(json!({}))).into_response()
        }
        MockAction::ServerError => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({}))).into_response()
        }
        MockAction::OkFalse(err) => {
            (StatusCode::OK, Json(json!({ "ok": false, "error": err }))).into_response()
        }
    }
}

async fn handle_update(
    State(s): State<Arc<MockState>>,
    headers: HeaderMap,
    Json(_body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    record_auth(&s, &headers);
    Json(json!({ "ok": true, "ts": "1700000010.000100" }))
}

#[tokio::test]
async fn auth_test_returns_identity_on_ok() {
    let state = Arc::new(MockState::default());
    let base = start_mock_server(state.clone()).await;
    let api = LiveSlackApi::with_base_url("xoxb-good".into(), base);
    let id = api.auth_test().await.expect("auth_test");
    assert_eq!(id.user_id, "U999");
    assert_eq!(id.team_id, "T999");
    assert_eq!(id.bot_id, "B999");
    assert_eq!(id.username, "relixbot");
    // Auth header was the `Bearer xoxb-...` form (not `Bot ...`).
    let auth = state.last_auth.lock().await.clone();
    assert_eq!(auth, "Bearer xoxb-good");
}

#[tokio::test]
async fn auth_test_maps_ok_false_to_client_error() {
    let state = Arc::new(MockState::default());
    let base = start_mock_server(state).await;
    let api = LiveSlackApi::with_base_url("xoxb-bad-token".into(), base);
    let err = api.auth_test().await.expect_err("must error");
    match err {
        SlackApiError::ClientError(msg) => assert!(msg.contains("invalid_auth")),
        other => panic!("expected ClientError, got {other:?}"),
    }
}

#[tokio::test]
async fn conversations_history_filters_subtypes_and_bot_messages() {
    let state = Arc::new(MockState::default());
    let base = start_mock_server(state).await;
    let api = LiveSlackApi::with_base_url("xoxb-good".into(), base);
    let msgs = api.conversations_history("C0", "").await.expect("history");
    // Mock returns 4 items; subtype and bot_id are filtered out
    // at the parse layer.
    assert_eq!(msgs.len(), 2);
    // Reversed to chronological order.
    assert_eq!(msgs[0].ts, "1700000000.000100");
    assert_eq!(msgs[0].text, "first");
    assert_eq!(msgs[1].ts, "1700000002.000100");
    assert_eq!(msgs[1].text, "second");
}

#[tokio::test]
async fn post_message_posts_to_chat_endpoint() {
    let state = Arc::new(MockState::default());
    let base = start_mock_server(state.clone()).await;
    let api = LiveSlackApi::with_base_url("xoxb-good".into(), base);
    api.chat_post_message(&OutgoingMessage {
        channel_id: "C0".into(),
        thread_ts: "1700000000.000100".into(),
        text: "hello".into(),
        blocks: Vec::new(),
    })
    .await
    .expect("post");
    assert_eq!(state.send_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn chat_update_calls_update_endpoint() {
    let state = Arc::new(MockState::default());
    let base = start_mock_server(state).await;
    let api = LiveSlackApi::with_base_url("xoxb-good".into(), base);
    api.chat_update("C0", "1700000000.000100", "edited")
        .await
        .expect("update");
}

#[tokio::test]
async fn post_message_retries_on_429_then_succeeds() {
    let state = Arc::new(MockState::default());
    {
        let mut g = state.send_script.lock().await;
        // First call: 429 with Retry-After=1 (clamped to >=1s).
        // Second call: succeeds.
        *g = vec![MockAction::RateLimited { retry_after: 1 }, MockAction::Ok];
    }
    let base = start_mock_server(state.clone()).await;
    let api = LiveSlackApi::with_base_url("xoxb-good".into(), base);
    api.chat_post_message(&OutgoingMessage {
        channel_id: "C0".into(),
        thread_ts: String::new(),
        text: "x".into(),
        blocks: Vec::new(),
    })
    .await
    .expect("must succeed after retry");
    assert_eq!(state.send_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn post_message_retries_on_5xx_then_succeeds() {
    let state = Arc::new(MockState::default());
    {
        let mut g = state.send_script.lock().await;
        *g = vec![MockAction::ServerError, MockAction::Ok];
    }
    let base = start_mock_server(state.clone()).await;
    let api = LiveSlackApi::with_base_url("xoxb-good".into(), base);
    api.chat_post_message(&OutgoingMessage {
        channel_id: "C0".into(),
        thread_ts: String::new(),
        text: "y".into(),
        blocks: Vec::new(),
    })
    .await
    .expect("must succeed after backoff");
    assert_eq!(state.send_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn post_message_ok_false_is_client_error_not_retried() {
    let state = Arc::new(MockState::default());
    {
        let mut g = state.send_script.lock().await;
        *g = vec![MockAction::OkFalse("channel_not_found")];
    }
    let base = start_mock_server(state.clone()).await;
    let api = LiveSlackApi::with_base_url("xoxb-good".into(), base);
    let err = api
        .chat_post_message(&OutgoingMessage {
            channel_id: "C0".into(),
            thread_ts: String::new(),
            text: "z".into(),
            blocks: Vec::new(),
        })
        .await
        .expect_err("must error on ok=false");
    match err {
        SlackApiError::ClientError(msg) => assert!(msg.contains("channel_not_found")),
        other => panic!("expected ClientError, got {other:?}"),
    }
    // Exactly one attempt — ok=false is not retried.
    assert_eq!(state.send_calls.load(Ordering::SeqCst), 1);
}
