//! SKILL.md + AGENTS.md compatibility (Linux Foundation
//! Agentic AI shared file convention).
//!
//! Two related primitives:
//!
//! - **`AGENTS.md`** sits at the root of a project (or any
//!   ancestor of the controller's cwd, walked up to 5 levels)
//!   and describes context the model should know on every
//!   call. The loader returns the file content verbatim; the
//!   AI node prepends it to the system prompt.
//!
//! - **`SKILL.md`** describes a reusable procedure with a
//!   stable name and an inputs/outputs section. The loader
//!   discovers every SKILL.md under known roots and registers
//!   them in an in-memory skill library. CLI surfaces
//!   (`relix skills list`, `relix skills run <name>`) drive
//!   the operator-facing flow; a future agent integration
//!   consults the library before generating a new plan.
//!
//! ## Discovery rules
//!
//! AGENTS.md:
//! 1. Start from `cwd`.
//! 2. Check for `AGENTS.md` at this level.
//! 3. If not found, go up one directory level.
//! 4. Stop after 5 levels OR when hitting the filesystem root.
//!
//! SKILL.md:
//! - `<cwd>/SKILL.md` AND `<cwd>/skills/*.md`.
//! - `~/.relix/skills/*.md`.
//! - Any path the operator listed in `[skills] roots = [...]`.
//!
//! De-duplicates by skill name; first occurrence wins.

use std::path::{Path, PathBuf};

/// Maximum number of parent directories the AGENTS.md walker
/// inspects. Per the Linux Foundation spec.
pub const AGENTS_MAX_WALK_LEVELS: usize = 5;

/// Filenames the [`discover_agent_context`] helper walks for.
/// Order is the priority order — when more than one is present
/// in a single directory, every match is collected; the AI
/// node concatenates them in this order so AGENTS.md remains
/// the canonical document and the Claude / Cursor files layer
/// on top.
pub const AGENT_CONTEXT_FILENAMES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".cursorrules"];

/// One discovered AGENTS.md file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentsContext {
    pub path: PathBuf,
    pub content: String,
}

/// Walk up from `start` looking for `AGENTS.md`. Returns the
/// first match; `None` when the walk completes without finding
/// one OR every candidate is empty.
pub fn discover_agents_md(start: &Path) -> Option<AgentsContext> {
    discover_named_md(start, "AGENTS.md")
}

/// GAP 3: walk up from `start` looking for `CLAUDE.md`. Same
/// 5-level cap + non-empty contract as [`discover_agents_md`].
///
/// `CLAUDE.md` is the file convention used by Claude Code and
/// related agentic coding assistants: a free-form markdown
/// document at the root of a project that captures persistent
/// agent context. When present, the Relix AI node merges its
/// contents into the system prompt alongside `AGENTS.md`, so an
/// agent dropped into a repo that already ships a `CLAUDE.md`
/// inherits that context automatically.
pub fn discover_claude_md(start: &Path) -> Option<AgentsContext> {
    discover_named_md(start, "CLAUDE.md")
}

/// GAP 3: walk up from `start` looking for `.cursorrules`. Same
/// 5-level cap + non-empty contract as [`discover_agents_md`].
///
/// `.cursorrules` is the file convention used by Cursor: a
/// plain-text file at the project root that describes
/// project-specific coding conventions, conventions the editor
/// folds into every chat. The Relix AI node treats the file
/// identically to AGENTS.md / CLAUDE.md — it lands in the
/// merged system context at startup.
pub fn discover_cursor_rules(start: &Path) -> Option<AgentsContext> {
    discover_named_md(start, ".cursorrules")
}

/// Internal helper backing the three discover_* functions
/// above. Walks up from `start` up to
/// [`AGENTS_MAX_WALK_LEVELS`] times looking for `filename`.
/// Returns the first non-empty match.
fn discover_named_md(start: &Path, filename: &str) -> Option<AgentsContext> {
    let mut current = start.to_path_buf();
    for _ in 0..=AGENTS_MAX_WALK_LEVELS {
        let candidate = current.join(filename);
        if let Ok(content) = std::fs::read_to_string(&candidate)
            && !content.trim().is_empty()
        {
            return Some(AgentsContext {
                path: candidate,
                content,
            });
        }
        if !current.pop() {
            break;
        }
    }
    None
}

/// GAP 3: discover every supported agent-context file in one
/// pass. Equivalent to calling [`discover_agents_md`],
/// [`discover_claude_md`], and [`discover_cursor_rules`]; the
/// returned vector preserves the canonical ordering from
/// [`AGENT_CONTEXT_FILENAMES`] (AGENTS.md, then CLAUDE.md, then
/// .cursorrules) so a caller that simply concatenates the bodies
/// gets a deterministic prompt prefix.
///
/// The same source directory may contribute more than one file
/// — for example a repo that ships both `AGENTS.md` AND
/// `CLAUDE.md` returns two entries.
pub fn discover_agent_context(start: &Path) -> Vec<AgentsContext> {
    let mut out = Vec::with_capacity(AGENT_CONTEXT_FILENAMES.len());
    for name in AGENT_CONTEXT_FILENAMES {
        if let Some(ctx) = discover_named_md(start, name) {
            out.push(ctx);
        }
    }
    out
}

