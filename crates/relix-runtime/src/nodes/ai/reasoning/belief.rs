//! GAP 16 Component 3 — belief state tracker.
//!
//! Per-session structured working memory: claim + confidence
//! plus sources plus conflicts. Sits alongside the four-layer
//! memory store; the belief store is session-scoped (sessions
//! end → optional roll-up into Layer 4 model), and individual
//! beliefs are tracked through their full lifecycle (add,
//! reinforce, contradict, supersede).
//!
//! See `crates/relix-runtime/src/nodes/ai/reasoning/mod.rs`
//! header for where this sits in the §7.29 architecture.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

/// One belief row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Belief {
    /// Stable id assigned at insert.
    pub id: String,
    /// Session this belief was learned in.
    pub session_id: String,
    /// Free-form claim text (`"Project deadline: Friday March 7th"`).
    pub claim: String,
    /// 0.0..=1.0 confidence. Operators use the configured
    /// `needs_resolution_floor` to surface low-confidence
    /// rows.
    pub confidence: f32,
    /// Comma-delimited source labels (`"user.alice", "calendar"`).
    pub sources: Vec<String>,
    /// Unix-ms timestamp of the most recent update.
    pub updated_at_ms: i64,
    /// `true` when the belief currently disagrees with a
    /// previously-recorded belief in the same session AND
    /// the conflict has not been resolved.
    pub in_conflict: bool,
}

/// One row in the conflicts ledger.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BeliefConflict {
    pub id: String,
    pub session_id: String,
    pub belief_id: String,
    pub new_claim: String,
    pub old_claim: String,
    pub detected_at_ms: i64,
    pub resolved_at_ms: Option<i64>,
}

#[derive(Debug, thiserror::Error)]
pub enum BeliefStoreError {
    #[error("belief store: sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("belief store: lock poisoned")]
    Lock,
    #[error("belief store: io: {0}")]
    Io(String),
    #[error("belief store: serialization: {0}")]
    Serialization(String),
}

/// SQLite-backed belief store. Cheap to clone.
#[derive(Clone)]
pub struct BeliefStore {
    conn: Arc<Mutex<Connection>>,
}

impl BeliefStore {
    /// Open at `path`. Creates parent dirs + schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BeliefStoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| BeliefStoreError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// In-memory store (tests + dev).
    pub fn open_in_memory() -> Result<Self, BeliefStoreError> {
        let conn = Connection::open_in_memory()?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Insert or update a belief on `session_id`. If a belief
    /// with the same `claim` (case-folded) already exists,
    /// the existing row is `reinforce`-merged: confidence
    /// becomes the max of the old + new, sources union, and
    /// `updated_at_ms` advances.
    ///
    /// When the new claim semantically contradicts an existing
    /// claim — detected by [`contradicts`] — both rows are kept
    /// AND a conflict row lands in `belief_conflicts`. Callers
    /// resolve via [`Self::resolve_conflict`].
    ///
    /// Returns the persisted belief.
    pub fn add_or_reinforce(
        &self,
        session_id: &str,
        claim: &str,
        confidence: f32,
        sources: &[String],
        now_ms: i64,
    ) -> Result<Belief, BeliefStoreError> {
        let conn = self.conn.lock().map_err(|_| BeliefStoreError::Lock)?;
        // Look for an existing claim on the session.
        let existing: Option<Belief> = list_for_session_inner(&conn, session_id)?
            .into_iter()
            .find(|b| b.claim.eq_ignore_ascii_case(claim));
        if let Some(prev) = existing {
            // Same claim → reinforce.
            let merged_conf = prev.confidence.max(confidence);
            let mut merged_sources = prev.sources.clone();
            for s in sources {
                if !merged_sources.iter().any(|x| x.eq_ignore_ascii_case(s)) {
                    merged_sources.push(s.clone());
                }
            }
            let sources_json = serde_json::to_string(&merged_sources)
                .map_err(|e| BeliefStoreError::Serialization(e.to_string()))?;
            conn.execute(
                "UPDATE beliefs \
                 SET confidence = ?1, sources = ?2, updated_at_ms = ?3 \
                 WHERE id = ?4",
                params![merged_conf as f64, sources_json, now_ms, prev.id],
            )?;
            return Ok(Belief {
                confidence: merged_conf,
                sources: merged_sources,
                updated_at_ms: now_ms,
                ..prev
            });
        }

        // Brand-new claim. Detect contradiction against every
        // other belief on this session.
        let contradicted: Option<Belief> = list_for_session_inner(&conn, session_id)?
            .into_iter()
            .find(|b| contradicts(&b.claim, claim));
        let id = mint_id(session_id, claim, now_ms);
        let sources_json = serde_json::to_string(sources)
            .map_err(|e| BeliefStoreError::Serialization(e.to_string()))?;
        conn.execute(
            "INSERT INTO beliefs (id, session_id, claim, confidence, sources, updated_at_ms, in_conflict) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                session_id,
                claim,
                confidence as f64,
                sources_json,
                now_ms,
                contradicted.is_some() as i32,
            ],
        )?;
        if let Some(prev) = &contradicted {
            // Stamp the contradiction on both rows + a
            // conflict ledger entry.
            conn.execute(
                "UPDATE beliefs SET in_conflict = 1 WHERE id = ?1",
                params![prev.id],
            )?;
            let conflict_id = mint_id(session_id, &format!("conflict:{}|{}", prev.id, id), now_ms);
            conn.execute(
                "INSERT INTO belief_conflicts \
                 (id, session_id, belief_id, new_claim, old_claim, detected_at_ms, resolved_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                params![conflict_id, session_id, id, claim, prev.claim, now_ms],
            )?;
        }
        Ok(Belief {
            id,
            session_id: session_id.to_string(),
            claim: claim.to_string(),
            confidence,
            sources: sources.to_vec(),
            updated_at_ms: now_ms,
            in_conflict: contradicted.is_some(),
        })
    }

