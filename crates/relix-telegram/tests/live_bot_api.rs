//! Integration tests for `LiveBotApi`. Each test stands up a
//! localhost axum server that emulates the Bot API surface
//! (just the methods we exercise) and points `LiveBotApi` at
//! it via `with_base_url`. No network — these run offline in
//! CI.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use serde_json::json;
use tokio::net::TcpListener;

use relix_telegram::{BotApi, BotApiError, LiveBotApi, OutgoingMessage, ParseMode};

/// Per-test mock state. The handlers count attempts so the
/// retry-path tests can assert "retried until success."
#[derive(Default)]
struct MockState {
    /// For the retry tests: count `sendMessage` calls and
    /// switch on the configured behaviour.
    send_calls: AtomicU32,
    /// Behaviour the next-N `sendMessage` calls should
    /// exhibit. Indexed by attempt (0-based).
    send_script: tokio::sync::Mutex<Vec<MockAction>>,
}

#[derive(Clone, Debug)]
enum MockAction {
    Ok,
    RateLimited { retry_after: u32 },
    ServerError,
    Unauthorized,
}

async fn start_mock_server(state: Arc<MockState>) -> String {
    let app = Router::new()
        .route("/bot:token/getMe", post(handle_get_me))
        .route("/bot:token/getUpdates", post(handle_get_updates))
        .route("/bot:token/sendMessage", post(handle_send_message))
        .route("/bot:token/sendChatAction", post(handle_send_chat_action))
        .route("/bot:token/editMessageText", post(handle_edit_message_text))
        .route(
            "/bot:token/answerCallbackQuery",
            post(handle_answer_callback_query),
        )
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn handle_get_me(
    State(_s): State<Arc<MockState>>,
    Path(token): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if token == "bad-token" {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok": false, "description": "Unauthorized"})),
        ));
    }
    Ok(Json(json!({
        "ok": true,
        "result": {
            "id": 12345,
            "username": "relixbot",
            "first_name": "Relix"
        }
    })))
}

async fn handle_get_updates(
    State(_s): State<Arc<MockState>>,
    Path(_token): Path<String>,
    Json(_body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "result": [
            {
                "update_id": 1,
                "message": {
                    "message_id": 5,
                    "from": { "id": 42, "username": "alice" },
                    "chat": { "id": 100 },
                    "text": "hello"
                }
            }
        ]
    }))
}

async fn handle_send_message(
    State(s): State<Arc<MockState>>,
    Path(_token): Path<String>,
    Json(_body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let n = s.send_calls.fetch_add(1, Ordering::SeqCst) as usize;
    let script = s.send_script.lock().await;
    let action = script.get(n).cloned().unwrap_or(MockAction::Ok);
    drop(script);
    match action {
        MockAction::Ok => Ok(Json(json!({"ok": true, "result": true}))),
        MockAction::RateLimited { retry_after } => Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "ok": false,
                "description": "Too Many Requests",
                "parameters": { "retry_after": retry_after }
            })),
        )),
        MockAction::ServerError => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "description": "boom"})),
        )),
        MockAction::Unauthorized => Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok": false, "description": "Unauthorized"})),
        )),
    }
}

