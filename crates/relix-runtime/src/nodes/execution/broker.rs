//! Agent access broker — per-agent allow/deny lists + rate
//! limiting over capability dispatch.
//!
//! The broker holds one [`AccessPolicy`] per agent name and
//! exposes a single [`check`](AgentAccessBroker::check)
//! entry point that returns `Allow`, `Deny`, or
//! `RateLimited`. The dispatch bridge consults the broker
//! before every handler so a misbehaving agent can't sneak
//! past a capability it's been forbidden from.
//!
//! ## Policy semantics
//!
//! - **Deny list** wins outright. If the capability is in
//!   `denied_capabilities`, the broker returns `Deny`.
//! - **Allow list** is opt-in. When `allowed_capabilities`
//!   is empty, the agent is unrestricted (subject to deny +
//!   rate-limit). When it's non-empty, only listed
//!   capabilities are permitted.
//! - **Rate limit** enforces `max_calls_per_minute` over a
//!   60-second sliding window. A request that would push
//!   the agent past the cap returns `RateLimited` with the
//!   remaining seconds until the oldest call in the window
//!   expires.
//!
//! Cost cap (`max_cost_cents_per_hour`) is carried on the
//! policy for forward compatibility with the upcoming cost-
//! tracker integration; today the broker doesn't enforce it
//! (the field is read by the dashboard / audit surface).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// One agent's access rules. `agent` is the operator-supplied
/// name used as the lookup key.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AccessPolicy {
    pub agent: String,
    #[serde(default)]
    pub allowed_capabilities: Vec<String>,
    #[serde(default)]
    pub denied_capabilities: Vec<String>,
    #[serde(default = "default_calls_per_minute")]
    pub max_calls_per_minute: u32,
    #[serde(default = "default_cost_per_hour")]
    pub max_cost_cents_per_hour: u32,
}

/// CORR PART 3: append a call timestamp inside an already-
/// locked map, trimming entries outside the 60-second window
/// so the per-agent vec doesn't grow forever. Free function so
/// [`AgentAccessBroker::atomic_check_and_record`] can call it
/// while it already owns the lock.
fn record_call_locked(counts: &mut HashMap<String, Vec<i64>>, agent: &str, now: i64) {
    let entry = counts.entry(agent.to_string()).or_default();
    entry.push(now);
    let cutoff = now - 60;
    entry.retain(|ts| *ts >= cutoff);
}

fn default_calls_per_minute() -> u32 {
    60
}

fn default_cost_per_hour() -> u32 {
    500
}

/// Verdict returned by [`AgentAccessBroker::check`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccessDecision {
    Allow,
    Deny { reason: String },
    RateLimited { retry_after_secs: u64 },
}

/// Per-agent capability broker with a built-in sliding-
/// window rate limiter.
#[derive(Clone)]
pub struct AgentAccessBroker {
    policies: HashMap<String, AccessPolicy>,
    call_counts: Arc<Mutex<HashMap<String, Vec<i64>>>>,
}

