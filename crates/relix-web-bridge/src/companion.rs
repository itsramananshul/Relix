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
use crate::spine::call_ai_chat;

const DEFAULT_PEER: &str = "coordinator";

/// The AI peer session id for companion action selection — distinct from the
/// Prime planner's session so the two request-time seams never share state.
const COMPANION_AI_SESSION: &str = "companion-actions";
/// Hard cap on the prompt sent to the model — bounds cost and keeps the request
/// tight (user message + a few summary lines, never a state dump).
const COMPANION_AI_PROMPT_MAX: usize = 4000;
/// Hard cap on the model's reply we will even attempt to parse — a model that
/// returns more than this is treated as unusable (→ deterministic fallback).
const COMPANION_MODEL_OUTPUT_MAX: usize = 8192;

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize)]
pub struct CompanionRequest {
    pub message: String,
    /// `"ai"` opts into the model-assisted **action-selection** seam
    /// (`relix-dashboard-design.md` §13; `relix-product-roadmap-current.md` §9):
    /// the bridge asks the AI peer to choose ONE strict-JSON action from the
    /// allowlist, validates it into a [`CompanionAction`], then runs the SAME
    /// governed code path as the deterministic parser. Any other value (or
    /// absent) is byte-for-byte the historical rule-based path. Falls back
    /// deterministically whenever the model is unreachable or its choice fails
    /// validation — and never fakes an AI success.
    #[serde(default)]
    pub mode: Option<String>,
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
    /// Honest AI provenance for the action selection (only present in `mode:"ai"`):
    /// `llm_used` (the model chose a valid action that was executed), `fallback`
    /// (the model's choice was unusable → deterministic parser), or `unavailable`
    /// (no model reachable → deterministic parser). Absent on the deterministic
    /// path, mirroring the Prime planner's `ai_mode` contract.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_mode: Option<String>,
    /// True only when `ai_mode == "llm_used"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_used: Option<bool>,
    /// A short, SAFE reason a fallback/unavailable path was taken (never echoes
    /// raw model text — only the bridge's own validator/transport messages).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_reason: Option<String>,
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
    let mut reply =
        format!("{total} Operative(s) on the crew — {active} active, {pending} pending");
    if other > 0 {
        reply.push_str(&format!(", {other} other"));
    }
    reply.push('.');
    reply
}

// ── AI action selection (opt-in `mode:"ai"`) ─────────────────────────────────
// The model NEVER calls a tool, has its freeform text executed, or chooses a
// capability: it returns ONE strict-JSON action from a fixed allowlist, which
// the bridge validates into a `CompanionAction` and runs through the SAME
// governed path as the deterministic parser. Everything here is pure + tested.

/// A bounded, secret-redacted snapshot of company state for the AI prompt — a
/// few summary LINES, never a raw JSON dump. Built best-effort: any capability
/// that fails just yields an empty line (the model still gets the message).
#[derive(Debug, Default, Clone)]
struct CompanionContext {
    roster: String,
    attention: String,
    board: String,
}

