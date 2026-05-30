//! GAP 4 — auto-skill extraction pipeline.
//!
//! Runs as a post-`ai.chat` hook. After a successful chat
//! returns to the caller, the AI handler spawns this extractor
//! in a tokio task; the caller never waits.
//!
//! Five stages, all best-effort:
//!
//! 1. **Complexity scoring** — five weighted signals (see
//!    [`ComplexityScorer`]). Below the configured floor, the
//!    extractor returns early without an LLM call.
//! 2. **Duplicate check** — embed the candidate description, query
//!    the SkillStore for prior skills by the same agent, and
//!    look for cosine similarity ≥ [`SkillExtractorConfig::dup_threshold`]
//!    against any of the prior descriptions. On hit, bump
//!    `usage_count` and nudge confidence; never create a new
//!    skill.
//! 3. **LLM synthesis** — the prompt in
//!    [`SKILL_EXTRACTION_PROMPT_TEMPLATE`] asks for STRICT JSON
//!    with a known schema. Anything else is dropped.
//! 4. **Parse + validate** — name under 40 chars, snake_case-ish;
//!    description under 120 chars; 2-6 steps; 2-5 tags. Failed
//!    validation logs a warn and returns.
//! 5. **Store** — insert into the SkillStore with `confidence =
//!    0.5`, `usage_count = 0`, the original task's first
//!    `EXAMPLES_PER_SKILL` worth of input + output captured as
//!    seed examples.
//!
//! The extractor is non-blocking by design — it must never panic
//! the spawned task. Every failure mode logs a structured warn
//! and returns silently.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::nodes::ai::skill_store::{
    SkillStatus, SkillStep, SkillStore, StoredSkill, mint_skill_id,
};
use crate::nodes::memory::curator::AiDispatcher;
use crate::nodes::memory::curator::EmbeddingDispatcher;

/// Adapter that wraps a local [`crate::nodes::ai::provider::ChatProvider`]
/// so the skill extractor can call the synthesis LLM without
/// going through the mesh. Avoids the round-trip cost AND the
/// recursion hazard (a mesh hop back into `ai.chat` would spawn
/// another extractor).
pub struct LocalProviderAiDispatcher {
    provider: std::sync::Arc<dyn crate::nodes::ai::provider::ChatProvider>,
    model: String,
}

impl LocalProviderAiDispatcher {
    pub fn new(
        provider: std::sync::Arc<dyn crate::nodes::ai::provider::ChatProvider>,
        model: String,
    ) -> Self {
        Self { provider, model }
    }
}

#[async_trait::async_trait]
impl AiDispatcher for LocalProviderAiDispatcher {
    async fn chat(&self, session_id: &str, prompt: &str, history: &str) -> Option<String> {
        use crate::nodes::ai::provider::ChatInput;
        let input = ChatInput {
            session_id: session_id.to_string(),
            prompt: prompt.to_string(),
            history: history.to_string(),
            model: self.model.clone(),
            system_prompt: None,
            ..Default::default()
        };
        match self.provider.generate_reply(input).await {
            Ok(out) => Some(out.text),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "skill extractor: local provider synthesis call failed"
                );
                None
            }
        }
    }
}

/// Adapter that wraps a local [`crate::nodes::ai::provider::ChatProvider`]
/// so the duplicate-check path can embed candidate descriptions
/// against existing skills without an extra mesh hop.
pub struct LocalProviderEmbedDispatcher {
    provider: std::sync::Arc<dyn crate::nodes::ai::provider::ChatProvider>,
}

