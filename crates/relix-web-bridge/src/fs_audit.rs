//! PH-BRIDGE-FS-AUDIT — HTTP proxy for `tool.fs.audit_recent` on a
//! tool node. Mirror of the PH-BRIDGE-MCP / PH-BRIDGE-MCP-AUDIT
//! pattern, but the data source is runtime-side (the per-jail
//! `FsAuditRing`), not bridge-side.
//!
//! One endpoint:
//!
//! - `GET /v1/fs/audit?peer=<alias>&max=<N>&op=<filter>` — proxies
//!   `tool.fs.audit_recent` to the named tool peer (default alias
//!   `"tool"`). Returns JSON:
//!
//!   ```json
//!   {
//!     "peer": "tool",
//!     "entries": [
//!       {"ts_secs": 1716, "op": "write",
//!        "rel_path": "notes.md", "bytes": 142,
//!        "caller_subject_id": "f00b…"}
//!     ],
//!     "count": 1
//!   }
//!   ```
//!
//! The audit ring lives on the tool node (per-jail, bounded at 256,
//! resets on tool-node restart) — same posture as the MCP audit ring
//! lives on the bridge. This proxy just translates the tab-delim
//! responder body into structured JSON for the dashboard.
//!
//! Wire-arg shape sent to the responder: when `op` is set, we send a
//! JSON arg `{"max": N, "op": "<op>"}`; otherwise we send the bare
//! integer max. Both forms are accepted by `tool.fs.audit_recent`.
//!
//! Fail modes (mirror PH-BRIDGE-MCP):
//! - Bridge mesh client not initialized → 503 ServiceUnavailable.
//! - Peer alias unknown → 404 NotFound.
//! - Any other dispatch failure → 502 BadGateway.
//! - `op` not in `{write, append, patch, fuzzy_replace}` → 400
//!   BadRequest (the responder enforces this; the bridge propagates
//!   the responder's `INVALID_ARGS` cause).

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use relix_core::types::error_kinds;
use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "tool";
/// Cap the operator-supplied `max` to a reasonable upper bound so a
/// bored client can't ask for a million rows. The runtime ring is
/// only 256 deep anyway, but the bridge enforces its own cap here.
const MAX_ROW_CAP: usize = 500;

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    #[serde(default)]
    pub peer: Option<String>,
    /// Maximum entries returned. Defaults to 100 when absent.
    #[serde(default)]
    pub max: Option<usize>,
    /// Optional op filter — one of `write`, `append`, `patch`,
    /// `fuzzy_replace`. Validated by the responder; the bridge
    /// only forwards.
    #[serde(default)]
    pub op: Option<String>,
}

/// PH-BRIDGE-FS-AUDIT: one row of the response. Field-for-field
/// with the runtime's `FsAuditEntry` so the dashboard / CLI can
/// drop the JSON straight into a typed view.
#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct FsAuditRow {
    pub ts_secs: i64,
    pub op: String,
    pub rel_path: String,
    pub bytes: usize,
    pub caller_subject_id: String,
}