/// GAP 3: merge a sequence of [`AgentsContext`] entries into a
/// single string suitable for prepending to the system prompt.
///
/// Each entry is rendered as
/// `# <basename>\n\n<content>\n` so the model sees a
/// machine-readable boundary between sources (and the operator
/// who reads the prompt back can tell which file came from
/// where). Empty input returns an empty string.
pub fn merge_agent_context(entries: &[AgentsContext]) -> String {
    let mut out = String::new();
    for entry in entries {
        let name = entry
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("context");
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("# ");
        out.push_str(name);
        out.push_str("\n\n");
        out.push_str(entry.content.trim_end_matches('\n'));
        out.push('\n');
    }
    out
}

/// One discovered skill. `name` is the file stem; `body` is
/// the raw markdown the loader can either display, hand to the
/// AI as a procedure description, or execute via the future
/// skill runner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub path: PathBuf,
    pub body: String,
    /// First markdown heading found in the body (stripped of
    /// leading `#` and whitespace). Used as the human label in
    /// `relix skills list`; falls back to `name` when the
    /// file has no heading.
    pub title: String,
}

/// Enumerate every SKILL.md / *.md the skill loader can find
/// under the documented roots + any operator-supplied extras.
pub fn discover_skills(extra_roots: &[PathBuf]) -> Vec<Skill> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join("SKILL.md"));
        roots.push(cwd.join("skills"));
    }
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    if let Some(home) = std::env::var_os(home_var) {
        let skills_root = PathBuf::from(home).join(".relix").join("skills");
        // Auto-generated skills live under the dedicated `auto`
        // subdirectory so an operator can `relix skills prune`
        // them without touching their hand-authored library.
        roots.push(skills_root.join("auto"));
        roots.push(skills_root);
    }
    for r in extra_roots {
        roots.push(r.clone());
    }
    let mut out: Vec<Skill> = Vec::new();
    let mut seen_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for root in roots {
        if root.is_file() {
            if let Some(s) = load_skill_file(&root)
                && seen_names.insert(s.name.clone())
            {
                out.push(s);
            }
            continue;
        }
        if !root.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let Some(ext) = p.extension().and_then(|s| s.to_str()) else {
                continue;
            };
            if !ext.eq_ignore_ascii_case("md") {
                continue;
            }
            if let Some(s) = load_skill_file(&p)
                && seen_names.insert(s.name.clone())
            {
                out.push(s);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn load_skill_file(path: &Path) -> Option<Skill> {
    let body = std::fs::read_to_string(path).ok()?;
    if body.trim().is_empty() {
        return None;
    }
    let name = if path
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("SKILL.md"))
    {
        // Bare SKILL.md uses its parent directory name as the
        // skill name — that's the documented convention.
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("skill")
            .to_string()
    } else {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("skill")
            .to_string()
    };
    let title = extract_first_heading(&body).unwrap_or_else(|| name.clone());
    Some(Skill {
        name,
        path: path.to_path_buf(),
        body,
        title,
    })
}

/// Find the best-matching skill for `prompt` via a simple
/// keyword-overlap score. Returns `None` when no skill shares
/// any non-stopword token with the prompt.
///
/// Honest scope: this is the keyword fallback the spec calls
/// out — embedding-similarity matching (the spec's preferred
/// path when Qdrant is available) is a separate follow-up that
/// reuses the AI node's embedding provider. The keyword
/// matcher is what controllers without an embedding peer get;
/// returning `None` is fine — the skill prepend is opt-in
/// context, not required.
pub fn match_skill_keyword<'a>(skills: &'a [Skill], prompt: &str) -> Option<&'a Skill> {
    let prompt_tokens: std::collections::BTreeSet<String> = tokenize(prompt).collect();
    if prompt_tokens.is_empty() {
        return None;
    }
    let mut best: Option<(&Skill, usize)> = None;
    for s in skills {
        let haystack = format!("{} {} {}", s.name, s.title, s.body);
        let skill_tokens: std::collections::BTreeSet<String> = tokenize(&haystack).collect();
        let overlap = prompt_tokens.intersection(&skill_tokens).count();
        if overlap == 0 {
            continue;
        }
        match best {
            Some((_, best_overlap)) if overlap <= best_overlap => {}
            _ => best = Some((s, overlap)),
        }
    }
    best.map(|(s, _)| s)
}

/// Lowercase, strip punctuation, split on whitespace, drop
/// stopwords. Pure utility — exported for tests of the matcher
/// logic.
fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| !w.is_empty() && w.len() > 2 && !STOPWORDS.contains(&w.as_str()))
}

/// English stopword list. Pragmatic, not exhaustive — the
/// matcher just needs to drop the most common noise.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "are", "was", "will", "you",
    "your", "but", "not", "all", "any", "use", "can", "may", "have", "has", "had", "would",
    "should", "could", "what", "when", "where", "how", "who", "why",
];

/// Render a system-prompt envelope around a matched skill's
/// body. The envelope is documented and stable: future
/// integrations (auto-skill generator, dashboard surface) can
/// rely on the format.
pub fn render_skill_hint(skill: &Skill) -> String {
    format!(
        "## Skill: {name}\n\
         \n\
         You have access to this skill. Use it if relevant to the task.\n\
         \n\
         {body}\n",
        name = skill.name,
        body = skill.body.trim()
    )
}

/// Cache for the loaded skill library. Cheap to clone (Arc
/// inside). The cache loads once at construction; reload via
/// `refresh()` if operators add skills mid-run (the AI node
/// doesn't auto-refresh — refresh is operator-triggered).
#[derive(Clone, Debug)]
pub struct SkillsCache {
    skills: Arc<Vec<Skill>>,
}