/// Compact one-line board summary from `brief.board_summary`: `key=count` pairs
/// for the known board columns, so the model sees the shape without a JSON dump.
/// Best-effort — an unexpected shape yields an empty string.
fn summarize_board_counts(v: &serde_json::Value) -> String {
    let Some(obj) = v.as_object() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for status in BOARD_STATUSES {
        if let Some(n) = obj.get(*status).and_then(|x| x.as_u64()) {
            parts.push(format!("{status}={n}"));
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("Board: {}", parts.join(", "))
    }
}

/// Gather a small, secret-redacted company context for the prompt. Every read
/// goes through the same governed capabilities the deterministic path uses, and
/// any failure is non-fatal (the model just plans with less context).
async fn fetch_companion_context(state: &AppState) -> CompanionContext {
    let mut ctx = CompanionContext::default();
    if let Ok(body) = call_peer(state, "agent.operatives", b"").await {
        ctx.roster = summarize_roster(&parse_json(&body));
    }
    if let Ok(body) = call_peer(state, "company.actions", b"").await {
        // Flatten the summarizer's bullets into one line for a tight prompt.
        ctx.attention = summarize_actions(&parse_json(&body)).replace('\n', "; ");
    }
    if let Ok(body) = call_peer(state, "brief.board_summary", b"").await {
        ctx.board = summarize_board_counts(&parse_json(&body));
    }
    // Defense in depth: company titles/names are operator-authored free text, so
    // redact secrets from every line BEFORE it can reach the model.
    ctx.roster = relix_runtime::rig::redact_secrets(&ctx.roster, "");
    ctx.attention = relix_runtime::rig::redact_secrets(&ctx.attention, "");
    ctx.board = relix_runtime::rig::redact_secrets(&ctx.board, "");
    ctx
}

/// Build the bounded, secret-redacted action-selection prompt. PURE — the model
/// is steered toward a STRICT JSON action from the allowlist, but it is never
/// trusted: `validate_model_action` re-checks every field before execution.
fn build_companion_ai_prompt(redacted_message: &str, ctx: &CompanionContext) -> String {
    let mut context_block = String::new();
    for line in [
        ctx.roster.as_str(),
        ctx.attention.as_str(),
        ctx.board.as_str(),
    ] {
        let line = line.trim();
        if !line.is_empty() {
            context_block.push_str("- ");
            context_block.push_str(line);
            context_block.push('\n');
        }
    }
    if context_block.is_empty() {
        context_block.push_str("(no company context available)\n");
    }
    let prompt = format!(
        "You are the Relix company companion. Choose EXACTLY ONE action that best serves the \
operator's message. Respond with ONLY a single JSON object, no prose, no code fence, matching \
exactly one of these shapes:\n\
{{\"action\":\"attention\"}}  // ranked next actions\n\
{{\"action\":\"blocked\"}}  // briefs waiting on a blocker\n\
{{\"action\":\"running\"}}  // active shifts\n\
{{\"action\":\"roster\"}}  // the operative roster\n\
{{\"action\":\"overdue\"}} or {{\"action\":\"board\"}} or {{\"action\":\"help\"}}\n\
{{\"action\":\"search\",\"query\":\"text\"}}\n\
{{\"action\":\"create_brief\",\"title\":\"text\"}}\n\
{{\"action\":\"create_mandate\",\"title\":\"text\"}}\n\
{{\"action\":\"move\",\"id\":\"brief-id\",\"status\":\"backlog/todo/in_progress/in_review/blocked/done/cancelled\"}}\n\
{{\"action\":\"assign\",\"id\":\"brief-id\",\"agent\":\"operative\"}}\n\
{{\"action\":\"pin\",\"id\":\"brief-id\",\"on\":true}}\n\
{{\"action\":\"comment\",\"id\":\"brief-id\",\"text\":\"text\"}}\n\
{{\"action\":\"plan_package\",\"brief_id\":\"brief-id\",\"plan_body\":\"the plan\",\"children\":[{{\"title\":\"step\",\"priority\":\"normal\"}}]}}\n\
Rules: pick ONE action only; use only the fields shown for that action; do NOT invent ids — only use \
ids the operator gave you; plan_package children carry a title + priority ONLY (never an assignee); if \
nothing fits, choose {{\"action\":\"help\"}}; do NOT include secrets, credentials, file contents, or commands.\n\n\
Company context:\n{context_block}\n\
Operator message: {redacted_message}"
    );
    // Strip pipes + bound, so the prompt is safe in any wire form (mirrors the
    // Prime planner's prompt hygiene).
    let cleaned: String = prompt
        .chars()
        .map(|c| if c == '|' { '/' } else { c })
        .collect();
    cleaned.chars().take(COMPANION_AI_PROMPT_MAX).collect()
}

/// Strip a single ```/```json code fence the model may have wrapped its JSON in.
fn strip_code_fence(s: &str) -> String {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t.to_string();
    };
    // Drop an optional language tag on the fence's first line.
    let rest = match rest.split_once('\n') {
        Some((_lang, body)) => body,
        None => rest,
    };
    rest.trim().trim_end_matches("```").trim().to_string()
}