impl LocalProviderEmbedDispatcher {
    pub fn new(provider: std::sync::Arc<dyn crate::nodes::ai::provider::ChatProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait::async_trait]
impl EmbeddingDispatcher for LocalProviderEmbedDispatcher {
    async fn embed(
        &self,
        model: &str,
        texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, crate::nodes::memory::EmbeddingError> {
        let input = crate::nodes::ai::provider::EmbedInput {
            model: model.to_string(),
            texts: texts.iter().map(|s| s.to_string()).collect(),
        };
        match self.provider.generate_embeddings(input).await {
            Ok(out) => Ok(out.vectors),
            Err(e) => Err(crate::nodes::memory::EmbeddingError::Responder(format!(
                "local provider embed: {e}"
            ))),
        }
    }
}

/// Default minimum complexity score that gates extraction.
pub const DEFAULT_MIN_COMPLEXITY: f32 = 0.6;

/// Default cosine threshold above which the candidate is
/// considered a duplicate of an existing skill.
pub const DEFAULT_DUP_THRESHOLD: f32 = 0.85;

/// Cap on the number of (input, output) example pairs kept per
/// skill in the store.
pub const EXAMPLES_PER_SKILL: usize = 3;

/// Model name the extractor passes to the synthesis call. The
/// default is `openrouter/anthropic/claude-3-5-haiku` — same
/// cheap-tier model the memory dialectic surface uses.
pub const DEFAULT_EXTRACTION_MODEL: &str = "openrouter/anthropic/claude-3-5-haiku";

/// Configuration applied at extractor construction. All values
/// have defaults so callers wiring through `[skills]` config
/// don't have to populate every field.
#[derive(Clone, Debug)]
pub struct SkillExtractorConfig {
    pub min_complexity_score: f32,
    pub dup_threshold: f32,
    pub examples_per_skill: usize,
    pub extraction_model: String,
    pub embedding_model: String,
    /// Hard wall-clock cap on the synthesis call. The hook is
    /// fire-and-forget but we still bound it so a wedged
    /// provider can't accumulate idle tokio tasks.
    pub synthesis_timeout_secs: u64,
}

impl Default for SkillExtractorConfig {
    fn default() -> Self {
        Self {
            min_complexity_score: DEFAULT_MIN_COMPLEXITY,
            dup_threshold: DEFAULT_DUP_THRESHOLD,
            examples_per_skill: EXAMPLES_PER_SKILL,
            extraction_model: DEFAULT_EXTRACTION_MODEL.to_string(),
            embedding_model: "nomic-embed-text-v1.5".to_string(),
            synthesis_timeout_secs: 30,
        }
    }
}

/// All inputs the extractor needs about a completed task.
#[derive(Clone, Debug)]
pub struct TaskCompletion {
    pub session_id: String,
    pub agent_name: String,
    pub prompt: String,
    pub response: String,
    pub response_word_count: usize,
    pub tool_calls: Vec<String>,
    pub asked_for_structured_output: bool,
    pub duration_secs: i64,
    pub session_turns: usize,
    pub success: bool,
}

/// Public surface: scores a completed task and (when eligible)
/// drives the rest of the pipeline.
#[derive(Clone)]
pub struct SkillExtractor {
    store: Arc<SkillStore>,
    ai_cell: Arc<tokio::sync::OnceCell<Arc<dyn AiDispatcher>>>,
    embed_cell: Arc<tokio::sync::OnceCell<Arc<dyn EmbeddingDispatcher>>>,
    config: SkillExtractorConfig,
}

impl SkillExtractor {
    pub fn new(
        store: Arc<SkillStore>,
        ai_cell: Arc<tokio::sync::OnceCell<Arc<dyn AiDispatcher>>>,
        embed_cell: Arc<tokio::sync::OnceCell<Arc<dyn EmbeddingDispatcher>>>,
        config: SkillExtractorConfig,
    ) -> Self {
        Self {
            store,
            ai_cell,
            embed_cell,
            config,
        }
    }

    /// Borrow the inner SkillStore. Used by the controller wiring
    /// and by the refinement engine so both surfaces talk to the
    /// same connection.
    pub fn store(&self) -> Arc<SkillStore> {
        self.store.clone()
    }

