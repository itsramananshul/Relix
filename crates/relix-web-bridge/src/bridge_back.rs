//! Bridge-back HTTP surface for Rigs.
//!
//! These endpoints are intentionally narrow. They accept only the
//! per-Shift `brt_*` token minted by the coordinator dispatcher and
//! proxy a small set of Brief-local methods back to the coordinator.

use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "coordinator";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize)]
pub struct AgentBody {
    pub agent_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CommentBody {
    pub agent_id: String,
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct RelationBody {
    pub agent_id: String,
    pub other: String,
}

#[derive(Debug, Deserialize)]
pub struct DossierBody {
    pub agent_id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct SnagsBody {
    pub agent_id: String,
    #[serde(default)]
    pub blockers: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ClearanceBody {
    pub agent_id: String,
    pub method: String,
    #[serde(default)]
    pub category: Option<String>,
    pub reason: String,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

pub async fn comment(
    State(state): State<AppState>,
    Path(brief_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<CommentBody>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let token = bearer(&headers)?;
    authorize(&state, token, &brief_id, &req.agent_id, "brief.comment").await?;
    if req.text.trim().is_empty() {
        return Err(bad("text required"));
    }
    let arg = format!("{brief_id}|{}|{}", clean_agent(&req.agent_id)?, req.text);
    call_peer(&state, "brief.comment", arg.as_bytes()).await?;
    ok_json()
}

pub async fn subbrief(
    State(state): State<AppState>,
    Path(brief_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<RelationBody>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let token = bearer(&headers)?;
    authorize(&state, token, &brief_id, &req.agent_id, "brief.subbrief").await?;
    let other = clean_atom("other", &req.other)?;
    let arg = format!("{brief_id}|{other}");
    call_peer(&state, "brief.subbrief", arg.as_bytes()).await?;
    ok_json()
}

pub async fn dossier(
    State(state): State<AppState>,
    Path(brief_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<DossierBody>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let token = bearer(&headers)?;
    authorize(&state, token, &brief_id, &req.agent_id, "brief.dossier_add").await?;
    let kind = clean_atom("kind", &req.kind)?;
    let title = clean_atom("title", &req.title)?;
    if req.body.trim().is_empty() {
        return Err(bad("body required"));
    }
    let arg = format!("{brief_id}|{kind}|{title}|{}", req.body);
    call_peer(&state, "brief.dossier_add", arg.as_bytes()).await?;
    ok_json()
}

pub async fn set_snags(
    State(state): State<AppState>,
    Path(brief_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<SnagsBody>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let token = bearer(&headers)?;
    authorize(&state, token, &brief_id, &req.agent_id, "brief.set_snags").await?;
    let blockers = req
        .blockers
        .iter()
        .map(|b| clean_csv_atom("blocker", b))
        .collect::<Result<Vec<_>, _>>()?
        .join(",");
    let arg = format!("{brief_id}|{blockers}");
    call_peer(&state, "brief.set_snags", arg.as_bytes()).await?;
    ok_json()
}

pub async fn claim_holder(
    State(state): State<AppState>,
    Path(brief_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<AgentBody>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let token = bearer(&headers)?;
    authorize(
        &state,
        token,
        &brief_id,
        &req.agent_id,
        "brief.claim_holder",
    )
    .await?;
    json_passthrough(call_peer(&state, "brief.claim_holder", brief_id.as_bytes()).await?)
}

pub async fn clearance(
    State(state): State<AppState>,
    Path(brief_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<ClearanceBody>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let token = bearer(&headers)?;
    authorize(
        &state,
        token,
        &brief_id,
        &req.agent_id,
        "brief.clearance_request",
    )
    .await?;
    let method = clean_atom("method", &req.method)?;
    let category = clean_atom("category", req.category.as_deref().unwrap_or("bridge_back"))?;
    let reason = clean_atom("reason", &req.reason)?;
    let ttl = req
        .ttl_secs
        .map(|v| v.clamp(30, 86_400).to_string())
        .unwrap_or_default();
    let arg = format!(
        "{brief_id}|{}|{method}|{category}|{reason}|{ttl}",
        clean_agent(&req.agent_id)?,
    );
    json_passthrough(call_peer(&state, "brief.clearance_request", arg.as_bytes()).await?)
}

async fn authorize(
    state: &AppState,
    token: &str,
    brief_id: &str,
    agent_id: &str,
    method: &str,
) -> Result<(), (StatusCode, Json<ApiError>)> {
    let agent_id = clean_agent(agent_id)?;
    let arg = format!("{token}|{brief_id}|{agent_id}|{method}");
    let body = call_peer(state, "bridge_back.authorize", arg.as_bytes()).await?;
    match std::str::from_utf8(&body).unwrap_or("").trim() {
        "allow" => Ok(()),
        "deny" => Err((
            StatusCode::FORBIDDEN,
            Json(ApiError {
                error: "bridge-back token is not authorized for this Brief, agent, or method"
                    .into(),
            }),
        )),
        other => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("unexpected bridge_back.authorize response: {other}"),
            }),
        )),
    }
}

fn bearer(headers: &HeaderMap) -> Result<&str, (StatusCode, Json<ApiError>)> {
    let Some(raw) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ApiError {
                error: "missing Authorization bearer".into(),
            }),
        ));
    };
    let Some(rest) = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
    else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ApiError {
                error: "missing Authorization bearer".into(),
            }),
        ));
    };
    let token = rest.trim();
    if token.starts_with("brt_") && !token.contains('|') {
        Ok(token)
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(ApiError {
                error: "invalid bridge-back bearer".into(),
            }),
        ))
    }
}

