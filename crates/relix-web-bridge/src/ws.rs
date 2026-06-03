//! `GET /ws/chat` — WebSocket streaming chat endpoint.
//!
//! The client opens a WebSocket with `Authorization: Bearer <token>`
//! (a missing or empty header returns 401 before the upgrade).
//! After the upgrade, the client sends ONE JSON message:
//!
//! ```json
//! { "session_id": "...", "message": "...", "model": "..." }
//! ```
//!
//! `model` is optional and currently informational only — the
//! provider routing lives on the AI node, not in the bridge.
//!
//! The server replies with a stream of JSON messages, one per
//! frame, terminated by a `done` (or `error` on failure):
//!
//! ```json
//! { "type": "chunk", "text": "Hello" }
//! { "type": "chunk", "text": " world" }
//! { "type": "done",  "session_id": "...", "text": "Hello world" }
//! { "type": "error", "message": "..." }
//! ```
//!
//! ## Where the streaming actually happens
//!
//! The `ChatProvider` trait now has a real `generate_reply_stream`
//! method (mock + OpenAI-compatible providers override it; the
//! default impl wraps `generate_reply`). Through the **mesh**
//! though, the alpha bridge calls the AI peer via the synchronous
//! `ai.chat` capability — the chat flow runs to completion before
//! the bridge sees the final reply. This endpoint therefore
//! delivers the bridge-level chunking shape (same as the existing
//! `/chat/stream` SSE), now framed over WebSocket and using the
//! same word-by-word splitter the mock provider streams with.
//! End-to-end provider-native streaming through the mesh requires
//! libp2p stream support and lands post-alpha; the trait-level
//! API exists today so the wire and client code don't need to
//! change when that lands.

use std::time::Duration;

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::AppState;
use crate::flow::{FlowExecError, execute_chat_flow};

#[derive(Debug, Deserialize)]
struct WsRequest {
    session_id: String,
    message: String,
    #[serde(default)]
    workspace_lease_id: Option<String>,
    /// Reserved for future per-call model override; currently
    /// informational. Provider routing lives on the AI node.
    #[serde(default)]
    #[allow(dead_code)]
    model: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChunkMsg<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

/// `GET /ws/chat`. Validates the `Authorization: Bearer <token>`
/// header on the upgrade request, takes one slot from the
/// per-principal concurrent-WS rate limit, then hands the socket
/// off to [`run_ws_session`]. The rate-limit guard is moved into
/// the session so it drops (and frees the slot) only when the
/// socket actually closes.
pub async fn chat_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    // SEC PART 3: WS upgrade runs the full bearer
    // validation pipeline — presence, scheme, non-empty,
    // AND constant-time compare against the bridge token.
    // Pre-fix path accepted any non-empty bearer.
    if let Err(reason) = parse_bearer(&headers, state.bridge_token.value()) {
        return (StatusCode::UNAUTHORIZED, reason).into_response();
    }
    let principal = ws_principal(&headers);
    let guard = match state.rate_limits.ws_acquire(&principal) {
        Ok(g) => g,
        Err(limit) => {
            return ws_limit_response(limit);
        }
    };
    upgrade.on_upgrade(move |socket| run_ws_session(socket, state, guard))
}

fn ws_principal(headers: &HeaderMap) -> String {
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "anon".to_string())
}

/// 429 response mirroring the HTTP rate-limit middleware's
/// shape: `retry_after_secs` body field + matching
/// `Retry-After` header. Used when the per-principal concurrent
/// WebSocket cap is exceeded.
fn ws_limit_response(limit: u32) -> Response {
    use axum::http::HeaderValue;
    use axum::http::header::RETRY_AFTER;
    let body = json!({
        "error": "rate_limit_exceeded",
        "retry_after_secs": 60,
        "ws_limit": limit,
    })
    .to_string();
    let mut resp = (StatusCode::TOO_MANY_REQUESTS, body).into_response();
    resp.headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static("60"));
    resp
}