    /// Run the full pipeline against `task`. Returns an
    /// [`ExtractionOutcome`] describing what happened. Errors
    /// are wrapped into `Outcome::Failed` rather than returned
    /// so the spawned task can stay infallible.
    pub async fn run(&self, task: TaskCompletion) -> ExtractionOutcome {
        if !task.success {
            return ExtractionOutcome::SkippedFailedTask;
        }
        let score = score_complexity(&task);
        if score < self.config.min_complexity_score {
            tracing::debug!(
                session_id = %task.session_id,
                score,
                threshold = self.config.min_complexity_score,
                "skill extractor: complexity below floor; skipping"
            );
            return ExtractionOutcome::SkippedLowComplexity(score);
        }

        // Duplicate check. Best-effort — when the embedding
        // dispatcher is missing or fails, we proceed to synthesis
        // (the worst case is a near-duplicate skill, which the
        // refinement engine will later merge).
        match self.duplicate_check(&task).await {
            DuplicateCheck::Hit(existing_id) => {
                if let Err(e) = self.store.increment_usage(&existing_id) {
                    tracing::warn!(
                        error = %e,
                        skill_id = %existing_id,
                        "skill extractor: increment_usage failed"
                    );
                    return ExtractionOutcome::Failed(format!("increment usage: {e}"));
                }
                let _ = self.store.record_example(
                    &existing_id,
                    &task.prompt,
                    &task.response,
                    self.config.examples_per_skill,
                );
                // Tiny confidence bump (same as the
                // refinement engine's no-feedback path).
                if let Ok(Some(skill)) = self.store.get(&existing_id) {
                    let bumped = (skill.confidence + 0.01).clamp(0.05, 0.95);
                    let _ = self.store.update_confidence(&existing_id, bumped);
                }
                return ExtractionOutcome::DuplicateBumped(existing_id);
            }
            DuplicateCheck::NoEmbedder | DuplicateCheck::Miss => {
                // Continue to synthesis.
            }
        }

        let synth_result = self.synthesize(&task).await;
        let parsed = match synth_result {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    session_id = %task.session_id,
                    error = %e,
                    "skill extractor: synthesis failed; skipping"
                );
                return ExtractionOutcome::Failed(e);
            }
        };

        if let Err(e) = validate_parsed_skill(&parsed) {
            tracing::warn!(
                session_id = %task.session_id,
                error = %e,
                "skill extractor: parsed skill rejected by validator; skipping"
            );
            return ExtractionOutcome::Failed(format!("validation: {e}"));
        }

        let now = unix_millis();
        let id = mint_skill_id(&task.agent_name, &parsed.name);
        let skill = StoredSkill {
            id: id.clone(),
            name: parsed.name,
            description: parsed.description,
            source_agent: task.agent_name.clone(),
            version: 1,
            confidence: 0.5,
            usage_count: 0,
            last_used_ms: None,
            created_at_ms: now,
            updated_at_ms: now,
            tags: parsed.tags,
            steps: parsed.steps,
            example_inputs: vec![task.prompt.clone()],
            example_outputs: vec![task.response.clone()],
            status: SkillStatus::Active,
            tenant_id: None,
        };
        match self.store.insert(&skill) {
            Ok(_) => {
                tracing::info!(
                    session_id = %task.session_id,
                    skill_id = %id,
                    agent = %task.agent_name,
                    complexity_score = score,
                    "skill extractor: new skill captured"
                );
                ExtractionOutcome::Created(id)
            }
            Err(e) => {
                tracing::warn!(error = %e, "skill extractor: insert failed");
                ExtractionOutcome::Failed(format!("insert: {e}"))
            }
        }
    }

    /// Spawn the pipeline on the current tokio runtime. Caller
    /// does NOT await — this returns the JoinHandle immediately
    /// so the chat handler can keep returning the response
    /// without backpressure from skill extraction.
    pub fn spawn(
        self: Arc<Self>,
        task: TaskCompletion,
    ) -> tokio::task::JoinHandle<ExtractionOutcome> {
        tokio::spawn(async move { self.run(task).await })
    }