async fn handle_send_chat_action(
    State(_s): State<Arc<MockState>>,
    Path(_token): Path<String>,
    Json(_body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    Json(json!({"ok": true, "result": true}))
}

async fn handle_edit_message_text(
    State(_s): State<Arc<MockState>>,
    Path(_token): Path<String>,
    Json(_body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    Json(json!({"ok": true, "result": true}))
}

async fn handle_answer_callback_query(
    State(_s): State<Arc<MockState>>,
    Path(_token): Path<String>,
    Json(_body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    Json(json!({"ok": true, "result": true}))
}

// ── Tests ────────────────────────────────────────────────

#[tokio::test]
async fn get_me_returns_identity_on_success() {
    let s = Arc::new(MockState::default());
    let base = start_mock_server(s).await;
    let api = LiveBotApi::with_base_url("good-token".into(), base);
    let id = api.get_me().await.unwrap();
    assert_eq!(id.user_id, 12345);
    assert_eq!(id.username, "relixbot");
    assert_eq!(id.first_name, "Relix");
}

#[tokio::test]
async fn get_me_returns_client_error_on_bad_token() {
    let s = Arc::new(MockState::default());
    let base = start_mock_server(s).await;
    let api = LiveBotApi::with_base_url("bad-token".into(), base);
    let err = api.get_me().await.unwrap_err();
    match err {
        BotApiError::ClientError(msg) => {
            assert!(msg.contains("401") || msg.contains("Unauthorized"));
        }
        other => panic!("expected ClientError, got {other:?}"),
    }
}

#[tokio::test]
async fn get_updates_decodes_text_message() {
    let s = Arc::new(MockState::default());
    let base = start_mock_server(s).await;
    let api = LiveBotApi::with_base_url("good-token".into(), base);
    let updates = api.get_updates(0).await.unwrap();
    assert_eq!(updates.len(), 1);
    let m = &updates[0];
    assert_eq!(m.chat_id, 100);
    assert_eq!(m.user_id, 42);
    assert_eq!(m.message_id, 5);
    assert_eq!(m.text, "hello");
    assert_eq!(m.username, "alice");
}

#[tokio::test]
async fn send_message_succeeds_on_first_try() {
    let s = Arc::new(MockState::default());
    let base = start_mock_server(s.clone()).await;
    let api = LiveBotApi::with_base_url("t".into(), base);
    let out = OutgoingMessage {
        chat_id: 100,
        reply_to_message_id: 5,
        text: "hi".into(),
        parse_mode: None,
        reply_markup: None,
    };
    api.send_message(&out).await.unwrap();
    assert_eq!(s.send_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn send_message_retries_5xx_then_succeeds() {
    let s = Arc::new(MockState::default());
    {
        let mut script = s.send_script.lock().await;
        script.push(MockAction::ServerError);
        script.push(MockAction::ServerError);
        script.push(MockAction::Ok);
    }
    let base = start_mock_server(s.clone()).await;
    let api = LiveBotApi::with_base_url("t".into(), base);
    let out = OutgoingMessage {
        chat_id: 100,
        reply_to_message_id: 0,
        text: "retry me".into(),
        parse_mode: None,
        reply_markup: None,
    };
    // Patch backoff via tokio's time pause is fiddly across
    // reqwest's blocking I/O; instead, this test runs in real
    // time but the backoff is fast (1s + 2s).
    tokio::time::timeout(Duration::from_secs(15), api.send_message(&out))
        .await
        .expect("retry path completes within budget")
        .unwrap();
    assert_eq!(s.send_calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn send_message_honours_429_retry_after_zero_succeeds() {
    let s = Arc::new(MockState::default());
    {
        let mut script = s.send_script.lock().await;
        script.push(MockAction::RateLimited { retry_after: 1 });
        script.push(MockAction::Ok);
    }
    let base = start_mock_server(s.clone()).await;
    let api = LiveBotApi::with_base_url("t".into(), base);
    let out = OutgoingMessage {
        chat_id: 100,
        reply_to_message_id: 0,
        text: "rate-limited then ok".into(),
        parse_mode: None,
        reply_markup: None,
    };
    tokio::time::timeout(Duration::from_secs(10), api.send_message(&out))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s.send_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn send_message_does_not_retry_unauthorized() {
    let s = Arc::new(MockState::default());
    {
        let mut script = s.send_script.lock().await;
        script.push(MockAction::Unauthorized);
    }
    let base = start_mock_server(s.clone()).await;
    let api = LiveBotApi::with_base_url("t".into(), base);
    let out = OutgoingMessage {
        chat_id: 100,
        reply_to_message_id: 0,
        text: "x".into(),
        parse_mode: None,
        reply_markup: None,
    };
    let err = api.send_message(&out).await.unwrap_err();
    match err {
        BotApiError::ClientError(_) => {}
        other => panic!("expected ClientError, got {other:?}"),
    }
    // Critically: only ONE attempt.
    assert_eq!(s.send_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn send_chat_action_typing_round_trips() {
    let s = Arc::new(MockState::default());
    let base = start_mock_server(s).await;
    let api = LiveBotApi::with_base_url("t".into(), base);
    api.send_chat_action(100, "typing").await.unwrap();
}

#[tokio::test]
async fn edit_message_text_with_markdown_v2() {
    let s = Arc::new(MockState::default());
    let base = start_mock_server(s).await;
    let api = LiveBotApi::with_base_url("t".into(), base);
    api.edit_message_text(100, 5, "*done*", Some(ParseMode::MarkdownV2))
        .await
        .unwrap();
}

#[tokio::test]
async fn answer_callback_query_with_toast() {
    let s = Arc::new(MockState::default());
    let base = start_mock_server(s).await;
    let api = LiveBotApi::with_base_url("t".into(), base);
    api.answer_callback_query("cb-1", Some("approved"))
        .await
        .unwrap();
}
