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

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::activity::{ToolInvocationActivity, append_tool_invocation_activity};
use crate::config::AppState;
use crate::tenant::{DEFAULT_TENANT, current_subject, current_tenant};

const DEFAULT_PEER: &str = "tool";

#[derive(Debug, Deserialize)]
pub struct CapturesQuery {
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
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
    let task_id = clean_optional_id(q.task_id.as_deref(), "task_id")?;
    let run_id = clean_optional(q.run_id.as_deref());
    let peer = q.peer.as_deref().unwrap_or(DEFAULT_PEER);
    let bytes = match call_peer_bytes(
        &state,
        peer,
        "tool.browser.capture_read",
        filename.as_bytes(),
        task_id.as_deref(),
    )
    .await
    {
        Ok(bytes) => {
            record_capture_activity(
                &state,
                peer,
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &filename,
            );
            bytes
        }
        Err(err) => {
            record_capture_activity(
                &state,
                peer,
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &filename,
            );
            return Err(err);
        }
    };
    let mut builder = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "image/png")
        // Captures are immutable once written (filename is
        // unique per failure), so a short cache is safe and
        // saves repeat fetches when an operator scrolls back
        // and forth in the chronicle.
        .header(axum::http::header::CACHE_CONTROL, "public, max-age=60")
        .header("X-Frame-Options", "DENY");
    if let Some(task_id) = task_id.as_deref() {
        builder = builder.header("X-Relix-Task-Id", task_id);
    }
    if let Some(run_id) = run_id.as_deref() {
        builder = builder.header("X-Relix-Run-Id", run_id);
    }
    Ok(builder
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
    task_id: Option<&str>,
) -> Result<Vec<u8>, (StatusCode, Json<ApiError>)> {
    let mesh = state.mesh_client.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError {
            error: "bridge mesh client not initialized (peer discovery failed at startup)".into(),
        }),
    ))?;
    let envelope = build_request_with_tenant(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        state.cfg.transport.deadline_secs,
        None,
        None,
        task_id.map(str::to_string),
        crate::tenant::current_tenant_or_none(),
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

fn record_capture_activity(
    state: &AppState,
    peer: &str,
    task_id: Option<&str>,
    run_id: Option<&str>,
    decision: &str,
    filename: &str,
) {
    let tenant_id = current_tenant().unwrap_or_else(|| DEFAULT_TENANT.to_string());
    let actor = current_subject().unwrap_or_else(|| "tool.browser.capture_read".to_string());
    let detail = format!("filename={filename}; bytes_format=png");
    if let Err(e) = append_tool_invocation_activity(
        state.cfg.transport.data_dir.as_deref(),
        ToolInvocationActivity {
            tenant_id: &tenant_id,
            actor: &actor,
            peer,
            method: "tool.browser.capture_read",
            task_id,
            run_id,
            decision,
            detail: &detail,
        },
    ) {
        tracing::warn!(error = %e, "failed to append browser capture activity");
    }
    if let (Some(rec), Some(task_id)) = (state.task_recorder.as_ref(), task_id) {
        let payload = format!("peer={peer} outcome={decision} {detail}");
        let rec = rec.clone();
        let task_id = task_id.to_string();
        tokio::spawn(async move {
            rec.event(&task_id, "tool.browser.capture_read", &payload)
                .await;
        });
    }
}

fn clean_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn clean_optional_id(
    value: Option<&str>,
    field: &str,
) -> Result<Option<String>, (StatusCode, Json<ApiError>)> {
    let Some(clean) = clean_optional(value) else {
        return Ok(None);
    };
    if clean.len() == 32 && clean.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(Some(clean))
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: format!("{field} must be 32 hex chars"),
            }),
        ))
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

    #[test]
    fn captures_query_accepts_task_and_run_context() {
        let q: CapturesQuery = serde_json::from_value(serde_json::json!({
            "peer": "tool",
            "task_id": "0123456789abcdef0123456789abcdef",
            "run_id": "run-1"
        }))
        .unwrap();
        assert_eq!(q.peer.as_deref(), Some("tool"));
        assert_eq!(
            q.task_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(q.run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn clean_optional_id_rejects_invalid_task_id() {
        assert!(clean_optional_id(None, "task_id").unwrap().is_none());
        assert_eq!(
            clean_optional_id(Some(" 0123456789abcdef0123456789abcdef "), "task_id")
                .unwrap()
                .as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        let err = clean_optional_id(Some("bad"), "task_id").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.0.error, "task_id must be 32 hex chars");
    }
}
