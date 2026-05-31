//! GAP 12 — structured evidence records.
//!
//! Companion to the GAP-11 transactional gateway. Every
//! dispatch through
//! [`super::dispatcher::ToolDispatcher::dispatch_with_options`]
//! produces one [`EvidenceRecord`] when the dispatcher is wired
//! with an [`EvidenceStoreSink`].
//!
//! The schema matches the §7.26 Component 3 spec:
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS evidence_records (
//!     evidence_id          TEXT PRIMARY KEY,
//!     action_id            TEXT NOT NULL,
//!     actor_id             TEXT NOT NULL,
//!     tenant_id            TEXT NOT NULL DEFAULT 'default',
//!     tool                 TEXT NOT NULL,
//!     arguments_redacted   TEXT NOT NULL,
//!     policy_decision      TEXT NOT NULL,
//!     reversibility        TEXT NOT NULL,
//!     tier                 TEXT NOT NULL DEFAULT 'unknown',
//!     started_at_ms        INTEGER NOT NULL,
//!     completed_at_ms      INTEGER,
//!     duration_ms          INTEGER,
//!     cost_usd             REAL,
//!     state_before         TEXT,
//!     state_after          TEXT,
//!     diff                 TEXT,
//!     error                TEXT,
//!     recorded_at_ms       INTEGER NOT NULL
//! );
//! ```
//!
//! The diff is computed inline when `state_before` and
//! `state_after` are both present and both text — a pure-Rust
//! unified-diff formatter so we don't pull a new dep.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::gateway_tier::GatewayTier;
use crate::nodes::tool::dispatcher::{EvidenceCaptureCtx, EvidenceCaptureSink};

#[derive(Debug, thiserror::Error)]
pub enum EvidenceStoreError {
    #[error("evidence store: sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("evidence store: json: {0}")]
    Json(String),
}

impl From<serde_json::Error> for EvidenceStoreError {
    fn from(e: serde_json::Error) -> Self {
        EvidenceStoreError::Json(e.to_string())
    }
}

/// One row in `evidence_records`. Mirrors the spec's
/// machine-readable artifact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub evidence_id: String,
    pub action_id: String,
    pub actor_id: String,
    #[serde(default = "default_tenant")]
    pub tenant_id: String,
    pub tool: String,
    pub arguments_redacted: String,
    pub policy_decision: String,
    pub reversibility: String,
    pub tier: String,
    pub started_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub recorded_at_ms: i64,
}

fn default_tenant() -> String {
    "default".to_string()
}

/// Optional probe a tool author can register to describe how to
/// snapshot the relevant state before / after the dispatch. For
/// `tool.fs.write`, this might list the affected file's
/// contents. For `tool.browser.navigate`, the current URL. Tools
/// that have no useful pre-state leave the probe unregistered
/// and the evidence row carries `state_before = NULL`.
pub trait StateProbe: Send + Sync {
    fn snapshot(&self, tool: &str, args: &str) -> Option<String>;
}

/// Cheap-to-clone SQLite-backed evidence store.
#[derive(Clone)]
pub struct EvidenceStore {
    conn: Arc<Mutex<Connection>>,
    anonymizer: Arc<crate::training::PiiAnonymizer>,
    state_probe: Option<Arc<dyn StateProbe>>,
}

