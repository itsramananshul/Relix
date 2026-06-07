//! Prime Orchestration Authoring v1 — opt-in, constrained model authoring of the
//! *text* (titles / dossiers / checklists) of an EXISTING orchestration skeleton
//! (company-model §4.6 the Mandate→Brief materialisation; §12.5/§12.5A the Prime
//! planner / model-assisted seam, here applied to the Brief work-object text).
//!
//! **THE MODEL IS NOT THE PERMISSION SYSTEM.** This module lets an opt-in model
//! author ONLY the human-facing text of the orchestration skeleton the
//! deterministic readiness logic has ALREADY computed: the parent Brief, the
//! active role-track Briefs, and the per-agent subject-execution Briefs. It
//! CANNOT invent a role, an agent, a Brief id, a source marker, a dependency, an
//! assignee, an approval, a budget change, or a tool — those are all fixed by
//! [`super::handlers::handle_orchestrate`], whose gates (approved strategy, ready
//! team, assign-Key, reviewer stamping, max_briefs cap, placeholder behaviour,
//! source-marker idempotency) stay authoritative. The blueprint is keyed STRICTLY
//! by the stable role keys + subject (agent) keys the snapshot offered; the parser
//! rejects any unknown / mis-shaped / overlong key or value. If model output is
//! invalid / unavailable / disabled, the caller falls back to the deterministic
//! titles + dossiers exactly as today.
//!
//! This module is PURE and dependency-light (snapshot → prompt → parse), so the
//! prompt builder and the validator are fully unit-tested without a mesh or a
//! provider. The live mesh `ai.chat` wiring (the shared
//! [`super::prime_deliberation::PrimeAiDecider`]) and the deterministic fallback
//! that bounds it live in `prime_driver`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

/// Hard cap on the prompt we hand the model — bounds cost and keeps the request
/// tight (a bounded snapshot only, never a repo / file / secret dump).
pub const MAX_ORCH_PROMPT_CHARS: usize = 4000;
/// Hard cap on the raw model output we will even attempt to parse. A larger blob
/// is rejected outright (→ deterministic fallback) rather than parsed.
pub const MAX_ORCH_OUTPUT_CHARS: usize = 8000;
/// Hard cap on any authored title (chars). A title is a single short line.
pub const MAX_ORCH_TITLE_CHARS: usize = 120;
/// Hard cap on any authored dossier body (chars).
pub const MAX_ORCH_DOSSIER_CHARS: usize = 600;
/// Hard cap on the number of checklist items per item.
pub const MAX_ORCH_CHECKLIST_ITEMS: usize = 8;
/// Hard cap on a single checklist item (chars).
pub const MAX_ORCH_CHECKLIST_ITEM_CHARS: usize = 120;
/// Hard cap on how many role / subject entries the snapshot offers (and the
/// parser accepts). The orchestration tree is already `max_briefs`-bounded; this
/// caps the prompt so a large Guild never produces an unbounded request.
pub const MAX_ORCH_ENTRIES: usize = 24;
/// Hard cap on the bounded approved-strategy excerpt fed to the model.
pub const MAX_ORCH_STRATEGY_EXCERPT_CHARS: usize = 600;
/// The opaque, stable key the parent Brief is offered + authored under.
pub const PARENT_KEY: &str = "parent";

/// How the orchestration skeleton's TEXT was authored on a single
/// `orchestrate_assign_ready` action — surfaced on the tick record so the
/// operator sees the provenance instead of guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimeOrchestrationMode {
    /// Orchestration authoring is off (the env flag is unset): the deterministic
    /// title + dossier helpers authored every work object.
    DeterministicOnly,
    /// The model returned a valid, bounded, in-key blueprint that was used for
    /// the newly-created work objects.
    LlmUsed,
    /// The model answered but its output was malformed / out-of-key / overlong /
    /// mis-shaped, so the deterministic text was used instead.
    Fallback,
    /// The model could not be reached (no decider / mesh / AI peer, or the call
    /// failed), so the deterministic text was used.
    Unavailable,
}

impl PrimeOrchestrationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PrimeOrchestrationMode::DeterministicOnly => "deterministic_only",
            PrimeOrchestrationMode::LlmUsed => "llm_used",
            PrimeOrchestrationMode::Fallback => "fallback",
            PrimeOrchestrationMode::Unavailable => "unavailable",
        }
    }
}

