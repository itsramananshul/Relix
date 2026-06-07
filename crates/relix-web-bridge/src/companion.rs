//! The **companion** command surface (Phase 5, materialize-work
//! half) — now a **company-aware action spine**
//! (`relix-dashboard-design.md` §13; `relix-product-roadmap-current.md` §9).
//!
//! A deterministic, rule-based command parser that turns plain-text
//! operator input into product-spine actions and executes them
//! through the mesh. It is *not* an LLM — it is the verifiable
//! materialize-work spine the companion is built on: the parser is a
//! pure function with exhaustive tests, and a model can later replace
//! the parsing step while reusing the same execution path.
//!
//! Beyond the create/move/comment verbs, it reads live company state
//! (`company.actions`, `brief.blocked_list`, `brief.runs`,
//! `agent.operatives`) and can open a **governed plan package**
//! (`brief.plan_package_open`) — every read and write goes through the
//! SAME mesh capabilities + governance the dashboard uses; nothing
//! bypasses approvals or mutates a store directly. LLM-driven action
//! selection remains future (`product-spine-implementation.md`).
//!
//! `POST /v1/spine/companion {"message": "..."}` →
//! `{"action": "...", "reply": "...", "result": <json|null>}`.

use axum::{Json, extract::State, http::StatusCode};
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
    CreateBrief {
        title: String,
    },
    CreateMandate {
        title: String,
    },
    Move {
        id: String,
        status: String,
    },
    Assign {
        id: String,
        agent: String,
    },
    Pin {
        id: String,
        on: bool,
    },
    Comment {
        id: String,
        text: String,
    },
    Overdue,
    Board,
    Search {
        query: String,
    },
    /// "what needs attention" / "next actions" — the ranked Action Center.
    Attention,
    /// "what is blocked" / "blocked work" — Briefs waiting on a blocker.
    BlockedWork,
    /// "what is running" / "active runs" — Shifts that are not terminal.
    RunningWork,
    /// "who is on the crew" / "roster" / "agents" — the Operative roster.
    Roster,
    /// A governed plan package: an immutable plan + a child-task proposal +
    /// an approval-bound confirm, opened via `brief.plan_package_open`. The
    /// operator still approves the confirm before any child is materialized.
    PlanPackage {
        brief_id: String,
        plan_body: String,
        children: Vec<PlanChild>,
    },
    Help,
    /// Unparseable — carries the original for the reply.
    Unknown,
}

/// One proposed child task in a [`CompanionAction::PlanPackage`]. Title plus a
/// validated Brief priority (defaults to `normal` when none is given).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanChild {
    pub title: String,
    pub priority: String,
}

/// Valid Brief priorities (mirrors the coordinator's `brief::PRIORITIES`).
const PRIORITIES: &[&str] = &["low", "normal", "high", "urgent"];