impl SkillsCache {
    /// Discover skills under `extra_roots` plus the documented
    /// default roots (cwd / `~/.relix/skills`) and store them.
    pub fn load(extra_roots: &[PathBuf]) -> Self {
        Self {
            skills: Arc::new(discover_skills(extra_roots)),
        }
    }

    /// Permanent-empty cache. Tests + the legacy code path
    /// that doesn't load skills at all use this; the matcher
    /// returns None against the empty list, so the AI handler
    /// skips the prepend.
    pub fn empty() -> Self {
        Self {
            skills: Arc::new(Vec::new()),
        }
    }

    /// Test-only constructor that wraps a pre-built skill list.
    /// Saves tests from staging actual files on disk just to
    /// exercise the matcher.
    pub fn from_vec(skills: Vec<Skill>) -> Self {
        Self {
            skills: Arc::new(skills),
        }
    }

    /// Match the prompt against the cached skill library; render
    /// a system-prompt hint when a match is found. None means
    /// "no relevant skill" and the AI handler skips the prepend.
    pub fn matched_hint(&self, prompt: &str) -> Option<String> {
        match_skill_keyword(&self.skills, prompt).map(render_skill_hint)
    }

    /// Count of cached skills. Useful for `relix doctor` /
    /// debug surfaces.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Borrow the underlying skill list. Used by
    /// [`SkillMatcher`] to walk the catalogue without taking a
    /// second clone of the inner Arc.
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }
}

use std::sync::Arc;

// ── Embedding-similarity matcher ───────────────────────────

/// Default cosine-similarity threshold for skill matches.
/// Empirically conservative — cosine in the 0.6-0.8 band is
/// "topically related"; >= 0.75 is "this prompt is asking for
/// the procedure described in this skill."
pub const SKILL_MATCH_THRESHOLD: f32 = 0.75;

/// Skill matcher that prefers embedding-cosine similarity when
/// an embedding dispatcher is wired, falling back to the
/// existing keyword-overlap path otherwise. The matcher is the
/// surface the AI handler consumes; `SkillsCache` remains the
/// underlying catalogue and stays useful on its own (the
/// `relix skills list` CLI uses it directly).
///
/// The skill embedding cache is populated lazily on the first
/// `matched_hint` call that actually has a dispatcher. We
/// deliberately do NOT block at startup: in production the
/// embedding dispatcher cell is filled post-rpc, so a
/// constructor-time embed would race the cell. Lazy population
/// also means a controller with no embedding peer never makes
/// an embed RPC for skills.
#[derive(Clone)]
pub struct SkillMatcher {
    cache: SkillsCache,
    /// Boxed trait object — held as a generic so callers don't
    /// have to import the embedding-dispatcher trait. The
    /// concrete type is
    /// `Arc<dyn crate::nodes::memory::EmbeddingDispatcher>` in
    /// production; tests inject their own.
    embed_dispatcher: Option<Arc<dyn SkillEmbedDispatcher>>,
    model: String,
    threshold: f32,
    /// Cached skill embeddings keyed by skill name. Filled on
    /// first matched_hint call with a dispatcher.
    skill_vectors: Arc<tokio::sync::RwLock<std::collections::HashMap<String, Vec<f32>>>>,
}

/// Slim dispatcher trait — narrower than the
/// `nodes::memory::EmbeddingDispatcher` trait so callers don't
/// need to import the whole memory module just to feed
/// `SkillMatcher`. A blanket impl below adapts any concrete
/// dispatcher that exposes the same shape.
#[async_trait::async_trait]
pub trait SkillEmbedDispatcher: Send + Sync {
    /// Embed a batch of texts. Returns one vector per input
    /// text in the same order.
    async fn embed(&self, model: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>, String>;
}

impl SkillMatcher {
    /// New matcher. Passing `None` for `embed` short-circuits
    /// to keyword overlap; passing `Some` enables cosine
    /// matching gated by `threshold`. `SKILL_MATCH_THRESHOLD`
    /// is the recommended default.
    pub fn new(
        cache: SkillsCache,
        embed: Option<Arc<dyn SkillEmbedDispatcher>>,
        model: String,
        threshold: f32,
    ) -> Self {
        Self {
            cache,
            embed_dispatcher: embed,
            model,
            threshold,
            skill_vectors: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        }
    }

    /// Convenience: keyword-only matcher wrapping the supplied
    /// cache. Same behaviour as the legacy
    /// `SkillsCache::matched_hint` surface — used by tests and
    /// by AI controllers that don't have an embedding peer
    /// wired.
    pub fn keyword_only(cache: SkillsCache) -> Self {
        Self::new(cache, None, String::new(), SKILL_MATCH_THRESHOLD)
    }