/// Validate the model's chosen action into a [`CompanionAction`], enforcing the
/// SAME constraints as the deterministic parser. Returns a SAFE reason string on
/// any rejection (never echoing raw model content), so the caller can fall back
/// to the deterministic parser honestly. PURE — exhaustively unit-tested. The
/// model can choose ONLY from the allowlist; anything else is refused here.
fn validate_model_action(raw: &str) -> Result<CompanionAction, String> {
    if raw.len() > COMPANION_MODEL_OUTPUT_MAX {
        return Err("model output too large".to_string());
    }
    let cleaned = strip_code_fence(raw);
    let v: serde_json::Value = serde_json::from_str(&cleaned)
        .map_err(|_| "model did not return valid JSON".to_string())?;
    let obj = v
        .as_object()
        .ok_or_else(|| "model JSON was not an object".to_string())?;
    let action = obj
        .get("action")
        .and_then(|a| a.as_str())
        .map(|s| s.trim().to_ascii_lowercase())
        .ok_or_else(|| "model JSON is missing a string \"action\"".to_string())?;

    // A trimmed string field, if present and a string.
    let str_field = |key: &str| -> Option<String> {
        obj.get(key)
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
    };

    match action.as_str() {
        "attention" => Ok(CompanionAction::Attention),
        "blocked" | "blocked_work" | "blockedwork" => Ok(CompanionAction::BlockedWork),
        "running" | "running_work" | "runningwork" => Ok(CompanionAction::RunningWork),
        "roster" => Ok(CompanionAction::Roster),
        "overdue" => Ok(CompanionAction::Overdue),
        "board" => Ok(CompanionAction::Board),
        "help" => Ok(CompanionAction::Help),
        "search" => {
            let query = str_field("query").unwrap_or_default();
            if query.is_empty() {
                return Err("search needs a non-empty \"query\"".to_string());
            }
            Ok(CompanionAction::Search { query })
        }
        "create_brief" => {
            let title = str_field("title").unwrap_or_default();
            if title.is_empty() {
                return Err("create_brief needs a non-empty \"title\"".to_string());
            }
            if title.contains('|') {
                return Err("title must not contain `|`".to_string());
            }
            Ok(CompanionAction::CreateBrief { title })
        }
        "create_mandate" => {
            let title = str_field("title").unwrap_or_default();
            if title.is_empty() {
                return Err("create_mandate needs a non-empty \"title\"".to_string());
            }
            if title.contains('|') {
                return Err("title must not contain `|`".to_string());
            }
            Ok(CompanionAction::CreateMandate { title })
        }
        "move" => {
            let id = str_field("id").unwrap_or_default();
            if id.is_empty() {
                return Err("move needs a non-empty \"id\"".to_string());
            }
            let status = str_field("status")
                .unwrap_or_default()
                .to_ascii_lowercase()
                .replace(' ', "_");
            if !BOARD_STATUSES.contains(&status.as_str()) {
                return Err("move needs a valid board \"status\"".to_string());
            }
            Ok(CompanionAction::Move { id, status })
        }
        "assign" => {
            let id = str_field("id").unwrap_or_default();
            let agent = str_field("agent").unwrap_or_default();
            if id.is_empty() || agent.is_empty() {
                return Err("assign needs \"id\" and \"agent\"".to_string());
            }
            if agent.contains('|') {
                return Err("agent must not contain `|`".to_string());
            }
            Ok(CompanionAction::Assign { id, agent })
        }
        "pin" => {
            let id = str_field("id").unwrap_or_default();
            if id.is_empty() {
                return Err("pin needs a non-empty \"id\"".to_string());
            }
            // Default to pinning; an explicit `on:false` unpins.
            let on = obj.get("on").and_then(|x| x.as_bool()).unwrap_or(true);
            Ok(CompanionAction::Pin { id, on })
        }
        "comment" => {
            let id = str_field("id").unwrap_or_default();
            let text = str_field("text").unwrap_or_default();
            if id.is_empty() || text.is_empty() {
                return Err("comment needs \"id\" and \"text\"".to_string());
            }
            Ok(CompanionAction::Comment { id, text })
        }
        "plan_package" => {
            let brief_id = str_field("brief_id")
                .or_else(|| str_field("task_id"))
                .unwrap_or_default();
            if brief_id.is_empty() {
                return Err("plan_package needs a non-empty \"brief_id\"".to_string());
            }
            let plan_body = str_field("plan_body")
                .or_else(|| str_field("body"))
                .unwrap_or_default();
            let children = validate_model_children(obj.get("children"))?;
            // Same refusal as the parser: a plan that materializes nothing.
            if let Some(why) = plan_package_problem(&plan_body, &children) {
                return Err(why.to_string());
            }
            Ok(CompanionAction::PlanPackage {
                brief_id,
                plan_body,
                children,
            })
        }
        // Never echo the raw chosen string back — keep the reason safe + generic.
        _ => Err("the chosen action is not in the allowlist".to_string()),
    }
}