impl EvidenceStore {
    pub fn open(
        path: &Path,
        anonymizer: Arc<crate::training::PiiAnonymizer>,
    ) -> Result<Self, EvidenceStoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::log_integrity_warning(&conn, "execution_evidence");
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            anonymizer,
            state_probe: None,
        })
    }

    pub fn open_in_memory(
        anonymizer: Arc<crate::training::PiiAnonymizer>,
    ) -> Result<Self, EvidenceStoreError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            anonymizer,
            state_probe: None,
        })
    }

    /// Install a [`StateProbe`] so the dispatcher can capture
    /// `state_before` / `state_after` for tools that opt in.
    pub fn with_state_probe(mut self, probe: Arc<dyn StateProbe>) -> Self {
        self.state_probe = Some(probe);
        self
    }

    fn migrate(conn: &Connection) -> Result<(), EvidenceStoreError> {
        crate::db::ensure_migration_table(conn)?;
        let current = crate::db::current_migration_version(conn)?;
        if current < 1 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS evidence_records (\
                     evidence_id         TEXT PRIMARY KEY,\
                     action_id           TEXT NOT NULL,\
                     actor_id            TEXT NOT NULL,\
                     tenant_id           TEXT NOT NULL DEFAULT 'default',\
                     tool                TEXT NOT NULL,\
                     arguments_redacted  TEXT NOT NULL,\
                     policy_decision     TEXT NOT NULL,\
                     reversibility       TEXT NOT NULL,\
                     tier                TEXT NOT NULL DEFAULT 'unknown',\
                     started_at_ms       INTEGER NOT NULL,\
                     completed_at_ms     INTEGER,\
                     duration_ms         INTEGER,\
                     cost_usd            REAL,\
                     state_before        TEXT,\
                     state_after         TEXT,\
                     diff                TEXT,\
                     error               TEXT,\
                     recorded_at_ms      INTEGER NOT NULL\
                 );\
                 CREATE INDEX IF NOT EXISTS evidence_records_action \
                     ON evidence_records(action_id);\
                 CREATE INDEX IF NOT EXISTS evidence_records_actor \
                     ON evidence_records(actor_id, recorded_at_ms DESC);\
                 CREATE INDEX IF NOT EXISTS evidence_records_tool \
                     ON evidence_records(tool, recorded_at_ms DESC);",
            )?;
            crate::db::record_migration_applied(conn, 1)?;
        }
        Ok(())
    }

    /// Persist one record.
    pub fn record(&self, r: &EvidenceRecord) -> Result<(), EvidenceStoreError> {
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("'evidence store lock poisoned'; recovering inner state");
            e.into_inner()
        });
        conn.execute(
            "INSERT INTO evidence_records \
             (evidence_id, action_id, actor_id, tenant_id, tool, arguments_redacted, \
              policy_decision, reversibility, tier, started_at_ms, completed_at_ms, \
              duration_ms, cost_usd, state_before, state_after, diff, error, recorded_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                r.evidence_id,
                r.action_id,
                r.actor_id,
                r.tenant_id,
                r.tool,
                r.arguments_redacted,
                r.policy_decision,
                r.reversibility,
                r.tier,
                r.started_at_ms,
                r.completed_at_ms,
                r.duration_ms,
                r.cost_usd,
                r.state_before,
                r.state_after,
                r.diff,
                r.error,
                r.recorded_at_ms,
            ],
        )?;
        Ok(())
    }

    /// Filter on action_id, actor_id, or both. Newest-first.
    /// `limit` is clamped to [1, 1000].
    pub fn list(
        &self,
        action_id: Option<&str>,
        actor_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EvidenceRecord>, EvidenceStoreError> {
        let limit_i: i64 = limit.clamp(1, 1000) as i64;
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("'evidence store lock poisoned'; recovering inner state");
            e.into_inner()
        });
        let (sql, params_vec): (&str, Vec<rusqlite::types::Value>) = match (action_id, actor_id) {
            (None, None) => (
                "SELECT evidence_id, action_id, actor_id, tenant_id, tool, arguments_redacted, \
                        policy_decision, reversibility, tier, started_at_ms, completed_at_ms, \
                        duration_ms, cost_usd, state_before, state_after, diff, error, recorded_at_ms \
                 FROM evidence_records \
                 ORDER BY recorded_at_ms DESC, evidence_id ASC \
                 LIMIT ?1",
                vec![limit_i.into()],
            ),
            (Some(a), None) => (
                "SELECT evidence_id, action_id, actor_id, tenant_id, tool, arguments_redacted, \
                        policy_decision, reversibility, tier, started_at_ms, completed_at_ms, \
                        duration_ms, cost_usd, state_before, state_after, diff, error, recorded_at_ms \
                 FROM evidence_records WHERE action_id = ?2 \
                 ORDER BY recorded_at_ms DESC, evidence_id ASC \
                 LIMIT ?1",
                vec![limit_i.into(), a.to_string().into()],
            ),
            (None, Some(a)) => (
                "SELECT evidence_id, action_id, actor_id, tenant_id, tool, arguments_redacted, \
                        policy_decision, reversibility, tier, started_at_ms, completed_at_ms, \
                        duration_ms, cost_usd, state_before, state_after, diff, error, recorded_at_ms \
                 FROM evidence_records WHERE actor_id = ?2 \
                 ORDER BY recorded_at_ms DESC, evidence_id ASC \
                 LIMIT ?1",
                vec![limit_i.into(), a.to_string().into()],
            ),
            (Some(act), Some(actor)) => (
                "SELECT evidence_id, action_id, actor_id, tenant_id, tool, arguments_redacted, \
                        policy_decision, reversibility, tier, started_at_ms, completed_at_ms, \
                        duration_ms, cost_usd, state_before, state_after, diff, error, recorded_at_ms \
                 FROM evidence_records WHERE action_id = ?2 AND actor_id = ?3 \
                 ORDER BY recorded_at_ms DESC, evidence_id ASC \
                 LIMIT ?1",
                vec![
                    limit_i.into(),
                    act.to_string().into(),
                    actor.to_string().into(),
                ],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params_vec.iter()), row_to_record)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    pub fn get(&self, evidence_id: &str) -> Result<Option<EvidenceRecord>, EvidenceStoreError> {
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("'evidence store lock poisoned'; recovering inner state");
            e.into_inner()
        });
        let row = conn
            .query_row(
                "SELECT evidence_id, action_id, actor_id, tenant_id, tool, arguments_redacted, \
                        policy_decision, reversibility, tier, started_at_ms, completed_at_ms, \
                        duration_ms, cost_usd, state_before, state_after, diff, error, recorded_at_ms \
                 FROM evidence_records WHERE evidence_id = ?1",
                params![evidence_id],
                row_to_record,
            )
            .optional()?;
        match row {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }
}