/// The companion's help reply — framed as a company companion, not a toy command
/// list. Every line maps to a governed mesh capability; nothing bypasses
/// approvals.
const HELP_TEXT: &str = "I'm your company companion. I read live company state and turn plain requests into governed work — through the same approvals as the dashboard, never around them.\n\nAsk me about the company:\n• what needs attention — your ranked next actions\n• what is blocked — Briefs waiting on a blocker\n• what is running — active Shifts\n• who is on the crew — the Operative roster\n• overdue · board · search <q>\n\nCreate & move work:\n• create brief <title> · create mandate <title>\n• move <id> to <status> · assign <id> to <agent>\n• pin <id> · comment <id>: <text>\n\nOpen a governed plan package (you approve before anything is created):\n• plan package <brief_id>: <plan body> => child: <title>; child high: <title>";

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
    // Company-aware read intents — matched on the whole message (minus trailing
    // punctuation) so they stay deterministic and never swallow a verb command.
    let nlower = lower.trim_end_matches(['?', '.', '!']).trim();
    const ATTENTION: &[&str] = &[
        "what needs attention",
        "what needs my attention",
        "next actions",
        "what should i do",
        "what should i do next",
        "what do i do next",
        "what's next",
        "whats next",
    ];
    const BLOCKED: &[&str] = &[
        "what is blocked",
        "what's blocked",
        "whats blocked",
        "blocked work",
        "blocked",
    ];
    const RUNNING: &[&str] = &[
        "what is running",
        "what's running",
        "whats running",
        "active runs",
        "active shifts",
        "running",
    ];
    const ROSTER: &[&str] = &[
        "who is on the crew",
        "who's on the crew",
        "whos on the crew",
        "roster",
        "crew",
        "agents",
        "operatives",
    ];
    if ATTENTION.contains(&nlower) {
        return CompanionAction::Attention;
    }
    if BLOCKED.contains(&nlower) {
        return CompanionAction::BlockedWork;
    }
    if RUNNING.contains(&nlower) {
        return CompanionAction::RunningWork;
    }
    if ROSTER.contains(&nlower) {
        return CompanionAction::Roster;
    }
    if lower == "overdue" || lower == "what's overdue" || lower == "whats overdue" {
        return CompanionAction::Overdue;
    }
    if lower == "board" || lower == "status" {
        return CompanionAction::Board;
    }
    // "plan package <brief_id>: <plan body> => child: <t>; child high: <t>"
    for p in ["plan package ", "new plan package "] {
        if let Some(rest) = after(p)
            && let Some(action) = parse_plan_package(&rest)
        {
            return action;
        }
    }
    for p in ["create brief ", "new brief ", "add brief "] {
        if let Some(t) = after(p)
            && !t.is_empty()
        {
            return CompanionAction::CreateBrief { title: t };
        }
    }
    for p in [
        "create mandate ",
        "new mandate ",
        "add mandate ",
        "new goal ",
        "create goal ",
    ] {
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
    // "comment <id>: <text>"
    if let Some(rest) = after("comment ")
        && let Some(idx) = rest.find(':')
    {
        let id_raw = rest[..idx].trim();
        // Allow the natural "comment on <id>:" phrasing.
        let id = id_raw
            .strip_prefix("on ")
            .unwrap_or(id_raw)
            .trim()
            .to_string();
        let text = rest[idx + 1..].trim().to_string();
        if !id.is_empty() && !text.is_empty() {
            return CompanionAction::Comment { id, text };
        }
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
            let status = rest[idx + 4..]
                .trim()
                .to_ascii_lowercase()
                .replace(' ', "_");
            if !id.is_empty() && BOARD_STATUSES.contains(&status.as_str()) {
                return CompanionAction::Move { id, status };
            }
        }
    }
    CompanionAction::Unknown
}

/// Parse the tail of a `plan package …` command. `rest` is the original-case
/// text after the verb: `<brief_id>: <plan body> [=> child: <t>; …]`. Returns
/// `None` (→ `Unknown`) only when no `brief_id`/`:` can be found; an empty plan
/// body or a missing child list still yields a [`CompanionAction::PlanPackage`]
/// so the handler can refuse with a specific, helpful message (and so the
/// invalid cases stay unit-testable). Pure + total over its inputs.
fn parse_plan_package(rest: &str) -> Option<CompanionAction> {
    let (id_part, after_colon) = rest.split_once(':')?;
    let brief_id = id_part.trim().to_string();
    if brief_id.is_empty() {
        return None;
    }
    // The plan body is split from the child list on `=>`, NOT on `:`, so a body
    // may itself contain colons.
    let (body_part, children_part) = match after_colon.split_once("=>") {
        Some((b, c)) => (b.trim(), c.trim()),
        None => (after_colon.trim(), ""),
    };
    Some(CompanionAction::PlanPackage {
        brief_id,
        plan_body: body_part.to_string(),
        children: parse_plan_children(children_part),
    })
}

/// Parse the child-task segment of a plan-package command: a `;`-separated list
/// of `child[ <priority>]: <title>` entries. A segment that doesn't start with
/// `child`, carries an unrecognized priority, or has an empty title is dropped
/// (so the handler's zero-children refusal is honest). Pure.
fn parse_plan_children(spec: &str) -> Vec<PlanChild> {
    let mut out = Vec::new();
    for seg in spec.split(';') {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        let Some((head, title)) = seg.split_once(':') else {
            continue;
        };
        let title = title.trim();
        if title.is_empty() {
            continue;
        }
        let head = head.trim().to_ascii_lowercase();
        let Some(prio_tok) = head.strip_prefix("child") else {
            continue;
        };
        let prio_tok = prio_tok.trim();
        let priority = if prio_tok.is_empty() {
            "normal".to_string()
        } else if PRIORITIES.contains(&prio_tok) {
            prio_tok.to_string()
        } else {
            // An unrecognized priority word means the segment is malformed —
            // drop it rather than silently downgrade to `normal`.
            continue;
        };
        out.push(PlanChild {
            title: title.to_string(),
            priority,
        });
    }
    out
}

