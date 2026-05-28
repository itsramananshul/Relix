//! RELIX-7.24 — `AgentCapabilityRegistry`.
//!
//! Queryable index of every known agent peer + what it can
//! do. Built from three sources, listed in priority order:
//!
//! 1. **Local manifest** — every capability the coordinator's
//!    own [`crate::manifest::ManifestProvider`] has registered
//!    (these are the methods the coordinator node itself
//!    serves, e.g. `workflow.*`, `metrics.*`, `confidence.*`).
//!    The registry indexes them under the coordinator's own
//!    name as a single synthetic agent.
//! 2. **Agent-level config** — `[agents.<name>]` blocks from
//!    `ControllerConfig` that carry an explicit
//!    [`crate::controller_runtime::AgentCapabilityDecl`] list.
//!    These are the operator's authoritative descriptions
//!    and tags.
//! 3. **Runtime discovery** — entries cached in the
//!    [`crate::manifest::ManifestCache`] by the dispatch
//!    bridge after each peer's `node.manifest` round-trip.
//!    The planner reads from the cache when an agent has no
//!    explicit `capabilities` declaration.
//!
//! The registry is cheap-to-clone (everything sits behind
//! `Arc`s). All read methods are wait-free — a single
//! `RwLock` read.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::controller_runtime::AgentCapabilityDecl;
use crate::manifest::ManifestProvider;

/// Summary of one agent for the planner. Carried by
/// [`AgentCapabilityRegistry::list_agents`] +
/// [`AgentCapabilityRegistry::get_agent`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Agent name (the `[agents.<name>]` key OR the
    /// coordinator's own name for the synthetic local
    /// entry).
    pub name: String,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// libp2p peer alias the planner uses when emitting
    /// workflow step `peer` fields. `None` for the local
    /// synthetic agent (the workflow engine resolves it via
    /// the coordinator's `node.health` aliasing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// Every capability the agent exposes. Ordered for
    /// deterministic JSON output.
    pub capabilities: Vec<CapabilityInfo>,
}

/// One capability under an [`AgentInfo`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityInfo {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Result row from [`AgentCapabilityRegistry::find_agents_for_task`].
/// Sorted by `score` descending; ties broken by name ascending.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentMatch {
    pub agent: String,
    pub score: u32,
    /// Capabilities that contributed to the score (the ones
    /// whose tags or description matched at least one task
    /// keyword). Useful for the planner's downstream step-
    /// selection logic.
    pub matched_capabilities: Vec<String>,
}

/// The registry. Cheap to clone; every field is `Arc`-backed.
#[derive(Clone)]
pub struct AgentCapabilityRegistry {
    inner: Arc<RwLock<RegistryInner>>,
}

#[derive(Default)]
struct RegistryInner {
    /// Authoritative agent index. Built from the merge of all
    /// three sources at boot.
    agents: BTreeMap<String, AgentInfo>,
}