impl EvidenceCaptureSink for EvidenceStore {
    fn capture(&self, ctx: EvidenceCaptureCtx<'_>) {
        let started_at = ctx.started_at_ms;
        let completed_at = ctx.completed_at_ms;
        let duration_ms = (completed_at - started_at).max(0);
        let recorded_at_ms = unix_millis();
        let evidence_id = mint_evidence_id(ctx.action.action_id.as_str());
        // Redact via the configured anonymizer.
        let args_redacted = self.anonymizer.anonymize(ctx.args);
        // Policy decision: dry-run wins, then blocked, then "allowed".
        let policy_decision = if ctx.action.dry_run {
            "dry_run"
        } else if matches!(ctx.action.tier, GatewayTier::Blocked { .. }) {
            "blocked"
        } else {
            "allowed"
        };
        let reversibility = match &ctx.action.tier {
            GatewayTier::AutoCompensated { .. } => "auto_compensated",
            GatewayTier::HumanRollbackPlan { .. } => "human_rollback",
            GatewayTier::Blocked { .. } => "blocked",
        };
        // State probe (optional). Production wires this with
        // tool-specific snapshotters; the alpha leaves it
        // unwired and state_before/after stay NULL.
        let state_before = self
            .state_probe
            .as_ref()
            .and_then(|p| p.snapshot(ctx.tool, ctx.args));
        let state_after = self
            .state_probe
            .as_ref()
            .and_then(|p| p.snapshot(ctx.tool, ctx.args));
        let diff = match (state_before.as_ref(), state_after.as_ref()) {
            (Some(a), Some(b)) if a != b => Some(unified_diff(a, b)),
            _ => None,
        };
        let row = EvidenceRecord {
            evidence_id,
            action_id: ctx.action.action_id.clone(),
            actor_id: if ctx.agent.is_empty() {
                ctx.action.actor.clone().unwrap_or_default()
            } else {
                ctx.agent.to_string()
            },
            tenant_id: default_tenant(),
            tool: ctx.tool.to_string(),
            arguments_redacted: args_redacted,
            policy_decision: policy_decision.to_string(),
            reversibility: reversibility.to_string(),
            tier: ctx.action.tier.tag().to_string(),
            started_at_ms: started_at,
            completed_at_ms: Some(completed_at),
            duration_ms: Some(duration_ms),
            cost_usd: None,
            state_before,
            state_after,
            diff,
            error: ctx.error.map(str::to_string),
            recorded_at_ms,
        };
        if let Err(e) = self.record(&row) {
            tracing::warn!(error = %e, action_id = %row.action_id, "evidence store: record failed");
        }
    }
}