/// One active role offered to the authoring model: its stable role key (the
/// lowercased role, matching the `mandate:{id}:role:{key}` source marker) and the
/// active Operative's agent id (the subject key). Both are fixed by the
/// deterministic readiness logic; the model may only author text *for* them.
#[derive(Debug, Clone)]
pub struct PrimeOrchestrationRole {
    /// Lowercased role key (e.g. `engineer`).
    pub role_key: String,
    /// The active Operative's agent id — the subject key for this role's
    /// execution Brief.
    pub agent_id: String,
}

/// A bounded, secret-free snapshot the authoring model writes a blueprint from.
/// Built from the Mandate's own fields, a bounded approved-strategy excerpt, the
/// active role list (role key + agent id), the gap roles (context only — their
/// placeholder text stays deterministic), and the `max_briefs` cap. Never any
/// secret, credential, token, repo content, or large free-text dump.
#[derive(Debug, Clone)]
pub struct PrimeOrchestrationSnapshot {
    pub mandate_title: String,
    pub mandate_status: String,
    /// A bounded excerpt of the APPROVED strategy doc (may be empty).
    pub strategy_excerpt: String,
    /// Active roles with their staffed agent — the only keys the model may author.
    pub active_roles: Vec<PrimeOrchestrationRole>,
    /// Gap roles (missing / pending / blocked) as `(role_key, reason)` — context
    /// for the model only; their placeholder text is NOT model-authored.
    pub gap_roles: Vec<(String, String)>,
    pub max_briefs: usize,
}

impl PrimeOrchestrationSnapshot {
    /// Build a snapshot, bounding the strategy excerpt and the role/gap lists.
    pub fn new(
        mandate_title: &str,
        mandate_status: &str,
        strategy_doc: Option<&str>,
        active_roles: Vec<PrimeOrchestrationRole>,
        gap_roles: Vec<(String, String)>,
        max_briefs: usize,
    ) -> Self {
        let mandate_title = match mandate_title.trim() {
            "" => "(untitled Mandate)".to_string(),
            t => t.to_string(),
        };
        let mandate_status = match mandate_status.trim() {
            "" => "active".to_string(),
            s => s.to_string(),
        };
        let strategy_excerpt = strategy_doc.map(str::trim).unwrap_or_default();
        let strategy_excerpt = if strategy_excerpt.chars().count() > MAX_ORCH_STRATEGY_EXCERPT_CHARS
        {
            let clipped: String = strategy_excerpt
                .chars()
                .take(MAX_ORCH_STRATEGY_EXCERPT_CHARS)
                .collect();
            format!("{clipped}…")
        } else {
            strategy_excerpt.to_string()
        };
        let active_roles: Vec<PrimeOrchestrationRole> =
            active_roles.into_iter().take(MAX_ORCH_ENTRIES).collect();
        let gap_roles: Vec<(String, String)> =
            gap_roles.into_iter().take(MAX_ORCH_ENTRIES).collect();
        Self {
            mandate_title,
            mandate_status,
            strategy_excerpt,
            active_roles,
            gap_roles,
            max_briefs,
        }
    }

    /// The role keys offered for authoring (active roles only).
    pub fn offered_role_keys(&self) -> Vec<String> {
        self.active_roles
            .iter()
            .map(|r| r.role_key.clone())
            .collect()
    }

    /// The subject (agent id) keys offered for authoring.
    pub fn offered_subject_keys(&self) -> Vec<String> {
        self.active_roles
            .iter()
            .map(|r| r.agent_id.clone())
            .collect()
    }
}

/// The authored text for ONE work object (parent / role track / subject). Every
/// field is optional + bounded; an absent field falls back to the deterministic
/// text for that work object. `title` is a single short line; `dossier` is a
/// short body; `checklist` is a bounded list of single-line items.
#[derive(Debug, Clone, Default)]
pub struct PrimeOrchestrationItem {
    pub title: Option<String>,
    pub dossier: Option<String>,
    pub checklist: Vec<String>,
}

impl PrimeOrchestrationItem {
    /// Render the authored dossier + checklist into a single Dossier body, or
    /// `None` when neither was authored. The checklist is appended as a Markdown
    /// `## Checklist` section so the deterministic Dossier shape is preserved.
    pub fn dossier_body(&self) -> Option<String> {
        let mut s = self.dossier.clone().unwrap_or_default();
        if !self.checklist.is_empty() {
            if !s.is_empty() {
                s.push_str("\n\n");
            }
            s.push_str("## Checklist\n");
            for item in &self.checklist {
                s.push_str("- ");
                s.push_str(item);
                s.push('\n');
            }
        }
        let s = s.trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    }
}

