//! Allowlist policy engine — alpha simplification of RELIX-1 §1.13 step 9 (target: Cedar).
//!
//! Policy is a TOML file with this shape:
//!
//! ```toml
//! [admit]
//! groups = ["chat-users", "tool-users", "memory-admin"]
//!
//! # Per-method rules. Allow if caller satisfies *any* matching rule. Default deny.
//! [[rules]]
//! name = "chat_users_chat"
//! method = "ai.chat"
//! allow_groups = ["chat-users"]
//!
//! [[rules]]
//! name = "tool_users_fetch"
//! method = "tool.web_fetch"
//! allow_groups = ["tool-users"]
//! ```
//!
//! The engine's `evaluate` signature mirrors Cedar's `(principal, action, resource, context)`
//! so the Gate-2 swap is straightforward.

use serde::{Deserialize, Serialize};

use crate::identity::VerifiedIdentity;

/// Decision outcomes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Allowed. `matched_rule` names the rule that admitted the call.
    Allow {
        /// Name of the matched rule (audit-visible).
        matched_rule: String,
    },
    /// Denied with a reason. `matched_rule` is `None` for default-deny.
    Deny {
        /// Human-readable reason.
        reason: String,
        /// Name of the rule that explicitly denied (if any).
        matched_rule: Option<String>,
    },
    // RequireApproval deferred to Gate 2 (SIMP-004).
}

/// Top-level policy file shape (loaded from disk via [`PolicyEngine::from_toml`]).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PolicyFile {
    /// Coarse-grained node-level admission. Empty = admit any verified identity.
    #[serde(default)]
    pub admit: AdmitSection,
    /// Per-method allow rules.
    #[serde(default, rename = "rules")]
    pub rules: Vec<Rule>,
}

/// Node-level admission: who may speak to this node at all (RELIX-5 §H.3 coarse layer).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AdmitSection {
    /// Allow callers in any of these groups. Empty = any identity admitted (alpha default).
    #[serde(default)]
    pub groups: Vec<String>,
}

/// One per-method rule.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rule {
    /// Operator-readable rule name; appears in audit when matched.
    pub name: String,
    /// Method this rule covers (exact match for alpha; wildcards future).
    pub method: String,
    /// Caller must hold at least one of these groups.
    #[serde(default)]
    pub allow_groups: Vec<String>,
}

/// The engine. Holds the parsed policy and an empty/default fallback.
#[derive(Clone, Debug)]
pub struct PolicyEngine {
    file: PolicyFile,
}

impl PolicyEngine {
    /// Construct from a parsed [`PolicyFile`].
    pub fn new(file: PolicyFile) -> Self {
        Self { file }
    }

    /// Load policy from TOML text.
    pub fn from_toml(text: &str) -> Result<Self, PolicyError> {
        let file: PolicyFile =
            toml::from_str(text).map_err(|e| PolicyError::Parse(e.to_string()))?;
        Ok(Self::new(file))
    }

    /// Load policy from a TOML file on disk.
    pub fn from_path(path: &std::path::Path) -> Result<Self, PolicyError> {
        let text = std::fs::read_to_string(path).map_err(|e| PolicyError::Io(e.to_string()))?;
        Self::from_toml(&text)
    }

    /// A permissive default: admit any verified identity for any method (development only).
    /// Used by node binaries when no policy file is configured. Logs a warning at startup.
    pub fn permissive() -> Self {
        Self {
            file: PolicyFile::default(),
        }
    }

    /// Evaluate a call.
    ///
    /// Order:
    /// 1. Node-level admission (`[admit]`): if any groups configured, caller must hold one.
    /// 2. Per-method rules (`[[rules]]`): caller must match an applicable rule.
    /// 3. Default deny.
    pub fn evaluate(&self, caller: &VerifiedIdentity, method: &str) -> Decision {
        // 1. Node admission.
        if !self.file.admit.groups.is_empty() && !caller.has_any_group(&self.file.admit.groups) {
            return Decision::Deny {
                reason: format!("caller {} not admitted by [admit] groups", caller.name),
                matched_rule: None,
            };
        }

        // 2. Per-method rules. First matching rule wins.
        for rule in &self.file.rules {
            if rule.method == method {
                if rule.allow_groups.is_empty() {
                    // A rule with no group constraint is an unconditional allow for that method.
                    return Decision::Allow {
                        matched_rule: rule.name.clone(),
                    };
                }
                if caller.has_any_group(&rule.allow_groups) {
                    return Decision::Allow {
                        matched_rule: rule.name.clone(),
                    };
                }
            }
        }

        // 3. Default deny.
        Decision::Deny {
            reason: format!(
                "no allow rule for method {} matches caller {} (groups={:?})",
                method, caller.name, caller.groups
            ),
            matched_rule: None,
        }
    }