    async fn duplicate_check(&self, task: &TaskCompletion) -> DuplicateCheck {
        let dispatcher = match self.embed_cell.get() {
            Some(d) => d.clone(),
            None => return DuplicateCheck::NoEmbedder,
        };
        // Cheap path: only the same agent's skills count as
        // candidates. The duplicate-merge spec is per-agent.
        let candidates = match self
            .store
            .list(&crate::nodes::ai::skill_store::SkillFilter {
                agent: Some(task.agent_name.clone()),
                status: Some(SkillStatus::Active),
                limit: Some(200),
                ..Default::default()
            }) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "skill extractor: list for dedup failed");
                return DuplicateCheck::Miss;
            }
        };
        if candidates.is_empty() {
            return DuplicateCheck::Miss;
        }
        // Embed (candidate descriptions, candidate's task prompt summary)
        // in one batch — saves a round trip when the dispatcher
        // supports batched embeds.
        let probe = task_probe(task);
        let mut texts: Vec<String> = Vec::with_capacity(candidates.len() + 1);
        texts.push(probe.clone());
        for c in &candidates {
            texts.push(format!("{}\n{}", c.name, c.description));
        }
        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let vectors = match dispatcher.embed(&self.config.embedding_model, &refs).await {
            Ok(v) if v.len() == texts.len() => v,
            Ok(other) => {
                tracing::warn!(
                    got = other.len(),
                    want = texts.len(),
                    "skill extractor: dispatcher returned wrong vector count; dedup skipped"
                );
                return DuplicateCheck::Miss;
            }
            Err(e) => {
                tracing::warn!(error = %e, "skill extractor: embed failed; dedup skipped");
                return DuplicateCheck::Miss;
            }
        };
        let query_vec = &vectors[0];
        if query_vec.is_empty() {
            return DuplicateCheck::Miss;
        }
        let mut best: f32 = 0.0;
        let mut best_id: Option<String> = None;
        for (i, c) in candidates.iter().enumerate() {
            let v = &vectors[i + 1];
            let sim = cosine_similarity(query_vec, v);
            if sim > best {
                best = sim;
                best_id = Some(c.id.clone());
            }
        }
        if best >= self.config.dup_threshold
            && let Some(id) = best_id
        {
            tracing::info!(
                similarity = best,
                skill_id = %id,
                "skill extractor: duplicate detected; bumping usage"
            );
            return DuplicateCheck::Hit(id);
        }
        DuplicateCheck::Miss
    }

    async fn synthesize(&self, task: &TaskCompletion) -> Result<ParsedSkill, String> {
        let dispatcher = match self.ai_cell.get() {
            Some(d) => d.clone(),
            None => return Err("ai dispatcher not configured".into()),
        };
        let prompt = build_synthesis_prompt(task);
        let history = "";
        let call_fut = dispatcher.chat(&task.session_id, &prompt, history);
        let reply = match tokio::time::timeout(
            Duration::from_secs(self.config.synthesis_timeout_secs),
            call_fut,
        )
        .await
        {
            Ok(Some(text)) => text,
            Ok(None) => return Err("ai dispatcher returned None".into()),
            Err(_) => return Err("synthesis timed out".into()),
        };
        let json_slice = extract_json_object(&reply)
            .ok_or_else(|| format!("no JSON object in reply: {reply:.200}"))?;
        let parsed: ParsedSkill = serde_json::from_str(&json_slice)
            .map_err(|e| format!("parse JSON: {e} (body: {json_slice:.200})"))?;
        Ok(parsed)
    }
}

/// What the extractor decided to do with one completion. The
/// caller (typically the AI handler) only cares for tests / log
/// shipping; the variants are listed exhaustively so each
/// short-circuit path is visible.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtractionOutcome {
    Created(String),
    DuplicateBumped(String),
    SkippedFailedTask,
    SkippedLowComplexity(f32),
    Failed(String),
}

#[derive(Debug)]
enum DuplicateCheck {
    Hit(String),
    Miss,
    NoEmbedder,
}

/// Parsed JSON the synthesis call is required to emit.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ParsedSkill {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub steps: Vec<SkillStep>,
}

/// Reject the parsed payload when it violates the spec'd shape.
pub fn validate_parsed_skill(p: &ParsedSkill) -> Result<(), String> {
    if p.name.is_empty() || p.name.len() > 40 {
        return Err(format!(
            "name must be 1..=40 chars, got {} ({})",
            p.name.len(),
            p.name
        ));
    }
    // Snake-case-ish: alphanumeric + underscores only.
    if !p
        .name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(format!(
            "name must be snake_case (alphanumeric + underscore), got {}",
            p.name
        ));
    }
    if p.description.is_empty() || p.description.len() > 120 {
        return Err(format!(
            "description must be 1..=120 chars, got {}",
            p.description.len()
        ));
    }
    if !(2..=6).contains(&p.steps.len()) {
        return Err(format!("steps must be 2..=6, got {}", p.steps.len()));
    }
    if !(2..=5).contains(&p.tags.len()) {
        return Err(format!("tags must be 2..=5, got {}", p.tags.len()));
    }
    for s in &p.steps {
        if s.step.trim().is_empty() {
            return Err("step body must be non-empty".into());
        }
    }
    Ok(())
}