/// Run one WebSocket chat session end-to-end. Reads the request,
/// executes the chat flow, splits the materialised reply
/// word-by-word, streams chunks over the socket, and finishes
/// with a `done` or `error` JSON message. The `_ws_guard`
/// argument owns the per-principal rate-limit slot; the slot is
/// released when the function returns (success, error, or panic).
async fn run_ws_session(
    mut socket: WebSocket,
    state: AppState,
    _ws_guard: crate::rate_limit::WsGuard,
) {
    let Some(req) = read_request(&mut socket).await else {
        return;
    };

    match execute_chat_flow(
        &state,
        &req.session_id,
        &req.message,
        req.workspace_lease_id.as_deref(),
    )
    .await
    {
        Ok(outcome) => {
            stream_reply(
                &mut socket,
                &req.session_id,
                &outcome.reply,
                outcome.workspace_lease_id.as_deref(),
                outcome.workspace_path.as_deref(),
            )
            .await;
        }
        Err(e) => {
            let msg = match e {
                FlowExecError::InvalidInput(s) => s,
                FlowExecError::Transport(s) => format!("mesh transport: {s}"),
                FlowExecError::Unavailable(s) => s,
                FlowExecError::Internal(s) => s,
            };
            let payload = json!({ "type": "error", "message": msg }).to_string();
            let _ = socket.send(Message::Text(payload)).await;
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

/// Read the client's opening JSON request. Returns `None` if the
/// socket closes early, the payload is binary, or the JSON
/// doesn't parse — in each of those cases we just hang up
/// silently (no `error` frame, since the protocol assumes a
/// request message and we never received one).
async fn read_request(socket: &mut WebSocket) -> Option<WsRequest> {
    use futures::StreamExt;
    let msg = socket.next().await?.ok()?;
    let text = match msg {
        Message::Text(t) => t,
        Message::Binary(b) => String::from_utf8(b).ok()?,
        _ => return None,
    };
    serde_json::from_str::<WsRequest>(&text).ok()
}

/// Split `reply` into word-sized pieces with a 20ms gap between
/// frames so a human watching the dashboard sees the response
/// appearing word-by-word. Always finishes with a `done` frame
/// carrying the full assembled text.
async fn stream_reply(
    socket: &mut WebSocket,
    session_id: &str,
    reply: &str,
    workspace_lease_id: Option<&str>,
    workspace_path: Option<&str>,
) {
    let chunks = split_words(reply);
    for (i, chunk) in chunks.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let payload = serde_json::to_string(&ChunkMsg {
            kind: "chunk",
            text: chunk,
        })
        .unwrap_or_else(|_| String::new());
        if socket.send(Message::Text(payload)).await.is_err() {
            return;
        }
    }
    let done = json!({
        "type":       "done",
        "session_id": session_id,
        "text":       reply,
        "workspace_lease_id": workspace_lease_id,
        "workspace_path": workspace_path,
    })
    .to_string();
    let _ = socket.send(Message::Text(done)).await;
}

/// SEC PART 3: verify the WebSocket-upgrade bearer header
/// against the expected bridge token, identical to the
/// HTTP bearer pipeline. Pre-fix path accepted ANY non-empty
/// token, so a local process could open a WS by passing
/// `Bearer x`. The full validation pipeline runs here:
/// presence, scheme, non-empty, AND constant-time compare
/// against the expected bridge token. Missing header /
/// wrong scheme / empty token / mismatched token all
/// return a structured error suitable for the 401 body.
pub fn parse_bearer(headers: &HeaderMap, expected_token: &str) -> Result<(), &'static str> {
    let Some(raw) = headers.get(AUTHORIZATION) else {
        return Err("missing Authorization: Bearer <token> header\n");
    };
    let Ok(value) = raw.to_str() else {
        return Err("Authorization header is not valid UTF-8\n");
    };
    let mut parts = value.splitn(2, char::is_whitespace);
    let scheme = parts.next().unwrap_or("").trim();
    let token = parts.next().unwrap_or("").trim();
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return Err("Authorization header must use the Bearer scheme\n");
    }
    if token.is_empty() {
        return Err("Authorization Bearer token is empty\n");
    }
    if !ct_eq_ws_token(token, expected_token) {
        return Err("Authorization Bearer token did not match\n");
    }
    Ok(())
}

/// Constant-time string compare for the WS bearer path so
/// per-byte short-circuits do not leak token-prefix timing.
fn ct_eq_ws_token(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.as_bytes().iter().zip(b.as_bytes().iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Split a string into word-shaped emission chunks. Whitespace is
/// attached to the preceding word so concatenating the result
/// reproduces the original. Lossless and round-trip safe.
pub fn split_words(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in s.chars() {
        current.push(ch);
        if ch.is_whitespace() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn hdrs(bearer: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(b) = bearer {
            h.insert(AUTHORIZATION, HeaderValue::from_str(b).unwrap());
        }
        h
    }

    #[test]
    fn parse_bearer_accepts_matching_token() {
        // SEC PART 3: the WS pipeline now compares the
        // presented token against the expected bridge
        // token. Non-empty alone is no longer sufficient.
        assert!(parse_bearer(&hdrs(Some("Bearer abc123")), "abc123").is_ok());
        assert!(parse_bearer(&hdrs(Some("bearer abc123")), "abc123").is_ok());
    }

    #[test]
    fn parse_bearer_rejects_missing_header() {
        let e = parse_bearer(&hdrs(None), "anything").unwrap_err();
        assert!(e.contains("missing"));
    }

    #[test]
    fn parse_bearer_rejects_wrong_scheme() {
        let e = parse_bearer(&hdrs(Some("Basic dXNlcjpwYXNz")), "anything").unwrap_err();
        assert!(e.contains("Bearer"));
    }

    #[test]
    fn parse_bearer_rejects_empty_token() {
        let e = parse_bearer(&hdrs(Some("Bearer ")), "anything").unwrap_err();
        assert!(e.contains("empty"));
        let e = parse_bearer(&hdrs(Some("Bearer    ")), "anything").unwrap_err();
        assert!(e.contains("empty"));
    }

    #[test]
    fn sec_p3_parse_bearer_rejects_mismatched_token() {
        // SEC PART 3: the WS pipeline runs the full bearer
        // check. A bearer whose value does not match the
        // bridge token is refused with a "did not match"
        // message, not silently admitted.
        let e = parse_bearer(&hdrs(Some("Bearer wrong-token")), "right-token").unwrap_err();
        assert!(e.contains("did not match"), "got {e}");
    }

    #[test]
    fn split_words_round_trips_through_join() {
        let s = "Hello world\nThis is a test";
        let chunks = split_words(s);
        assert_eq!(chunks.concat(), s);
        assert_eq!(chunks[0], "Hello ");
        assert_eq!(chunks[1], "world\n");
    }

    #[test]
    fn split_words_empty_yields_nothing() {
        assert!(split_words("").is_empty());
    }

    #[test]
    fn ws_request_deserialises_with_optional_model() {
        let r: WsRequest =
            serde_json::from_str(r#"{"session_id":"s1","message":"hi"}"#).expect("parse");
        assert_eq!(r.session_id, "s1");
        assert_eq!(r.message, "hi");
        assert!(r.model.is_none());
        let r: WsRequest =
            serde_json::from_str(r#"{"session_id":"s1","message":"hi","model":"relix-mock"}"#)
                .expect("parse");
        assert_eq!(r.model.as_deref(), Some("relix-mock"));
    }
}