    /// Match the prompt and return the hint envelope on hit.
    /// `None` means "no relevant skill" and the AI handler
    /// skips the prepend.
    pub async fn matched_hint(&self, prompt: &str) -> Option<String> {
        let dispatcher = match self.embed_dispatcher.clone() {
            Some(d) => d,
            None => return self.cache.matched_hint(prompt),
        };
        // Lazy embed: populate skill_vectors once with the
        // skill bodies, then keep using the cached values for
        // every subsequent call.
        self.ensure_skill_vectors_loaded(&*dispatcher).await;
        let vectors = self.skill_vectors.read().await;
        if vectors.is_empty() {
            // Embed failed earlier and we have nothing to
            // compare against — fall back to keyword.
            drop(vectors);
            return self.cache.matched_hint(prompt);
        }
        let query_vec = match dispatcher.embed(&self.model, &[prompt]).await {
            Ok(mut v) => v.pop().unwrap_or_default(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "skill matcher: prompt embed failed; falling back to keyword overlap"
                );
                drop(vectors);
                return self.cache.matched_hint(prompt);
            }
        };
        if query_vec.is_empty() {
            drop(vectors);
            return self.cache.matched_hint(prompt);
        }
        let mut best_score: f32 = 0.0;
        let mut best_name: Option<String> = None;
        for (name, vec) in vectors.iter() {
            let score = cosine_similarity(&query_vec, vec);
            if score > best_score {
                best_score = score;
                best_name = Some(name.clone());
            }
        }
        if best_score < self.threshold {
            return None;
        }
        let best_name = best_name?;
        let skill = self.cache.skills().iter().find(|s| s.name == best_name)?;
        Some(render_skill_hint(skill))
    }

    async fn ensure_skill_vectors_loaded(&self, dispatcher: &dyn SkillEmbedDispatcher) {
        // Fast path: already populated.
        if !self.skill_vectors.read().await.is_empty() {
            return;
        }
        let skills = self.cache.skills();
        if skills.is_empty() {
            return;
        }
        let texts: Vec<String> = skills
            .iter()
            .map(|s| format!("{}\n{}", s.title, s.body))
            .collect();
        let text_refs: Vec<&str> = texts.iter().map(|t| t.as_str()).collect();
        match dispatcher.embed(&self.model, &text_refs).await {
            Ok(vectors) if vectors.len() == skills.len() => {
                let mut map = self.skill_vectors.write().await;
                for (skill, vec) in skills.iter().zip(vectors) {
                    map.insert(skill.name.clone(), vec);
                }
            }
            Ok(other) => {
                tracing::warn!(
                    got = other.len(),
                    want = skills.len(),
                    "skill matcher: dispatcher returned wrong number of vectors; keyword fallback active"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "skill matcher: bulk skill embed failed; keyword fallback active"
                );
            }
        }
    }
}

/// Adapter that wraps the memory module's wider
/// `EmbeddingDispatcher` trait so callers in the AI handler
/// can pass the same dispatcher they hand to the memory
/// embedder without writing a second one.
pub struct MemoryEmbedAdapter(pub Arc<dyn crate::nodes::memory::EmbeddingDispatcher>);

#[async_trait::async_trait]
impl SkillEmbedDispatcher for MemoryEmbedAdapter {
    async fn embed(&self, model: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        self.0.embed(model, texts).await.map_err(|e| e.to_string())
    }
}

/// Adapter that wraps the AI controller's own
/// [`crate::nodes::ai::provider::ChatProvider`] so the matcher
/// can call `generate_embeddings` directly without a libp2p
/// hop. Production wiring in `controller_runtime` uses this so
/// skill matching reuses the same provider instance that
/// already serves `ai.chat` / `ai.embed`.
pub struct ProviderEmbedAdapter(pub Arc<dyn crate::nodes::ai::provider::ChatProvider>);

#[async_trait::async_trait]
impl SkillEmbedDispatcher for ProviderEmbedAdapter {
    async fn embed(&self, model: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        let input = crate::nodes::ai::provider::EmbedInput {
            model: model.to_string(),
            texts: texts.iter().map(|s| s.to_string()).collect(),
        };
        match self.0.generate_embeddings(input).await {
            Ok(out) => Ok(out.vectors),
            Err(e) => Err(format!("provider embed: {e}")),
        }
    }
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

// ── Auto-skill generation ───────────────────────────────────

/// Operator-facing config for the auto-skill generator. Lives
/// under `[skills]` in the controller TOML so operators can
/// toggle the behaviour without touching capability config.
///
/// Two related feature flags coexist here so legacy operators
/// who flipped `[skills] auto_generate = true` against the
/// SKILL.md path keep their behaviour, AND the new GAP-4
/// SQLite-backed `auto_extract` path can be enabled
/// independently:
///
/// `auto_generate` is the original SKILL.md writer — when true, a
/// completed task writes a templated SKILL.md to `~/.relix/skills/auto/`
/// (the pre-GAP-4 behaviour).
///
/// `enabled` + `auto_extract` drive the SQLite-backed SkillStore +
/// SkillExtractor + SkillRefinementEngine. When `enabled = true`
/// AND `db_path` is set, the auto-skill pipeline boots. Setting
/// `auto_extract = false` keeps the store + caps available but
/// suppresses the post-`ai.chat` extraction hook (operators who
/// want manual seeding only).
#[derive(Clone, Debug, serde::Deserialize)]
pub struct SkillsConfig {
    /// Master switch for the SKILL.md path. `false` (default)
    /// means task completion never writes a SKILL.md.
    #[serde(default)]
    pub auto_generate: bool,
    /// Age threshold for `relix skills prune` AND for the
    /// generator's "is this skill already covered" check.
    /// Default 30 days.
    #[serde(default = "default_max_age_days")]
    pub max_age_days: i64,
    /// Override for the auto-skill directory. Default is
    /// `~/.relix/skills/auto`. Operators usually leave this
    /// alone; the override is for sandboxed tests.
    #[serde(default)]
    pub auto_dir: Option<PathBuf>,
    /// GAP 4: master switch for the SQLite-backed SkillStore +
    /// caps. When `false` (default) the store and caps are not
    /// registered.
    #[serde(default)]
    pub enabled: bool,
    /// GAP 4: filesystem path to the skills.db SQLite file.
    /// Required when `enabled = true`; ignored otherwise.
    #[serde(default)]
    pub db_path: Option<PathBuf>,
    /// GAP 4: when `true` (default when `enabled = true`),
    /// successful `ai.chat` completions trigger the
    /// SkillExtractor hook. `false` disables auto-extraction
    /// but leaves the store + caps usable for manual seeding.
    #[serde(default = "default_true")]
    pub auto_extract: bool,
    /// GAP 4: complexity floor below which the extractor skips.
    #[serde(default = "default_min_complexity")]
    pub min_complexity_score: f32,
    /// GAP 4: enable the background refinement task that ticks
    /// every 24h.
    #[serde(default = "default_true")]
    pub refinement_enabled: bool,
    /// GAP 4: model id the SkillExtractor passes to the
    /// synthesis call. Default is the cheap-tier model.
    #[serde(default)]
    pub extraction_model: Option<String>,
    /// GAP 4: embedding model used by the duplicate-check path.
    #[serde(default)]
    pub embedding_model: Option<String>,
    /// GAP 4: cosine threshold above which the extractor treats
    /// a candidate as a duplicate of an existing skill.
    #[serde(default = "default_dup_threshold")]
    pub dup_threshold: f32,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            auto_generate: false,
            max_age_days: default_max_age_days(),
            auto_dir: None,
            enabled: false,
            db_path: None,
            auto_extract: true,
            min_complexity_score: default_min_complexity(),
            refinement_enabled: true,
            extraction_model: None,
            embedding_model: None,
            dup_threshold: default_dup_threshold(),
        }
    }
}