/// A validated orchestration blueprint: optional parent text + per-role-track and
/// per-subject authored text, keyed STRICTLY by the offered role / subject keys.
/// The caller applies a field ONLY to the matching newly-created work object; an
/// absent field (or an absent key) falls back to the deterministic text, and an
/// already-existing work object's title is NEVER clobbered.
#[derive(Debug, Clone, Default)]
pub struct PrimeOrchestrationBlueprint {
    pub parent: PrimeOrchestrationItem,
    /// Role-track text keyed by lowercased role key.
    pub roles: BTreeMap<String, PrimeOrchestrationItem>,
    /// Subject-execution text keyed by agent id.
    pub subjects: BTreeMap<String, PrimeOrchestrationItem>,
}

impl PrimeOrchestrationBlueprint {
    pub fn parent_title(&self) -> Option<&str> {
        self.parent.title.as_deref()
    }
    pub fn parent_dossier_body(&self) -> Option<String> {
        self.parent.dossier_body()
    }
    pub fn role(&self, role_key: &str) -> Option<&PrimeOrchestrationItem> {
        self.roles.get(role_key)
    }
    pub fn subject(&self, agent_id: &str) -> Option<&PrimeOrchestrationItem> {
        self.subjects.get(agent_id)
    }
}

/// Replace pipe + non-whitespace control chars (keep `\n`/`\t`) so a block is
/// safe inside a pipe-delimited wire and a log line.
fn sanitize_block(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '|' {
                '/'
            } else if c.is_control() && c != '\n' && c != '\t' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Replace pipe + ALL control chars (including newlines/tabs) with a space, for a
/// single-line field (a title or a checklist item).
fn sanitize_inline(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '|' {
                '/'
            } else if c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Strip a single leading/trailing markdown code fence (```json … ``` or ``` …
/// ```) if present, returning the inner body. Leaves un-fenced input untouched.
fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let rest = match rest.find('\n') {
        Some(nl) => &rest[nl + 1..],
        None => rest,
    };
    rest.trim()
        .strip_suffix("```")
        .map_or(rest.trim(), str::trim)
}

/// Build the bounded, sanitized orchestration-authoring prompt. PURE +
/// unit-tested. The model is told the orchestration skeleton is ALREADY decided
/// (roles, agents, assignments are fixed) and that its only job is to author
/// concise titles / dossiers / checklists for the listed keys, as strict JSON
/// keyed ONLY by the offered role / subject keys. Because the coordinator
/// re-validates + re-gates everything (the model authors text only), the prompt
/// only needs to steer — it is never trusted.
pub fn build_orchestration_prompt(snap: &PrimeOrchestrationSnapshot) -> String {
    let mut roles_block = String::new();
    for r in &snap.active_roles {
        roles_block.push_str(&format!(
            "- role_key: {role} | subject_key (agent): {agent}\n",
            role = r.role_key,
            agent = r.agent_id,
        ));
    }
    if roles_block.is_empty() {
        roles_block.push_str("(no active role tracks)\n");
    }
    let gaps_block = if snap.gap_roles.is_empty() {
        "(none)".to_string()
    } else {
        snap.gap_roles
            .iter()
            .map(|(role, reason)| format!("{role} ({reason})"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let strategy = if snap.strategy_excerpt.is_empty() {
        "(no approved strategy excerpt provided)".to_string()
    } else {
        snap.strategy_excerpt.clone()
    };
    let raw = format!(
        "You are Prime, a company planning lead. The orchestration skeleton for the Mandate below is \
ALREADY decided by the system: the roles, the staffed agents, the assignments, the Brief ids, and \
the dependencies are FIXED and you cannot change them. Your ONLY job is to author concise, useful \
TEXT (titles, dossiers, checklists) for the work objects listed by key.\n\
Rules:\n\
- Respond with ONLY a single JSON object, no prose, no code fence.\n\
- Shape: {{\"parent\":{{\"title\":\"…\",\"dossier\":\"…\"}},\"roles\":{{\"<role_key>\":{{\"title\":\"…\",\"dossier\":\"…\",\"checklist\":[\"…\"]}}}},\"subjects\":{{\"<subject_key>\":{{\"title\":\"…\",\"dossier\":\"…\",\"checklist\":[\"…\"]}}}}}}.\n\
- Use ONLY the role_key and subject_key values listed below; do NOT invent keys, roles, agents, or ids.\n\
- Every field is optional; omit a key or field to keep the system's default text.\n\
- Titles are one short line (<= {title_cap} chars). Dossiers are short (<= {dossier_cap} chars). \
At most {checklist_cap} checklist items, each one short line.\n\
- Do NOT include secrets, credentials, tokens, file contents, or shell/tool commands.\n\n\
Mandate:\n\
- title: {title}\n\
- status: {status}\n\
- max work objects: {max_briefs}\n\
- approved strategy excerpt: {strategy}\n\
- staffing gaps (context only; their text is fixed): {gaps}\n\n\
Work objects to author (author the parent under key \"{parent_key}\"):\n\
{roles_block}",
        title_cap = MAX_ORCH_TITLE_CHARS,
        dossier_cap = MAX_ORCH_DOSSIER_CHARS,
        checklist_cap = MAX_ORCH_CHECKLIST_ITEMS,
        title = snap.mandate_title,
        status = snap.mandate_status,
        max_briefs = snap.max_briefs,
        strategy = strategy,
        gaps = gaps_block,
        parent_key = PARENT_KEY,
        roles_block = roles_block,
    );
    let cleaned = sanitize_block(&raw);
    cleaned.chars().take(MAX_ORCH_PROMPT_CHARS).collect()
}

/// Validate a single string field (title / dossier) under `cap`. `inline` strips
/// newlines (a title); otherwise newlines/tabs are kept (a dossier body). Rejects
/// a non-string value and an overlong string.
fn parse_text_field(
    v: &Value,
    cap: usize,
    inline: bool,
    what: &str,
) -> Result<Option<String>, String> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => {
            if s.chars().count() > cap {
                return Err(format!("{what} too long"));
            }
            let cleaned = if inline {
                sanitize_inline(s.trim())
            } else {
                sanitize_block(s.trim())
            };
            let cleaned = cleaned.trim().to_string();
            if cleaned.is_empty() {
                Ok(None)
            } else {
                Ok(Some(cleaned))
            }
        }
        _ => Err(format!("{what} must be a string")),
    }
}

/// Parse + validate one authored item object `{title?, dossier?, checklist?}`.
/// Rejects a non-object item, an array where a string is expected, an overlong
/// title / dossier / checklist item, and too many checklist items.
fn parse_item(v: &Value, what: &str) -> Result<PrimeOrchestrationItem, String> {
    let obj = match v {
        Value::Object(m) => m,
        Value::Array(_) => return Err(format!("{what} is an array, not an object")),
        _ => return Err(format!("{what} is not a JSON object")),
    };
    let title = match obj.get("title") {
        Some(t) => parse_text_field(t, MAX_ORCH_TITLE_CHARS, true, &format!("{what} title"))?,
        None => None,
    };
    let dossier = match obj.get("dossier") {
        Some(d) => parse_text_field(d, MAX_ORCH_DOSSIER_CHARS, false, &format!("{what} dossier"))?,
        None => None,
    };
    let checklist = match obj.get("checklist") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            if items.len() > MAX_ORCH_CHECKLIST_ITEMS {
                return Err(format!("{what} checklist has too many items"));
            }
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    Value::String(s) => {
                        if s.chars().count() > MAX_ORCH_CHECKLIST_ITEM_CHARS {
                            return Err(format!("{what} checklist item too long"));
                        }
                        let cleaned = sanitize_inline(s.trim()).trim().to_string();
                        if !cleaned.is_empty() {
                            out.push(cleaned);
                        }
                    }
                    _ => return Err(format!("{what} checklist item must be a string")),
                }
            }
            out
        }
        Some(_) => return Err(format!("{what} checklist must be an array")),
    };
    Ok(PrimeOrchestrationItem {
        title,
        dossier,
        checklist,
    })
}