impl AgentCapabilityRegistry {
    /// Build an empty registry. Production callers use
    /// [`Self::from_sources`] which seeds from the three
    /// priority paths in one shot.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(RegistryInner::default())),
        }
    }

    /// Build the registry from the documented three sources:
    ///
    /// - `manifest`: the coordinator's own manifest. Indexed
    ///   under `local_name` as a single synthetic agent.
    /// - `agents_cfg`: the `[agents.<name>]` config map.
    ///   Operator-declared `capabilities` are the
    ///   authoritative description for those agents.
    /// - `peer_manifests`: agents whose `capabilities` is
    ///   empty fall back to the cached peer manifest the
    ///   bridge has populated from `node.manifest`. Pass an
    ///   empty map when no cache is wired (the planner then
    ///   only sees explicitly-declared capabilities).
    pub fn from_sources(
        local_name: &str,
        manifest: &ManifestProvider,
        agents_cfg: &BTreeMap<String, crate::controller_runtime::AgentSection>,
        peer_manifests: &BTreeMap<String, Vec<CapabilityInfo>>,
    ) -> Self {
        let registry = Self::new();
        registry.seed(local_name, manifest, agents_cfg, peer_manifests);
        registry
    }

    /// Idempotent reseed: clears + repopulates. Used by tests
    /// and the operator-facing `planning.reload_agents` cap
    /// (if the runtime ever exposes hot-reload).
    pub fn seed(
        &self,
        local_name: &str,
        manifest: &ManifestProvider,
        agents_cfg: &BTreeMap<String, crate::controller_runtime::AgentSection>,
        peer_manifests: &BTreeMap<String, Vec<CapabilityInfo>>,
    ) {
        let mut out: BTreeMap<String, AgentInfo> = BTreeMap::new();

        // (1) Local manifest. Synthetic agent named after the
        // coordinator so it shows up in list_agents alongside
        // remote peers.
        let snap = manifest.snapshot();
        let local_caps: Vec<CapabilityInfo> = snap
            .capabilities
            .iter()
            .map(|c| CapabilityInfo {
                method: c.method_name.clone(),
                description: c.description.clone(),
                tags: c.categories.clone(),
            })
            .collect();
        if !local_caps.is_empty() {
            out.insert(
                local_name.to_string(),
                AgentInfo {
                    name: local_name.to_string(),
                    description: Some(format!(
                        "Local coordinator ({} capabilities registered)",
                        local_caps.len()
                    )),
                    peer: None,
                    capabilities: local_caps,
                },
            );
        }

        // (2) Explicit per-agent declarations + (3) fallback
        // to cached peer manifest.
        for (name, section) in agents_cfg {
            let mut caps: Vec<CapabilityInfo> =
                section.capabilities.iter().map(decl_to_info).collect();
            if caps.is_empty() {
                // Fallback: read from cached peer manifest.
                let peer_alias = section.peer.clone().unwrap_or_else(|| name.clone());
                if let Some(remote_caps) = peer_manifests.get(&peer_alias) {
                    caps = remote_caps.clone();
                }
            }
            let info = AgentInfo {
                name: name.clone(),
                description: section.description.clone(),
                peer: section.peer.clone().or_else(|| Some(name.clone())),
                capabilities: caps,
            };
            out.insert(name.clone(), info);
        }

        let mut g = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.agents = out;
    }

    /// Snapshot every known agent, ordered alphabetically by
    /// name. Cheap clone — typically dozens of entries at
    /// most.
    pub fn list_agents(&self) -> Vec<AgentInfo> {
        let g = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.agents.values().cloned().collect()
    }

    /// Look up one agent by name. `None` when the agent
    /// hasn't been seeded.
    pub fn get_agent(&self, name: &str) -> Option<AgentInfo> {
        let g = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.agents.get(name).cloned()
    }

    /// Score every known agent against `task_description`
    /// using keyword overlap between the task and the agent's
    /// description + per-capability tags. Returns agents
    /// sorted by descending score; ties broken by name
    /// ascending. Agents whose score is 0 are excluded.
    ///
    /// Scoring rules:
    ///
    /// - Each matching task-keyword → tag pair adds 3 points.
    /// - Each matching task-keyword → method-name pair adds 2.
    /// - Each matching task-keyword → agent-description word
    ///   pair adds 1.
    /// - Each matching task-keyword → capability-description
    ///   word pair adds 1.
    ///
    /// "Keywords" are whitespace-split lowercase words of
    /// length ≥ 3 with common stopwords removed.
    pub fn find_agents_for_task(&self, task_description: &str) -> Vec<AgentMatch> {
        let task_keywords = extract_keywords(task_description);
        if task_keywords.is_empty() {
            return Vec::new();
        }
        let g = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut matches: Vec<AgentMatch> = Vec::with_capacity(g.agents.len());
        for info in g.agents.values() {
            let (score, matched_caps) = score_agent(info, &task_keywords);
            if score > 0 {
                matches.push(AgentMatch {
                    agent: info.name.clone(),
                    score,
                    matched_capabilities: matched_caps,
                });
            }
        }
        matches.sort_by(|a, b| b.score.cmp(&a.score).then(a.agent.cmp(&b.agent)));
        matches
    }

    /// Test-facing accessor returning the count of indexed
    /// agents. Used by the wiring tests to assert seeding
    /// landed without exposing internals.
    pub fn agent_count(&self) -> usize {
        let g = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.agents.len()
    }
}

impl Default for AgentCapabilityRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── pure helpers ──────────────────────────────────────────

fn decl_to_info(decl: &AgentCapabilityDecl) -> CapabilityInfo {
    CapabilityInfo {
        method: decl.method.clone(),
        description: decl.description.clone(),
        tags: decl.tags.clone(),
    }
}