/// Validate a model `children` array into [`PlanChild`]s. Every child needs a
/// non-empty title and a valid (or defaulted) priority, and MUST NOT carry an
/// assignee hint — the governed plan package is priorities-only, so a smuggled
/// assignment is refused outright (→ deterministic fallback). PURE.
fn validate_model_children(v: Option<&serde_json::Value>) -> Result<Vec<PlanChild>, String> {
    let arr = match v {
        Some(serde_json::Value::Array(a)) => a,
        Some(_) => return Err("plan_package \"children\" must be an array".to_string()),
        None => return Err("plan_package needs a \"children\" array".to_string()),
    };
    let mut out = Vec::with_capacity(arr.len());
    for child in arr {
        let obj = child
            .as_object()
            .ok_or_else(|| "each plan_package child must be an object".to_string())?;
        if obj.contains_key("assignee") || obj.contains_key("agent") || obj.contains_key("assign") {
            return Err("plan_package children must not carry assignee hints".to_string());
        }
        let title = obj
            .get("title")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if title.is_empty() {
            return Err("each plan_package child needs a non-empty \"title\"".to_string());
        }
        let priority = match obj.get("priority") {
            None | Some(serde_json::Value::Null) => "normal".to_string(),
            Some(serde_json::Value::String(p)) => {
                let p = p.trim().to_ascii_lowercase();
                if p.is_empty() {
                    "normal".to_string()
                } else if PRIORITIES.contains(&p.as_str()) {
                    p
                } else {
                    return Err("plan_package child has an invalid \"priority\"".to_string());
                }
            }
            Some(_) => return Err("plan_package child \"priority\" must be a string".to_string()),
        };
        out.push(PlanChild { title, priority });
    }
    Ok(out)
}

/// `POST /v1/spine/companion` — parse (or AI-select) + execute one command.
///
/// Default (`mode` absent / not `"ai"`): the deterministic parser chooses the
/// action — byte-for-byte the historical behavior. `mode:"ai"`: the model peer
/// chooses ONE validated action and the SAME governed handler runs it; any
/// failure degrades to the deterministic parser with an honest `ai_mode`.
pub async fn handle(
    State(state): State<AppState>,
    Json(req): Json<CompanionRequest>,
) -> Result<Json<CompanionResponse>, (StatusCode, Json<ApiError>)> {
    if req.mode.as_deref() == Some("ai") {
        return handle_ai(&state, &req.message).await;
    }
    let action = parse_command(&req.message);
    Ok(Json(execute_action(&state, action, &req.message).await?))
}

/// Model-assisted action selection. The model may ONLY pick a strict-JSON action
/// from the allowlist; it never calls a tool, never has its freeform text run.
/// The bridge validates that choice into a [`CompanionAction`] and runs the SAME
/// governed handler as the deterministic path. On a missing model, unusable
/// output, or a choice that fails validation, we fall back to the deterministic
/// parser and report the honest `ai_mode` — we never fake an AI success.
async fn handle_ai(
    state: &AppState,
    message: &str,
) -> Result<Json<CompanionResponse>, (StatusCode, Json<ApiError>)> {
    // Redact secrets BEFORE anything reaches the provider (defense in depth).
    let redacted = relix_runtime::rig::redact_secrets(message, "");
    let ctx = fetch_companion_context(state).await;
    let prompt = build_companion_ai_prompt(&redacted, &ctx);

    let (action, ai_mode, ai_reason) =
        match call_ai_chat(state, COMPANION_AI_SESSION, &prompt).await {
            Ok(model_output) => match validate_model_action(&model_output) {
                Ok(action) => (action, "llm_used", None),
                // The model answered but its choice was unusable. Reasons are the
                // bridge's own validator strings — never raw model text.
                Err(reason) => (parse_command(message), "fallback", Some(reason)),
            },
            // No model reachable — honest `unavailable`, deterministic action.
            Err(reason) => (parse_command(message), "unavailable", Some(reason)),
        };

    let mut resp = execute_action(state, action, message).await?;
    resp.ai_mode = Some(ai_mode.to_string());
    resp.ai_used = Some(ai_mode == "llm_used");
    if let Some(note) = ai_fallback_note(ai_mode) {
        resp.reply = format!("{}\n\n{note}", resp.reply);
    }
    resp.ai_reason = ai_reason;
    Ok(Json(resp))
}

/// A short, honest reply suffix when AI mode did NOT drive the action, so the
/// operator is never misled into thinking the model chose it. `None` for
/// `llm_used` (and any non-AI path). Pure — unit-tested.
fn ai_fallback_note(ai_mode: &str) -> Option<&'static str> {
    match ai_mode {
        "fallback" => Some("(The AI suggestion wasn't usable — I used the rule-based parser.)"),
        "unavailable" => Some("(AI is unavailable — I used the rule-based parser.)"),
        _ => None,
    }
}