/// Parse a keyed map (`roles` / `subjects`) constrained to `offered`. STRICT:
/// rejects a non-object map, an unknown key, more entries than offered, and any
/// mis-shaped / overlong item.
fn parse_keyed_map(
    v: Option<&Value>,
    offered: &BTreeSet<&str>,
    what: &str,
) -> Result<BTreeMap<String, PrimeOrchestrationItem>, String> {
    let Some(v) = v else {
        return Ok(BTreeMap::new());
    };
    let obj = match v {
        Value::Null => return Ok(BTreeMap::new()),
        Value::Object(m) => m,
        Value::Array(_) => return Err(format!("`{what}` is an array, not an object")),
        _ => return Err(format!("`{what}` must be an object")),
    };
    if obj.len() > offered.len() {
        return Err(format!("`{what}` lists more keys than were offered"));
    }
    let mut out = BTreeMap::new();
    for (key, item_val) in obj {
        if !offered.contains(key.as_str()) {
            return Err(format!("unknown {what} key `{key}` not in the offered set"));
        }
        let item = parse_item(item_val, &format!("{what} `{key}`"))?;
        out.insert(key.clone(), item);
    }
    Ok(out)
}

/// Validate + parse a raw model reply into a [`PrimeOrchestrationBlueprint`]
/// constrained to the offered role + subject keys. STRICT: rejects empty /
/// overlong output, non-object JSON, an unknown top-level key, an unknown role /
/// subject key, an array where an object is expected, and any overlong / mis-shaped
/// field. On any rejection the caller falls back to the deterministic text. PURE +
/// unit-tested.
pub fn parse_orchestration_blueprint(
    raw: &str,
    offered_role_keys: &[String],
    offered_subject_keys: &[String],
) -> Result<PrimeOrchestrationBlueprint, String> {
    if raw.chars().count() > MAX_ORCH_OUTPUT_CHARS {
        return Err("model output too long".to_string());
    }
    let body = strip_code_fence(raw);
    if body.is_empty() {
        return Err("empty model output".to_string());
    }
    let value: Value = serde_json::from_str(body).map_err(|e| format!("malformed JSON: {e}"))?;
    let obj = match &value {
        Value::Object(map) => map,
        Value::Array(_) => return Err("model output is an array, not an object".to_string()),
        _ => return Err("model output is not a JSON object".to_string()),
    };
    // Reject any unknown top-level key — the model may only author the three
    // known sections; anything else is an out-of-shape reply.
    for key in obj.keys() {
        if key != PARENT_KEY && key != "roles" && key != "subjects" {
            return Err(format!("unknown top-level key `{key}`"));
        }
    }
    let parent = match obj.get(PARENT_KEY) {
        None | Some(Value::Null) => PrimeOrchestrationItem::default(),
        Some(v) => parse_item(v, "parent")?,
    };
    let role_set: BTreeSet<&str> = offered_role_keys.iter().map(String::as_str).collect();
    let subject_set: BTreeSet<&str> = offered_subject_keys.iter().map(String::as_str).collect();
    let roles = parse_keyed_map(obj.get("roles"), &role_set, "roles")?;
    let subjects = parse_keyed_map(obj.get("subjects"), &subject_set, "subjects")?;
    Ok(PrimeOrchestrationBlueprint {
        parent,
        roles,
        subjects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> PrimeOrchestrationSnapshot {
        PrimeOrchestrationSnapshot::new(
            "Ship v1",
            "active",
            Some("# Strategy\nDeliver the product."),
            vec![
                PrimeOrchestrationRole {
                    role_key: "engineer".into(),
                    agent_id: "agent-eng".into(),
                },
                PrimeOrchestrationRole {
                    role_key: "designer".into(),
                    agent_id: "agent-dsn".into(),
                },
            ],
            vec![("qa".into(), "pending hire".into())],
            16,
        )
    }

    fn ks(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn accepts_a_valid_blueprint() {
        let raw = r#"{
            "parent": {"title": "Execute: Ship v1", "dossier": "Top-level plan."},
            "roles": {"engineer": {"title": "Engineering track", "dossier": "Build it.", "checklist": ["wire api", "tests"]}},
            "subjects": {"agent-eng": {"title": "Eng exec", "dossier": "Do the work."}}
        }"#;
        let bp = parse_orchestration_blueprint(
            raw,
            &ks(&["engineer", "designer"]),
            &ks(&["agent-eng", "agent-dsn"]),
        )
        .expect("valid blueprint accepted");
        assert_eq!(bp.parent_title(), Some("Execute: Ship v1"));
        assert_eq!(
            bp.role("engineer").unwrap().title.as_deref(),
            Some("Engineering track")
        );
        let body = bp.role("engineer").unwrap().dossier_body().unwrap();
        assert!(body.contains("Build it."));
        assert!(body.contains("## Checklist"));
        assert!(body.contains("- wire api"));
        assert_eq!(
            bp.subject("agent-eng").unwrap().title.as_deref(),
            Some("Eng exec")
        );
    }

    #[test]
    fn accepts_a_partial_blueprint() {
        // Only a parent title; everything else falls back deterministically.
        let raw = r#"{"parent":{"title":"Just a parent"}}"#;
        let bp = parse_orchestration_blueprint(raw, &ks(&["engineer"]), &ks(&["agent-eng"]))
            .expect("partial blueprint accepted");
        assert_eq!(bp.parent_title(), Some("Just a parent"));
        assert!(bp.parent_dossier_body().is_none());
        assert!(bp.role("engineer").is_none());
    }

    #[test]
    fn rejects_unknown_role_key() {
        let e = parse_orchestration_blueprint(
            r#"{"roles":{"marketing":{"title":"x"}}}"#,
            &ks(&["engineer"]),
            &ks(&["agent-eng"]),
        )
        .unwrap_err();
        assert!(e.contains("unknown roles key"), "got: {e}");
    }

    #[test]
    fn rejects_unknown_subject_key() {
        let e = parse_orchestration_blueprint(
            r#"{"subjects":{"agent-x":{"title":"x"}}}"#,
            &ks(&["engineer"]),
            &ks(&["agent-eng"]),
        )
        .unwrap_err();
        assert!(e.contains("unknown subjects key"), "got: {e}");
    }

    #[test]
    fn rejects_unknown_top_level_key() {
        let e = parse_orchestration_blueprint(
            r#"{"dependencies":{"a":"b"}}"#,
            &ks(&["engineer"]),
            &ks(&["agent-eng"]),
        )
        .unwrap_err();
        assert!(e.contains("unknown top-level key"), "got: {e}");
    }

    #[test]
    fn rejects_array_top_level() {
        let e = parse_orchestration_blueprint(r#"["x"]"#, &ks(&["engineer"]), &ks(&["agent-eng"]))
            .unwrap_err();
        assert!(e.contains("array"), "got: {e}");
    }

    #[test]
    fn rejects_array_where_object_expected() {
        let e = parse_orchestration_blueprint(
            r#"{"roles":["engineer"]}"#,
            &ks(&["engineer"]),
            &ks(&["agent-eng"]),
        )
        .unwrap_err();
        assert!(e.contains("array"), "got: {e}");
    }

    #[test]
    fn rejects_item_that_is_an_array() {
        let e = parse_orchestration_blueprint(
            r#"{"roles":{"engineer":["x"]}}"#,
            &ks(&["engineer"]),
            &ks(&["agent-eng"]),
        )
        .unwrap_err();
        assert!(e.contains("is an array"), "got: {e}");
    }

    #[test]
    fn rejects_prose() {
        let e = parse_orchestration_blueprint(
            "Sure! Here is the plan.",
            &ks(&["engineer"]),
            &ks(&["agent-eng"]),
        )
        .unwrap_err();
        assert!(e.contains("malformed JSON"), "got: {e}");
    }

    #[test]
    fn rejects_overlong_output() {
        let raw = "x".repeat(MAX_ORCH_OUTPUT_CHARS + 1);
        let e = parse_orchestration_blueprint(&raw, &ks(&["engineer"]), &ks(&["agent-eng"]))
            .unwrap_err();
        assert!(e.contains("too long"), "got: {e}");
    }

    #[test]
    fn rejects_overlong_title() {
        let long = "x".repeat(MAX_ORCH_TITLE_CHARS + 1);
        let raw = format!(r#"{{"parent":{{"title":"{long}"}}}}"#);
        let e = parse_orchestration_blueprint(&raw, &ks(&["engineer"]), &ks(&["agent-eng"]))
            .unwrap_err();
        assert!(e.contains("title too long"), "got: {e}");
    }

    #[test]
    fn rejects_overlong_dossier() {
        let long = "x".repeat(MAX_ORCH_DOSSIER_CHARS + 1);
        let raw = format!(r#"{{"roles":{{"engineer":{{"dossier":"{long}"}}}}}}"#);
        let e = parse_orchestration_blueprint(&raw, &ks(&["engineer"]), &ks(&["agent-eng"]))
            .unwrap_err();
        assert!(e.contains("dossier too long"), "got: {e}");
    }

    #[test]
    fn rejects_too_many_checklist_items() {
        let items: Vec<String> = (0..MAX_ORCH_CHECKLIST_ITEMS + 1)
            .map(|i| format!("\"item {i}\""))
            .collect();
        let raw = format!(
            r#"{{"roles":{{"engineer":{{"checklist":[{}]}}}}}}"#,
            items.join(",")
        );
        let e = parse_orchestration_blueprint(&raw, &ks(&["engineer"]), &ks(&["agent-eng"]))
            .unwrap_err();
        assert!(e.contains("too many items"), "got: {e}");
    }

    #[test]
    fn rejects_non_string_checklist_item() {
        let e = parse_orchestration_blueprint(
            r#"{"roles":{"engineer":{"checklist":[3]}}}"#,
            &ks(&["engineer"]),
            &ks(&["agent-eng"]),
        )
        .unwrap_err();
        assert!(e.contains("must be a string"), "got: {e}");
    }

    #[test]
    fn sanitizes_pipe_to_slash_in_title_and_dossier() {
        let raw = r#"{"parent":{"title":"a | b | c","dossier":"step a | step b"}}"#;
        let bp = parse_orchestration_blueprint(raw, &ks(&["engineer"]), &ks(&["agent-eng"]))
            .expect("accepted");
        assert!(!bp.parent_title().unwrap().contains('|'));
        assert!(bp.parent_title().unwrap().contains('/'));
        assert!(!bp.parent_dossier_body().unwrap().contains('|'));
    }

    #[test]
    fn sanitizes_control_chars_and_collapses_title_newlines() {
        // A NUL is stripped everywhere; a newline in a TITLE becomes a space, but
        // a newline in a DOSSIER body is kept.
        let raw = "{\"parent\":{\"title\":\"a\\u0000b\\nc\",\"dossier\":\"line1\\nline2\"}}";
        let bp = parse_orchestration_blueprint(raw, &ks(&["engineer"]), &ks(&["agent-eng"]))
            .expect("accepted");
        let title = bp.parent_title().unwrap();
        assert!(!title.contains('\u{0}'));
        assert!(!title.contains('\n'));
        assert!(bp.parent_dossier_body().unwrap().contains('\n'));
    }

    #[test]
    fn prompt_is_bounded_pipe_free_and_lists_only_offered_keys() {
        let p = build_orchestration_prompt(&snap());
        assert!(p.chars().count() <= MAX_ORCH_PROMPT_CHARS);
        assert!(!p.contains('|'), "prompt must be pipe-free");
        assert!(p.contains("engineer"));
        assert!(p.contains("agent-eng"));
        assert!(p.contains("parent"));
        assert!(p.contains("JSON"));
        // Gaps are listed as context but the gap role is never an offered authoring key
        // (it appears only in the gaps line, not as a role_key entry to author).
        assert!(p.contains("qa (pending hire)"));
    }

    #[test]
    fn prompt_is_clamped_for_many_roles() {
        let roles: Vec<PrimeOrchestrationRole> = (0..200)
            .map(|i| PrimeOrchestrationRole {
                role_key: format!("role{i}"),
                agent_id: format!("agent{i}"),
            })
            .collect();
        let s = PrimeOrchestrationSnapshot::new("T", "active", None, roles, vec![], 16);
        // The snapshot bounds the offered roles…
        assert!(s.active_roles.len() <= MAX_ORCH_ENTRIES);
        // …and the prompt is bounded regardless.
        let p = build_orchestration_prompt(&s);
        assert!(p.chars().count() <= MAX_ORCH_PROMPT_CHARS);
    }

    #[test]
    fn mode_strings_are_stable() {
        assert_eq!(
            PrimeOrchestrationMode::DeterministicOnly.as_str(),
            "deterministic_only"
        );
        assert_eq!(PrimeOrchestrationMode::LlmUsed.as_str(), "llm_used");
        assert_eq!(PrimeOrchestrationMode::Fallback.as_str(), "fallback");
        assert_eq!(PrimeOrchestrationMode::Unavailable.as_str(), "unavailable");
    }
}