    /// Returns true if a permissive (no-rules) engine. Useful for startup warnings.
    pub fn is_permissive(&self) -> bool {
        self.file.admit.groups.is_empty() && self.file.rules.is_empty()
    }
}

/// Policy-layer errors.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// TOML parse failure.
    #[error("parse: {0}")]
    Parse(String),
    /// File read failure.
    #[error("io: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeId;

    fn mk_id(name: &str, groups: &[&str]) -> VerifiedIdentity {
        VerifiedIdentity {
            subject_id: NodeId::from_pubkey(name.as_bytes()),
            name: name.into(),
            org_id: NodeId::from_pubkey(b"org"),
            groups: groups.iter().map(|s| s.to_string()).collect(),
            role: "agent".into(),
            clearance: "internal".into(),
            bundle_id: [0u8; 32],
        }
    }

    fn engine_for(text: &str) -> PolicyEngine {
        PolicyEngine::from_toml(text).expect("parse policy")
    }

    #[test]
    fn allowed_group_passes_with_matched_rule() {
        let engine = engine_for(
            r#"
            [[rules]]
            name = "chat_users_chat"
            method = "ai.chat"
            allow_groups = ["chat-users"]
            "#,
        );
        let alice = mk_id("alice", &["chat-users"]);
        match engine.evaluate(&alice, "ai.chat") {
            Decision::Allow { matched_rule } => assert_eq!(matched_rule, "chat_users_chat"),
            d => panic!("expected Allow, got {:?}", d),
        }
    }

    #[test]
    fn missing_group_denied() {
        let engine = engine_for(
            r#"
            [[rules]]
            name = "chat_users_chat"
            method = "ai.chat"
            allow_groups = ["chat-users"]
            "#,
        );
        let bob = mk_id("bob", &["guest"]);
        match engine.evaluate(&bob, "ai.chat") {
            Decision::Deny { matched_rule, .. } => assert!(matched_rule.is_none()),
            d => panic!("expected Deny, got {:?}", d),
        }
    }

    #[test]
    fn unknown_method_default_denied() {
        let engine = engine_for(
            r#"
            [[rules]]
            name = "x"
            method = "ai.chat"
            allow_groups = ["chat-users"]
            "#,
        );
        let alice = mk_id("alice", &["chat-users"]);
        match engine.evaluate(&alice, "ai.unrelated") {
            Decision::Deny { .. } => {}
            d => panic!("expected Deny, got {:?}", d),
        }
    }

    #[test]
    fn admit_section_blocks_off_groups() {
        let engine = engine_for(
            r#"
            [admit]
            groups = ["chat-users"]

            [[rules]]
            name = "ai_for_all_admitted"
            method = "ai.chat"
            allow_groups = ["chat-users"]
            "#,
        );
        let guest = mk_id("guest", &["guest"]);
        match engine.evaluate(&guest, "ai.chat") {
            Decision::Deny {
                matched_rule,
                reason,
            } => {
                assert!(matched_rule.is_none());
                assert!(reason.contains("admit"));
            }
            d => panic!("expected admit-deny, got {:?}", d),
        }
    }

    #[test]
    fn permissive_engine_allows_nothing_by_default_deny() {
        // Permissive engine has no rules, so default-deny still applies per-method.
        // Only node-admission is permissive (admits any identity).
        let engine = PolicyEngine::permissive();
        let alice = mk_id("alice", &["anything"]);
        match engine.evaluate(&alice, "ai.chat") {
            Decision::Deny { .. } => {}
            d => panic!("expected default-deny, got {:?}", d),
        }
        assert!(engine.is_permissive());
    }
}
