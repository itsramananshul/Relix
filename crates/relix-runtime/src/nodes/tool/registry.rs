//! Tool registry — discoverable, searchable surface over the
//! tool node's capability set.
//!
//! Agents and operators both need to find the right tool for
//! a task without memorising every capability name. The
//! registry exposes:
//!
//! - **Keyword search** — fast token-overlap match against
//!   the tool's name + description + tags. Uses the same
//!   algorithm as the skill matcher's keyword fallback so
//!   the two surfaces behave consistently.
//! - **Semantic search** — cosine similarity against
//!   pre-embedded tool descriptions. Falls back to keyword
//!   search when no embeddings are loaded (e.g., the
//!   embedding peer isn't wired yet).
//!
//! The registry is *additive*: the existing
//! `CapabilityDescriptor` set published per node stays the
//! source of truth for dispatch. This module is a discovery
//! layer on top.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use relix_core::capability::CapabilityDescriptor;

/// One tool's discoverable metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    pub reversible: bool,
    pub rollback_hint: Option<String>,
    pub tags: Vec<String>,
}

impl ToolDefinition {
    /// Render the searchable corpus for this tool: name +
    /// description + tags joined by whitespace. Used by
    /// both the keyword matcher and the embedder.
    pub fn search_text(&self) -> String {
        let mut s = String::new();
        s.push_str(&self.name);
        s.push(' ');
        s.push_str(&self.description);
        for t in &self.tags {
            s.push(' ');
            s.push_str(t);
        }
        s
    }
}

/// Tool registry. Cheap to clone (one Arc-backed embedding
/// cache + the tool vec).
#[derive(Clone)]
pub struct ToolRegistry {
    tools: Arc<Vec<ToolDefinition>>,
    embeddings: Arc<RwLock<HashMap<String, Vec<f32>>>>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<ToolDefinition>) -> Self {
        Self {
            tools: Arc::new(tools),
            embeddings: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Build from a slice of `CapabilityDescriptor`s. The
    /// descriptor's metadata maps as follows:
    /// `method_name` → `name`, `description` → `description`,
    /// `categories` → `tags`. Reversibility is inferred from
    /// the same mutating-verb heuristic the planner uses.
    pub fn from_capability_descriptors(descriptors: &[CapabilityDescriptor]) -> Self {
        let tools: Vec<ToolDefinition> = descriptors
            .iter()
            .map(|d| ToolDefinition {
                name: d.method_name.clone(),
                description: d
                    .description
                    .clone()
                    .unwrap_or_else(|| d.method_name.clone()),
                input_schema: serde_json::Value::Object(Default::default()),
                output_schema: serde_json::Value::Object(Default::default()),
                reversible: !is_irreversible_name(&d.method_name),
                rollback_hint: None,
                tags: d.categories.clone(),
            })
            .collect();
        Self::new(tools)
    }

    /// Keyword search — same algorithm as the skill matcher's
    /// keyword fallback. Tokens lowercased + stopword-
    /// filtered; tools are ranked by intersection size; the
    /// top `limit` are returned. Zero-overlap tools are
    /// excluded so callers can distinguish "no match" from
    /// "weak match".
    pub fn keyword_search(&self, query: &str, limit: usize) -> Vec<&ToolDefinition> {
        let query_tokens: BTreeSet<String> = tokenize(query).collect();
        if query_tokens.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(&ToolDefinition, usize)> = Vec::new();
        for t in self.tools.iter() {
            let haystack = t.search_text();
            let tool_tokens: BTreeSet<String> = tokenize(&haystack).collect();
            let overlap = query_tokens.intersection(&tool_tokens).count();
            if overlap > 0 {
                scored.push((t, overlap));
            }
        }
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
        scored.into_iter().take(limit).map(|(t, _)| t).collect()
    }

    /// Semantic search via cosine similarity. Requires
    /// embeddings to have been loaded via [`embed_all`].
    /// When the registry has no embeddings (or the query
    /// embedding is empty), falls back to keyword search.
    pub async fn semantic_search(
        &self,
        query: &str,
        query_embedding: Vec<f32>,
        limit: usize,
        threshold: f32,
    ) -> Vec<&ToolDefinition> {
        if query_embedding.is_empty() {
            return self.keyword_search(query, limit);
        }
        let stored = self.embeddings.read().await;
        if stored.is_empty() {
            drop(stored);
            return self.keyword_search(query, limit);
        }
        let mut scored: Vec<(&ToolDefinition, f32)> = Vec::new();
        for t in self.tools.iter() {
            if let Some(vec) = stored.get(&t.name) {
                let score = cosine(&query_embedding, vec);
                if score >= threshold {
                    scored.push((t, score));
                }
            }
        }
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.name.cmp(&b.0.name))
        });
        scored.into_iter().take(limit).map(|(t, _)| t).collect()
    }

    /// Pre-embed every tool's search text. `embed_fn`
    /// returns one vector per input text in the same order.
    /// Vector-count mismatches are logged + the partial set
    /// is discarded; callers can retry on a later tick.
    pub async fn embed_all<F>(&self, embed_fn: F)
    where
        F: Fn(Vec<String>) -> BoxFuture<'static, Result<Vec<Vec<f32>>, String>>,
    {
        let texts: Vec<String> = self.tools.iter().map(|t| t.search_text()).collect();
        if texts.is_empty() {
            return;
        }
        let vectors = match embed_fn(texts).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "tool registry: bulk embed failed; keyword search active");
                return;
            }
        };
        if vectors.len() != self.tools.len() {
            tracing::warn!(
                got = vectors.len(),
                want = self.tools.len(),
                "tool registry: embed count mismatch; embeddings discarded"
            );
            return;
        }
        let mut map = self.embeddings.write().await;
        map.clear();
        for (t, v) in self.tools.iter().zip(vectors) {
            map.insert(t.name.clone(), v);
        }
    }

    /// Return every registered tool. The slice points into
    /// the registry's internal `Arc<Vec<_>>` so callers don't
    /// pay for a clone.
    pub fn all(&self) -> &[ToolDefinition] {
        &self.tools
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// `true` when embeddings have been loaded for at least
    /// one tool. The semantic-search path checks this before
    /// running cosine; tests use it to verify `embed_all`
    /// completed.
    pub async fn has_embeddings(&self) -> bool {
        !self.embeddings.read().await.is_empty()
    }
}