/// Five weighted signals, all bounded so the total never
/// exceeds 1.0.
pub fn score_complexity(task: &TaskCompletion) -> f32 {
    let mut score = 0.0f32;
    if task.response_word_count > 200 {
        score += 0.3;
    }
    if !task.tool_calls.is_empty() {
        score += 0.2;
    }
    if task.asked_for_structured_output {
        score += 0.2;
    }
    if task.duration_secs > 3 {
        score += 0.1;
    }
    if task.session_turns > 3 {
        score += 0.2;
    }
    score.min(1.0)
}

/// Decide whether the user prompt is asking for structured
/// output (JSON / code / table). Substring + heuristic — the
/// alpha doesn't need a parser here.
pub fn detect_structured_output(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    let markers = [
        "json",
        "yaml",
        "table",
        "schema",
        "csv",
        "```",
        "code block",
        "function ",
        "class ",
        "snippet",
        "list of ",
        "as json",
        "as a table",
        "as code",
    ];
    markers.iter().any(|m| lower.contains(m))
}

/// First-200-words slice of a response, used in the synthesis
/// prompt so the LLM has bounded input.
pub fn first_n_words(text: &str, n: usize) -> String {
    let mut out: Vec<&str> = Vec::with_capacity(n);
    for w in text.split_whitespace() {
        out.push(w);
        if out.len() >= n {
            break;
        }
    }
    out.join(" ")
}

fn task_probe(task: &TaskCompletion) -> String {
    // Whatever we hand the embed dispatcher should mirror what
    // we store in `description`. We have no description yet at
    // this point so we approximate one from the prompt + first
    // 50 words of the response. Stable + cheap.
    format!(
        "{}\n{}",
        first_n_words(&task.prompt, 40),
        first_n_words(&task.response, 50)
    )
}

