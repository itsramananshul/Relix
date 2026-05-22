//! W2-002g — HTTP proxy for `tool.browser.capture_read`.
//! Lets the dashboard fetch failure screenshots back from
//! whichever peer ran the browser session that produced them.
//!
//! One endpoint:
//!
//! - `GET /v1/browser/captures/:filename?peer=<alias>` —
//!   proxies `tool.browser.capture_read(<filename>)`. On
//!   success returns the raw PNG bytes with
//!   `Content-Type: image/png` + a modest cache header. On
//!   failure returns JSON `{ "error": "..." }` with a
//!   matching status (400/404/502/503).
//!
//! Defence in depth: even though the runtime side
//! (W2-002f) validates the filename, the bridge re-validates
//! using the same rules so an obviously-bad URL like
//! `/v1/browser/captures/..%2Fpasswd` never even hits the
//! mesh — the bridge denies it locally with 400.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::Response,
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "tool";

#[derive(Debug, Deserialize)]
pub struct CapturesQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

pub async fn capture(
    State(state): State<AppState>,
    Path(filename): Path<String>,
    Query(q): Query<CapturesQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    // Pre-flight validation mirrors the runtime rules. Catches
    // the obvious attacks at the edge so we don't waste a mesh
    // RTT on a request the responder would reject anyway.
    if let Err(msg) = validate_filename(&filename) {
        return Err((StatusCode::BAD_REQUEST, Json(ApiError { error: msg })));
    }
    let peer = q.peer.as_deref().unwrap_or(DEFAULT_PEER);
    let bytes = call_peer_bytes(
        &state,
        peer,
        "tool.browser.capture_read",
        filename.as_bytes(),
    )
    .await?;
    Ok(axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "image/png")
        // Captures are immutable once written (filename is
        // unique per failure), so a short cache is safe and
        // saves repeat fetches when an operator scrolls back
        // and forth in the chronicle.
        .header(axum::http::header::CACHE_CONTROL, "public, max-age=60")
        .header("X-Frame-Options", "DENY")
        .body(axum::body::Body::from(bytes))
        .expect("captures response builds"))
}

/// Bridge-side filename validation. Matches the runtime
/// (`handle_capture_read`) rules byte-for-byte.
pub fn validate_filename(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("filename required".into());
    }
    if name.len() > 256 {
        return Err("filename too long (>256)".into());
    }
    if name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.contains('\0')
        || name.contains(':')
    {
        return Err(format!(
            "unsafe filename '{name}' (path separators, '..', NUL, and ':' rejected)"
        ));
    }
    if !name.to_ascii_lowercase().ends_with(".png") {
        return Err(format!("filename '{name}' must end with .png"));
    }
    Ok(())
}

/// Variant of the existing `call_peer` helper that returns
/// raw bytes — the response body is a PNG, not UTF-8.
async fn call_peer_bytes(
    state: &AppState,
    alias: &str,
    method: &str,
    arg: &[u8],
) -> Result<Vec<u8>, (StatusCode, Json<ApiError>)> {
    let mesh = state.mesh_client.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError {
            error: "bridge mesh client not initialized (peer discovery failed at startup)".into(),
        }),
    ))?;
    let envelope = build_request(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        state.cfg.transport.deadline_secs,
    );
    let resp_bytes = mesh.call(alias, envelope).await.map_err(|e| {
        let msg = e.to_string();
        let lower = msg.to_ascii_lowercase();
        let status = if lower.contains("unknown alias") || lower.contains("no peer") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_GATEWAY
        };
        (status, Json(ApiError { error: msg }))
    })?;
    let resp = decode_response(&resp_bytes).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("decode response: {e}"),
            }),
        )
    })?;
    match resp.res {
        ResponseResult::Ok(body) => Ok(body.to_vec()),
        ResponseResult::Err(env) => {
            // INVALID_ARGS from the responder is the operator's
            // problem (bad filename / dir not configured) →
            // surface as 400. Everything else is the upstream's
            // problem → 502.
            let status = if env.kind == relix_core::types::error_kinds::INVALID_ARGS {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            };
            Err((
                status,
                Json(ApiError {
                    error: format!("responder err kind={} cause={}", env.kind, env.cause),
                }),
            ))
        }
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response from tool.browser.capture_read".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty() {
        let e = validate_filename("").unwrap_err();
        assert!(e.contains("required"), "got: {e}");
    }

    #[test]
    fn rejects_dotdot() {
        let e = validate_filename("../etc/passwd.png").unwrap_err();
        assert!(e.contains("unsafe"), "got: {e}");
    }

    #[test]
    fn rejects_forward_slash() {
        let e = validate_filename("sub/file.png").unwrap_err();
        assert!(e.contains("unsafe"), "got: {e}");
    }

    #[test]
    fn rejects_backslash() {
        let e = validate_filename("sub\\file.png").unwrap_err();
        assert!(e.contains("unsafe"), "got: {e}");
    }

    #[test]
    fn rejects_colon() {
        let e = validate_filename("C:foo.png").unwrap_err();
        assert!(e.contains("unsafe"), "got: {e}");
    }

    #[test]
    fn rejects_non_png() {
        let e = validate_filename("shot.jpg").unwrap_err();
        assert!(e.contains(".png"), "got: {e}");
    }

    #[test]
    fn rejects_too_long() {
        let name: String = std::iter::repeat_n('a', 300).collect::<String>() + ".png";
        let e = validate_filename(&name).unwrap_err();
        assert!(e.contains("too long"), "got: {e}");
    }

    #[test]
    fn accepts_typical_capture_filename() {
        // Format the runtime writes: `<sessionid>-<unix_ms>.png`.
        validate_filename("abc123def456-1700000000123.png").unwrap();
    }

    #[test]
    fn accepts_uppercase_png_extension() {
        validate_filename("CAPTURE.PNG").unwrap();
    }
}