// ── Internal helpers — duplicated from skills.rs so the
// tool registry doesn't pull the AI skills surface as a
// transitive concern. The algorithms are intentionally the
// same so callers see consistent behaviour. ───────────────

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "are", "was", "will", "you",
    "your", "but", "not", "all", "any", "use", "can", "may", "have", "has", "had", "would",
    "should", "could", "what", "when", "where", "how", "who", "why",
];

fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| !w.is_empty() && w.len() > 2 && !STOPWORDS.contains(&w.as_str()))
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
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

/// Same heuristic as `nodes::ai::execution::planner` — when
/// the capability name contains a mutating verb, the tool
/// is irreversible by default. Operators can override the
/// inferred value when building the registry directly.
fn is_irreversible_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    for kw in [
        "write",
        "delete",
        "remove",
        "send",
        "post",
        "drop",
        "destroy",
        "publish",
        "overwrite",
    ] {
        if lower.contains(kw) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str, tags: &[&str]) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            input_schema: serde_json::Value::Object(Default::default()),
            output_schema: serde_json::Value::Object(Default::default()),
            reversible: !is_irreversible_name(name),
            rollback_hint: None,
            tags: tags.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn sample_registry() -> ToolRegistry {
        ToolRegistry::new(vec![
            tool(
                "tool.web_fetch",
                "Fetch the contents of a URL via HTTPS",
                &["network", "fetch"],
            ),
            tool(
                "tool.fs.write_file",
                "Write text content to a file under the jailed filesystem root",
                &["filesystem", "write"],
            ),
            tool(
                "tool.audio.transcribe",
                "Transcribe an audio clip to text via a local Whisper engine",
                &["audio", "transform"],
            ),
        ])
    }

    #[test]
    fn from_capability_descriptors_maps_fields_correctly() {
        let mut d = CapabilityDescriptor::unary("tool.web_fetch");
        d.description = Some("Fetch a URL via HTTPS".into());
        d.categories = vec!["network".into(), "fetch".into()];
        let registry = ToolRegistry::from_capability_descriptors(&[d]);
        assert_eq!(registry.len(), 1);
        let t = &registry.all()[0];
        assert_eq!(t.name, "tool.web_fetch");
        assert_eq!(t.description, "Fetch a URL via HTTPS");
        assert_eq!(t.tags, vec!["network", "fetch"]);
        // Web fetch isn't mutating → reversible.
        assert!(t.reversible);
    }

    #[test]
    fn from_capability_descriptors_marks_mutating_tools_irreversible() {
        let mut d = CapabilityDescriptor::unary("tool.fs.delete_file");
        d.description = Some("Delete a file".into());
        let registry = ToolRegistry::from_capability_descriptors(&[d]);
        let t = &registry.all()[0];
        assert!(!t.reversible);
    }

    #[test]
    fn keyword_search_finds_relevant_tools_ranked_by_overlap() {
        let registry = sample_registry();
        let hits = registry.keyword_search("fetch a webpage", 10);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].name, "tool.web_fetch");
    }

    #[test]
    fn keyword_search_returns_empty_for_no_match() {
        let registry = sample_registry();
        let hits = registry.keyword_search("entirely unrelated garbage zzz", 10);
        assert!(hits.is_empty());
    }

    #[test]
    fn keyword_search_respects_limit() {
        // Three tools, all mention "tool" — but "tool" is
        // 4 chars + non-stopword so it'll match all three.
        // Limit to 2 → at most 2 returned.
        let registry = sample_registry();
        let hits = registry.keyword_search("tool", 2);
        assert!(hits.len() <= 2);
    }

    #[tokio::test]
    async fn semantic_search_falls_back_to_keyword_when_no_embeddings() {
        let registry = sample_registry();
        assert!(!registry.has_embeddings().await);
        let hits = registry
            .semantic_search("fetch a webpage", vec![1.0, 0.0], 10, 0.5)
            .await;
        assert!(!hits.is_empty());
        assert_eq!(hits[0].name, "tool.web_fetch");
    }

    #[tokio::test]
    async fn semantic_search_uses_embeddings_when_loaded() {
        let registry = sample_registry();
        // Stub embed_fn: maps each tool description to a
        // distinct unit vector along axis i; the query at
        // axis 0 should pick the first tool.
        let embed_fn = |texts: Vec<String>| -> BoxFuture<'static, Result<Vec<Vec<f32>>, String>> {
            Box::pin(async move {
                let mut out: Vec<Vec<f32>> = Vec::new();
                for (i, _) in texts.iter().enumerate() {
                    let mut v = vec![0.0; 3];
                    if i < 3 {
                        v[i] = 1.0;
                    }
                    out.push(v);
                }
                Ok(out)
            })
        };
        registry.embed_all(embed_fn).await;
        assert!(registry.has_embeddings().await);
        let hits = registry
            .semantic_search("anything", vec![1.0, 0.0, 0.0], 10, 0.5)
            .await;
        // Query is axis-0 → cosine = 1.0 with tool[0]
        // (web_fetch).
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "tool.web_fetch");
    }

    #[tokio::test]
    async fn semantic_search_drops_below_threshold() {
        let registry = sample_registry();
        let embed_fn = |texts: Vec<String>| -> BoxFuture<'static, Result<Vec<Vec<f32>>, String>> {
            Box::pin(async move {
                let n = texts.len();
                Ok(vec![vec![1.0, 0.0]; n])
            })
        };
        registry.embed_all(embed_fn).await;
        // Orthogonal query → cosine = 0.0 < 0.5 → no hits.
        let hits = registry
            .semantic_search("anything", vec![0.0, 1.0], 10, 0.5)
            .await;
        assert!(hits.is_empty());
    }

    #[test]
    fn all_returns_every_registered_tool() {
        let registry = sample_registry();
        let all = registry.all();
        assert_eq!(all.len(), 3);
        let names: Vec<&str> = all.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"tool.web_fetch"));
        assert!(names.contains(&"tool.fs.write_file"));
        assert!(names.contains(&"tool.audio.transcribe"));
    }

    #[test]
    fn empty_registry_reports_empty() {
        let registry = ToolRegistry::new(vec![]);
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.keyword_search("anything", 10).is_empty());
    }
}
