//! RELIX-7.16 — `[[knowledge.groups]]` config + resolver.
//!
//! Operators define sharing groups under
//! `[[knowledge.groups]]` in the coordinator's TOML:
//!
//! ```toml
//! [[knowledge.groups]]
//! name = "research-team"
//! members = ["research-agent", "summarizer-agent", "analyst-agent"]
//! auto_share_layers = ["observation"]
//! min_quality_score = 0.8
//! ```
//!
//! - `name` — group identifier (must be unique across the
//!   `[[knowledge.groups]]` list at validation time).
//! - `members` — agent friendly names (the
//!   `IdentityBundle::name` the AI handler sees on inbound
//!   calls). An agent can be in multiple groups.
//! - `auto_share_layers` — which memory layers are
//!   eligible for auto-propagation. Empty / absent means
//!   none. Most operators want `["observation"]`; advanced
//!   ones can add `"model"` once Layer 4 is stable.
//! - `min_quality_score` — observations below this score are
//!   rejected by the trust checker even when group
//!   membership allows them. `None` means accept everything
//!   the gate otherwise permits.
//!
//! The resolver below caches a `BTreeMap<agent, Vec<group>>`
//! so the dispatch hot path is one map lookup per inbound
//! observation rather than a linear scan over every group.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::nodes::memory::schema::MemoryLayer;

/// One sharing group. Parsed from `[[knowledge.groups]]`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct SharingGroup {
    pub name: String,
    #[serde(default)]
    pub members: Vec<String>,
    /// Layer tags eligible for auto-propagation. Stored as
    /// strings on the wire (`"observation"`, `"model"`) so
    /// the TOML is operator-readable; resolved against
    /// [`MemoryLayer::parse`] at validation time.
    #[serde(default)]
    pub auto_share_layers: Vec<String>,
    /// Minimum quality score for accepted observations. `None`
    /// means the trust checker doesn't enforce a floor.
    #[serde(default)]
    pub min_quality_score: Option<f32>,
    /// RELIX-7.16 GAP 3: optional per-member node routing.
    /// When set, each entry pins a member agent to the memory
    /// node where the agent's observations live. The
    /// `KnowledgeService` consults this map when sharing —
    /// targets pinned to a remote node receive their copy via
    /// the `knowledge.accept_shared` mesh capability instead
    /// of a local-store write. Empty list (default) routes
    /// every member to the LOCAL store, preserving pre-7.16
    /// behaviour byte-for-byte.
    #[serde(default)]
    pub member_nodes: Vec<MemberNodeRoute>,
}

/// One row in [`SharingGroup::member_nodes`] — `agent` lives
/// on `node`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct MemberNodeRoute {
    pub agent: String,
    pub node: String,
}

impl SharingGroup {
    /// True iff `agent` is listed in this group's members.
    pub fn is_member(&self, agent: &str) -> bool {
        self.members.iter().any(|m| m == agent)
    }

    /// Resolve `auto_share_layers` into a typed set. Unknown
    /// layer tags are dropped silently — the validator warns
    /// at boot time so operators see the typo immediately.
    pub fn auto_layers(&self) -> BTreeSet<MemoryLayer> {
        self.auto_share_layers
            .iter()
            .filter_map(|s| MemoryLayer::parse(s.trim()))
            .collect()
    }

    /// RELIX-7.16 GAP 3: node lookup for `agent`. Returns
    /// `None` when the member has no `member_nodes` entry —
    /// the service then routes locally.
    pub fn node_for_agent(&self, agent: &str) -> Option<&str> {
        self.member_nodes
            .iter()
            .find(|r| r.agent == agent)
            .map(|r| r.node.as_str())
    }
}