/// Stopwords filtered out of task keyword extraction. Short
/// list; the goal is to drop the noisiest function words
/// before the per-pair scoring loop sees them.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "are", "was", "were", "have",
    "has", "but", "you", "your", "our", "all", "any", "use", "using", "should", "would", "could",
    "what", "when", "where", "which", "who", "why", "how",
];

/// Lowercase, strip punctuation, split on whitespace, drop
/// stopwords + tokens shorter than 3 characters. The result
/// is a `BTreeSet` (no duplicates, deterministic order).
fn extract_keywords(text: &str) -> BTreeSet<String> {
    let lower = text.to_lowercase();
    lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !STOPWORDS.contains(w))
        .map(|w| w.to_string())
        .collect()
}

fn score_agent(info: &AgentInfo, keywords: &BTreeSet<String>) -> (u32, Vec<String>) {
    let mut score: u32 = 0;
    let mut matched_caps: BTreeSet<String> = BTreeSet::new();

    // Description-level overlap.
    if let Some(desc) = &info.description {
        let desc_words = extract_keywords(desc);
        for kw in keywords {
            if desc_words.contains(kw) {
                score += 1;
            }
        }
    }

    // Per-capability scoring.
    for cap in &info.capabilities {
        let mut cap_score: u32 = 0;
        for kw in keywords {
            // Tag matches are the strongest signal — operators
            // write tags specifically to drive planner picks.
            if cap.tags.iter().any(|t| t.to_lowercase() == *kw) {
                cap_score += 3;
            }
            // Method-name segments (e.g. `tool.web_search` →
            // `tool`, `web_search`).
            let method_segments: Vec<String> = cap
                .method
                .split(|c: char| !c.is_alphanumeric())
                .map(|s| s.to_lowercase())
                .collect();
            if method_segments.iter().any(|s| s == kw) {
                cap_score += 2;
            }
            // Capability-description overlap.
            if let Some(d) = &cap.description
                && extract_keywords(d).contains(kw)
            {
                cap_score += 1;
            }
        }
        if cap_score > 0 {
            matched_caps.insert(cap.method.clone());
            score += cap_score;
        }
    }

    (score, matched_caps.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller_runtime::{AgentCapabilityDecl, AgentSection};
    use relix_core::capability::CapabilityDescriptor;
    use relix_core::types::NodeId;

    fn fixture_manifest() -> ManifestProvider {
        let m = ManifestProvider::new(
            NodeId::from_pubkey(b"local"),
            "coordinator",
            "coordinator",
            NodeId::from_pubkey(b"org"),
            vec![],
        );
        m.add_capability(
            CapabilityDescriptor::unary("workflow.run")
                .with_description("Run a stored workflow.")
                .with_categories(["workflow".into()]),
        );
        m
    }

    fn agent_section(
        description: Option<&str>,
        peer: Option<&str>,
        caps: Vec<AgentCapabilityDecl>,
    ) -> AgentSection {
        AgentSection {
            training: None,
            peer: peer.map(|s| s.into()),
            description: description.map(|s| s.into()),
            capabilities: caps,
        }
    }

    fn decl(method: &str, description: &str, tags: &[&str]) -> AgentCapabilityDecl {
        AgentCapabilityDecl {
            method: method.into(),
            description: Some(description.into()),
            tags: tags.iter().map(|s| (*s).into()).collect(),
        }
    }

    fn seed_two_agents() -> AgentCapabilityRegistry {
        let mut cfg = BTreeMap::new();
        cfg.insert(
            "research-agent".into(),
            agent_section(
                Some("Specialised in web research and summarisation"),
                Some("research-peer"),
                vec![
                    decl("ai.chat", "General research queries", &["research", "web"]),
                    decl("tool.web_search", "Direct web search", &["search"]),
                ],
            ),
        );
        cfg.insert(
            "code-agent".into(),
            agent_section(
                Some("Writes, reviews, and debugs code"),
                Some("code-peer"),
                vec![decl(
                    "ai.chat",
                    "Code generation and review",
                    &["code", "programming"],
                )],
            ),
        );
        AgentCapabilityRegistry::from_sources(
            "coordinator",
            &fixture_manifest(),
            &cfg,
            &BTreeMap::new(),
        )
    }

    #[test]
    fn list_agents_returns_all_configured_agents_plus_local_synthetic() {
        let r = seed_two_agents();
        let agents = r.list_agents();
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        // Sorted alphabetically: code-agent, coordinator, research-agent.
        assert_eq!(names, vec!["code-agent", "coordinator", "research-agent"]);
    }

    #[test]
    fn get_agent_returns_full_info_or_none() {
        let r = seed_two_agents();
        let a = r.get_agent("research-agent").expect("present");
        assert_eq!(a.peer.as_deref(), Some("research-peer"));
        assert_eq!(a.capabilities.len(), 2);
        assert!(r.get_agent("does-not-exist").is_none());
    }

    #[test]
    fn find_agents_for_task_returns_agents_sorted_by_match_score() {
        let r = seed_two_agents();
        // "research the latest web developments" → strong
        // match for research-agent (research + web tags),
        // weaker for code-agent.
        let matches = r.find_agents_for_task("Research the latest web developments");
        assert!(!matches.is_empty());
        assert_eq!(matches[0].agent, "research-agent");
        // Sorted descending by score.
        for w in matches.windows(2) {
            assert!(
                w[0].score >= w[1].score,
                "scores not descending: {:?}",
                matches
            );
        }
    }

    #[test]
    fn find_agents_for_task_returns_empty_when_no_keyword_matches() {
        let r = seed_two_agents();
        let matches = r.find_agents_for_task("xylophone unicorn parsnip");
        assert!(
            matches.is_empty(),
            "expected empty for nonsense task, got {matches:?}"
        );
    }

    #[test]
    fn an_agent_with_more_matching_tags_scores_higher() {
        let mut cfg = BTreeMap::new();
        cfg.insert(
            "broad".into(),
            agent_section(
                Some("does a bit of everything"),
                None,
                vec![decl("ai.chat", "general", &["research"])],
            ),
        );
        cfg.insert(
            "focused".into(),
            agent_section(
                Some("focused research helper"),
                None,
                vec![decl(
                    "ai.chat",
                    "deep research with web tools",
                    &["research", "web", "search"],
                )],
            ),
        );
        let r = AgentCapabilityRegistry::from_sources(
            "coord",
            &fixture_manifest(),
            &cfg,
            &BTreeMap::new(),
        );
        let matches = r.find_agents_for_task("research the web search engines");
        assert_eq!(matches[0].agent, "focused");
        assert!(matches[0].score > matches[1].score);
    }

    #[test]
    fn extract_keywords_drops_stopwords_and_short_tokens() {
        let kw = extract_keywords("The fox is in the box and you should run.");
        assert!(!kw.contains("the"));
        assert!(!kw.contains("you"));
        assert!(!kw.contains("is"));
        assert!(kw.contains("fox"));
        assert!(kw.contains("box"));
        assert!(kw.contains("run"));
    }

    #[test]
    fn cached_peer_manifest_fallback_kicks_in_when_capabilities_is_empty() {
        let mut cfg = BTreeMap::new();
        cfg.insert(
            "remote-agent".into(),
            agent_section(Some("a remote helper"), Some("remote-peer"), vec![]),
        );
        let mut cache = BTreeMap::new();
        cache.insert(
            "remote-peer".into(),
            vec![CapabilityInfo {
                method: "tool.web_fetch".into(),
                description: Some("Fetch a URL".into()),
                tags: vec!["web".into()],
            }],
        );
        let r = AgentCapabilityRegistry::from_sources("coord", &fixture_manifest(), &cfg, &cache);
        let a = r.get_agent("remote-agent").unwrap();
        assert_eq!(a.capabilities.len(), 1);
        assert_eq!(a.capabilities[0].method, "tool.web_fetch");
    }

    #[test]
    fn local_manifest_indexes_under_local_name() {
        let r = seed_two_agents();
        let local = r.get_agent("coordinator").expect("local synthetic");
        assert!(
            local
                .capabilities
                .iter()
                .any(|c| c.method == "workflow.run"),
            "{:?}",
            local
        );
    }

    #[test]
    fn empty_task_description_returns_empty_match_list() {
        let r = seed_two_agents();
        assert!(r.find_agents_for_task("").is_empty());
        assert!(r.find_agents_for_task("   ").is_empty());
    }
}