fn mint_evidence_id(action_id: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(action_id.as_bytes());
    hasher.update(b"|");
    hasher.update(unix_millis().to_le_bytes().as_ref());
    let mut rnd = [0u8; 8];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut rnd);
    hasher.update(&rnd);
    let hex = hex::encode(&hasher.finalize().as_bytes()[..16]);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Produce a minimal unified-diff string between `a` and `b`.
/// Used when both `state_before` and `state_after` are text.
/// Pure Rust + zero deps — the alpha's diff fidelity is
/// "operator can see what changed", not "byte-perfect patch".
pub fn unified_diff(a: &str, b: &str) -> String {
    let a_lines: Vec<&str> = a.lines().collect();
    let b_lines: Vec<&str> = b.lines().collect();
    let mut out = String::new();
    out.push_str("--- state_before\n+++ state_after\n");
    let mut i = 0;
    let mut j = 0;
    while i < a_lines.len() || j < b_lines.len() {
        match (a_lines.get(i), b_lines.get(j)) {
            (Some(la), Some(lb)) if la == lb => {
                out.push_str(&format!(" {la}\n"));
                i += 1;
                j += 1;
            }
            (Some(la), Some(_)) if !b_lines[j..].contains(la) => {
                out.push_str(&format!("-{la}\n"));
                i += 1;
            }
            (Some(_), Some(lb)) => {
                out.push_str(&format!("+{lb}\n"));
                j += 1;
            }
            (Some(la), None) => {
                out.push_str(&format!("-{la}\n"));
                i += 1;
            }
            (None, Some(lb)) => {
                out.push_str(&format!("+{lb}\n"));
                j += 1;
            }
            (None, None) => break,
        }
    }
    out
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn row_to_record(
    r: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<EvidenceRecord, EvidenceStoreError>> {
    let evidence_id: String = r.get(0)?;
    let action_id: String = r.get(1)?;
    let actor_id: String = r.get(2)?;
    let tenant_id: String = r.get(3)?;
    let tool: String = r.get(4)?;
    let arguments_redacted: String = r.get(5)?;
    let policy_decision: String = r.get(6)?;
    let reversibility: String = r.get(7)?;
    let tier: String = r.get(8)?;
    let started_at_ms: i64 = r.get(9)?;
    let completed_at_ms: Option<i64> = r.get(10)?;
    let duration_ms: Option<i64> = r.get(11)?;
    let cost_usd: Option<f64> = r.get(12)?;
    let state_before: Option<String> = r.get(13)?;
    let state_after: Option<String> = r.get(14)?;
    let diff: Option<String> = r.get(15)?;
    let error: Option<String> = r.get(16)?;
    let recorded_at_ms: i64 = r.get(17)?;
    Ok(Ok(EvidenceRecord {
        evidence_id,
        action_id,
        actor_id,
        tenant_id,
        tool,
        arguments_redacted,
        policy_decision,
        reversibility,
        tier,
        started_at_ms,
        completed_at_ms,
        duration_ms,
        cost_usd,
        state_before,
        state_after,
        diff,
        error,
        recorded_at_ms,
    }))
}

// ── Cap registration ─────────────────────────────────────

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use relix_core::types::{ErrorEnvelope, error_kinds};

#[derive(Debug, Deserialize, Default)]
pub(crate) struct EvidenceArgs {
    #[serde(default)]
    pub action_id: Option<String>,
    #[serde(default)]
    pub actor_id: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub(crate) struct EvidenceResponse {
    pub records: Vec<EvidenceRecord>,
    pub count: usize,
}

/// Register the `execution.evidence` capability on the bridge.
pub fn register(bridge: &mut DispatchBridge, store: Arc<EvidenceStore>) {
    let store = store.clone();
    bridge.register(
        "execution.evidence",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = store.clone();
            async move { handle_evidence(&store, &ctx) }
        })),
    );
}