/// Top-level `[knowledge]` config block.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct KnowledgeConfig {
    /// Every defined sharing group.
    #[serde(default)]
    pub groups: Vec<SharingGroup>,
    /// Auto-share tick interval in seconds. Defaults to 60.
    /// Operators with very slow / very fast environments
    /// can override.
    #[serde(default = "default_auto_share_interval")]
    pub auto_share_interval_secs: u64,
    /// Per-agent observation-count cap. The trust checker
    /// drops the lowest-quality observations when accepting
    /// a new batch would push the agent over this number.
    /// `None` disables the cap; defaults to 10_000 — large
    /// enough that normal operation never trips it but small
    /// enough that a runaway auto-share loop hits a stop.
    #[serde(default = "default_observation_cap")]
    pub max_observations_per_agent: Option<u32>,
    /// RELIX-7.16 GAP 1: `[knowledge.quality_scorer]` —
    /// configures the periodic `MemoryQualityScorer` that
    /// stamps `quality:<f>` tags on Layer 3 observations
    /// missing one. Absent / `enabled = false` leaves the
    /// task unspawned and the trust checker's quality floor
    /// only enforces against operator-stamped tags.
    #[serde(default)]
    pub quality_scorer: super::quality_scorer::MemoryQualityScorerConfig,
    /// RELIX-7.16 GAP 4: per-tick propagation budget. The
    /// `AutoShareTask` stops attempting new propagations once
    /// it's attempted this many in a single tick — the rest
    /// resume on the next tick (the round-robin cursor
    /// guarantees no agent is starved). `None` disables the
    /// budget so operators get the pre-7.16 unbounded
    /// behaviour; defaults to 200 — large enough that a
    /// healthy cluster never trips it but small enough that
    /// a misconfigured floor can't spend the whole tick
    /// pumping rejections.
    #[serde(default = "default_auto_share_per_tick_budget")]
    pub auto_share_per_tick_budget: Option<u32>,
    /// RELIX-7.16 GAP 4: per-agent propagation cap WITHIN a
    /// single tick. Once the task attempts this many
    /// (target, observation) pairs for one source agent it
    /// rotates to the next; the round-robin cursor resumes
    /// at the same agent on the next tick. `None` disables
    /// the per-agent cap; defaults to 50 — keeps any one
    /// chatty agent from monopolising the budget.
    #[serde(default = "default_auto_share_per_agent_limit")]
    pub auto_share_per_agent_limit: Option<u32>,
}

fn default_auto_share_interval() -> u64 {
    60
}
fn default_observation_cap() -> Option<u32> {
    Some(10_000)
}
fn default_auto_share_per_tick_budget() -> Option<u32> {
    Some(200)
}
fn default_auto_share_per_agent_limit() -> Option<u32> {
    Some(50)
}

impl KnowledgeConfig {
    /// True iff at least one group has a populated member
    /// list. Used by the controller-runtime to decide
    /// whether to spawn the [`AutoShareTask`] at boot.
    pub fn has_active_groups(&self) -> bool {
        self.groups.iter().any(|g| !g.members.is_empty())
    }

    /// Validate the config at boot. Returns the resolved
    /// [`GroupResolver`] on success; the boot path logs a
    /// warning for every malformed entry and rejects only on
    /// hard errors (duplicate name, empty group name).
    pub fn resolve(&self) -> Result<GroupResolver, String> {
        let mut names: BTreeSet<&str> = BTreeSet::new();
        for g in &self.groups {
            if g.name.trim().is_empty() {
                return Err("[[knowledge.groups]] entry has empty name".into());
            }
            if !names.insert(g.name.as_str()) {
                return Err(format!(
                    "[[knowledge.groups]] duplicate group name: {g}",
                    g = g.name
                ));
            }
            for s in &g.auto_share_layers {
                if MemoryLayer::parse(s.trim()).is_none() {
                    tracing::warn!(
                        group = %g.name,
                        layer = %s,
                        "knowledge: unknown auto_share_layer; dropping"
                    );
                }
            }
        }
        let mut by_agent: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for g in &self.groups {
            for m in &g.members {
                by_agent.entry(m.clone()).or_default().push(g.name.clone());
            }
        }
        let mut by_name: BTreeMap<String, SharingGroup> = BTreeMap::new();
        for g in &self.groups {
            by_name.insert(g.name.clone(), g.clone());
        }
        Ok(GroupResolver { by_name, by_agent })
    }
}

/// Resolved index over the configured sharing groups. Cheap to
/// clone (two `BTreeMap`s of borrowed-from-Arc data live behind
/// the public methods). The coordinator builds one at boot and
/// hands it to the [`KnowledgeService`] + [`AutoShareTask`].
#[derive(Clone, Debug, Default)]
pub struct GroupResolver {
    by_name: BTreeMap<String, SharingGroup>,
    by_agent: BTreeMap<String, Vec<String>>,
}