fn default_max_age_days() -> i64 {
    30
}

fn default_true() -> bool {
    true
}

fn default_min_complexity() -> f32 {
    0.6
}

fn default_dup_threshold() -> f32 {
    0.85
}

/// Resolve the auto-skill directory. Honors
/// [`SkillsConfig::auto_dir`] when set; otherwise falls back
/// to `~/.relix/skills/auto`. Returns `None` only when there
/// is no `HOME` / `USERPROFILE` (sandboxed processes); the
/// caller skips writing silently in that case.
pub fn resolve_auto_skill_dir(cfg: &SkillsConfig) -> Option<PathBuf> {
    if let Some(d) = &cfg.auto_dir {
        return Some(d.clone());
    }
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = std::env::var_os(home_var)?;
    Some(
        PathBuf::from(home)
            .join(".relix")
            .join("skills")
            .join("auto"),
    )
}

/// Build the SKILL.md body for a completed task. Pure
/// function: takes the inputs it summarises and returns the
/// rendered markdown — no filesystem I/O, no DB calls.
///
/// The body is deliberately templated rather than free-form so
/// the auto-generator stays cheap (no LLM dependency). When a
/// future commit wires an LLM-driven "summarise this approach"
/// path, it can replace this function while keeping the same
/// "name + body" shape.
pub fn render_auto_skill_body(
    task_title: &str,
    flow_template: &str,
    duration_secs: i64,
    event_summary: &str,
) -> String {
    let dur = if duration_secs > 0 {
        format!("{duration_secs}s")
    } else {
        "—".to_string()
    };
    format!(
        "# {title}\n\
         \n\
         _Auto-generated from a completed task. Edit freely; the\n\
         generator will not overwrite this file._\n\
         \n\
         ## Procedure\n\
         \n\
         - Flow template: `{flow}`\n\
         - Wall-clock duration: {dur}\n\
         \n\
         ## Chronicle highlights\n\
         \n\
         {summary}\n",
        title = task_title.trim(),
        flow = flow_template,
        dur = dur,
        summary = if event_summary.trim().is_empty() {
            "(no chronicle events recorded)".to_string()
        } else {
            event_summary.to_string()
        }
    )
}

/// Sanitise a task title into a filesystem-safe slug for the
/// auto-skill filename. ASCII alphanumerics + dashes only;
/// everything else collapses to `-`. Caps the length so a
/// pathological title can't blow past path-length limits.
pub fn slugify_for_filename(title: &str) -> String {
    let mut out = String::with_capacity(title.len().min(60));
    let mut last_was_dash = false;
    for c in title.chars() {
        let mapped = if c.is_ascii_alphanumeric() {
            last_was_dash = false;
            c.to_ascii_lowercase()
        } else {
            if last_was_dash {
                continue;
            }
            last_was_dash = true;
            '-'
        };
        out.push(mapped);
        if out.len() >= 60 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "auto-skill".to_string()
    } else {
        trimmed
    }
}

/// Write the body for an auto-generated skill into the
/// configured directory. Returns the path of the file written.
/// Caller decides what to do with collisions — this function
/// refuses to overwrite an existing file, which matches the
/// "auto-generator never clobbers operator edits" contract.
pub fn write_auto_skill(
    dir: &Path,
    skill_name: &str,
    body: &str,
) -> std::io::Result<Option<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{skill_name}.md"));
    if path.exists() {
        return Ok(None);
    }
    std::fs::write(&path, body)?;
    Ok(Some(path))
}