/// Why a plan package can't be opened, or `None` if it is well-formed. A plan
/// package MUST carry a non-empty plan body AND at least one child task.
fn plan_package_problem(plan_body: &str, children: &[PlanChild]) -> Option<&'static str> {
    if plan_body.trim().is_empty() {
        return Some("a plan package needs a plan body");
    }
    if children.is_empty() {
        return Some("a plan package needs at least one child task (e.g. `=> child: …`)");
    }
    None
}

// ── Reply summarizers (pure — unit-tested without a mesh) ─────────────────────

/// Decode a capability body to JSON, or `Null` on any failure (the raw body is
/// still surfaced via the response `result` for the caller to inspect).
fn parse_json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or(serde_json::Value::Null)
}

/// A run is "active" when it has a non-empty, non-terminal status.
const TERMINAL_RUN: &[&str] = &["done", "failed", "refused", "interrupted", "cancelled"];

/// Summarize the `company.actions` feed into a plain-language reply: total count
/// plus the top 3 action titles. Calm when nothing needs the operator.
fn summarize_actions(v: &serde_json::Value) -> String {
    let actions = v.get("actions").and_then(|a| a.as_array());
    let total = v
        .get("counts")
        .and_then(|c| c.get("total"))
        .and_then(|t| t.as_u64())
        .or_else(|| actions.map(|a| a.len() as u64))
        .unwrap_or(0);
    if total == 0 {
        return "Nothing needs your attention right now — the board is calm.".to_string();
    }
    let mut reply = format!("{total} thing(s) need your attention. Top:");
    if let Some(arr) = actions {
        for it in arr.iter().take(3) {
            let title = it
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("(untitled)");
            reply.push_str(&format!("\n• {title}"));
        }
    }
    reply
}

