//! PH-BRIDGE-TERM-AUDIT — HTTP proxy for `tool.terminal.audit_recent`
//! on a tool node. Mirror of the PH-BRIDGE-FS-AUDIT shape: the
//! ring is runtime-side (per-`TerminalBackend`), bounded, resets
//! on tool-node restart; this proxy just structures the
//! tab-delim responder body into JSON for the dashboard.
//!
//! One endpoint:
//!
//! - `GET /v1/terminal/audit?peer=<alias>&max=<N>` — proxies
//!   `tool.terminal.audit_recent` to the named tool peer
//!   (default alias `"tool"`). Returns JSON:
//!
//!   ```json
//!   {
//!     "peer": "tool",
//!     "entries": [{
//!       "ts_secs": 1716,
//!       "command": "ls",
//!       "exit_code": 0,
//!       "duration_ms": 12,
//!       "timed_out": false,
//!       "cancelled": false,
//!       "caller_subject_id": "f00b…"
//!     }],
//!     "count": 1
//!   }
//!   ```
//!
//! Wire shape: arg is the bare integer max (the responder does
//! not accept the JSON shape that `tool.fs.audit_recent` does —
//! there's no op-filter for terminal entries today). The
//! responder also caps max server-side at the ring's default
//! capacity.
//!
//! Note: the responder intentionally drops `args` from the
//! audit body (only the command is logged) so the proxy mirrors
//! that posture — command yes, args no. Args are not safe to
//! ship into a dashboard verbatim (they can contain secrets a
//! caller passed on the command line); the responder owns that
//! decision.
//!
//! Fail modes (mirror PH-BRIDGE-FS-AUDIT):
//! - Bridge mesh client not initialized → 503.
//! - Peer alias unknown → 404.
//! - Any other dispatch failure → 502.
//! - INVALID_ARGS from responder → 400.

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
/// Cap the operator-supplied `max` to a reasonable upper bound.
/// The runtime ring is bounded already; this stops the bridge
/// from holding arbitrarily large parsed vecs for a misuse.
const MAX_ROW_CAP: usize = 500;

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    #[serde(default)]
    pub peer: Option<String>,
    /// Maximum entries returned. Defaults to 100 when absent.
    #[serde(default)]
    pub max: Option<usize>,
}

/// PH-BRIDGE-TERM-AUDIT: one row of the response. Mirrors the
/// runtime's `TerminalAuditEntry`, but without `args` (the
/// responder doesn't ship args over the wire — see module doc).
#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct TermAuditRow {
    pub ts_secs: i64,
    pub command: String,
    /// `Some(code)` on natural exit; `None` when the child was
    /// killed (timeout / cancel) or wait failed. The responder
    /// sends `"?"` for None; the bridge round-trips it as
    /// `None` so the dashboard can format consistently.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub cancelled: bool,
    pub caller_subject_id: String,
}

#[derive(Debug, Serialize)]
pub struct AuditResponse {
    pub peer: String,
    pub entries: Vec<TermAuditRow>,
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
    let arg = max.to_string();
    let body = call_peer(&state, &peer, "tool.terminal.audit_recent", arg.as_bytes()).await?;
    let entries = parse_entries(&body);
    let count = entries.len();
    Ok(Json(AuditResponse {
        peer,
        entries,
        count,
    }))
}

/// PH-BRIDGE-TERM-AUDIT: parse the tab-delim body returned by
/// `tool.terminal.audit_recent` (one row per line:
/// `ts_secs<TAB>command<TAB>exit_code<TAB>duration_ms<TAB>timed_out<TAB>cancelled<TAB>caller_subject_id`,
/// trailing `count=N` line). Drops malformed rows; ignores the
/// count trailer (we recompute count from the row vec).
///
/// `exit_code` is `?` when the child was killed; the bridge
/// emits `None` in that case so JSON consumers can distinguish
/// "exited 0" from "killed".
fn parse_entries(body: &str) -> Vec<TermAuditRow> {
    let mut out = Vec::new();
    for line in body.lines() {
        if line.starts_with("count=") || line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 7 {
            continue;
        }
        let ts_secs = match parts[0].parse::<i64>() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let command = parts[1].to_string();
        let exit_code = match parts[2] {
            "?" => None,
            s => s.parse::<i32>().ok(),
        };
        let duration_ms = parts[3].parse::<u64>().unwrap_or(0);
        let timed_out = parts[4] == "true";
        let cancelled = parts[5] == "true";
        let caller_subject_id = parts[6].to_string();
        out.push(TermAuditRow {
            ts_secs,
            command,
            exit_code,
            duration_ms,
            timed_out,
            cancelled,
            caller_subject_id,
        });
    }
    out
}

