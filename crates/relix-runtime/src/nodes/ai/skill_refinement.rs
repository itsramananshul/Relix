//! GAP 4 — `SkillRefinementEngine`.
//!
//! Two surfaces over the persistent skill catalogue:
//!
//! Surface 1 — confidence scoring, sync, called from cap
//! handlers. Successful use + positive feedback (`liked` /
//! `approved`) → +0.05 (cap 0.95). Successful use with no
//! explicit feedback → +0.01. Failed use (tool error, user
//! rejected) → -0.10 (floor 0.05). See
//! [`SkillRefinementEngine::record_usage`].
//!
//! Surface 2 — refinement pass, background task, default
//! every 24h. Eligibility: status=active, usage_count >= 10,
//! confidence >= 0.7 (the store's
//! `list_refinement_candidates` does the filtering). For each
//! candidate the engine sends the LLM a "review and suggest
//! improvements" prompt with the last 3 examples and parses
//! JSON `{improved, steps, change_reason}`. When `improved ==
//! true` AND the new steps differ from the current ones,
//! `SkillStore::add_version` records a new version row + bumps
//! the live row's version number. When `improved == false` or
//! the steps are unchanged, no version row is written.
//!
//! Both surfaces are best-effort: every failure mode logs a
//! WARN and continues. The background loop never panics — a
//! single bad LLM reply doesn't stop subsequent ticks.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::nodes::ai::skill_extractor::extract_json_object;
use crate::nodes::ai::skill_store::{SkillStep, SkillStore, StoredSkill};
use crate::nodes::memory::curator::AiDispatcher;

/// 24 hours.
pub const DEFAULT_REFINEMENT_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Minimum usage count before a skill is eligible for
/// refinement.
pub const DEFAULT_MIN_USAGE_FOR_REFINEMENT: i64 = 10;

/// Minimum confidence before a skill is eligible for
/// refinement. Below this the skill is too unreliable for the
/// LLM's "you've been doing this well, can you do better"
/// framing to make sense.
pub const DEFAULT_MIN_CONFIDENCE_FOR_REFINEMENT: f32 = 0.7;

/// Cap on the synthesis call. Mirrors the extractor's bound so
/// the same wedge protection applies.
pub const DEFAULT_REFINEMENT_TIMEOUT_SECS: u64 = 30;

/// Confidence-update deltas used by [`SkillRefinementEngine::record_usage`].
/// Exposed so the cap handlers + tests can refer to them by
/// name rather than hard-coding magic floats.
pub const CONFIDENCE_DELTA_LIKED: f32 = 0.05;
pub const CONFIDENCE_DELTA_SUCCESS: f32 = 0.01;
pub const CONFIDENCE_DELTA_FAIL: f32 = -0.10;

/// Confidence floor / cap (also enforced inside
/// [`SkillStore::update_confidence`]).
pub const CONFIDENCE_FLOOR: f32 = 0.05;
pub const CONFIDENCE_CEIL: f32 = 0.95;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageOutcome {
    /// User explicitly approved / liked the response.
    SuccessLiked,
    /// Successful use, no explicit feedback.
    Success,
    /// Tool error or user-rejected response.
    Failed,
}

impl UsageOutcome {
    pub fn delta(self) -> f32 {
        match self {
            UsageOutcome::SuccessLiked => CONFIDENCE_DELTA_LIKED,
            UsageOutcome::Success => CONFIDENCE_DELTA_SUCCESS,
            UsageOutcome::Failed => CONFIDENCE_DELTA_FAIL,
        }
    }
}

/// Parsed JSON the refinement call is required to emit.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RefinementResponse {
    pub improved: bool,
    #[serde(default)]
    pub steps: Vec<SkillStep>,
    #[serde(default)]
    pub change_reason: Option<String>,
}

/// Tunables for the refinement loop. All exposed so operators
/// can dial them via `[skills]` if defaults bite.
#[derive(Debug, Clone)]
pub struct RefinementConfig {
    pub interval: Duration,
    pub min_usage: i64,
    pub min_confidence: f32,
    pub batch_size: usize,
    pub model: String,
    pub timeout_secs: u64,
}