impl GroupResolver {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Names of every group this agent belongs to. Empty
    /// vec when the agent isn't in any group.
    pub fn groups_for_agent(&self, agent: &str) -> Vec<&SharingGroup> {
        self.by_agent
            .get(agent)
            .map(|names| {
                names
                    .iter()
                    .filter_map(|n| self.by_name.get(n))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    /// `true` iff `a` and `b` are in at least one shared group.
    pub fn share_path(&self, a: &str, b: &str) -> Option<&SharingGroup> {
        self.groups_for_agent(a)
            .into_iter()
            .find(|g| g.is_member(b))
    }

    /// Look up one group by name.
    pub fn get(&self, name: &str) -> Option<&SharingGroup> {
        self.by_name.get(name)
    }

    /// Iterate every group.
    pub fn iter(&self) -> impl Iterator<Item = &SharingGroup> {
        self.by_name.values()
    }

    /// True iff the resolver knows about no groups.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Static manifest descriptor pairs for the `knowledge.*`
/// capability surface. Used by the controller-runtime to
/// build [`relix_core::capability::CapabilityDescriptor`]s.
pub fn sharing_group_descriptors() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "knowledge.share",
            "RELIX-7.16: copy specific Layer 3 observations from \
             one agent to one or more target agents. Args JSON: \
             `{source_agent, target_agents:[..], observation_ids:[..], message?}`. \
             Validates the source agent matches each observation, \
             that every target shares a group with the source, \
             and runs the trust checker (memory-guard poison + \
             min_quality_score + observation-count cap). Returns \
             `{shared_count, rejection_count, rejections:[..]}`.",
        ),
        (
            "knowledge.list_shared",
            "RELIX-7.16: list observations an agent has RECEIVED \
             from other agents. Args JSON: `{agent, shared_by?, \
             date_from?, date_to?, min_quality_score?}`. Returns \
             summaries with the original source agent name + the \
             sharing note tag when present.",
        ),
        (
            "knowledge.group_broadcast",
            "RELIX-7.16: copy specific observations from their \
             source agent to every OTHER member of the named group \
             simultaneously. Args JSON: `{caller_agent, group, \
             observation_ids:[..], message?}`. Validates the caller \
             is a member of the group. Returns per-target \
             `{shared_count, rejection_count}` rolled into one envelope.",
        ),
        (
            "knowledge.groups",
            "RELIX-7.16: return every configured sharing group. \
             No args. Each entry has `name`, `members`, \
             `auto_share_layers`, `min_quality_score`.",
        ),
        (
            "knowledge.revoke",
            "RELIX-7.16: mark RECEIVED observations as revoked \
             (soft-delete via `valid_to`). Args JSON: \
             `{observation_ids:[..]}`. The original observation \
             on the source agent is unaffected. Revocations are \
             written to the chronicle. Returns \
             `{revoked_count, missing_ids:[..]}`.",
        ),
        (
            "knowledge.recall",
            "RELIX-7.16 GAP 2: recall SOURCE observations across \
             every receiver they were shared with. Args JSON: \
             `{source_agent, source_observation_ids:[..]}`. \
             For each source id the service reads its \
             `shared_with` list, computes the deterministic copy \
             id at each receiver, soft-deletes that copy, and \
             writes a chronicle event. The source record itself \
             is unaffected. Trust gate: the caller must be the \
             source agent (the record's `source` column must \
             match). Returns `{source_ids_processed, \
             total_copies_revoked, per_target:[..], \
             missing_source_ids:[..], unauthorised_source_ids:[..]}`.",
        ),
        (
            "knowledge.accept_shared",
            "RELIX-7.16 GAP 3: receive a signed observation \
             payload from a remote memory node and accept it \
             into the local layered store. Args JSON: the \
             `SignedSharePayload` carrying \
             `{source_node, source_agent, target_agent, \
             record, message?, signature, source_pubkey}`. \
             The receiver verifies the ed25519 signature \
             against the carried pubkey, runs the local \
             `TrustChecker`, builds the deterministic copy id \
             via `mint_copy_id(record.id, target_agent)`, and \
             inserts the copy. Returns `{copy_id, target_agent}` \
             on success; rejects with `INVALID_ARGS` carrying \
             the structured `RejectReason` on signature \
             mismatch or trust-check failure.",
        ),
        (
            "knowledge.autoshare_stats",
            "RELIX-7.16 GAP 4: snapshot the AutoShareTask's \
             lifetime counters. No args. Returns JSON \
             `{total_ticks, total_propagated, \
             total_budget_exhausted_ticks, total_rejected, \
             total_per_agent_limit_hits, last_tick_stats:{\
             agents_scanned, observations_eligible, \
             propagations_attempted, propagations_accepted, \
             propagations_rejected, budget_exhausted, \
             per_agent_limit_hit_agents}}`. Counters are \
             monotonic; operators use them to confirm the \
             backpressure budget is being honored and that \
             auto-share isn't starving any one source agent.",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::memory::schema::MemoryLayer;

    fn cfg_with(groups: Vec<SharingGroup>) -> KnowledgeConfig {
        KnowledgeConfig {
            groups,
            auto_share_interval_secs: default_auto_share_interval(),
            max_observations_per_agent: default_observation_cap(),
            quality_scorer: Default::default(),
            auto_share_per_tick_budget: None,
            auto_share_per_agent_limit: None,
        }
    }

    #[test]
    fn empty_config_resolves_to_empty_resolver() {
        let cfg = cfg_with(vec![]);
        let r = cfg.resolve().unwrap();
        assert!(r.is_empty());
        assert!(r.groups_for_agent("alice").is_empty());
        assert!(r.share_path("alice", "bob").is_none());
    }

    #[test]
    fn share_path_finds_common_group() {
        let cfg = cfg_with(vec![SharingGroup {
            name: "research".into(),
            members: vec!["alice".into(), "bob".into()],
            auto_share_layers: vec!["observation".into()],
            min_quality_score: Some(0.7),
            member_nodes: Vec::new(),
        }]);
        let r = cfg.resolve().unwrap();
        let g = r.share_path("alice", "bob").expect("common group");
        assert_eq!(g.name, "research");
        assert!(r.share_path("alice", "carol").is_none());
    }

    #[test]
    fn agent_in_multiple_groups_lists_each_group() {
        let cfg = cfg_with(vec![
            SharingGroup {
                name: "research".into(),
                members: vec!["alice".into(), "bob".into()],
                auto_share_layers: vec![],
                min_quality_score: None,
                member_nodes: Vec::new(),
            },
            SharingGroup {
                name: "ops".into(),
                members: vec!["alice".into(), "carol".into()],
                auto_share_layers: vec![],
                min_quality_score: None,
                member_nodes: Vec::new(),
            },
        ]);
        let r = cfg.resolve().unwrap();
        let alice_groups: Vec<&str> = r
            .groups_for_agent("alice")
            .iter()
            .map(|g| g.name.as_str())
            .collect();
        assert!(alice_groups.contains(&"research"));
        assert!(alice_groups.contains(&"ops"));
    }

    #[test]
    fn duplicate_group_name_is_rejected_at_resolve_time() {
        let cfg = cfg_with(vec![
            SharingGroup {
                name: "dupe".into(),
                ..Default::default()
            },
            SharingGroup {
                name: "dupe".into(),
                ..Default::default()
            },
        ]);
        let err = cfg.resolve().unwrap_err();
        assert!(err.contains("duplicate group name"));
    }

    #[test]
    fn empty_group_name_is_rejected() {
        let cfg = cfg_with(vec![SharingGroup {
            name: "  ".into(),
            ..Default::default()
        }]);
        assert!(cfg.resolve().is_err());
    }

    #[test]
    fn auto_layers_resolves_known_strings_to_typed_set() {
        let g = SharingGroup {
            name: "x".into(),
            members: vec![],
            auto_share_layers: vec!["observation".into(), "model".into(), "bogus".into()],
            min_quality_score: None,
            member_nodes: Vec::new(),
        };
        let layers = g.auto_layers();
        assert!(layers.contains(&MemoryLayer::Observation));
        assert!(layers.contains(&MemoryLayer::Model));
        assert!(!layers.contains(&MemoryLayer::Raw));
        assert_eq!(layers.len(), 2);
    }

    #[test]
    fn parses_full_toml_block() {
        let cfg: KnowledgeConfig = toml::from_str(
            r#"
            auto_share_interval_secs = 30
            max_observations_per_agent = 500

            [[groups]]
            name = "research"
            members = ["alice", "bob"]
            auto_share_layers = ["observation"]
            min_quality_score = 0.8

            [[groups]]
            name = "ops"
            members = ["alice", "carol"]
            auto_share_layers = []
            "#,
        )
        .unwrap();
        assert_eq!(cfg.auto_share_interval_secs, 30);
        assert_eq!(cfg.max_observations_per_agent, Some(500));
        assert_eq!(cfg.groups.len(), 2);
        let r = cfg.resolve().unwrap();
        let research = r.get("research").unwrap();
        assert_eq!(research.min_quality_score, Some(0.8));
    }

    #[test]
    fn has_active_groups_is_false_for_empty_member_lists() {
        let cfg = cfg_with(vec![SharingGroup {
            name: "empty".into(),
            members: vec![],
            auto_share_layers: vec![],
            min_quality_score: None,
            member_nodes: Vec::new(),
        }]);
        assert!(!cfg.has_active_groups());
        let cfg = cfg_with(vec![SharingGroup {
            name: "real".into(),
            members: vec!["a".into()],
            auto_share_layers: vec![],
            min_quality_score: None,
            member_nodes: Vec::new(),
        }]);
        assert!(cfg.has_active_groups());
    }
}