/// Execute a resolved [`CompanionAction`] through the governed mesh path. This is
/// the SINGLE execution path shared by the deterministic parser and AI mode, so
/// neither can reach a capability the other can't, and AI mode inherits every
/// governance check (approvals, plan-package confirm, tenant scope) unchanged.
async fn execute_action(
    state: &AppState,
    action: CompanionAction,
    message: &str,
) -> Result<CompanionResponse, (StatusCode, Json<ApiError>)> {
    match action {
        CompanionAction::Help => Ok(CompanionResponse {
            action: "help".into(),
            reply: HELP_TEXT.into(),
            result: None,
            ai_mode: None,
            ai_used: None,
            ai_reason: None,
        }),
        CompanionAction::Unknown => Ok(CompanionResponse {
            action: "unknown".into(),
            reply: format!(
                "I didn't understand \"{}\". Type help for commands.",
                message.trim()
            ),
            result: None,
            ai_mode: None,
            ai_used: None,
            ai_reason: None,
        }),
        CompanionAction::CreateBrief { title } => {
            if title.contains('|') {
                return Err(bad("title must not contain `|`"));
            }
            let arg = format!("{title}||||");
            let body = call_peer(state, "brief.create", arg.as_bytes()).await?;
            let id = String::from_utf8_lossy(&body).trim().to_string();
            Ok(CompanionResponse {
                action: "create_brief".into(),
                reply: format!("Created brief “{title}” ({id})."),
                result: Some(serde_json::json!({ "task_id": id })),
                ai_mode: None,
                ai_used: None,
                ai_reason: None,
            })
        }
        CompanionAction::CreateMandate { title } => {
            if title.contains('|') {
                return Err(bad("title must not contain `|`"));
            }
            let arg = format!("{title}|||");
            let body = call_peer(state, "mandate.create", arg.as_bytes()).await?;
            let id = String::from_utf8_lossy(&body).trim().to_string();
            Ok(CompanionResponse {
                action: "create_mandate".into(),
                reply: format!("Created mandate “{title}” ({id})."),
                result: Some(serde_json::json!({ "mandate_id": id })),
                ai_mode: None,
                ai_used: None,
                ai_reason: None,
            })
        }
        CompanionAction::Move { id, status } => {
            let arg = format!("{id}|{status}");
            call_peer(state, "brief.move", arg.as_bytes()).await?;
            Ok(CompanionResponse {
                action: "move".into(),
                reply: format!("Moved {id} → {status}."),
                result: None,
                ai_mode: None,
                ai_used: None,
                ai_reason: None,
            })
        }
        CompanionAction::Assign { id, agent } => {
            if agent.contains('|') {
                return Err(bad("agent must not contain `|`"));
            }
            let arg = format!("{id}|assignee|{agent}");
            call_peer(state, "brief.set", arg.as_bytes()).await?;
            Ok(CompanionResponse {
                action: "assign".into(),
                reply: format!("Assigned {id} → {agent}."),
                result: None,
                ai_mode: None,
                ai_used: None,
                ai_reason: None,
            })
        }
        CompanionAction::Pin { id, on } => {
            let arg = format!("{id}|{}", i32::from(on));
            call_peer(state, "brief.pin", arg.as_bytes()).await?;
            Ok(CompanionResponse {
                action: "pin".into(),
                reply: format!("{} {id}.", if on { "Pinned" } else { "Unpinned" }),
                result: None,
                ai_mode: None,
                ai_used: None,
                ai_reason: None,
            })
        }
        CompanionAction::Comment { id, text } => {
            // `text` is the trailing wire field so it may contain `|`.
            let arg = format!("{id}|operator|{text}");
            call_peer(state, "brief.comment", arg.as_bytes()).await?;
            Ok(CompanionResponse {
                action: "comment".into(),
                reply: format!("Commented on {id}."),
                result: None,
                ai_mode: None,
                ai_used: None,
                ai_reason: None,
            })
        }
        CompanionAction::Overdue => {
            let body = call_peer(state, "brief.overdue", b"|50").await?;
            let json: serde_json::Value =
                serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            let n = json.as_array().map(|a| a.len()).unwrap_or(0);
            Ok(CompanionResponse {
                action: "overdue".into(),
                reply: format!("{n} overdue brief(s)."),
                result: Some(json),
                ai_mode: None,
                ai_used: None,
                ai_reason: None,
            })
        }
        CompanionAction::Board => {
            let body = call_peer(state, "brief.board_summary", b"").await?;
            let json: serde_json::Value =
                serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            Ok(CompanionResponse {
                action: "board".into(),
                reply: "Board summary.".into(),
                result: Some(json),
                ai_mode: None,
                ai_used: None,
                ai_reason: None,
            })
        }
        CompanionAction::Search { query } => {
            let arg = format!("{query}|25");
            let body = call_peer(state, "brief.search", arg.as_bytes()).await?;
            let json: serde_json::Value =
                serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            let n = json.as_array().map(|a| a.len()).unwrap_or(0);
            Ok(CompanionResponse {
                action: "search".into(),
                reply: format!("{n} match(es) for “{query}”."),
                result: Some(json),
                ai_mode: None,
                ai_used: None,
                ai_reason: None,
            })
        }
        CompanionAction::Attention => {
            let body = call_peer(state, "company.actions", b"").await?;
            let json = parse_json(&body);
            let reply = summarize_actions(&json);
            Ok(CompanionResponse {
                action: "attention".into(),
                reply,
                result: Some(json),
                ai_mode: None,
                ai_used: None,
                ai_reason: None,
            })
        }
        CompanionAction::BlockedWork => {
            let body = call_peer(state, "brief.blocked_list", b"50").await?;
            let json = parse_json(&body);
            let reply = summarize_blocked(&json);
            Ok(CompanionResponse {
                action: "blocked".into(),
                reply,
                result: Some(json),
                ai_mode: None,
                ai_used: None,
                ai_reason: None,
            })
        }
        CompanionAction::RunningWork => {
            // The same recent-run ledger the dashboard's Runs page reads
            // (`GET /v1/runs` → `brief.runs`), filtered to active Shifts.
            let body = call_peer(state, "brief.runs", b"").await?;
            let json = parse_json(&body);
            let reply = summarize_runs(&json);
            Ok(CompanionResponse {
                action: "running".into(),
                reply,
                result: Some(json),
                ai_mode: None,
                ai_used: None,
                ai_reason: None,
            })
        }
        CompanionAction::Roster => {
            let body = call_peer(state, "agent.operatives", b"").await?;
            let json = parse_json(&body);
            let reply = summarize_roster(&json);
            Ok(CompanionResponse {
                action: "roster".into(),
                reply,
                result: Some(json),
                ai_mode: None,
                ai_used: None,
                ai_reason: None,
            })
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
            let arg_bytes = serde_json::to_vec(&arg).map_err(|e| bad(&format!("encode: {e}")))?;
            let body = call_peer(state, "brief.plan_package_open", &arg_bytes).await?;
            let json = parse_json(&body);
            let confirm = json
                .get("confirm_id")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            Ok(CompanionResponse {
                action: "plan_package".into(),
                reply: format!(
                    "Opened a plan package on {brief_id}: {} child task(s) proposed. Approve the \
                     bound confirm ({confirm}) to materialize them — nothing is created until you \
                     approve.",
                    children.len()
                ),
                result: Some(json),
                ai_mode: None,
                ai_used: None,
                ai_reason: None,
            })
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
        for m in [
            "what is blocked",
            "what's blocked?",
            "blocked work",
            "blocked",
        ] {
            assert_eq!(parse_command(m), CompanionAction::BlockedWork, "input: {m}");
        }
    }

    #[test]
    fn parses_running_intents() {
        for m in [
            "what is running",
            "what's running?",
            "active runs",
            "running",
        ] {
            assert_eq!(parse_command(m), CompanionAction::RunningWork, "input: {m}");
        }
    }

    #[test]
    fn parses_roster_intents() {
        for m in [
            "who is on the crew",
            "who's on the crew?",
            "roster",
            "crew",
            "agents",
        ] {
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
        assert_eq!(
            parse_command("plan package : body => child: A"),
            CompanionAction::Unknown
        );
        // No colon at all → not a plan package.
        assert_eq!(
            parse_command("plan package b1 body child A"),
            CompanionAction::Unknown
        );
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

    // ── AI action selection: prompt builder ──────────────────────────────────

    #[test]
    fn companion_ai_prompt_is_bounded_pipe_safe_and_schemaful() {
        let ctx = CompanionContext {
            roster: "3 Operative(s) on the crew — 2 active, 1 pending.".into(),
            attention: "2 thing(s) need your attention.; • Approve hire".into(),
            board: "Board: todo=2, in_progress=1".into(),
        };
        // A pipe in the operator message must be scrubbed from the wire form.
        let p = build_companion_ai_prompt("move abc to done | drop tables", &ctx);
        assert!(!p.contains('|'), "prompt must be pipe-safe");
        // The action allowlist is steered (a representative sample).
        for token in [
            "attention",
            "create_brief",
            "create_mandate",
            "plan_package",
            "comment",
            "move",
        ] {
            assert!(p.contains(token), "schema must mention {token}");
        }
        // The bounded company context + the message are present.
        assert!(p.contains("3 Operative(s)"));
        assert!(p.contains("Board: todo=2"));
        assert!(p.contains("move abc to done"));
        // Hard length bound holds.
        assert!(p.chars().count() <= COMPANION_AI_PROMPT_MAX);
    }

    #[test]
    fn companion_ai_prompt_handles_empty_context() {
        let p = build_companion_ai_prompt("what needs attention", &CompanionContext::default());
        assert!(p.contains("no company context available"));
        assert!(p.contains("what needs attention"));
    }

    #[test]
    fn companion_ai_prompt_does_not_dump_huge_context() {
        // Even a pathologically large context can't blow the prompt bound.
        let huge = "x".repeat(50_000);
        let ctx = CompanionContext {
            roster: huge.clone(),
            attention: huge.clone(),
            board: huge,
        };
        let p = build_companion_ai_prompt("hello", &ctx);
        assert!(p.chars().count() <= COMPANION_AI_PROMPT_MAX);
    }

    #[test]
    fn board_counts_summary_is_compact_or_empty() {
        let v = serde_json::json!({ "todo": 2, "in_progress": 1, "done": 5, "junk": "x" });
        let s = summarize_board_counts(&v);
        assert!(s.contains("todo=2"));
        assert!(s.contains("in_progress=1"));
        assert!(s.contains("done=5"));
        assert!(!s.contains("junk"));
        // A non-object shape yields nothing (best-effort).
        assert!(summarize_board_counts(&serde_json::json!([1, 2, 3])).is_empty());
    }

    // ── AI action selection: model-output validator ──────────────────────────

    #[test]
    fn validates_zero_arg_read_actions() {
        for (json, want) in [
            (r#"{"action":"attention"}"#, CompanionAction::Attention),
            (r#"{"action":"blocked"}"#, CompanionAction::BlockedWork),
            (r#"{"action":"running"}"#, CompanionAction::RunningWork),
            (r#"{"action":"roster"}"#, CompanionAction::Roster),
            (r#"{"action":"overdue"}"#, CompanionAction::Overdue),
            (r#"{"action":"board"}"#, CompanionAction::Board),
            (r#"{"action":"help"}"#, CompanionAction::Help),
        ] {
            assert_eq!(validate_model_action(json).unwrap(), want, "input: {json}");
        }
    }

    #[test]
    fn validates_field_actions_into_companion_action() {
        assert_eq!(
            validate_model_action(r#"{"action":"create_brief","title":"Ship auth"}"#).unwrap(),
            CompanionAction::CreateBrief {
                title: "Ship auth".into()
            }
        );
        assert_eq!(
            validate_model_action(r#"{"action":"search","query":"login"}"#).unwrap(),
            CompanionAction::Search {
                query: "login".into()
            }
        );
        assert_eq!(
            validate_model_action(r#"{"action":"assign","id":"b1","agent":"agt_eng"}"#).unwrap(),
            CompanionAction::Assign {
                id: "b1".into(),
                agent: "agt_eng".into()
            }
        );
        // `on` defaults to true (pin); an explicit false unpins.
        assert_eq!(
            validate_model_action(r#"{"action":"pin","id":"b1"}"#).unwrap(),
            CompanionAction::Pin {
                id: "b1".into(),
                on: true
            }
        );
        assert_eq!(
            validate_model_action(r#"{"action":"pin","id":"b1","on":false}"#).unwrap(),
            CompanionAction::Pin {
                id: "b1".into(),
                on: false
            }
        );
        assert_eq!(
            validate_model_action(r#"{"action":"comment","id":"b1","text":"ship it"}"#).unwrap(),
            CompanionAction::Comment {
                id: "b1".into(),
                text: "ship it".into()
            }
        );
    }

    #[test]
    fn validates_move_status_normalised_and_rejects_bad_status() {
        assert_eq!(
            validate_model_action(r#"{"action":"move","id":"b1","status":"In Progress"}"#).unwrap(),
            CompanionAction::Move {
                id: "b1".into(),
                status: "in_progress".into()
            }
        );
        assert!(
            validate_model_action(r#"{"action":"move","id":"b1","status":"nowhere"}"#).is_err()
        );
        assert!(validate_model_action(r#"{"action":"move","status":"done"}"#).is_err());
    }

    #[test]
    fn validates_plan_package_with_children() {
        let json = r#"{"action":"plan_package","brief_id":"b1","plan_body":"do it",
            "children":[{"title":"step one"},{"title":"step two","priority":"high"}]}"#;
        match validate_model_action(json).unwrap() {
            CompanionAction::PlanPackage {
                brief_id,
                plan_body,
                children,
            } => {
                assert_eq!(brief_id, "b1");
                assert_eq!(plan_body, "do it");
                assert_eq!(
                    children,
                    vec![
                        PlanChild {
                            title: "step one".into(),
                            priority: "normal".into()
                        },
                        PlanChild {
                            title: "step two".into(),
                            priority: "high".into()
                        },
                    ]
                );
            }
            other => panic!("expected PlanPackage, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_plan_package_and_bad_children() {
        // No body.
        assert!(validate_model_action(
            r#"{"action":"plan_package","brief_id":"b1","plan_body":"","children":[{"title":"x"}]}"#
        )
        .is_err());
        // No children.
        assert!(
            validate_model_action(
                r#"{"action":"plan_package","brief_id":"b1","plan_body":"plan","children":[]}"#
            )
            .is_err()
        );
        // Invalid priority.
        assert!(validate_model_action(
            r#"{"action":"plan_package","brief_id":"b1","plan_body":"p","children":[{"title":"x","priority":"asap"}]}"#
        )
        .is_err());
        // A smuggled assignee hint is refused (priorities-only governance).
        assert!(validate_model_action(
            r#"{"action":"plan_package","brief_id":"b1","plan_body":"p","children":[{"title":"x","assignee":"agt_eng"}]}"#
        )
        .is_err());
    }

    #[test]
    fn rejects_unsafe_fields_and_unknown_actions() {
        // A pipe in a non-trailing field is refused (same as the parser).
        assert!(validate_model_action(r#"{"action":"create_brief","title":"a|b"}"#).is_err());
        assert!(validate_model_action(r#"{"action":"assign","id":"b1","agent":"x|y"}"#).is_err());
        // Empty required payloads.
        assert!(validate_model_action(r#"{"action":"create_brief","title":""}"#).is_err());
        assert!(validate_model_action(r#"{"action":"search"}"#).is_err());
        // Unknown / non-allowlisted action.
        assert!(validate_model_action(r#"{"action":"delete_everything","id":"b1"}"#).is_err());
        assert!(validate_model_action(r#"{"action":"run","id":"b1"}"#).is_err());
    }

    #[test]
    fn rejects_non_json_and_non_object_and_oversized() {
        assert!(validate_model_action("Sure! I'll move that brief for you.").is_err());
        assert!(validate_model_action("[1,2,3]").is_err());
        assert!(validate_model_action(r#"{"no_action":"here"}"#).is_err());
        // Oversized output is treated as unusable BEFORE parsing.
        let big = format!("{{\"action\":\"help\",\"pad\":\"{}\"}}", "z".repeat(9000));
        assert!(validate_model_action(&big).is_err());
    }

    #[test]
    fn validator_strips_code_fence() {
        let fenced = "```json\n{\"action\":\"roster\"}\n```";
        assert_eq!(
            validate_model_action(fenced).unwrap(),
            CompanionAction::Roster
        );
        let bare_fence = "```\n{\"action\":\"board\"}\n```";
        assert_eq!(
            validate_model_action(bare_fence).unwrap(),
            CompanionAction::Board
        );
    }

    // ── AI provenance metadata ───────────────────────────────────────────────

    #[test]
    fn ai_fallback_note_only_on_non_llm_paths() {
        assert!(ai_fallback_note("llm_used").is_none());
        assert!(ai_fallback_note("fallback").unwrap().contains("rule-based"));
        assert!(
            ai_fallback_note("unavailable")
                .unwrap()
                .contains("unavailable")
        );
    }
}