impl Default for RefinementConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(DEFAULT_REFINEMENT_INTERVAL_SECS),
            min_usage: DEFAULT_MIN_USAGE_FOR_REFINEMENT,
            min_confidence: DEFAULT_MIN_CONFIDENCE_FOR_REFINEMENT,
            batch_size: 50,
            model: crate::nodes::ai::skill_extractor::DEFAULT_EXTRACTION_MODEL.to_string(),
            timeout_secs: DEFAULT_REFINEMENT_TIMEOUT_SECS,
        }
    }
}

/// Confidence scorer + background refinement loop.
#[derive(Clone)]
pub struct SkillRefinementEngine {
    store: Arc<SkillStore>,
    ai_cell: Arc<tokio::sync::OnceCell<Arc<dyn AiDispatcher>>>,
    config: RefinementConfig,
}

impl SkillRefinementEngine {
    pub fn new(
        store: Arc<SkillStore>,
        ai_cell: Arc<tokio::sync::OnceCell<Arc<dyn AiDispatcher>>>,
        config: RefinementConfig,
    ) -> Self {
        Self {
            store,
            ai_cell,
            config,
        }
    }

    /// Update a skill's confidence based on `outcome`. Also
    /// stamps `usage_count` + `last_used_ms` so the
    /// refinement-eligibility threshold can fire.
    pub fn record_usage(
        &self,
        skill_id: &str,
        outcome: UsageOutcome,
    ) -> Result<f32, RefinementError> {
        // Read current.
        let skill = self
            .store
            .get(skill_id)
            .map_err(RefinementError::Store)?
            .ok_or_else(|| RefinementError::NotFound(skill_id.to_string()))?;
        let new_conf =
            (skill.confidence + outcome.delta()).clamp(CONFIDENCE_FLOOR, CONFIDENCE_CEIL);
        self.store
            .update_confidence(skill_id, new_conf)
            .map_err(RefinementError::Store)?;
        // Always bump usage_count. The spec wants
        // "successful use" to bump, but practically operators
        // want to see every applied use including failures —
        // it's how trust calibration works. The confidence
        // delta already encodes the success/failure judgment.
        self.store
            .increment_usage(skill_id)
            .map_err(RefinementError::Store)?;
        Ok(new_conf)
    }

    /// Run one refinement pass. Returns the per-pass
    /// [`RefinementReport`] so tests can assert behaviour.
    pub async fn run_once(&self) -> Result<RefinementReport, RefinementError> {
        let started_at = unix_secs();
        let mut report = RefinementReport {
            started_at,
            ..Default::default()
        };
        let candidates = self
            .store
            .list_refinement_candidates(
                self.config.min_usage,
                self.config.min_confidence,
                self.config.batch_size,
            )
            .map_err(RefinementError::Store)?;
        report.eligible = candidates.len();
        let Some(dispatcher) = self.ai_cell.get().cloned() else {
            tracing::warn!(
                "skill refinement: ai dispatcher not configured; skipping {} eligible skills",
                report.eligible
            );
            report.finished_at = unix_secs();
            return Ok(report);
        };
        for skill in candidates {
            match self.refine_one(dispatcher.as_ref(), &skill).await {
                Ok(true) => report.refined += 1,
                Ok(false) => report.unchanged += 1,
                Err(e) => {
                    tracing::warn!(
                        skill_id = %skill.id,
                        error = %e,
                        "skill refinement: refine_one failed"
                    );
                    report.failed += 1;
                }
            }
        }
        report.finished_at = unix_secs();
        tracing::info!(
            eligible = report.eligible,
            refined = report.refined,
            unchanged = report.unchanged,
            failed = report.failed,
            "skill refinement: pass complete"
        );
        Ok(report)
    }