impl AgentAccessBroker {
    /// New broker from a vec of policies. The HashMap keys
    /// off `policy.agent`; later entries with the same name
    /// overwrite earlier ones.
    pub fn new(policies: Vec<AccessPolicy>) -> Self {
        let mut map = HashMap::with_capacity(policies.len());
        for p in policies {
            map.insert(p.agent.clone(), p);
        }
        Self {
            policies: map,
            call_counts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Empty broker — every check returns `Allow`. The
    /// dispatch bridge falls back to this when no
    /// `[[execution.agents]]` policies are configured.
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Check whether `agent` may call `capability`. The check
    /// is read-only; callers record successful dispatches
    /// via [`record_call`](Self::record_call) so the rate
    /// limiter sees them.
    pub fn check(&self, agent: &str, capability: &str) -> AccessDecision {
        let policy = match self.policies.get(agent) {
            Some(p) => p,
            // Unknown agents pass through. Operators who
            // want a deny-by-default posture register an
            // explicit policy with an empty allow + full
            // deny list.
            None => return AccessDecision::Allow,
        };
        if policy.denied_capabilities.iter().any(|c| c == capability) {
            return AccessDecision::Deny {
                reason: format!("agent '{agent}' denied capability '{capability}' by deny list"),
            };
        }
        if !policy.allowed_capabilities.is_empty()
            && !policy.allowed_capabilities.iter().any(|c| c == capability)
        {
            return AccessDecision::Deny {
                reason: format!("agent '{agent}' has an allow list and '{capability}' isn't on it"),
            };
        }
        if let Some(retry) = self.rate_limit_retry_secs(agent, policy.max_calls_per_minute) {
            return AccessDecision::RateLimited {
                retry_after_secs: retry,
            };
        }
        AccessDecision::Allow
    }

    /// Record a successful dispatch — should be called only
    /// after the handler returns Ok (or just before; the
    /// rate limiter is best-effort against bursts, not a
    /// strict semaphore).
    pub fn record_call(&self, agent: &str) {
        let now = unix_secs();
        let mut counts = self.call_counts.lock().unwrap_or_else(|e| {
            tracing::warn!("agent access broker counts poisoned; recovering inner state");
            e.into_inner()
        });
        record_call_locked(&mut counts, agent, now);
    }

    /// CORR PART 3: atomic check + record under a single lock.
    /// Pre-fix path called `check()` (took + released the lock)
    /// then `record_call()` (took + released the lock again),
    /// which let two callers both observe headroom and both
    /// burn through it concurrently. The atomic variant takes
    /// the lock once: the rate-limit check reads the live
    /// counts, and on `Allow` it appends a record before
    /// releasing the lock — so a concurrent caller observing
    /// the same instant sees the bumped count and is correctly
    /// throttled.
    pub fn atomic_check_and_record(&self, agent: &str, capability: &str) -> AccessDecision {
        let policy = match self.policies.get(agent) {
            Some(p) => p,
            None => {
                // Unknown agent still goes through the record
                // step so the operator's `/v1/agents/access`
                // dashboard counts background traffic too.
                let now = unix_secs();
                let mut counts = self.call_counts.lock().unwrap_or_else(|e| {
                    tracing::warn!("agent access broker counts poisoned; recovering inner state");
                    e.into_inner()
                });
                record_call_locked(&mut counts, agent, now);
                return AccessDecision::Allow;
            }
        };
        if policy.denied_capabilities.iter().any(|c| c == capability) {
            return AccessDecision::Deny {
                reason: format!("agent '{agent}' denied capability '{capability}' by deny list"),
            };
        }
        if !policy.allowed_capabilities.is_empty()
            && !policy.allowed_capabilities.iter().any(|c| c == capability)
        {
            return AccessDecision::Deny {
                reason: format!("agent '{agent}' has an allow list and '{capability}' isn't on it"),
            };
        }
        let now = unix_secs();
        let mut counts = self.call_counts.lock().unwrap_or_else(|e| {
            tracing::warn!("agent access broker counts poisoned; recovering inner state");
            e.into_inner()
        });
        // Inline rate-limit check against the same locked map
        // so a second caller racing this one observes the
        // bumped record below.
        if policy.max_calls_per_minute > 0 {
            let cutoff = now - 60;
            if let Some(entry) = counts.get(agent) {
                let recent = entry.iter().filter(|t| **t >= cutoff).count() as u32;
                if recent >= policy.max_calls_per_minute {
                    let oldest = entry
                        .iter()
                        .filter(|t| **t >= cutoff)
                        .min()
                        .copied()
                        .unwrap_or(now);
                    let retry = (oldest + 60 - now).max(1) as u64;
                    return AccessDecision::RateLimited {
                        retry_after_secs: retry,
                    };
                }
            }
        }
        record_call_locked(&mut counts, agent, now);
        AccessDecision::Allow
    }

    /// Returns `Some(seconds_until_retry)` when the agent
    /// has hit its cap; `None` when it has headroom. Public
    /// so tests + the dashboard can inspect without going
    /// through the full check path.
    pub fn rate_limit_retry_secs(&self, agent: &str, max_per_minute: u32) -> Option<u64> {
        if max_per_minute == 0 {
            return None;
        }
        let counts = self.call_counts.lock().unwrap_or_else(|e| {
            tracing::warn!("agent access broker counts poisoned; recovering inner state");
            e.into_inner()
        });
        let entry = counts.get(agent)?;
        let now = unix_secs();
        let cutoff = now - 60;
        let recent = entry.iter().filter(|t| **t >= cutoff).count() as u32;
        if recent < max_per_minute {
            return None;
        }
        // Oldest recent call dictates when the agent gets
        // headroom back.
        let oldest = entry
            .iter()
            .filter(|t| **t >= cutoff)
            .min()
            .copied()
            .unwrap_or(now);
        let retry = (oldest + 60 - now).max(1) as u64;
        Some(retry)
    }

    /// Snapshot of the broker for the `/v1/agents/access`
    /// endpoint. Returns a structured view operators can use
    /// to audit which agents have which policies + how many
    /// calls they've made in the last 60 seconds.
    pub fn snapshot(&self) -> Vec<AgentAccessSnapshotEntry> {
        let now = unix_secs();
        let cutoff = now - 60;
        let counts = self.call_counts.lock().unwrap();
        let mut out: Vec<AgentAccessSnapshotEntry> = self
            .policies
            .values()
            .map(|p| {
                let recent_calls = counts
                    .get(&p.agent)
                    .map(|v| v.iter().filter(|t| **t >= cutoff).count())
                    .unwrap_or(0);
                AgentAccessSnapshotEntry {
                    policy: p.clone(),
                    recent_calls_60s: recent_calls,
                }
            })
            .collect();
        out.sort_by(|a, b| a.policy.agent.cmp(&b.policy.agent));
        out
    }
}

/// One row in [`AgentAccessBroker::snapshot`]. Includes the
/// policy + a moving-window call count.
#[derive(Clone, Debug, Serialize)]
pub struct AgentAccessSnapshotEntry {
    pub policy: AccessPolicy,
    pub recent_calls_60s: usize,
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

    fn policy(agent: &str, allow: &[&str], deny: &[&str], rate: u32) -> AccessPolicy {
        AccessPolicy {
            agent: agent.to_string(),
            allowed_capabilities: allow.iter().map(|s| s.to_string()).collect(),
            denied_capabilities: deny.iter().map(|s| s.to_string()).collect(),
            max_calls_per_minute: rate,
            max_cost_cents_per_hour: 500,
        }
    }

    #[test]
    fn allow_when_capability_is_in_allowed_list() {
        let b = AgentAccessBroker::new(vec![policy(
            "alice",
            &["ai.chat", "memory.search"],
            &[],
            60,
        )]);
        assert_eq!(b.check("alice", "ai.chat"), AccessDecision::Allow);
        assert_eq!(b.check("alice", "memory.search"), AccessDecision::Allow);
    }

    #[test]
    fn deny_when_capability_is_in_denied_list() {
        let b = AgentAccessBroker::new(vec![policy(
            "alice",
            &[],
            &["tool.terminal", "fs.delete"],
            60,
        )]);
        match b.check("alice", "tool.terminal") {
            AccessDecision::Deny { reason } => {
                assert!(reason.contains("deny list"));
                assert!(reason.contains("tool.terminal"));
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn deny_when_capability_not_in_non_empty_allow_list() {
        let b = AgentAccessBroker::new(vec![policy("bob", &["ai.chat"], &[], 60)]);
        match b.check("bob", "memory.search") {
            AccessDecision::Deny { reason } => {
                assert!(reason.contains("allow list"));
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn deny_list_wins_over_allow_list() {
        // Even when a capability is in both lists, deny
        // takes priority — the operator's "no" must always
        // win.
        let b = AgentAccessBroker::new(vec![policy(
            "alice",
            &["fs.read", "fs.delete"],
            &["fs.delete"],
            60,
        )]);
        assert_eq!(b.check("alice", "fs.read"), AccessDecision::Allow);
        match b.check("alice", "fs.delete") {
            AccessDecision::Deny { .. } => {}
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn rate_limit_fires_when_calls_per_minute_exceeded() {
        let b = AgentAccessBroker::new(vec![policy("alice", &[], &[], 3)]);
        // First 3 calls allowed; 4th is rate-limited.
        for _ in 0..3 {
            assert_eq!(b.check("alice", "ai.chat"), AccessDecision::Allow);
            b.record_call("alice");
        }
        match b.check("alice", "ai.chat") {
            AccessDecision::RateLimited { retry_after_secs } => {
                assert!(retry_after_secs <= 60);
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn allow_when_allowed_capabilities_is_empty() {
        // Empty allow list + no deny = unrestricted (subject
        // only to rate limits).
        let b = AgentAccessBroker::new(vec![policy("alice", &[], &[], 60)]);
        assert_eq!(b.check("alice", "anything.goes"), AccessDecision::Allow);
        assert_eq!(b.check("alice", "tool.terminal"), AccessDecision::Allow);
    }

    #[test]
    fn unknown_agent_passes_through() {
        // No policy registered for "ghost" — every check
        // returns Allow. Operators who want deny-by-default
        // register an explicit empty-allow + comprehensive-
        // deny policy.
        let b = AgentAccessBroker::new(vec![policy("alice", &["x"], &[], 60)]);
        assert_eq!(b.check("ghost", "anything"), AccessDecision::Allow);
    }

    #[test]
    fn record_call_updates_count_for_snapshot() {
        let b = AgentAccessBroker::new(vec![policy("alice", &[], &[], 60)]);
        assert_eq!(b.snapshot()[0].recent_calls_60s, 0);
        b.record_call("alice");
        b.record_call("alice");
        let snap = b.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].recent_calls_60s, 2);
        assert_eq!(snap[0].policy.agent, "alice");
    }

    #[test]
    fn snapshot_is_sorted_by_agent_name() {
        let b = AgentAccessBroker::new(vec![
            policy("carol", &[], &[], 60),
            policy("alice", &[], &[], 60),
            policy("bob", &[], &[], 60),
        ]);
        let snap = b.snapshot();
        let names: Vec<&str> = snap.iter().map(|e| e.policy.agent.as_str()).collect();
        assert_eq!(names, vec!["alice", "bob", "carol"]);
    }

    #[test]
    fn empty_broker_allows_every_check() {
        let b = AgentAccessBroker::empty();
        assert_eq!(b.check("anyone", "anything"), AccessDecision::Allow);
        assert!(b.snapshot().is_empty());
    }

    // ── CORR PART 3: atomic check + record ─────────────────

    #[test]
    fn corr_p3_atomic_check_and_record_serialises_rate_limit() {
        // Pre-fix: check() then record_call() across two
        // calls let two concurrent callers both observe one
        // remaining token, both record, and exceed the cap.
        // Post-fix: atomic_check_and_record holds the lock
        // for the whole transaction so the second caller
        // observes the first's record before its own check.
        let b = AgentAccessBroker::new(vec![AccessPolicy {
            agent: "x".to_string(),
            allowed_capabilities: vec![],
            denied_capabilities: vec![],
            max_calls_per_minute: 1,
            max_cost_cents_per_hour: 0,
        }]);
        assert_eq!(
            b.atomic_check_and_record("x", "ai.chat"),
            AccessDecision::Allow
        );
        // Second call must be rate-limited, not Allow.
        match b.atomic_check_and_record("x", "ai.chat") {
            AccessDecision::RateLimited { .. } => {}
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn corr_p3_atomic_check_and_record_unknown_agent_still_allows() {
        // Unknown agents pass through (no policy), same as
        // the legacy `check`, but the atomic form still
        // appends a counter row so dashboards see the
        // traffic.
        let b = AgentAccessBroker::empty();
        assert_eq!(
            b.atomic_check_and_record("anon", "tool.web_fetch"),
            AccessDecision::Allow
        );
    }
}
