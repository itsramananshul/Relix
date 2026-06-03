//! The **companion** command surface (Phase 5, materialize-work
//! half).
//!
//! A deterministic, rule-based command parser that turns plain-text
//! operator input into product-spine actions and executes them
//! through the mesh. It is *not* an LLM — it is the verifiable
//! materialize-work spine the companion is built on: the parser is a
//! pure function with exhaustive tests, and a model can later replace
//! the parsing step while reusing the same execution path.
//!
//! `POST /v1/spine/companion {"message": "..."}` →
//! `{"action": "...", "reply": "...", "result": <json|null>}`.

use axum::{extract::State, http::StatusCode, Json};
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
pub struct CompanionRequest {
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct CompanionResponse {
    /// The parsed action name (`create_brief`, `move`, …).
    pub action: String,
    /// A short human-readable reply.
    pub reply: String,
    /// The raw capability result, when the action produced one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

/// What the parser resolved an operator message to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompanionAction {
    CreateBrief { title: String },
    CreateMandate { title: String },
    Move { id: String, status: String },
    Assign { id: String, agent: String },
    Pin { id: String, on: bool },
    Overdue,
    Board,
    Search { query: String },
    Help,
    /// Unparseable — carries the original for the reply.
    Unknown,
}

const BOARD_STATUSES: &[&str] = &[
    "backlog",
    "todo",
    "in_progress",
    "in_review",
    "blocked",
    "done",
    "cancelled",
];

/// Parse an operator message into a [`CompanionAction`]. Pure and
/// total — every input resolves to some variant. Case-insensitive on
/// the leading verb; the payload keeps its original case.
pub fn parse_command(message: &str) -> CompanionAction {
    let msg = message.trim();
    let lower = msg.to_ascii_lowercase();

    // Helper: strip a leading prefix (case-insensitive) and return the
    // remaining original-case tail, trimmed.
    let after = |prefix: &str| -> Option<String> {
        if lower.starts_with(prefix) {
            Some(msg[prefix.len()..].trim().to_string())
        } else {
            None
        }
    };

    if lower == "help" || lower == "?" {
        return CompanionAction::Help;
    }
    if lower == "overdue" || lower == "what's overdue" || lower == "whats overdue" {
        return CompanionAction::Overdue;
    }
    if lower == "board" || lower == "status" {
        return CompanionAction::Board;
    }
    for p in ["create brief ", "new brief ", "add brief "] {
        if let Some(t) = after(p)
            && !t.is_empty()
        {
            return CompanionAction::CreateBrief { title: t };
        }
    }
    for p in ["create mandate ", "new mandate ", "add mandate ", "new goal ", "create goal "] {
        if let Some(t) = after(p)
            && !t.is_empty()
        {
            return CompanionAction::CreateMandate { title: t };
        }
    }
    for p in ["search ", "find "] {
        if let Some(q) = after(p)
            && !q.is_empty()
        {
            return CompanionAction::Search { query: q };
        }
    }
    // "pin <id>" / "unpin <id>"
    if let Some(id) = after("unpin ")
        && !id.is_empty()
    {
        return CompanionAction::Pin { id, on: false };
    }
    if let Some(id) = after("pin ")
        && !id.is_empty()
    {
        return CompanionAction::Pin { id, on: true };
    }
    // "assign <id> to <agent>"
    if let Some(rest) = after("assign ") {
        let rl = rest.to_ascii_lowercase();
        if let Some(idx) = rl.find(" to ") {
            let id = rest[..idx].trim().to_string();
            let agent = rest[idx + 4..].trim().to_string();
            if !id.is_empty() && !agent.is_empty() {
                return CompanionAction::Assign { id, agent };
            }
        }
    }
    // "move <id> to <status>"
    if let Some(rest) = after("move ") {
        let rl = rest.to_ascii_lowercase();
        if let Some(idx) = rl.find(" to ") {
            let id = rest[..idx].trim().to_string();
            let status = rest[idx + 4..].trim().to_ascii_lowercase().replace(' ', "_");
            if !id.is_empty() && BOARD_STATUSES.contains(&status.as_str()) {
                return CompanionAction::Move { id, status };
            }
        }
    }
    CompanionAction::Unknown
}

/// `POST /v1/spine/companion` — parse + execute one command.
pub async fn handle(
    State(state): State<AppState>,
    Json(req): Json<CompanionRequest>,
) -> Result<Json<CompanionResponse>, (StatusCode, Json<ApiError>)> {
    let action = parse_command(&req.message);
    match action {
        CompanionAction::Help => Ok(Json(CompanionResponse {
            action: "help".into(),
            reply: "Try: create brief <title> · create mandate <title> · move <id> to <status> · search <q> · overdue · board".into(),
            result: None,
        })),
        CompanionAction::Unknown => Ok(Json(CompanionResponse {
            action: "unknown".into(),
            reply: format!("I didn't understand \"{}\". Type help for commands.", req.message.trim()),
            result: None,
        })),
        CompanionAction::CreateBrief { title } => {
            if title.contains('|') {
                return Err(bad("title must not contain `|`"));
            }
            let arg = format!("{title}||||");
            let body = call_peer(&state, "brief.create", arg.as_bytes()).await?;
            let id = String::from_utf8_lossy(&body).trim().to_string();
            Ok(Json(CompanionResponse {
                action: "create_brief".into(),
                reply: format!("Created brief “{title}” ({id})."),
                result: Some(serde_json::json!({ "task_id": id })),
            }))
        }
        CompanionAction::CreateMandate { title } => {
            if title.contains('|') {
                return Err(bad("title must not contain `|`"));
            }
            let arg = format!("{title}|||");
            let body = call_peer(&state, "mandate.create", arg.as_bytes()).await?;
            let id = String::from_utf8_lossy(&body).trim().to_string();
            Ok(Json(CompanionResponse {
                action: "create_mandate".into(),
                reply: format!("Created mandate “{title}” ({id})."),
                result: Some(serde_json::json!({ "mandate_id": id })),
            }))
        }
        CompanionAction::Move { id, status } => {
            let arg = format!("{id}|{status}");
            call_peer(&state, "brief.move", arg.as_bytes()).await?;
            Ok(Json(CompanionResponse {
                action: "move".into(),
                reply: format!("Moved {id} → {status}."),
                result: None,
            }))
        }
        CompanionAction::Assign { id, agent } => {
            if agent.contains('|') {
                return Err(bad("agent must not contain `|`"));
            }
            let arg = format!("{id}|assignee|{agent}");
            call_peer(&state, "brief.set", arg.as_bytes()).await?;
            Ok(Json(CompanionResponse {
                action: "assign".into(),
                reply: format!("Assigned {id} → {agent}."),
                result: None,
            }))
        }
        CompanionAction::Pin { id, on } => {
            let arg = format!("{id}|{}", i32::from(on));
            call_peer(&state, "brief.pin", arg.as_bytes()).await?;
            Ok(Json(CompanionResponse {
                action: "pin".into(),
                reply: format!("{} {id}.", if on { "Pinned" } else { "Unpinned" }),
                result: None,
            }))
        }
        CompanionAction::Overdue => {
            let body = call_peer(&state, "brief.overdue", b"|50").await?;
            let json: serde_json::Value =
                serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            let n = json.as_array().map(|a| a.len()).unwrap_or(0);
            Ok(Json(CompanionResponse {
                action: "overdue".into(),
                reply: format!("{n} overdue brief(s)."),
                result: Some(json),
            }))
        }
        CompanionAction::Board => {
            let body = call_peer(&state, "brief.board_summary", b"").await?;
            let json: serde_json::Value =
                serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            Ok(Json(CompanionResponse {
                action: "board".into(),
                reply: "Board summary.".into(),
                result: Some(json),
            }))
        }
        CompanionAction::Search { query } => {
            let arg = format!("{query}|25");
            let body = call_peer(&state, "brief.search", arg.as_bytes()).await?;
            let json: serde_json::Value =
                serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            let n = json.as_array().map(|a| a.len()).unwrap_or(0);
            Ok(Json(CompanionResponse {
                action: "search".into(),
                reply: format!("{n} match(es) for “{query}”."),
                result: Some(json),
            }))
        }
    }
}

fn bad(msg: &str) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError { error: msg.into() }),
    )
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
        (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: e.to_string(),
            }),
        )
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
        ResponseResult::Err(env) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("responder err kind={} cause={}", env.kind, env.cause),
            }),
        )),
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_create_brief_variants_keeping_case() {
        for p in ["create brief ", "New Brief ", "ADD BRIEF "] {
            assert_eq!(
                parse_command(&format!("{p}Ship the Auth Rewrite")),
                CompanionAction::CreateBrief {
                    title: "Ship the Auth Rewrite".into()
                }
            );
        }
    }

    #[test]
    fn parses_create_mandate_and_goal_synonyms() {
        assert_eq!(
            parse_command("new goal Grow revenue"),
            CompanionAction::CreateMandate {
                title: "Grow revenue".into()
            }
        );
        assert_eq!(
            parse_command("create mandate Ship v1"),
            CompanionAction::CreateMandate {
                title: "Ship v1".into()
            }
        );
    }

    #[test]
    fn parses_move_with_status_normalisation() {
        assert_eq!(
            parse_command("move abc123 to in progress"),
            CompanionAction::Move {
                id: "abc123".into(),
                status: "in_progress".into()
            }
        );
        assert_eq!(
            parse_command("MOVE xyz TO Done"),
            CompanionAction::Move {
                id: "xyz".into(),
                status: "done".into()
            }
        );
        // Unknown status → not a Move.
        assert_eq!(
            parse_command("move xyz to nowhere"),
            CompanionAction::Unknown
        );
    }

    #[test]
    fn parses_assign_and_pin() {
        assert_eq!(
            parse_command("assign abc to agt_eng"),
            CompanionAction::Assign {
                id: "abc".into(),
                agent: "agt_eng".into()
            }
        );
        assert_eq!(
            parse_command("pin abc"),
            CompanionAction::Pin { id: "abc".into(), on: true }
        );
        assert_eq!(
            parse_command("unpin abc"),
            CompanionAction::Pin { id: "abc".into(), on: false }
        );
        // "move" must NOT be swallowed by the pin/assign rules.
        assert_eq!(
            parse_command("move abc to done"),
            CompanionAction::Move { id: "abc".into(), status: "done".into() }
        );
    }

    #[test]
    fn parses_search_overdue_board_help() {
        assert_eq!(
            parse_command("find auth"),
            CompanionAction::Search { query: "auth".into() }
        );
        assert_eq!(parse_command("overdue"), CompanionAction::Overdue);
        assert_eq!(parse_command("board"), CompanionAction::Board);
        assert_eq!(parse_command("help"), CompanionAction::Help);
    }

    #[test]
    fn empty_payloads_and_gibberish_are_unknown() {
        assert_eq!(parse_command("create brief "), CompanionAction::Unknown);
        assert_eq!(parse_command("blah blah"), CompanionAction::Unknown);
        assert_eq!(parse_command(""), CompanionAction::Unknown);
    }
}