#[derive(Debug, Serialize)]
pub struct AuditResponse {
    pub peer: String,
    pub entries: Vec<FsAuditRow>,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

pub async fn audit(
    State(state): State<AppState>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<AuditResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.as_deref().unwrap_or(DEFAULT_PEER).to_string();
    let max = q.max.unwrap_or(100).clamp(1, MAX_ROW_CAP);
    let arg = build_arg(max, q.op.as_deref());
    let body = call_peer(&state, &peer, "tool.fs.audit_recent", arg.as_bytes()).await?;
    let entries = parse_entries(&body);
    let count = entries.len();
    Ok(Json(AuditResponse {
        peer,
        entries,
        count,
    }))
}

/// PH-BRIDGE-FS-AUDIT: build the wire arg for
/// `tool.fs.audit_recent`. When `op` is set, send a JSON arg;
/// otherwise send the bare integer. Both forms are accepted by
/// the responder (see `relix_runtime::nodes::tool::fs::handle_audit_recent`).
fn build_arg(max: usize, op: Option<&str>) -> String {
    match op {
        Some(op) if !op.is_empty() => {
            // Hand-rolled JSON (no need to pull serde_json for two
            // fields). max is a usize so escaping is trivial; op is
            // already validated for ascii by the responder, but we
            // additionally quote-escape just in case an operator
            // passes a `"` through the URL.
            let safe_op = op.replace('\\', "\\\\").replace('"', "\\\"");
            format!(r#"{{"max":{max},"op":"{safe_op}"}}"#)
        }
        _ => max.to_string(),
    }
}

/// PH-BRIDGE-FS-AUDIT: parse the tab-delim body returned by
/// `tool.fs.audit_recent` (one row per line:
/// `ts_secs<TAB>op<TAB>rel_path<TAB>bytes<TAB>caller_subject_id`,
/// trailing `count=N` line). Drops malformed rows; ignores the
/// count trailer (we recompute count from the row vec).
fn parse_entries(body: &str) -> Vec<FsAuditRow> {
    let mut out = Vec::new();
    for line in body.lines() {
        if line.starts_with("count=") || line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 5 {
            continue;
        }
        let ts_secs = match parts[0].parse::<i64>() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let bytes = parts[3].parse::<usize>().unwrap_or(0);
        out.push(FsAuditRow {
            ts_secs,
            op: parts[1].to_string(),
            rel_path: parts[2].to_string(),
            bytes,
            caller_subject_id: parts[4].to_string(),
        });
    }
    out
}

/// PH-BRIDGE-FS-AUDIT: dial-and-call the named peer. Same error
/// classification as `mcp::call_peer` (kept local — see future
/// PH-BRIDGE-DIAL-REFACTOR to hoist into a shared helper).
async fn call_peer(
    state: &AppState,
    alias: &str,
    method: &str,
    arg: &[u8],
) -> Result<String, (StatusCode, Json<ApiError>)> {
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
        None,
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
        ResponseResult::Ok(body) => String::from_utf8(body.to_vec()).map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: format!("response body utf8: {e}"),
                }),
            )
        }),
        ResponseResult::Err(env) => {
            // The responder validates `op` and returns INVALID_ARGS
            // for unknown ops. Surface that as 400 so curl gets a
            // meaningful code. Other responder errors (policy denied,
            // peer unreachable, internal) collapse to 502 — operator
            // sees the precise kind in the body.
            let status = if env.kind == error_kinds::INVALID_ARGS {
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
                error: "unexpected stream response from tool.fs.audit_recent".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_arg_bare_max_when_no_op() {
        assert_eq!(build_arg(10, None), "10");
        assert_eq!(build_arg(50, Some("")), "50");
    }

    #[test]
    fn build_arg_json_when_op_set() {
        assert_eq!(build_arg(25, Some("write")), r#"{"max":25,"op":"write"}"#);
        assert_eq!(
            build_arg(7, Some("fuzzy_replace")),
            r#"{"max":7,"op":"fuzzy_replace"}"#
        );
    }

    #[test]
    fn build_arg_escapes_quotes_in_op_filter() {
        // Defensive: an operator passing a stray `"` through the
        // URL shouldn't break the JSON. The responder will then
        // reject the unknown op, but the wire shape must remain
        // well-formed JSON either way.
        assert_eq!(
            build_arg(1, Some(r#"write"; drop"#)),
            r#"{"max":1,"op":"write\"; drop"}"#
        );
    }

    #[test]
    fn parse_entries_typical_body() {
        let body = "100\twrite\tnotes.md\t142\tf00b\n\
                    200\tappend\tlog.txt\t30\tbeef\n\
                    300\tpatch\tsrc/main.rs\t9001\tdead\n\
                    count=3\n";
        let rows = parse_entries(body);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].ts_secs, 100);
        assert_eq!(rows[0].op, "write");
        assert_eq!(rows[0].rel_path, "notes.md");
        assert_eq!(rows[0].bytes, 142);
        assert_eq!(rows[0].caller_subject_id, "f00b");
        assert_eq!(rows[1].op, "append");
        assert_eq!(rows[2].rel_path, "src/main.rs");
        assert_eq!(rows[2].bytes, 9001);
    }

    #[test]
    fn parse_entries_drops_malformed_rows() {
        // Missing columns + non-numeric ts_secs both dropped; the
        // bytes column falls back to 0 on parse failure (operators
        // see a row with 0 bytes rather than missing audit data).
        let body = "100\twrite\tok.md\t42\tcaller\n\
                    broken\twrite\tonly-three\n\
                    notanumber\twrite\tfoo\t99\tcaller\n\
                    400\tappend\tbar.txt\tnotanumber\tcaller\n\
                    count=4\n";
        let rows = parse_entries(body);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].rel_path, "ok.md");
        assert_eq!(rows[1].rel_path, "bar.txt");
        assert_eq!(rows[1].bytes, 0);
    }

    #[test]
    fn parse_entries_skips_count_and_blanks() {
        let body = "\ncount=0\n";
        assert!(parse_entries(body).is_empty());
    }

    #[test]
    fn parse_entries_handles_extra_columns_gracefully() {
        // Forward-compat: if the runtime grows a sixth column,
        // current bridge still picks up the first five.
        let body = "100\twrite\tok.md\t42\tcaller\textra-col-future\n\
                    count=1\n";
        let rows = parse_entries(body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].caller_subject_id, "caller");
    }

    #[test]
    fn parse_entries_caller_id_is_carried_verbatim() {
        // The bridge does not abbreviate the caller_subject_id; the
        // dashboard does its own truncation for display.
        let body = "100\twrite\tx\t1\t0123456789abcdef0123456789abcdef\n\
                    count=1\n";
        let rows = parse_entries(body);
        assert_eq!(
            rows[0].caller_subject_id,
            "0123456789abcdef0123456789abcdef"
        );
    }
}