/// PH-BRIDGE-TERM-AUDIT: dial-and-call the named peer. Same
/// error classification as `fs_audit::call_peer` (intentional
/// duplication — see PH-BRIDGE-DIAL-REFACTOR future cleanup).
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
                error: "unexpected stream response from tool.terminal.audit_recent".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_entries_typical_body() {
        let body = "100\tls\t0\t12\tfalse\tfalse\tcaller-a\n\
                    200\tgrep -r foo .\t1\t450\tfalse\tfalse\tcaller-b\n\
                    300\tsleep 9999\t?\t30000\ttrue\tfalse\tcaller-c\n\
                    400\tnc -l\t?\t5\tfalse\ttrue\tcaller-d\n\
                    count=4\n";
        let rows = parse_entries(body);
        assert_eq!(rows.len(), 4);

        assert_eq!(rows[0].command, "ls");
        assert_eq!(rows[0].exit_code, Some(0));
        assert!(!rows[0].timed_out);
        assert!(!rows[0].cancelled);

        assert_eq!(rows[1].command, "grep -r foo .");
        assert_eq!(rows[1].exit_code, Some(1));

        // Timed-out child: exit_code is None (responder sent "?").
        assert_eq!(rows[2].command, "sleep 9999");
        assert_eq!(rows[2].exit_code, None);
        assert!(rows[2].timed_out);
        assert!(!rows[2].cancelled);

        // Cancelled child: exit_code is None, cancelled flag set.
        assert_eq!(rows[3].command, "nc -l");
        assert_eq!(rows[3].exit_code, None);
        assert!(!rows[3].timed_out);
        assert!(rows[3].cancelled);
    }

    #[test]
    fn parse_entries_drops_malformed_rows() {
        // Missing columns + non-numeric ts_secs both dropped; the
        // duration_ms column falls back to 0 on parse failure.
        let body = "100\tok\t0\t10\tfalse\tfalse\tcaller\n\
                    broken\tonly-one-field\n\
                    notanumber\tcmd\t0\t10\tfalse\tfalse\tcaller\n\
                    400\tcmd\t0\tnotanumber\tfalse\tfalse\tcaller\n\
                    count=4\n";
        let rows = parse_entries(body);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].command, "ok");
        assert_eq!(rows[1].command, "cmd");
        assert_eq!(rows[1].duration_ms, 0);
    }

    #[test]
    fn parse_entries_skips_count_and_blanks() {
        let body = "\ncount=0\n";
        assert!(parse_entries(body).is_empty());
    }

    #[test]
    fn parse_entries_handles_extra_columns_gracefully() {
        // Forward-compat: if the runtime grows an eighth column
        // (e.g. exit_signal), current bridge picks up the first
        // seven.
        let body = "100\tls\t0\t1\tfalse\tfalse\tcaller\tfuture-col\n\
                    count=1\n";
        let rows = parse_entries(body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].caller_subject_id, "caller");
    }

    #[test]
    fn parse_entries_treats_unparseable_exit_as_none() {
        // The responder only emits numeric or "?", but defensive
        // parsing should never panic on a non-numeric non-"?"
        // value either. Non-numeric → None (operator sees a
        // killed-style row rather than missing audit data).
        let body = "100\tcmd\tgibberish\t10\tfalse\tfalse\tcaller\ncount=1\n";
        let rows = parse_entries(body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].exit_code, None);
    }

    #[test]
    fn parse_entries_distinguishes_timed_out_from_cancelled() {
        // The mutual-exclusivity is a runtime invariant — at
        // most one of timed_out/cancelled can be true on the
        // same row — but the parser should not enforce it. If
        // the runtime ever sends both, both flags surface to
        // the dashboard.
        let body = "100\tcmd\t?\t1\ttrue\ttrue\tcaller\ncount=1\n";
        let rows = parse_entries(body);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].timed_out);
        assert!(rows[0].cancelled);
    }

    #[test]
    fn term_audit_row_skips_exit_code_when_none() {
        // The JSON serialization omits exit_code on killed rows
        // so the dashboard can detect the absence rather than
        // shipping a synthetic sentinel.
        let row = TermAuditRow {
            ts_secs: 1,
            command: "x".into(),
            exit_code: None,
            duration_ms: 0,
            timed_out: true,
            cancelled: false,
            caller_subject_id: "c".into(),
        };
        let s = serde_json::to_string(&row).unwrap();
        assert!(!s.contains("exit_code"));
    }

    #[test]
    fn term_audit_row_includes_exit_code_when_some() {
        let row = TermAuditRow {
            ts_secs: 1,
            command: "x".into(),
            exit_code: Some(42),
            duration_ms: 0,
            timed_out: false,
            cancelled: false,
            caller_subject_id: "c".into(),
        };
        let s = serde_json::to_string(&row).unwrap();
        assert!(s.contains(r#""exit_code":42"#));
    }
}