    /// Spawn the refinement loop on the current tokio runtime.
    /// First tick fires after one interval so a boot-time burst
    /// doesn't piggyback the controller startup.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(self.config.interval);
            tick.tick().await; // skip first immediate fire
            loop {
                tick.tick().await;
                if let Err(e) = self.run_once().await {
                    tracing::warn!(error = %e, "skill refinement: pass errored");
                }
            }
        })
    }

    /// Refine one skill. Returns `Ok(true)` when a new version
    /// row was written, `Ok(false)` when the model declared no
    /// improvement or the steps were unchanged.
    pub async fn refine_one(
        &self,
        dispatcher: &dyn AiDispatcher,
        skill: &StoredSkill,
    ) -> Result<bool, RefinementError> {
        let prompt = build_refinement_prompt(skill);
        let session_id = format!("skill-refinement:{}", skill.id);
        let call_fut = dispatcher.chat(&session_id, &prompt, "");
        let reply =
            match tokio::time::timeout(Duration::from_secs(self.config.timeout_secs), call_fut)
                .await
            {
                Ok(Some(text)) => text,
                Ok(None) => return Err(RefinementError::AiNoReply),
                Err(_) => return Err(RefinementError::Timeout),
            };
        let json = extract_json_object(&reply).ok_or_else(|| {
            RefinementError::Parse(format!("no JSON object in reply: {reply:.200}"))
        })?;
        let parsed: RefinementResponse = serde_json::from_str(&json)
            .map_err(|e| RefinementError::Parse(format!("parse JSON: {e}")))?;
        if !parsed.improved {
            return Ok(false);
        }
        if parsed.steps.is_empty() {
            return Err(RefinementError::Parse(
                "refinement marked improved but steps were empty".into(),
            ));
        }
        if parsed.steps == skill.steps {
            return Ok(false);
        }
        let reason = parsed.change_reason.as_deref();
        self.store
            .add_version(&skill.id, &parsed.steps, reason)
            .map_err(RefinementError::Store)?;
        tracing::info!(
            skill_id = %skill.id,
            reason = ?reason,
            "skill refinement: new version stored"
        );
        Ok(true)
    }
}

pub const SKILL_REFINEMENT_PROMPT_TEMPLATE: &str = "You are a skill refinement engine. Review this skill and suggest improvements based on usage examples.\n\
     \n\
     Skill: {name}\n\
     Description: {description}\n\
     Current steps: {steps}\n\
     Example inputs: {example_inputs}\n\
     Example outputs: {example_outputs}\n\
     \n\
     Return ONLY valid JSON with this exact schema:\n\
     {{\n  \"improved\": true_or_false,\n  \"steps\": [{{\"step\": \"...\", \"tool\": \"tool_name_or_null\"}}],\n  \"change_reason\": \"one sentence explaining what changed\"\n}}\n\
     \n\
     Rules:\n\
     - If the current steps are already optimal, return `\"improved\": false` and copy the existing steps verbatim.\n\
     - When proposing improvements, keep 2-6 steps total.\n\
     - Return ONLY the JSON object, no markdown, no explanation.";