fn clean_agent(agent_id: &str) -> Result<&str, (StatusCode, Json<ApiError>)> {
    clean_atom("agent_id", agent_id)
}

fn clean_atom<'a>(label: &str, value: &'a str) -> Result<&'a str, (StatusCode, Json<ApiError>)> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(bad(&format!("{label} required")));
    }
    if trimmed.contains('|') {
        return Err(bad(&format!("{label} must not contain `|`")));
    }
    Ok(trimmed)
}

fn clean_csv_atom<'a>(
    label: &str,
    value: &'a str,
) -> Result<&'a str, (StatusCode, Json<ApiError>)> {
    let trimmed = clean_atom(label, value)?;
    if trimmed.contains(',') {
        return Err(bad(&format!("{label} must not contain `,`")));
    }
    Ok(trimmed)
}

fn bad(msg: &str) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError { error: msg.into() }),
    )
}

fn ok_json() -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(br#"{"ok":true}"#.to_vec())
}

fn json_passthrough(body: Vec<u8>) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let payload = if body.is_empty() {
        b"null".to_vec()
    } else {
        body
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload))
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError {
                    error: format!("build response: {e}"),
                }),
            )
        })
}

async fn call_peer(
    state: &AppState,
    method: &str,
    arg: &[u8],
) -> Result<Vec<u8>, (StatusCode, Json<ApiError>)> {
    let mesh = state.mesh_client.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError {
            error: "bridge mesh client not initialized".into(),
        }),
    ))?;
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(5, 60);
    let envelope = build_request_with_tenant(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        deadline_secs,
        None,
        None,
        None,
        crate::tenant::current_tenant_or_none(),
    );
    let resp_bytes = mesh.call(DEFAULT_PEER, envelope).await.map_err(|e| {
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
            let cause = env.cause;
            let lower = cause.to_ascii_lowercase();
            let status = if lower.contains("not found") {
                StatusCode::NOT_FOUND
            } else if env.kind == 5 {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            };
            Err((
                status,
                Json(ApiError {
                    error: format!("responder err kind={} cause={cause}", env.kind),
                }),
            ))
        }
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response from coordinator".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_accepts_only_bridge_back_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer brt_0123456789abcdef".parse().unwrap(),
        );
        assert_eq!(bearer(&headers).unwrap(), "brt_0123456789abcdef");

        headers.insert(
            header::AUTHORIZATION,
            "Bearer global_token".parse().unwrap(),
        );
        assert_eq!(bearer(&headers).unwrap_err().0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn atoms_reject_wire_delimiters() {
        assert!(clean_atom("x", "ok").is_ok());
        assert!(clean_atom("x", "bad|x").is_err());
        assert!(clean_csv_atom("x", "bad,x").is_err());
        assert!(clean_atom("reason", "do the thing|also fake ttl").is_err());
    }
}
