//! Integration tests for `LiveDiscordApi`. Each test stands up a
//! localhost axum server that emulates Discord's REST surface
//! (just the endpoints we hit) and points `LiveDiscordApi` at it
//! via `with_base_url`. No network — these run offline in CI.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post},
};
use serde_json::json;
use tokio::net::TcpListener;

use relix_discord::{DiscordApi, DiscordApiError, LiveDiscordApi, OutgoingMessage};

#[derive(Default)]
struct MockState {
    /// Count send-message attempts so retry tests can assert.
    send_calls: AtomicU32,
    /// Scripted send-message behaviour, indexed by attempt
    /// (0-based).
    send_script: tokio::sync::Mutex<Vec<MockAction>>,
    /// Last Authorization header seen — asserted to be
    /// `Bot <token>`.
    last_auth: tokio::sync::Mutex<String>,
}

#[derive(Clone, Debug)]
enum MockAction {
    Ok,
    RateLimited { retry_after: f64 },
    ServerError,
}

async fn start_mock_server(state: Arc<MockState>) -> String {
    let app = Router::new()
        .route("/users/@me", get(handle_get_me))
        .route(
            "/channels/:cid/messages",
            get(handle_get_messages).post(handle_send_message),
        )
        .route("/channels/:cid/typing", post(handle_send_typing))
        .route(
            "/channels/:cid/messages/:mid",
            delete(handle_delete_message),
        )
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

async fn handle_get_me(
    State(s): State<Arc<MockState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    record_auth(&s, &headers);
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if auth == "Bot bad-token" {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"message": "401: Unauthorized", "code": 0})),
        ));
    }
    Ok(Json(json!({
        "id": "12345",
        "username": "relixbot"
    })))
}

async fn handle_get_messages(
    State(s): State<Arc<MockState>>,
    Path(cid): Path<String>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    record_auth(&s, &headers);
    // Newest-first per Discord. Mirror that order in the response.
    Json(json!([
        {
            "id": "9002",
            "channel_id": cid,
            "content": "second",
            "author": { "id": "42", "username": "alice", "bot": false }
        },
        {
            "id": "9001",
            "channel_id": cid,
            "content": "first",
            "author": { "id": "42", "username": "alice", "bot": false }
        }
    ]))
}

async fn handle_send_message(
    State(s): State<Arc<MockState>>,
    Path(_cid): Path<String>,
    headers: HeaderMap,
    Json(_body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    record_auth(&s, &headers);
    let n = s.send_calls.fetch_add(1, Ordering::SeqCst) as usize;
    let script = s.send_script.lock().await;
    let action = script.get(n).cloned().unwrap_or(MockAction::Ok);
    drop(script);
    match action {
        MockAction::Ok => Ok(Json(json!({ "id": "9999" }))),
        MockAction::RateLimited { retry_after } => Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "message": "You are being rate limited.",
                "retry_after": retry_after,
                "global": false
            })),
        )),
        MockAction::ServerError => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({})))),
    }
}

async fn handle_send_typing(
    State(s): State<Arc<MockState>>,
    Path(_cid): Path<String>,
    headers: HeaderMap,
) -> StatusCode {
    record_auth(&s, &headers);
    StatusCode::NO_CONTENT
}

async fn handle_delete_message(
    State(s): State<Arc<MockState>>,
    Path(_p): Path<(String, String)>,
    headers: HeaderMap,
) -> StatusCode {
    record_auth(&s, &headers);
    StatusCode::NO_CONTENT
}

#[tokio::test]
async fn get_me_returns_identity_on_200() {
    let state = Arc::new(MockState::default());
    let base = start_mock_server(state.clone()).await;
    let api = LiveDiscordApi::with_base_url("good-token".into(), base);
    let id = api.get_me().await.expect("get_me");
    assert_eq!(id.user_id, "12345");
    assert_eq!(id.username, "relixbot");
    // Auth header was the `Bot <token>` form, not `Bearer`.
    let auth = state.last_auth.lock().await.clone();
    assert_eq!(auth, "Bot good-token");
}

#[tokio::test]
async fn get_me_returns_client_error_on_401() {
    let state = Arc::new(MockState::default());
    let base = start_mock_server(state).await;
    let api = LiveDiscordApi::with_base_url("bad-token".into(), base);
    let err = api.get_me().await.expect_err("must error");
    assert!(matches!(err, DiscordApiError::ClientError(_)));
}

#[tokio::test]
async fn get_messages_returns_chronological_order() {
    let state = Arc::new(MockState::default());
    let base = start_mock_server(state).await;
    let api = LiveDiscordApi::with_base_url("good-token".into(), base);
    let msgs = api.get_messages("100", "").await.expect("get_messages");
    assert_eq!(msgs.len(), 2);
    // The server returned newest-first; the client reversed so the
    // controller sees chronological order.
    assert_eq!(msgs[0].message_id, "9001");
    assert_eq!(msgs[0].content, "first");
    assert_eq!(msgs[1].message_id, "9002");
    assert_eq!(msgs[1].content, "second");
}

#[tokio::test]
async fn send_message_posts_to_channel_endpoint() {
    let state = Arc::new(MockState::default());
    let base = start_mock_server(state.clone()).await;
    let api = LiveDiscordApi::with_base_url("good-token".into(), base);
    api.send_message(&OutgoingMessage {
        channel_id: "100".into(),
        reply_to_message_id: "9000".into(),
        content: "hello".into(),
        components: Vec::new(),
    })
    .await
    .expect("send_message");
    assert_eq!(state.send_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn send_typing_posts_to_typing_endpoint() {
    let state = Arc::new(MockState::default());
    let base = start_mock_server(state.clone()).await;
    let api = LiveDiscordApi::with_base_url("good-token".into(), base);
    api.send_typing("100").await.expect("send_typing");
    let auth = state.last_auth.lock().await.clone();
    assert_eq!(auth, "Bot good-token");
}

#[tokio::test]
async fn delete_message_calls_delete_endpoint() {
    let state = Arc::new(MockState::default());
    let base = start_mock_server(state).await;
    let api = LiveDiscordApi::with_base_url("good-token".into(), base);
    api.delete_message("100", "9000").await.expect("delete");
}

#[tokio::test]
async fn send_message_retries_on_429_then_succeeds() {
    let state = Arc::new(MockState::default());
    {
        let mut g = state.send_script.lock().await;
        // First call: 429 with retry_after=0.05s (clamped to 1s
        // ceiling by the client). Second call: succeeds.
        *g = vec![
            MockAction::RateLimited { retry_after: 0.05 },
            MockAction::Ok,
        ];
    }
    let base = start_mock_server(state.clone()).await;
    let api = LiveDiscordApi::with_base_url("good-token".into(), base);
    api.send_message(&OutgoingMessage {
        channel_id: "100".into(),
        reply_to_message_id: String::new(),
        content: "x".into(),
        components: Vec::new(),
    })
    .await
    .expect("must succeed after retry");
    assert_eq!(state.send_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn send_message_retries_on_5xx_then_succeeds() {
    let state = Arc::new(MockState::default());
    {
        let mut g = state.send_script.lock().await;
        *g = vec![MockAction::ServerError, MockAction::Ok];
    }
    let base = start_mock_server(state.clone()).await;
    let api = LiveDiscordApi::with_base_url("good-token".into(), base);
    api.send_message(&OutgoingMessage {
        channel_id: "100".into(),
        reply_to_message_id: String::new(),
        content: "y".into(),
        components: Vec::new(),
    })
    .await
    .expect("must succeed after backoff");
    assert_eq!(state.send_calls.load(Ordering::SeqCst), 2);
}