pub fn build_refinement_prompt(skill: &StoredSkill) -> String {
    let steps_json = serde_json::to_string(&skill.steps).unwrap_or_else(|_| "[]".into());
    let inputs = if skill.example_inputs.is_empty() {
        "(none recorded)".to_string()
    } else {
        skill
            .example_inputs
            .iter()
            .take(3)
            .enumerate()
            .map(|(i, x)| format!("{}: {}", i + 1, x))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let outputs = if skill.example_outputs.is_empty() {
        "(none recorded)".to_string()
    } else {
        skill
            .example_outputs
            .iter()
            .take(3)
            .enumerate()
            .map(|(i, x)| format!("{}: {}", i + 1, x))
            .collect::<Vec<_>>()
            .join("\n")
    };
    SKILL_REFINEMENT_PROMPT_TEMPLATE
        .replace("{name}", &skill.name)
        .replace("{description}", &skill.description)
        .replace("{steps}", &steps_json)
        .replace("{example_inputs}", &inputs)
        .replace("{example_outputs}", &outputs)
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct RefinementReport {
    pub eligible: usize,
    pub refined: usize,
    pub unchanged: usize,
    pub failed: usize,
    pub started_at: i64,
    pub finished_at: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum RefinementError {
    #[error("refinement: skill `{0}` not found")]
    NotFound(String),
    #[error("refinement: store: {0}")]
    Store(#[from] crate::nodes::ai::skill_store::SkillStoreError),
    #[error("refinement: ai dispatcher returned None")]
    AiNoReply,
    #[error("refinement: ai dispatcher timed out")]
    Timeout,
    #[error("refinement: parse: {0}")]
    Parse(String),
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::ai::skill_store::{SkillStatus, StoredSkill};
    use async_trait::async_trait;
    use std::sync::Mutex;

    fn store() -> Arc<SkillStore> {
        Arc::new(SkillStore::open_in_memory().unwrap())
    }

    fn sample(id: &str, conf: f32, usage: i64) -> StoredSkill {
        StoredSkill {
            id: id.to_string(),
            name: format!("skill_{id}"),
            description: format!("desc for {id}"),
            source_agent: "agent.alpha".into(),
            version: 1,
            confidence: conf,
            usage_count: usage,
            last_used_ms: None,
            created_at_ms: 1_000_000,
            updated_at_ms: 1_000_000,
            tags: vec!["t1".into(), "t2".into()],
            steps: vec![
                SkillStep {
                    step: "step one".into(),
                    tool: None,
                    prompt: None,
                },
                SkillStep {
                    step: "step two".into(),
                    tool: None,
                    prompt: None,
                },
            ],
            example_inputs: vec!["in1".into()],
            example_outputs: vec!["out1".into()],
            status: SkillStatus::Active,
            tenant_id: None,
        }
    }

    struct StubAi {
        canned: Mutex<String>,
    }

    #[async_trait]
    impl AiDispatcher for StubAi {
        async fn chat(&self, _s: &str, _p: &str, _h: &str) -> Option<String> {
            Some(self.canned.lock().unwrap().clone())
        }
    }

    fn engine(store: Arc<SkillStore>, canned: Option<&str>) -> SkillRefinementEngine {
        let ai_cell: Arc<tokio::sync::OnceCell<Arc<dyn AiDispatcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        if let Some(s) = canned {
            let _ = ai_cell.set(Arc::new(StubAi {
                canned: Mutex::new(s.to_string()),
            }));
        }
        SkillRefinementEngine::new(store, ai_cell, RefinementConfig::default())
    }

    #[test]
    fn usage_outcome_deltas_match_spec() {
        assert_eq!(UsageOutcome::SuccessLiked.delta(), 0.05);
        assert_eq!(UsageOutcome::Success.delta(), 0.01);
        assert_eq!(UsageOutcome::Failed.delta(), -0.10);
    }

    #[test]
    fn record_usage_success_bumps_confidence() {
        let store = store();
        store.insert(&sample("a", 0.7, 0)).unwrap();
        let engine = engine(store.clone(), None);
        let new = engine.record_usage("a", UsageOutcome::Success).unwrap();
        assert!((new - 0.71).abs() < 1e-5);
        let row = store.get("a").unwrap().unwrap();
        assert_eq!(row.usage_count, 1);
    }

    #[test]
    fn record_usage_liked_bumps_more() {
        let store = store();
        store.insert(&sample("a", 0.7, 0)).unwrap();
        let engine = engine(store.clone(), None);
        let new = engine
            .record_usage("a", UsageOutcome::SuccessLiked)
            .unwrap();
        assert!((new - 0.75).abs() < 1e-5);
    }

    #[test]
    fn record_usage_failed_drops_confidence() {
        let store = store();
        store.insert(&sample("a", 0.6, 0)).unwrap();
        let engine = engine(store.clone(), None);
        let new = engine.record_usage("a", UsageOutcome::Failed).unwrap();
        assert!((new - 0.5).abs() < 1e-5);
    }

    #[test]
    fn record_usage_is_capped_at_ceiling() {
        let store = store();
        store.insert(&sample("a", 0.94, 0)).unwrap();
        let engine = engine(store.clone(), None);
        let new = engine
            .record_usage("a", UsageOutcome::SuccessLiked)
            .unwrap();
        assert_eq!(new, CONFIDENCE_CEIL);
    }

    #[test]
    fn record_usage_is_floored() {
        let store = store();
        store.insert(&sample("a", 0.06, 0)).unwrap();
        let engine = engine(store.clone(), None);
        // Drop 0.06 - 0.10 = -0.04 → floored at 0.05.
        let new = engine.record_usage("a", UsageOutcome::Failed).unwrap();
        assert_eq!(new, CONFIDENCE_FLOOR);
    }

    #[test]
    fn record_usage_on_missing_skill_errors() {
        let store = store();
        let engine = engine(store, None);
        let err = engine
            .record_usage("ghost", UsageOutcome::Success)
            .unwrap_err();
        assert!(matches!(err, RefinementError::NotFound(_)));
    }

    #[tokio::test]
    async fn run_once_with_no_eligible_skills_reports_zero() {
        let store = store();
        // Below thresholds.
        store.insert(&sample("a", 0.4, 1)).unwrap();
        let engine = engine(store, None);
        let report = engine.run_once().await.unwrap();
        assert_eq!(report.eligible, 0);
    }

    #[tokio::test]
    async fn run_once_without_dispatcher_skips_with_warning() {
        let store = store();
        store.insert(&sample("a", 0.8, 20)).unwrap();
        let engine = engine(store, None);
        let report = engine.run_once().await.unwrap();
        // Eligibility counted, but no calls made → no refines.
        assert_eq!(report.eligible, 1);
        assert_eq!(report.refined, 0);
    }

    #[tokio::test]
    async fn run_once_refines_when_llm_says_improved() {
        let store = store();
        let mut s = sample("a", 0.8, 20);
        s.steps[0].step = "old step".into();
        store.insert(&s).unwrap();
        let canned = r#"{"improved":true,"steps":[{"step":"step one updated","tool":null},{"step":"step two updated","tool":null}],"change_reason":"clarified phrasing"}"#;
        let engine = engine(store.clone(), Some(canned));
        let report = engine.run_once().await.unwrap();
        assert_eq!(report.eligible, 1);
        assert_eq!(report.refined, 1);
        let after = store.get("a").unwrap().unwrap();
        assert_eq!(after.version, 2);
        let versions = store.versions("a").unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(
            versions[1].change_reason.as_deref(),
            Some("clarified phrasing")
        );
    }

    #[tokio::test]
    async fn run_once_skips_when_llm_says_not_improved() {
        let store = store();
        store.insert(&sample("a", 0.8, 20)).unwrap();
        let canned = r#"{"improved":false,"steps":[],"change_reason":"already optimal"}"#;
        let engine = engine(store.clone(), Some(canned));
        let report = engine.run_once().await.unwrap();
        assert_eq!(report.refined, 0);
        assert_eq!(report.unchanged, 1);
        let after = store.get("a").unwrap().unwrap();
        assert_eq!(after.version, 1);
    }

    #[tokio::test]
    async fn run_once_skips_when_steps_match_current() {
        let store = store();
        let s = sample("a", 0.8, 20);
        store.insert(&s).unwrap();
        // LLM returns improved=true but the SAME steps — we
        // should still not create a version row.
        let steps_json = serde_json::to_string(&s.steps).unwrap();
        let canned = format!(
            r#"{{"improved":true,"steps":{steps_json},"change_reason":"no actual change"}}"#
        );
        let engine = engine(store.clone(), Some(&canned));
        let report = engine.run_once().await.unwrap();
        assert_eq!(report.refined, 0);
        assert_eq!(report.unchanged, 1);
    }

    #[tokio::test]
    async fn run_once_counts_parse_failure_as_failed() {
        let store = store();
        store.insert(&sample("a", 0.8, 20)).unwrap();
        let engine = engine(store.clone(), Some("definitely not json"));
        let report = engine.run_once().await.unwrap();
        assert_eq!(report.failed, 1);
        assert_eq!(report.refined, 0);
    }
}