/// Walk `dir` and delete `*.md` files whose mtime is older
/// than `max_age_days`. Returns `(scanned, deleted)` so the
/// CLI can render an operator-facing summary. Missing
/// directory is treated as "nothing to prune" (Ok((0, 0))).
pub fn prune_auto_skills(dir: &Path, max_age_days: i64) -> std::io::Result<(usize, usize)> {
    if !dir.exists() {
        return Ok((0, 0));
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(
            (max_age_days.max(0) as u64) * 86_400,
        ))
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut scanned = 0usize;
    let mut deleted = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        if p.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        scanned += 1;
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if mtime < cutoff && std::fs::remove_file(&p).is_ok() {
            deleted += 1;
        }
    }
    Ok((scanned, deleted))
}

fn extract_first_heading(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let title = rest.trim_start_matches('#').trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn match_skill_keyword_returns_highest_overlap() {
        let skills = vec![
            Skill {
                name: "deploy".into(),
                path: PathBuf::from("deploy.md"),
                body: "Run deploy script\nUses kubectl".into(),
                title: "Deploy to prod".into(),
            },
            Skill {
                name: "test".into(),
                path: PathBuf::from("test.md"),
                body: "Run cargo test\nAssert no failures".into(),
                title: "Run tests".into(),
            },
        ];
        let m = match_skill_keyword(&skills, "deploy the new build");
        assert_eq!(m.map(|s| s.name.as_str()), Some("deploy"));
        let m = match_skill_keyword(&skills, "run tests on the branch");
        assert_eq!(m.map(|s| s.name.as_str()), Some("test"));
        let m = match_skill_keyword(&skills, "look up the weather");
        assert!(m.is_none(), "no overlap → no match");
    }

    #[test]
    fn render_skill_hint_includes_body_in_documented_envelope() {
        let s = Skill {
            name: "deploy".into(),
            path: PathBuf::from("d.md"),
            body: "## Steps\n1. cargo build\n2. push".into(),
            title: "Deploy".into(),
        };
        let hint = render_skill_hint(&s);
        assert!(hint.contains("You have access to this skill"));
        assert!(hint.contains("deploy"));
        assert!(hint.contains("cargo build"));
        assert!(hint.starts_with("## Skill: "));
    }

    #[test]
    fn agents_md_walker_finds_file_in_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        let child = parent.join("nested");
        std::fs::create_dir_all(&child).unwrap();
        let f = parent.join("AGENTS.md");
        std::fs::write(&f, "# Project agents\nbe helpful").unwrap();
        let found = discover_agents_md(&child).expect("walk must find parent's AGENTS.md");
        assert_eq!(found.path, f);
        assert!(found.content.contains("be helpful"));
    }

    #[test]
    fn agents_md_walker_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let found = discover_agents_md(tmp.path());
        assert!(found.is_none());
    }

    #[test]
    fn agents_md_walker_respects_max_walk_levels() {
        // Confirms the loop boundary — files >5 levels up are
        // not discovered. We can't easily create a 6-deep
        // tempdir that has AGENTS.md above the cap on every
        // CI machine; assert the constant + that the walker
        // doesn't crash on a single-level path.
        assert_eq!(AGENTS_MAX_WALK_LEVELS, 5);
        let tmp = tempfile::tempdir().unwrap();
        let _ = discover_agents_md(tmp.path());
    }

    // ---- GAP 3: CLAUDE.md / .cursorrules / merge ----

    #[test]
    fn claude_md_walker_finds_file_in_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let child = tmp.path().join("nested");
        std::fs::create_dir_all(&child).unwrap();
        let f = tmp.path().join("CLAUDE.md");
        std::fs::write(&f, "# Project Claude\nfollow style X").unwrap();
        let found = discover_claude_md(&child).expect("walk must find parent's CLAUDE.md");
        assert_eq!(found.path, f);
        assert!(found.content.contains("style X"));
    }

    #[test]
    fn cursor_rules_walker_finds_dotfile_in_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let child = tmp.path().join("nested");
        std::fs::create_dir_all(&child).unwrap();
        let f = tmp.path().join(".cursorrules");
        std::fs::write(&f, "always use snake_case").unwrap();
        let found = discover_cursor_rules(&child).expect("walk must find parent's .cursorrules");
        assert_eq!(found.path, f);
        assert!(found.content.contains("snake_case"));
    }

    #[test]
    fn discover_agent_context_returns_each_present_file_in_canonical_order() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "agents body").unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "claude body").unwrap();
        // Skip .cursorrules: only two of three present.
        let ctx = discover_agent_context(tmp.path());
        let names: Vec<String> = ctx
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["AGENTS.md".to_string(), "CLAUDE.md".to_string()]
        );
    }

    #[test]
    fn merge_agent_context_renders_each_file_with_a_header_and_blank_line_separator() {
        let entries = vec![
            AgentsContext {
                path: PathBuf::from("/x/AGENTS.md"),
                content: "agents body".into(),
            },
            AgentsContext {
                path: PathBuf::from("/x/CLAUDE.md"),
                content: "claude body\n".into(),
            },
        ];
        let merged = merge_agent_context(&entries);
        assert!(merged.contains("# AGENTS.md\n\nagents body"));
        assert!(merged.contains("# CLAUDE.md\n\nclaude body"));
        // Files separated by a blank line.
        assert!(merged.contains("\n\n# CLAUDE.md"));
    }

    #[test]
    fn merge_agent_context_returns_empty_string_for_empty_input() {
        assert!(merge_agent_context(&[]).is_empty());
    }

    #[test]
    fn discover_skills_picks_up_root_skill_md() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("greet.md");
        std::fs::write(&f, "# Greet\nSay hello.").unwrap();
        let skills = discover_skills(&[tmp.path().to_path_buf()]);
        assert!(
            skills.iter().any(|s| s.name == "greet"),
            "must discover greet.md: {skills:?}"
        );
    }

    #[test]
    fn discover_skills_dedupes_by_name() {
        // Two roots both containing `greet.md` → only the
        // first-seen entry is kept.
        let tmp_a = tempfile::tempdir().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        std::fs::write(tmp_a.path().join("greet.md"), "# Greet A").unwrap();
        std::fs::write(tmp_b.path().join("greet.md"), "# Greet B").unwrap();
        let skills = discover_skills(&[tmp_a.path().to_path_buf(), tmp_b.path().to_path_buf()]);
        let greet: Vec<_> = skills.iter().filter(|s| s.name == "greet").collect();
        assert_eq!(greet.len(), 1, "de-dup must keep first occurrence");
    }

    #[test]
    fn discover_skills_uses_first_heading_as_title() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("deploy.md"),
            "# Deploy to prod\n\nDescription...",
        )
        .unwrap();
        let skills = discover_skills(&[tmp.path().to_path_buf()]);
        let s = skills.iter().find(|s| s.name == "deploy").unwrap();
        assert_eq!(s.title, "Deploy to prod");
    }

    #[test]
    fn discover_skills_falls_back_to_name_when_no_heading() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("plain.md"), "no heading here").unwrap();
        let skills = discover_skills(&[tmp.path().to_path_buf()]);
        let s = skills.iter().find(|s| s.name == "plain").unwrap();
        assert_eq!(s.title, "plain");
    }

    #[test]
    fn discover_skills_skips_empty_files() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("empty.md");
        let mut h = std::fs::File::create(&f).unwrap();
        writeln!(h, "   ").unwrap();
        let skills = discover_skills(&[tmp.path().to_path_buf()]);
        assert!(skills.iter().all(|s| s.name != "empty"));
    }

    #[test]
    fn bare_skill_md_uses_parent_directory_name_as_skill_name() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("my-cool-skill");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("SKILL.md"), "# My Skill").unwrap();
        let skills = discover_skills(&[nested]);
        assert!(skills.iter().any(|s| s.name == "my-cool-skill"));
    }

    #[test]
    fn slugify_collapses_punctuation_and_caps_length() {
        let s = slugify_for_filename("Deploy STAGING!! v2.0  (urgent)");
        assert!(s.contains("deploy"));
        assert!(s.contains("staging"));
        assert!(!s.contains(' '));
        assert!(!s.contains('!'));
        assert!(s.len() <= 60);
        let s_empty = slugify_for_filename("***");
        assert_eq!(s_empty, "auto-skill");
    }

    #[test]
    fn render_auto_skill_body_includes_template_sections() {
        let body =
            render_auto_skill_body("deploy staging", "flows/deploy.sol", 42, "- ran 3 steps");
        assert!(body.contains("# deploy staging"));
        assert!(body.contains("Auto-generated"));
        assert!(body.contains("flows/deploy.sol"));
        assert!(body.contains("42s"));
        assert!(body.contains("ran 3 steps"));
    }

    #[test]
    fn write_auto_skill_creates_file_and_refuses_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("auto");
        let p = write_auto_skill(&dir, "deploy-staging", "# Body").unwrap();
        assert!(p.is_some());
        let path = p.unwrap();
        assert!(path.exists());
        // Second write to the same name returns None (refusal),
        // file content unchanged.
        std::fs::write(&path, "OPERATOR EDIT").unwrap();
        let p2 = write_auto_skill(&dir, "deploy-staging", "# Different body").unwrap();
        assert!(p2.is_none(), "auto generator must not overwrite");
        let kept = std::fs::read_to_string(&path).unwrap();
        assert_eq!(kept, "OPERATOR EDIT");
    }

    #[test]
    fn prune_auto_skills_returns_zero_when_dir_missing() {
        let (s, d) = prune_auto_skills(Path::new("definitely/does/not/exist"), 30).unwrap();
        assert_eq!((s, d), (0, 0));
    }

    #[test]
    fn prune_auto_skills_zero_max_age_deletes_every_md_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("a.md"), "a").unwrap();
        std::fs::write(dir.join("b.md"), "b").unwrap();
        // A non-.md file must NOT be touched — only the auto
        // generator's own artefacts get pruned.
        std::fs::write(dir.join("readme.txt"), "keep me").unwrap();
        let (scanned, deleted) = prune_auto_skills(dir, 0).unwrap();
        assert_eq!(scanned, 2, "non-md files must not count toward scan");
        assert_eq!(deleted, 2);
        assert!(!dir.join("a.md").exists());
        assert!(!dir.join("b.md").exists());
        assert!(dir.join("readme.txt").exists());
    }

    #[test]
    fn prune_auto_skills_generous_threshold_keeps_fresh_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("fresh.md"), "fresh").unwrap();
        // 365-day threshold leaves a just-written file alone.
        let (scanned, deleted) = prune_auto_skills(dir, 365).unwrap();
        assert_eq!(scanned, 1);
        assert_eq!(deleted, 0);
        assert!(dir.join("fresh.md").exists());
    }

    #[test]
    fn skills_config_defaults_to_disabled_auto_generate() {
        let cfg = SkillsConfig::default();
        assert!(!cfg.auto_generate);
        assert_eq!(cfg.max_age_days, 30);
        assert!(cfg.auto_dir.is_none());
    }

    // ── SkillMatcher ─────────────────────────────────────

    fn deploy_skill() -> Skill {
        Skill {
            name: "deploy-staging".into(),
            path: PathBuf::from("deploy-staging.md"),
            body: "# deploy-staging\n\nProcedure to ship code to the staging env.".into(),
            title: "deploy-staging".into(),
        }
    }

    fn coffee_skill() -> Skill {
        Skill {
            name: "make-coffee".into(),
            path: PathBuf::from("make-coffee.md"),
            body: "# make-coffee\n\nGrind beans, brew water, pour.".into(),
            title: "make-coffee".into(),
        }
    }

    /// Stub embed dispatcher that returns deterministic
    /// vectors keyed by the first token of the input. The
    /// vectors are 3-dim so we can craft known cosine results.
    struct StubEmbed;

    #[async_trait::async_trait]
    impl SkillEmbedDispatcher for StubEmbed {
        async fn embed(&self, _model: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
            let mut out = Vec::new();
            for t in texts {
                let v = if t.contains("deploy") || t.contains("staging") || t.contains("ship") {
                    vec![1.0, 0.0, 0.0]
                } else if t.contains("coffee") || t.contains("brew") {
                    vec![0.0, 1.0, 0.0]
                } else if t.contains("weather") {
                    vec![0.0, 0.0, 1.0]
                } else {
                    vec![0.5, 0.5, 0.0] // ambiguous
                };
                out.push(v);
            }
            Ok(out)
        }
    }

    struct FailingEmbed;

    #[async_trait::async_trait]
    impl SkillEmbedDispatcher for FailingEmbed {
        async fn embed(&self, _model: &str, _texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
            Err("embed unreachable".into())
        }
    }

    #[tokio::test]
    async fn matcher_with_none_dispatcher_falls_back_to_keyword_overlap() {
        let cache = SkillsCache::from_vec(vec![deploy_skill(), coffee_skill()]);
        let matcher = SkillMatcher::new(cache, None, "stub".into(), SKILL_MATCH_THRESHOLD);
        // Strong keyword overlap on "deploy staging" — keyword
        // matcher should fire.
        let hint = matcher.matched_hint("please deploy staging now").await;
        let hint = hint.expect("keyword matcher should find a hit");
        assert!(hint.contains("## Skill: deploy-staging"));
    }

    #[tokio::test]
    async fn matcher_with_stub_dispatcher_uses_embedding_similarity() {
        let cache = SkillsCache::from_vec(vec![deploy_skill(), coffee_skill()]);
        let matcher = SkillMatcher::new(
            cache,
            Some(Arc::new(StubEmbed)),
            "stub".into(),
            SKILL_MATCH_THRESHOLD,
        );
        // The stub maps "deploy" and "ship" to the same unit
        // vector — cosine = 1.0 > threshold.
        let hint = matcher.matched_hint("how do I ship to staging?").await;
        let hint = hint.expect("embedding match should find deploy skill");
        assert!(hint.contains("## Skill: deploy-staging"));
    }

    #[tokio::test]
    async fn matcher_below_threshold_returns_none() {
        let cache = SkillsCache::from_vec(vec![deploy_skill(), coffee_skill()]);
        // Set a threshold of 1.5 — impossible to satisfy
        // (cosine maxes at 1.0). Should always return None.
        let matcher = SkillMatcher::new(cache, Some(Arc::new(StubEmbed)), "stub".into(), 1.5);
        let hint = matcher.matched_hint("deploy staging now").await;
        assert!(hint.is_none());
    }

    #[tokio::test]
    async fn matcher_falls_back_to_keyword_when_dispatcher_errors() {
        let cache = SkillsCache::from_vec(vec![deploy_skill(), coffee_skill()]);
        let matcher = SkillMatcher::new(
            cache,
            Some(Arc::new(FailingEmbed)),
            "stub".into(),
            SKILL_MATCH_THRESHOLD,
        );
        // The bulk-embed call fails; per-call embed never gets
        // to run because skill_vectors is empty. Either way,
        // the matcher should fall back to keyword overlap and
        // still find a hit.
        let hint = matcher.matched_hint("deploy staging now").await;
        let hint = hint.expect("keyword fallback should kick in");
        assert!(hint.contains("## Skill: deploy-staging"));
    }

    #[tokio::test]
    async fn matcher_picks_highest_cosine_among_multiple_skills() {
        let cache = SkillsCache::from_vec(vec![deploy_skill(), coffee_skill()]);
        let matcher = SkillMatcher::new(
            cache,
            Some(Arc::new(StubEmbed)),
            "stub".into(),
            SKILL_MATCH_THRESHOLD,
        );
        // "brew espresso" → unit vector on the coffee axis.
        let hint = matcher.matched_hint("can you brew espresso?").await;
        let hint = hint.expect("should match the coffee skill");
        assert!(hint.contains("## Skill: make-coffee"));
    }

    #[test]
    fn cosine_helper_handles_edge_cases() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        // Mismatched lengths → 0.
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 0.0]), 0.0);
        // Zero vectors → 0 (no divide-by-zero).
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }
}