/// Find the first balanced `{ ... }` block in `text`. Used to
/// strip markdown fences / commentary off the LLM reply before
/// `serde_json::from_str`.
pub fn extract_json_object(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut start: Option<usize> = None;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0
                    && let Some(s) = start
                {
                    return Some(text[s..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

pub const SKILL_EXTRACTION_PROMPT_TEMPLATE: &str = "You are a skill extraction engine. A successful AI task just completed. \
     Extract it as a reusable skill.\n\
     \n\
     Task request: {request}\n\
     Task response summary: {response_summary}\n\
     Tools used: {tools}\n\
     \n\
     Return ONLY valid JSON with this exact schema:\n\
     {{\n  \"name\": \"short_snake_case_name\",\n  \"description\": \"One sentence describing what this skill does.\",\n  \"tags\": [\"tag1\", \"tag2\"],\n  \"steps\": [\n    {{\"step\": \"description of step 1\", \"tool\": \"tool_name_or_null\"}},\n    {{\"step\": \"description of step 2\", \"tool\": null}}\n  ]\n}}\n\
     \n\
     Rules:\n\
     - name must be under 40 characters, snake_case, no spaces\n\
     - description must be one sentence, under 120 characters\n\
     - tags must be 2-5 relevant keywords\n\
     - steps must be 2-6 steps\n\
     - Return ONLY the JSON object, no markdown, no explanation";

pub fn build_synthesis_prompt(task: &TaskCompletion) -> String {
    let tools = if task.tool_calls.is_empty() {
        "none".to_string()
    } else {
        task.tool_calls.join(", ")
    };
    SKILL_EXTRACTION_PROMPT_TEMPLATE
        .replace("{request}", &first_n_words(&task.prompt, 80))
        .replace("{response_summary}", &first_n_words(&task.response, 200))
        .replace("{tools}", &tools)
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Re-export the SkillStoreError for callers that want to plumb
/// extractor failures through the same error type.
pub use crate::nodes::ai::skill_store::SkillStoreError as ReExportSkillStoreError;

// Keep a single `Send + Sync` bound on `ExtractionOutcome` so
// the tokio::spawn return type is visible.
impl ExtractionOutcome {
    pub fn is_created(&self) -> bool {
        matches!(self, ExtractionOutcome::Created(_))
    }
}

// Silence the unused-re-export when no caller imports it.
#[doc(hidden)]
pub fn _touch_reexport() -> Option<ReExportSkillStoreError> {
    None
}

#[allow(dead_code)]
fn assert_send_sync<T: Send + Sync>() {}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    fn baseline_task() -> TaskCompletion {
        TaskCompletion {
            session_id: "sess-1".into(),
            agent_name: "agent.alpha".into(),
            prompt: "fetch the latest deployment status".into(),
            response: "ok the deployment is healthy".into(),
            response_word_count: 5,
            tool_calls: vec![],
            asked_for_structured_output: false,
            duration_secs: 1,
            session_turns: 1,
            success: true,
        }
    }

    fn high_complexity_task() -> TaskCompletion {
        TaskCompletion {
            session_id: "sess-1".into(),
            agent_name: "agent.alpha".into(),
            prompt: "fetch the latest deployment status as JSON for our k8s cluster".into(),
            response: (0..220)
                .map(|i| format!("word{i}"))
                .collect::<Vec<_>>()
                .join(" "),
            response_word_count: 220,
            tool_calls: vec!["k8s.list_pods".into()],
            asked_for_structured_output: true,
            duration_secs: 5,
            session_turns: 4,
            success: true,
        }
    }

    fn store() -> Arc<SkillStore> {
        Arc::new(SkillStore::open_in_memory().unwrap())
    }

    type AiCell = Arc<tokio::sync::OnceCell<Arc<dyn AiDispatcher>>>;
    type EmbedCell = Arc<tokio::sync::OnceCell<Arc<dyn EmbeddingDispatcher>>>;

    fn empty_cells() -> (AiCell, EmbedCell) {
        (
            Arc::new(tokio::sync::OnceCell::new()),
            Arc::new(tokio::sync::OnceCell::new()),
        )
    }

    #[test]
    fn complexity_under_floor_for_baseline_task() {
        let s = score_complexity(&baseline_task());
        assert!(s < DEFAULT_MIN_COMPLEXITY, "baseline got {s}");
    }

    #[test]
    fn complexity_full_score_for_dense_task() {
        let s = score_complexity(&high_complexity_task());
        assert!(s >= 0.9, "expected near-full score, got {s}");
    }

    #[test]
    fn detect_structured_output_picks_up_json_marker() {
        assert!(detect_structured_output("return as JSON please"));
        assert!(detect_structured_output("output a table"));
        assert!(detect_structured_output("write a class for me"));
        assert!(!detect_structured_output("how are you today?"));
    }

    #[test]
    fn extract_json_object_strips_markdown_fences() {
        let raw = "Here you go:\n```json\n{\"name\":\"x\",\"v\":1}\n```\nThanks.";
        let got = extract_json_object(raw).expect("should find the object");
        assert!(got.contains("\"name\":\"x\""));
        assert!(got.contains("\"v\":1"));
    }

    #[test]
    fn extract_json_object_handles_nested_objects() {
        let raw = "{\"outer\": {\"inner\": {\"a\": 1}}, \"k\": 2}";
        let got = extract_json_object(raw).unwrap();
        // Re-parse to confirm balance was preserved.
        let v: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["k"], 2);
    }

    #[test]
    fn extract_json_object_ignores_braces_inside_strings() {
        let raw = "{\"k\":\"value with } in it\", \"k2\": 3}";
        let got = extract_json_object(raw).unwrap();
        let v: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["k2"], 3);
    }

    #[test]
    fn validate_rejects_long_name() {
        let p = ParsedSkill {
            name: "a".repeat(50),
            description: "ok desc".into(),
            tags: vec!["t1".into(), "t2".into()],
            steps: vec![
                SkillStep {
                    step: "s1".into(),
                    tool: None,
                    prompt: None,
                },
                SkillStep {
                    step: "s2".into(),
                    tool: None,
                    prompt: None,
                },
            ],
        };
        assert!(validate_parsed_skill(&p).is_err());
    }

    #[test]
    fn validate_rejects_non_snake_case_name() {
        let p = ParsedSkill {
            name: "kebab-case".into(),
            description: "ok desc".into(),
            tags: vec!["t1".into(), "t2".into()],
            steps: vec![
                SkillStep {
                    step: "s1".into(),
                    tool: None,
                    prompt: None,
                },
                SkillStep {
                    step: "s2".into(),
                    tool: None,
                    prompt: None,
                },
            ],
        };
        assert!(validate_parsed_skill(&p).is_err());
    }

    #[test]
    fn validate_rejects_too_few_steps() {
        let p = ParsedSkill {
            name: "ok_name".into(),
            description: "ok desc".into(),
            tags: vec!["t1".into(), "t2".into()],
            steps: vec![SkillStep {
                step: "only one".into(),
                tool: None,
                prompt: None,
            }],
        };
        assert!(validate_parsed_skill(&p).is_err());
    }

    #[test]
    fn validate_rejects_wrong_tag_count() {
        let p = ParsedSkill {
            name: "ok_name".into(),
            description: "ok desc".into(),
            tags: vec!["only_one".into()],
            steps: vec![
                SkillStep {
                    step: "s1".into(),
                    tool: None,
                    prompt: None,
                },
                SkillStep {
                    step: "s2".into(),
                    tool: None,
                    prompt: None,
                },
            ],
        };
        assert!(validate_parsed_skill(&p).is_err());
    }

    #[test]
    fn validate_accepts_canonical_shape() {
        let p = ParsedSkill {
            name: "deploy_to_staging".into(),
            description: "Deploys the current branch to the staging cluster.".into(),
            tags: vec!["deploy".into(), "staging".into(), "k8s".into()],
            steps: vec![
                SkillStep {
                    step: "build the image".into(),
                    tool: Some("docker.build".into()),
                    prompt: None,
                },
                SkillStep {
                    step: "push and rollout".into(),
                    tool: Some("kubectl.rollout".into()),
                    prompt: None,
                },
            ],
        };
        validate_parsed_skill(&p).unwrap();
    }

    // ── Stubs for the dispatchers ──

    struct StubAi {
        canned: Mutex<String>,
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl AiDispatcher for StubAi {
        async fn chat(&self, _session_id: &str, _prompt: &str, _history: &str) -> Option<String> {
            *self.calls.lock().unwrap() += 1;
            Some(self.canned.lock().unwrap().clone())
        }
    }

    struct StubEmbed {
        canned: Vec<Vec<f32>>,
    }

    #[async_trait]
    impl EmbeddingDispatcher for StubEmbed {
        async fn embed(
            &self,
            _model: &str,
            texts: &[&str],
        ) -> Result<Vec<Vec<f32>>, crate::nodes::memory::EmbeddingError> {
            if self.canned.len() == texts.len() {
                Ok(self.canned.clone())
            } else if self.canned.len() == 1 {
                Ok(vec![self.canned[0].clone(); texts.len()])
            } else {
                Ok(vec![vec![1.0, 0.0]; texts.len()])
            }
        }
    }

    #[tokio::test]
    async fn run_skips_failed_task() {
        let store = store();
        let (ai, em) = empty_cells();
        let ex = SkillExtractor::new(store.clone(), ai, em, SkillExtractorConfig::default());
        let mut t = high_complexity_task();
        t.success = false;
        let out = ex.run(t).await;
        assert_eq!(out, ExtractionOutcome::SkippedFailedTask);
    }

    #[tokio::test]
    async fn run_skips_low_complexity_task() {
        let store = store();
        let (ai, em) = empty_cells();
        let ex = SkillExtractor::new(store.clone(), ai, em, SkillExtractorConfig::default());
        let out = ex.run(baseline_task()).await;
        assert!(matches!(out, ExtractionOutcome::SkippedLowComplexity(_)));
    }

    #[tokio::test]
    async fn run_synthesizes_and_inserts_when_eligible() {
        let store = store();
        let (ai_cell, embed_cell) = empty_cells();
        let canned = r#"{"name":"deploy_to_staging","description":"Deploy the current branch to staging.","tags":["deploy","staging","k8s"],"steps":[{"step":"build","tool":"docker.build"},{"step":"push","tool":null}]}"#;
        let ai = Arc::new(StubAi {
            canned: Mutex::new(canned.to_string()),
            calls: Mutex::new(0),
        });
        ai_cell.set(ai.clone() as Arc<dyn AiDispatcher>).ok();
        // No embed dispatcher → dedup skipped, synthesis runs.
        let ex = SkillExtractor::new(
            store.clone(),
            ai_cell,
            embed_cell,
            SkillExtractorConfig::default(),
        );
        let out = ex.run(high_complexity_task()).await;
        match out {
            ExtractionOutcome::Created(_) => {}
            other => panic!("expected Created, got {other:?}"),
        }
        let rows = store
            .list(&crate::nodes::ai::skill_store::SkillFilter::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "deploy_to_staging");
        assert!((rows[0].confidence - 0.5).abs() < 1e-6);
    }

    #[tokio::test]
    async fn run_drops_malformed_ai_reply() {
        let store = store();
        let (ai_cell, embed_cell) = empty_cells();
        let ai = Arc::new(StubAi {
            canned: Mutex::new("definitely not json".to_string()),
            calls: Mutex::new(0),
        });
        ai_cell.set(ai as Arc<dyn AiDispatcher>).ok();
        let ex = SkillExtractor::new(
            store.clone(),
            ai_cell,
            embed_cell,
            SkillExtractorConfig::default(),
        );
        let out = ex.run(high_complexity_task()).await;
        assert!(matches!(out, ExtractionOutcome::Failed(_)));
        assert!(
            store
                .list(&crate::nodes::ai::skill_store::SkillFilter::default())
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn duplicate_check_hit_bumps_usage_count() {
        let store = store();
        // Seed an existing skill whose description is the exact
        // probe shape we'll compute.
        let mut s = crate::nodes::ai::skill_store::StoredSkill {
            id: "existing".into(),
            name: "deploy_to_staging".into(),
            description: "Deploys the current branch to staging.".into(),
            source_agent: "agent.alpha".into(),
            version: 1,
            confidence: 0.5,
            usage_count: 0,
            last_used_ms: None,
            created_at_ms: 1,
            updated_at_ms: 1,
            tags: vec!["deploy".into(), "staging".into()],
            steps: vec![
                SkillStep {
                    step: "build".into(),
                    tool: None,
                    prompt: None,
                },
                SkillStep {
                    step: "push".into(),
                    tool: None,
                    prompt: None,
                },
            ],
            example_inputs: vec![],
            example_outputs: vec![],
            status: SkillStatus::Active,
            tenant_id: None,
        };
        // Make this row's confidence reusable in the assert
        // below.
        s.confidence = 0.5;
        store.insert(&s).unwrap();
        let (ai_cell, embed_cell) = empty_cells();
        // Stub embed that hands back identical vectors for every
        // input — cosine similarity is 1.0 → duplicate.
        let embed = Arc::new(StubEmbed {
            canned: vec![vec![1.0, 0.0, 0.0]],
        });
        embed_cell.set(embed as Arc<dyn EmbeddingDispatcher>).ok();
        let ex = SkillExtractor::new(
            store.clone(),
            ai_cell,
            embed_cell,
            SkillExtractorConfig::default(),
        );
        let out = ex.run(high_complexity_task()).await;
        assert_eq!(out, ExtractionOutcome::DuplicateBumped("existing".into()));
        let row = store.get("existing").unwrap().unwrap();
        assert_eq!(row.usage_count, 1);
        // Confidence nudged up by 0.01 from 0.5.
        assert!((row.confidence - 0.51).abs() < 1e-5);
    }

    #[tokio::test]
    async fn extractor_is_non_blocking_via_spawn() {
        // The spawn surface returns immediately; the caller
        // never awaits the inner computation.
        let store = store();
        let (ai_cell, em_cell) = empty_cells();
        let ex = Arc::new(SkillExtractor::new(
            store,
            ai_cell,
            em_cell,
            SkillExtractorConfig::default(),
        ));
        let started = std::time::Instant::now();
        let handle = ex.spawn(baseline_task());
        // We didn't await — the call should have returned
        // promptly.
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
        // Now await to confirm the task didn't crash.
        let out = handle.await.unwrap();
        assert!(matches!(out, ExtractionOutcome::SkippedLowComplexity(_)));
    }
}