    /// List every belief on `session_id`, ordered by
    /// `updated_at_ms` descending.
    pub fn list_for_session(&self, session_id: &str) -> Result<Vec<Belief>, BeliefStoreError> {
        let conn = self.conn.lock().map_err(|_| BeliefStoreError::Lock)?;
        list_for_session_inner(&conn, session_id)
    }

    /// Beliefs on `session_id` whose confidence is at or below
    /// `floor`. Caller surfaces these as
    /// `needs_resolution` candidates.
    pub fn list_needs_resolution(
        &self,
        session_id: &str,
        floor: f32,
    ) -> Result<Vec<Belief>, BeliefStoreError> {
        let conn = self.conn.lock().map_err(|_| BeliefStoreError::Lock)?;
        let all = list_for_session_inner(&conn, session_id)?;
        Ok(all
            .into_iter()
            .filter(|b| b.confidence <= floor || b.in_conflict)
            .collect())
    }

    /// Unresolved conflict rows for `session_id`.
    pub fn list_conflicts(
        &self,
        session_id: &str,
    ) -> Result<Vec<BeliefConflict>, BeliefStoreError> {
        let conn = self.conn.lock().map_err(|_| BeliefStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, belief_id, new_claim, old_claim, detected_at_ms, resolved_at_ms \
             FROM belief_conflicts \
             WHERE session_id = ?1 AND resolved_at_ms IS NULL \
             ORDER BY detected_at_ms DESC",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok(BeliefConflict {
                id: r.get(0)?,
                session_id: r.get(1)?,
                belief_id: r.get(2)?,
                new_claim: r.get(3)?,
                old_claim: r.get(4)?,
                detected_at_ms: r.get(5)?,
                resolved_at_ms: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Mark a conflict resolved. Caller picks one of the two
    /// rival claims as the new head (winner_belief_id);
    /// the loser belief is left in the table with
    /// `in_conflict = false` so the chronicle survives.
    pub fn resolve_conflict(
        &self,
        conflict_id: &str,
        winner_belief_id: &str,
        loser_belief_id: &str,
        at_ms: i64,
    ) -> Result<(), BeliefStoreError> {
        let conn = self.conn.lock().map_err(|_| BeliefStoreError::Lock)?;
        conn.execute(
            "UPDATE belief_conflicts SET resolved_at_ms = ?1 WHERE id = ?2",
            params![at_ms, conflict_id],
        )?;
        conn.execute(
            "UPDATE beliefs SET in_conflict = 0 WHERE id IN (?1, ?2)",
            params![winner_belief_id, loser_belief_id],
        )?;
        Ok(())
    }

    /// Drop every row on `session_id`. Used by session-end
    /// cleanup; conflicts go with the beliefs.
    pub fn purge_session(&self, session_id: &str) -> Result<(), BeliefStoreError> {
        let conn = self.conn.lock().map_err(|_| BeliefStoreError::Lock)?;
        conn.execute(
            "DELETE FROM belief_conflicts WHERE session_id = ?1",
            params![session_id],
        )?;
        conn.execute(
            "DELETE FROM beliefs WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS beliefs (
             id            TEXT PRIMARY KEY,
             session_id    TEXT NOT NULL,
             claim         TEXT NOT NULL,
             confidence    REAL NOT NULL,
             sources       TEXT NOT NULL DEFAULT '[]',
             updated_at_ms INTEGER NOT NULL,
             in_conflict   INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS beliefs_session_updated \
             ON beliefs(session_id, updated_at_ms DESC);
         CREATE TABLE IF NOT EXISTS belief_conflicts (
             id              TEXT PRIMARY KEY,
             session_id      TEXT NOT NULL,
             belief_id       TEXT NOT NULL,
             new_claim       TEXT NOT NULL,
             old_claim       TEXT NOT NULL,
             detected_at_ms  INTEGER NOT NULL,
             resolved_at_ms  INTEGER
         );
         CREATE INDEX IF NOT EXISTS belief_conflicts_session_unresolved \
             ON belief_conflicts(session_id) WHERE resolved_at_ms IS NULL;",
    )
}

fn list_for_session_inner(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<Belief>, BeliefStoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, claim, confidence, sources, updated_at_ms, in_conflict \
         FROM beliefs \
         WHERE session_id = ?1 \
         ORDER BY updated_at_ms DESC",
    )?;
    let rows = stmt.query_map(params![session_id], |r| {
        let sources_json: String = r.get(4)?;
        let sources: Vec<String> = serde_json::from_str(&sources_json).unwrap_or_default();
        let in_conflict: i64 = r.get(6)?;
        Ok(Belief {
            id: r.get(0)?,
            session_id: r.get(1)?,
            claim: r.get(2)?,
            confidence: r.get::<_, f64>(3)? as f32,
            sources,
            updated_at_ms: r.get(5)?,
            in_conflict: in_conflict != 0,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Semantic-contradiction detector. The detector is
/// deliberately conservative — we only flag a pair as
/// contradictory when both claims share an early subject
/// phrase (the first few words case-folded) AND the rest of
/// the text differs. That catches the
/// `"Reporting frequency: weekly"` vs
/// `"Reporting frequency: monthly"` case the spec describes
/// without flagging unrelated claims.
///
/// Exposed as a pub helper so the unit tests can pin the
/// exact rule.
pub fn contradicts(a: &str, b: &str) -> bool {
    let al = a.trim().to_ascii_lowercase();
    let bl = b.trim().to_ascii_lowercase();
    if al == bl {
        return false;
    }
    // Pick the shared subject prefix at the first colon /
    // dash, or the first three words.
    let pa = subject_prefix(&al);
    let pb = subject_prefix(&bl);
    if pa.is_empty() || pb.is_empty() {
        return false;
    }
    pa == pb
}

fn subject_prefix(s: &str) -> &str {
    if let Some(idx) = s.find(':') {
        let head = s[..idx].trim();
        return head;
    }
    if let Some(idx) = s.find(" - ") {
        return s[..idx].trim();
    }
    // Fallback: first three space-delimited tokens.
    let mut end = 0;
    let mut tokens = 0;
    for (i, ch) in s.char_indices() {
        if ch.is_whitespace() {
            if i > 0 && !s[..i].ends_with(char::is_whitespace) {
                tokens += 1;
            }
            if tokens == 3 {
                end = i;
                break;
            }
        }
    }
    if end == 0 {
        return s.trim_end();
    }
    s[..end].trim()
}

fn mint_id(session: &str, claim: &str, ts_ms: i64) -> String {
    let mut h = blake3::Hasher::new();
    h.update(session.as_bytes());
    h.update(b"|");
    h.update(claim.as_bytes());
    h.update(b"|");
    h.update(&ts_ms.to_le_bytes());
    let hex = h.finalize().to_hex();
    format!("belief-{}", &hex.as_str()[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> BeliefStore {
        BeliefStore::open_in_memory().unwrap()
    }

    #[test]
    fn add_then_list_returns_the_belief() {
        let s = store();
        s.add_or_reinforce("sess", "Project deadline: Friday March 7th", 0.9, &[], 1)
            .unwrap();
        let rows = s.list_for_session("sess").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].claim.contains("March 7th"));
    }

    #[test]
    fn add_same_claim_reinforces_max_confidence_and_unions_sources() {
        let s = store();
        s.add_or_reinforce(
            "sess",
            "Budget: approximately $50,000",
            0.5,
            &["user".into()],
            1,
        )
        .unwrap();
        s.add_or_reinforce(
            "sess",
            "budget: approximately $50,000",
            0.8,
            &["calendar".into()],
            2,
        )
        .unwrap();
        let rows = s.list_for_session("sess").unwrap();
        assert_eq!(rows.len(), 1);
        assert!((rows[0].confidence - 0.8).abs() < 1e-6);
        assert!(rows[0].sources.iter().any(|x| x == "user"));
        assert!(rows[0].sources.iter().any(|x| x == "calendar"));
    }

    #[test]
    fn contradicting_claim_keeps_both_and_creates_conflict() {
        let s = store();
        s.add_or_reinforce("sess", "Reporting frequency: weekly", 0.7, &[], 1)
            .unwrap();
        s.add_or_reinforce("sess", "Reporting frequency: monthly", 0.6, &[], 2)
            .unwrap();
        let rows = s.list_for_session("sess").unwrap();
        assert_eq!(rows.len(), 2, "both kept: {rows:?}");
        assert!(rows.iter().all(|b| b.in_conflict));
        let cs = s.list_conflicts("sess").unwrap();
        assert_eq!(cs.len(), 1);
        assert!(cs[0].new_claim.contains("monthly"));
        assert!(cs[0].old_claim.contains("weekly"));
    }

    #[test]
    fn resolve_conflict_marks_winner_loser_and_records_resolution() {
        let s = store();
        let a = s
            .add_or_reinforce("sess", "Reporting frequency: weekly", 0.7, &[], 1)
            .unwrap();
        let b = s
            .add_or_reinforce("sess", "Reporting frequency: monthly", 0.6, &[], 2)
            .unwrap();
        let conflicts = s.list_conflicts("sess").unwrap();
        s.resolve_conflict(&conflicts[0].id, &b.id, &a.id, 3)
            .unwrap();
        let rows = s.list_for_session("sess").unwrap();
        assert!(rows.iter().all(|x| !x.in_conflict));
        assert!(s.list_conflicts("sess").unwrap().is_empty());
    }

    #[test]
    fn needs_resolution_surfaces_low_confidence_and_in_conflict_rows() {
        let s = store();
        s.add_or_reinforce("sess", "High: known fact", 0.95, &[], 1)
            .unwrap();
        s.add_or_reinforce("sess", "Low: shaky guess", 0.3, &[], 2)
            .unwrap();
        let needs = s.list_needs_resolution("sess", 0.5).unwrap();
        assert_eq!(needs.len(), 1);
        assert!(needs[0].claim.contains("shaky guess"));
    }

    #[test]
    fn contradiction_detector_pins_the_spec_case() {
        assert!(contradicts(
            "Reporting frequency: weekly",
            "Reporting frequency: monthly"
        ));
        // Same subject prefix but identical body → not a
        // contradiction.
        assert!(!contradicts(
            "Reporting frequency: weekly",
            "Reporting frequency: weekly"
        ));
        // Different subject prefix → never contradictory.
        assert!(!contradicts(
            "Project deadline: Friday March 7th",
            "Budget: $50,000"
        ));
    }

    #[test]
    fn purge_session_removes_beliefs_and_conflicts() {
        let s = store();
        s.add_or_reinforce("a", "Reporting frequency: weekly", 0.7, &[], 1)
            .unwrap();
        s.add_or_reinforce("a", "Reporting frequency: monthly", 0.6, &[], 2)
            .unwrap();
        s.add_or_reinforce("b", "untouched", 0.9, &[], 3).unwrap();
        s.purge_session("a").unwrap();
        assert!(s.list_for_session("a").unwrap().is_empty());
        assert!(s.list_conflicts("a").unwrap().is_empty());
        assert_eq!(s.list_for_session("b").unwrap().len(), 1);
    }
}