fn handle_evidence(store: &EvidenceStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: EvidenceArgs = if ctx.args.is_empty() {
        EvidenceArgs::default()
    } else {
        match serde_json::from_slice(&ctx.args) {
            Ok(a) => a,
            Err(e) => {
                return HandlerOutcome::Err(ErrorEnvelope {
                    kind: error_kinds::INVALID_ARGS,
                    cause: format!("execution.evidence: decode args: {e}"),
                    retry_hint: 2,
                    retry_after: None,
                });
            }
        }
    };
    let limit = args.limit.unwrap_or(20).clamp(1, 200);
    match store.list(args.action_id.as_deref(), args.actor_id.as_deref(), limit) {
        Ok(records) => {
            let count = records.len();
            let body = EvidenceResponse { records, count };
            match serde_json::to_vec(&body) {
                Ok(b) => HandlerOutcome::Ok(b),
                Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                    kind: error_kinds::RESPONDER_INTERNAL,
                    cause: format!("execution.evidence: encode: {e}"),
                    retry_hint: 1,
                    retry_after: None,
                }),
            }
        }
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("execution.evidence: {e}"),
            retry_hint: 1,
            retry_after: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::gateway_tier::{GatewayDispatchOptions, GatewayTier};
    use super::super::transaction_store::{GatewayActionRow, build_success_row};
    use super::*;
    use serde_json::json;

    fn store() -> EvidenceStore {
        EvidenceStore::open_in_memory(Arc::new(crate::training::PiiAnonymizer::disabled())).unwrap()
    }

    fn action_row(tx: &str) -> GatewayActionRow {
        let opts = GatewayDispatchOptions::default()
            .with_transaction_id(tx)
            .auto_compensated("memory.delete", json!({"id": "abc"}));
        let mut row = build_success_row(
            "memory.write",
            r#"{"text":"hi"}"#,
            Some("ok".into()),
            &opts,
            100,
            200,
        );
        row.action_id = format!("act-{tx}");
        row
    }

    fn capture_with_alice(store: &EvidenceStore, action: &GatewayActionRow) {
        store.capture(EvidenceCaptureCtx {
            action,
            agent: "alice",
            tool: "memory.write",
            args: r#"{"text":"hi","ssn":"123-45-6789"}"#,
            result: Some("ok"),
            error: None,
            started_at_ms: 100,
            completed_at_ms: 200,
        });
    }

    #[test]
    fn capture_records_one_row_per_action() {
        let s = store();
        let action = action_row("tx-1");
        capture_with_alice(&s, &action);
        let rows = s.list(None, Some("alice"), 10).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.action_id, action.action_id);
        assert_eq!(r.actor_id, "alice");
        assert_eq!(r.tool, "memory.write");
        assert_eq!(r.policy_decision, "allowed");
        assert_eq!(r.reversibility, "auto_compensated");
        assert_eq!(r.tier, "auto_compensated");
        assert_eq!(r.duration_ms, Some(100));
    }

    #[test]
    fn redaction_runs_through_the_anonymizer() {
        // Build an anonymizer that redacts SSN.
        let cfg = crate::training::PiiConfig {
            enabled: true,
            strategy: crate::training::pii::PiiStrategy::Redact,
            ..Default::default()
        };
        let anon = crate::training::PiiAnonymizer::from_config(&cfg);
        let s = EvidenceStore::open_in_memory(Arc::new(anon)).unwrap();
        let action = action_row("tx-redact");
        s.capture(EvidenceCaptureCtx {
            action: &action,
            agent: "alice",
            tool: "memory.write",
            args: r#"{"text":"my SSN is 123-45-6789"}"#,
            result: Some("ok"),
            error: None,
            started_at_ms: 0,
            completed_at_ms: 1,
        });
        let rows = s.list(None, None, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            !rows[0].arguments_redacted.contains("123-45-6789"),
            "SSN should be redacted in `arguments_redacted`"
        );
    }

    #[test]
    fn list_filters_by_action_id_then_actor_id() {
        let s = store();
        let mut a1 = action_row("tx-A");
        a1.action_id = "act-A".into();
        capture_with_alice(&s, &a1);
        let mut a2 = action_row("tx-B");
        a2.action_id = "act-B".into();
        s.capture(EvidenceCaptureCtx {
            action: &a2,
            agent: "bob",
            tool: "memory.write",
            args: "{}",
            result: Some("ok"),
            error: None,
            started_at_ms: 0,
            completed_at_ms: 1,
        });
        let by_action = s.list(Some("act-A"), None, 10).unwrap();
        assert_eq!(by_action.len(), 1);
        assert_eq!(by_action[0].action_id, "act-A");
        let by_actor = s.list(None, Some("bob"), 10).unwrap();
        assert_eq!(by_actor.len(), 1);
        assert_eq!(by_actor[0].actor_id, "bob");
        let both = s.list(Some("act-A"), Some("alice"), 10).unwrap();
        assert_eq!(both.len(), 1);
        let mismatched = s.list(Some("act-A"), Some("bob"), 10).unwrap();
        assert!(mismatched.is_empty());
    }

    #[test]
    fn state_probe_captures_before_and_after_with_diff() {
        struct ToggleProbe {
            counter: std::sync::Mutex<u32>,
        }
        impl StateProbe for ToggleProbe {
            fn snapshot(&self, _tool: &str, _args: &str) -> Option<String> {
                let mut c = self.counter.lock().unwrap();
                *c += 1;
                if *c == 1 {
                    Some("alpha\nbeta\n".into())
                } else {
                    Some("alpha\nbeta\nGAMMA\n".into())
                }
            }
        }
        let s = store().with_state_probe(Arc::new(ToggleProbe {
            counter: std::sync::Mutex::new(0),
        }));
        let action = action_row("tx-diff");
        capture_with_alice(&s, &action);
        let rows = s.list(None, None, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].state_before.is_some());
        assert!(rows[0].state_after.is_some());
        let diff = rows[0].diff.as_deref().unwrap();
        assert!(diff.contains("+GAMMA"));
        assert!(diff.starts_with("--- state_before"));
    }

    #[test]
    fn policy_decision_marks_dry_run_and_blocked() {
        let s = store();
        let mut dry = action_row("tx-dry");
        dry.action_id = "dry-1".into();
        dry.dry_run = true;
        capture_with_alice(&s, &dry);
        let mut blocked = action_row("tx-block");
        blocked.action_id = "block-1".into();
        blocked.tier = GatewayTier::Blocked {
            reason: "no rm -rf".into(),
        };
        capture_with_alice(&s, &blocked);
        let rows = s.list(None, Some("alice"), 10).unwrap();
        let dry_row = rows.iter().find(|r| r.action_id == "dry-1").unwrap();
        let block_row = rows.iter().find(|r| r.action_id == "block-1").unwrap();
        assert_eq!(dry_row.policy_decision, "dry_run");
        assert_eq!(block_row.policy_decision, "blocked");
        assert_eq!(block_row.reversibility, "blocked");
    }

    #[test]
    fn unified_diff_produces_minus_plus_lines() {
        let d = unified_diff("alpha\nbeta\n", "alpha\nGAMMA\n");
        assert!(d.contains("-beta"));
        assert!(d.contains("+GAMMA"));
    }

    #[test]
    fn unified_diff_on_identical_text_has_no_changes() {
        let d = unified_diff("alpha\nbeta\n", "alpha\nbeta\n");
        // No diff lines (lines starting with `-` or `+` after
        // the header).
        let changed_lines: Vec<&str> = d
            .lines()
            .filter(|l| {
                !l.starts_with("--- ")
                    && !l.starts_with("+++ ")
                    && (l.starts_with('-') || l.starts_with('+'))
            })
            .collect();
        assert!(
            changed_lines.is_empty(),
            "expected no change lines, got {changed_lines:?}"
        );
        assert!(d.contains(" alpha"));
        assert!(d.contains(" beta"));
    }

    #[test]
    fn mint_evidence_id_is_uuid_shaped() {
        let id = mint_evidence_id("act-1");
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
    }

    #[test]
    fn evidence_record_round_trips_through_get() {
        let s = store();
        let action = action_row("tx-get");
        capture_with_alice(&s, &action);
        let rows = s.list(None, None, 10).unwrap();
        let id = rows[0].evidence_id.clone();
        let again = s.get(&id).unwrap().unwrap();
        assert_eq!(again.evidence_id, id);
    }

    #[test]
    fn failed_dispatch_records_error_string() {
        let s = store();
        let action = action_row("tx-err");
        s.capture(EvidenceCaptureCtx {
            action: &action,
            agent: "alice",
            tool: "memory.write",
            args: "{}",
            result: None,
            error: Some("boom"),
            started_at_ms: 0,
            completed_at_ms: 1,
        });
        let rows = s.list(None, None, 10).unwrap();
        assert_eq!(rows[0].error.as_deref(), Some("boom"));
    }
}