/// Summarize `brief.blocked_list` (an array of Brief cards): blocked count plus
/// the top 3 ids/titles and how many blockers each is waiting on.
fn summarize_blocked(v: &serde_json::Value) -> String {
    let arr = v.as_array();
    let n = arr.map(|a| a.len()).unwrap_or(0);
    if n == 0 {
        return "No blocked work — nothing is waiting on a blocker.".to_string();
    }
    let mut reply = format!("{n} blocked Brief(s). Top:");
    if let Some(a) = arr {
        for c in a.iter().take(3) {
            let id = c.get("task_id").and_then(|x| x.as_str()).unwrap_or("?");
            let title = c
                .get("title")
                .and_then(|x| x.as_str())
                .unwrap_or("(untitled)");
            let blockers = c
                .get("blocked_by")
                .and_then(|x| x.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            reply.push_str(&format!("\n• {title} ({id}) — blocked by {blockers}"));
        }
    }
    reply
}

/// Summarize `brief.runs` (an array of run records): active (non-terminal) count
/// plus the top 3 active Shifts with their Rig + status.
fn summarize_runs(v: &serde_json::Value) -> String {
    let empty = Vec::new();
    let all = v.as_array().unwrap_or(&empty);
    let active: Vec<&serde_json::Value> = all
        .iter()
        .filter(|r| {
            let st = r.get("status").and_then(|s| s.as_str()).unwrap_or("");
            !st.is_empty() && !TERMINAL_RUN.contains(&st)
        })
        .collect();
    if active.is_empty() {
        return "No active Shifts running right now.".to_string();
    }
    let mut reply = format!("{} active Shift(s). Top:", active.len());
    for r in active.iter().take(3) {
        let id = r.get("run_id").and_then(|x| x.as_str()).unwrap_or("?");
        let rig = r.get("rig").and_then(|x| x.as_str()).unwrap_or("?");
        let status = r.get("status").and_then(|x| x.as_str()).unwrap_or("?");
        reply.push_str(&format!("\n• {id} on {rig} — {status}"));
    }
    reply
}

/// Summarize `agent.operatives` (the roster): total plus active/pending counts.
fn summarize_roster(v: &serde_json::Value) -> String {
    let arr = v.as_array();
    if arr.map(|a| a.is_empty()).unwrap_or(true) {
        return "No Operatives on the crew yet.".to_string();
    }
    let rows = arr.unwrap();
    let mut active = 0usize;
    let mut pending = 0usize;
    let mut other = 0usize;
    for o in rows {
        match o.get("status").and_then(|s| s.as_str()).unwrap_or("") {
            s if s.eq_ignore_ascii_case("active") => active += 1,
            s if s.eq_ignore_ascii_case("pending") => pending += 1,
            _ => other += 1,
        }
    }
    let total = rows.len();
    let mut reply = format!("{total} Operative(s) on the crew — {active} active, {pending} pending");
    if other > 0 {
        reply.push_str(&format!(", {other} other"));
    }
    reply.push('.');
    reply
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
            reply: HELP_TEXT.into(),
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
        CompanionAction::Comment { id, text } => {
            // `text` is the trailing wire field so it may contain `|`.
            let arg = format!("{id}|operator|{text}");
            call_peer(&state, "brief.comment", arg.as_bytes()).await?;
            Ok(Json(CompanionResponse {
                action: "comment".into(),
                reply: format!("Commented on {id}."),
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
        CompanionAction::Attention => {
            let body = call_peer(&state, "company.actions", b"").await?;
            let json = parse_json(&body);
            let reply = summarize_actions(&json);
            Ok(Json(CompanionResponse {
                action: "attention".into(),
                reply,
                result: Some(json),
            }))
        }
        CompanionAction::BlockedWork => {
            let body = call_peer(&state, "brief.blocked_list", b"50").await?;
            let json = parse_json(&body);
            let reply = summarize_blocked(&json);
            Ok(Json(CompanionResponse {
                action: "blocked".into(),
                reply,
                result: Some(json),
            }))
        }
        CompanionAction::RunningWork => {
            // The same recent-run ledger the dashboard's Runs page reads
            // (`GET /v1/runs` → `brief.runs`), filtered to active Shifts.
            let body = call_peer(&state, "brief.runs", b"").await?;
            let json = parse_json(&body);
            let reply = summarize_runs(&json);
            Ok(Json(CompanionResponse {
                action: "running".into(),
                reply,
                result: Some(json),
            }))
        }
        CompanionAction::Roster => {
            let body = call_peer(&state, "agent.operatives", b"").await?;
            let json = parse_json(&body);
            let reply = summarize_roster(&json);
            Ok(Json(CompanionResponse {
                action: "roster".into(),
                reply,
                result: Some(json),
            }))
        }
        CompanionAction::PlanPackage {
            brief_id,
            plan_body,
            children,
        } => {
            // Refuse an empty plan body or a childless package BEFORE any mesh
            // call — a plan package that materializes nothing is not a plan.
            if let Some(why) = plan_package_problem(&plan_body, &children) {
                return Err(bad(why));
            }
            let children_json: Vec<serde_json::Value> = children
                .iter()
                .map(|c| serde_json::json!({ "title": c.title, "priority": c.priority }))
                .collect();
            // Open through the SAME governed capability the dashboard composer
            // uses (`brief.plan_package_open`): an immutable plan Dossier + a
            // `suggest_tasks` proposal + an approval-bound confirm, atomically.
            // The operator must still approve the confirm before any child is
            // created. Assignee hints are deliberately omitted (priorities only).
            let arg = serde_json::json!({
                "task_id": brief_id,
                "author": "operator",
                "plan_body": plan_body,
                "children": children_json,
            });
            let arg_bytes =
                serde_json::to_vec(&arg).map_err(|e| bad(&format!("encode: {e}")))?;
            let body = call_peer(&state, "brief.plan_package_open", &arg_bytes).await?;
            let json = parse_json(&body);
            let confirm = json
                .get("confirm_id")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            Ok(Json(CompanionResponse {
                action: "plan_package".into(),
                reply: format!(
                    "Opened a plan package on {brief_id}: {} child task(s) proposed. Approve the \
                     bound confirm ({confirm}) to materialize them — nothing is created until you \
                     approve.",
                    children.len()
                ),
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
            CompanionAction::Pin {
                id: "abc".into(),
                on: true
            }
        );
        assert_eq!(
            parse_command("unpin abc"),
            CompanionAction::Pin {
                id: "abc".into(),
                on: false
            }
        );
        // "move" must NOT be swallowed by the pin/assign rules.
        assert_eq!(
            parse_command("move abc to done"),
            CompanionAction::Move {
                id: "abc".into(),
                status: "done".into()
            }
        );
    }

    #[test]
    fn parses_comment_with_optional_on_and_colon() {
        assert_eq!(
            parse_command("comment abc: looks good"),
            CompanionAction::Comment {
                id: "abc".into(),
                text: "looks good".into()
            }
        );
        assert_eq!(
            parse_command("comment on abc: ship it"),
            CompanionAction::Comment {
                id: "abc".into(),
                text: "ship it".into()
            }
        );
        // Missing text → not a comment.
        assert_eq!(parse_command("comment abc:"), CompanionAction::Unknown);
    }

    #[test]
    fn parses_search_overdue_board_help() {
        assert_eq!(
            parse_command("find auth"),
            CompanionAction::Search {
                query: "auth".into()
            }
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

    // ── Company-aware read intents ───────────────────────────────────────────

    #[test]
    fn parses_attention_intents_with_trailing_punctuation() {
        for m in [
            "what needs attention",
            "What needs my attention?",
            "next actions",
            "what should I do",
            "what should i do next?",
            "what's next",
            "whats next",
        ] {
            assert_eq!(parse_command(m), CompanionAction::Attention, "input: {m}");
        }
    }

    #[test]
    fn parses_blocked_intents() {
        for m in ["what is blocked", "what's blocked?", "blocked work", "blocked"] {
            assert_eq!(parse_command(m), CompanionAction::BlockedWork, "input: {m}");
        }
    }

    #[test]
    fn parses_running_intents() {
        for m in ["what is running", "what's running?", "active runs", "running"] {
            assert_eq!(parse_command(m), CompanionAction::RunningWork, "input: {m}");
        }
    }

    #[test]
    fn parses_roster_intents() {
        for m in ["who is on the crew", "who's on the crew?", "roster", "crew", "agents"] {
            assert_eq!(parse_command(m), CompanionAction::Roster, "input: {m}");
        }
    }

    #[test]
    fn read_intents_do_not_swallow_verb_commands() {
        // A create/move command that merely contains an intent word still parses
        // as the verb, because the intents match the WHOLE message only.
        assert_eq!(
            parse_command("create brief blocked the deploy"),
            CompanionAction::CreateBrief {
                title: "blocked the deploy".into()
            }
        );
        assert_eq!(
            parse_command("move abc to in_progress"),
            CompanionAction::Move {
                id: "abc".into(),
                status: "in_progress".into()
            }
        );
    }

    // ── Plan package ─────────────────────────────────────────────────────────

    #[test]
    fn parses_plan_package_with_priorities() {
        let action = parse_command(
            "plan package brf_9: Ship the auth rewrite in three tracks \
             => child: design the schema; child high: build the API; child urgent: cutover",
        );
        match action {
            CompanionAction::PlanPackage {
                brief_id,
                plan_body,
                children,
            } => {
                assert_eq!(brief_id, "brf_9");
                assert_eq!(plan_body, "Ship the auth rewrite in three tracks");
                assert_eq!(
                    children,
                    vec![
                        PlanChild {
                            title: "design the schema".into(),
                            priority: "normal".into()
                        },
                        PlanChild {
                            title: "build the API".into(),
                            priority: "high".into()
                        },
                        PlanChild {
                            title: "cutover".into(),
                            priority: "urgent".into()
                        },
                    ]
                );
            }
            other => panic!("expected PlanPackage, got {other:?}"),
        }
    }

    #[test]
    fn plan_package_body_may_contain_colons() {
        // The body/child split is on `=>`, so colons inside the body survive.
        match parse_command("plan package b1: do X: then Y => child: A") {
            CompanionAction::PlanPackage {
                brief_id,
                plan_body,
                children,
            } => {
                assert_eq!(brief_id, "b1");
                assert_eq!(plan_body, "do X: then Y");
                assert_eq!(children.len(), 1);
                assert_eq!(children[0].title, "A");
            }
            other => panic!("expected PlanPackage, got {other:?}"),
        }
    }

    #[test]
    fn plan_package_drops_malformed_children() {
        // Unrecognized priority + non-`child` segment + empty title are dropped.
        match parse_command(
            "plan package b1: body => child bogus: skip me; note: nope; child: keep; child low:",
        ) {
            CompanionAction::PlanPackage { children, .. } => {
                assert_eq!(
                    children,
                    vec![PlanChild {
                        title: "keep".into(),
                        priority: "normal".into()
                    }]
                );
            }
            other => panic!("expected PlanPackage, got {other:?}"),
        }
    }

    #[test]
    fn plan_package_without_brief_id_is_unknown() {
        assert_eq!(parse_command("plan package : body => child: A"), CompanionAction::Unknown);
        // No colon at all → not a plan package.
        assert_eq!(parse_command("plan package b1 body child A"), CompanionAction::Unknown);
    }

    #[test]
    fn plan_package_problem_flags_no_body_and_no_children() {
        let child = vec![PlanChild {
            title: "x".into(),
            priority: "normal".into(),
        }];
        // No body.
        assert!(plan_package_problem("   ", &child).is_some());
        // No children.
        assert!(plan_package_problem("a real plan", &[]).is_some());
        // The invalid cases also fall out of the parser as empty fields.
        match parse_command("plan package b1: ") {
            CompanionAction::PlanPackage {
                plan_body,
                children,
                ..
            } => {
                assert!(plan_body.is_empty());
                assert!(children.is_empty());
                assert!(plan_package_problem(&plan_body, &children).is_some());
            }
            other => panic!("expected PlanPackage, got {other:?}"),
        }
        // A well-formed package has no problem.
        assert!(plan_package_problem("a real plan", &child).is_none());
    }

    // ── Reply summarizers ────────────────────────────────────────────────────

    #[test]
    fn summarize_actions_top_three_and_calm() {
        let calm = serde_json::json!({ "actions": [], "counts": { "total": 0 } });
        assert!(summarize_actions(&calm).contains("calm"));

        let feed = serde_json::json!({
            "actions": [
                { "title": "Approve hire — Ada" },
                { "title": "Start: wire the login form" },
                { "title": "Review a completed Shift" },
                { "title": "Blocked: migrate the DB" },
            ],
            "counts": { "total": 4 },
        });
        let r = summarize_actions(&feed);
        assert!(r.contains("4 thing(s)"));
        assert!(r.contains("Approve hire — Ada"));
        assert!(r.contains("Start: wire the login form"));
        assert!(r.contains("Review a completed Shift"));
        // Only the top 3 titles are listed.
        assert!(!r.contains("Blocked: migrate the DB"));
    }

    #[test]
    fn summarize_blocked_counts_and_lists() {
        assert!(summarize_blocked(&serde_json::json!([])).contains("No blocked work"));
        let v = serde_json::json!([
            { "task_id": "b1", "title": "migrate the DB", "blocked_by": ["b9"] },
            { "task_id": "b2", "title": "ship UI", "blocked_by": [] },
        ]);
        let r = summarize_blocked(&v);
        assert!(r.contains("2 blocked Brief(s)"));
        assert!(r.contains("migrate the DB (b1) — blocked by 1"));
    }

    #[test]
    fn summarize_runs_filters_to_active() {
        let none = serde_json::json!([
            { "run_id": "r1", "status": "done", "rig": "claude" },
            { "run_id": "r2", "status": "failed", "rig": "echo" },
        ]);
        assert!(summarize_runs(&none).contains("No active Shifts"));

        let some = serde_json::json!([
            { "run_id": "r1", "status": "running", "rig": "claude" },
            { "run_id": "r2", "status": "done", "rig": "echo" },
            { "run_id": "r3", "status": "queued", "rig": "codex" },
        ]);
        let r = summarize_runs(&some);
        assert!(r.contains("2 active Shift(s)"));
        assert!(r.contains("r1 on claude — running"));
        assert!(r.contains("r3 on codex — queued"));
        assert!(!r.contains("r2"));
    }

    #[test]
    fn summarize_roster_counts_active_pending() {
        assert!(summarize_roster(&serde_json::json!([])).contains("No Operatives"));
        let v = serde_json::json!([
            { "agent_id": "a1", "status": "active" },
            { "agent_id": "a2", "status": "pending" },
            { "agent_id": "a3", "status": "active" },
            { "agent_id": "a4", "status": "retired" },
        ]);
        let r = summarize_roster(&v);
        assert!(r.contains("4 Operative(s)"));
        assert!(r.contains("2 active"));
        assert!(r.contains("1 pending"));
        assert!(r.contains("1 other"));
    }
}
