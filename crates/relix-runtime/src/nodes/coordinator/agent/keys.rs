//! Pure Operative **Keys** policy decisions (company-model §5.2).
//!
//! These functions hold *no* state and touch *no* I/O: they take the
//! relevant Keys (already read from an [`super::store::AgentProfile`])
//! plus the org-tree facts the caller has resolved, and return a
//! [`KeyVerdict`]. Keeping the decision pure means the spawn/assign
//! governance can be unit-tested exhaustively and reused from any
//! handler without dragging a store or a lock into the test.
//!
//! The verdict is deliberately three-valued:
//!
//! - [`KeyVerdict::Allow`] — the actor may perform the action now.
//! - [`KeyVerdict::Clearance`] — the action is permitted but must be
//!   routed up as a pending request that a Lead/Founder greenlights;
//!   nothing goes live silently.
//! - [`KeyVerdict::Deny`] — the actor lacks the Key; the action is
//!   refused with a readable reason (never a silent no-op).
//!
//! Default-deny (company-model §5.1): an Operative that has never had
//! a Key set carries `can_spawn_agents = false` / `can_assign_work =
//! false`, so the helpers refuse until the Founder grants the Key.

use serde::Serialize;

/// Verdict for a governed org/work action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum KeyVerdict {
    /// The action may proceed now.
    Allow,
    /// The action is permitted but must be created as a pending
    /// request for a Lead/Founder to greenlight — never live silently.
    Clearance { reason: String },
    /// The action is refused; `reason` is operator-readable.
    Deny { reason: String },
}

impl KeyVerdict {
    /// True only for [`KeyVerdict::Allow`].
    pub fn is_allow(&self) -> bool {
        matches!(self, KeyVerdict::Allow)
    }

    /// True for [`KeyVerdict::Deny`].
    pub fn is_deny(&self) -> bool {
        matches!(self, KeyVerdict::Deny { .. })
    }
}

/// Valid `spawn_route` values (company-model §5.2A). `direct` lets an
/// Operative create the (still pending-inert) hire itself; `lead` /
/// `founder` route the hire up for greenlight.
pub const SPAWN_ROUTES: &[&str] = &["direct", "lead", "founder"];

/// Valid `assign_scope` values (company-model §5.2B / §5.3).
pub const ASSIGN_SCOPES: &[&str] = &["any", "branch", "specific"];

/// Valid `configure_scope` values (company-model §5.2A).
pub const CONFIGURE_SCOPES: &[&str] = &["branch", "specific", "none"];

/// Normalise a stored `spawn_route`, defaulting to the safest value
/// (`founder` — route every hire up) when the value is unknown/empty.
pub fn normalize_spawn_route(route: &str) -> &str {
    let r = route.trim();
    if SPAWN_ROUTES.contains(&r) {
        r
    } else {
        "founder"
    }
}

/// Normalise a stored `assign_scope`, defaulting to the narrowest
/// value (`specific` — explicit allowlist only) when unknown/empty.
pub fn normalize_assign_scope(scope: &str) -> &str {
    let s = scope.trim();
    if ASSIGN_SCOPES.contains(&s) {
        s
    } else {
        "specific"
    }
}

/// Normalise a stored `configure_scope`, defaulting to `none`.
pub fn normalize_configure_scope(scope: &str) -> &str {
    let s = scope.trim();
    if CONFIGURE_SCOPES.contains(&s) {
        s
    } else {
        "none"
    }
}

/// Decide whether an Operative **actor** may spawn/hire another
/// Operative (company-model §5.2A). The Founder/Board path bypasses
/// this entirely — it is only consulted when an *agent* originates the
/// hire.
///
/// - `can_spawn = false` → [`KeyVerdict::Deny`].
/// - `spawn_route = direct` → [`KeyVerdict::Allow`] (the handler still
///   mints the hire **pending-inert**; "allow" means the actor may
///   originate it without escalation).
/// - `spawn_route = lead | founder` → [`KeyVerdict::Clearance`]: the
///   hire must be greenlit up the Line before it can go active.
pub fn spawn_verdict(can_spawn: bool, spawn_route: &str) -> KeyVerdict {
    if !can_spawn {
        return KeyVerdict::Deny {
            reason: "can_spawn_agents is off for this Operative".to_string(),
        };
    }
    match normalize_spawn_route(spawn_route) {
        "direct" => KeyVerdict::Allow,
        route => KeyVerdict::Clearance {
            reason: format!(
                "spawn_route={route}: the hire must be greenlit by your {route} before it goes active"
            ),
        },
    }
}

/// Decide whether an Operative **actor** may assign a Brief to
/// `assignee_id` (company-model §5.2B / §5.3). The Founder/Board path
/// bypasses this; it is only consulted for agent-originated assignment.
///
/// - `can_assign = false` → [`KeyVerdict::Deny`].
/// - `assign_scope = any` → [`KeyVerdict::Allow`].
/// - `assign_scope = branch` → allow iff `assignee_in_branch` (the
///   assignee is in the actor's Branch / manager subtree).
/// - `assign_scope = specific` → allow iff `assignee_id` is in
///   `allowed_agents`.
pub fn assign_verdict(
    can_assign: bool,
    assign_scope: &str,
    allowed_agents: &[String],
    assignee_id: &str,
    assignee_in_branch: bool,
) -> KeyVerdict {
    if !can_assign {
        return KeyVerdict::Deny {
            reason: "can_assign_work is off for this Operative".to_string(),
        };
    }
    match normalize_assign_scope(assign_scope) {
        "any" => KeyVerdict::Allow,
        "branch" => {
            if assignee_in_branch {
                KeyVerdict::Allow
            } else {
                KeyVerdict::Deny {
                    reason: format!(
                        "assign_scope=branch: `{assignee_id}` is not in this Operative's Branch"
                    ),
                }
            }
        }
        // "specific"
        _ => {
            if allowed_agents.iter().any(|a| a == assignee_id) {
                KeyVerdict::Allow
            } else {
                KeyVerdict::Deny {
                    reason: format!(
                        "assign_scope=specific: `{assignee_id}` is not in assign_allowed_agents"
                    ),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_denies_without_key() {
        assert!(spawn_verdict(false, "direct").is_deny());
        assert!(spawn_verdict(false, "founder").is_deny());
    }

    #[test]
    fn spawn_direct_allows_lead_and_founder_need_clearance() {
        assert_eq!(spawn_verdict(true, "direct"), KeyVerdict::Allow);
        assert!(matches!(
            spawn_verdict(true, "lead"),
            KeyVerdict::Clearance { .. }
        ));
        assert!(matches!(
            spawn_verdict(true, "founder"),
            KeyVerdict::Clearance { .. }
        ));
    }

    #[test]
    fn spawn_unknown_route_defaults_to_founder_clearance() {
        // An unknown/empty route normalises to the safest (founder),
        // which routes the hire up rather than letting it go direct.
        assert!(matches!(
            spawn_verdict(true, "garbage"),
            KeyVerdict::Clearance { .. }
        ));
        assert!(matches!(
            spawn_verdict(true, ""),
            KeyVerdict::Clearance { .. }
        ));
    }

    #[test]
    fn assign_denies_without_key() {
        assert!(assign_verdict(false, "any", &[], "agt_x", true).is_deny());
    }

    #[test]
    fn assign_any_scope_allows_anyone() {
        assert_eq!(
            assign_verdict(true, "any", &[], "agt_x", false),
            KeyVerdict::Allow
        );
    }

    #[test]
    fn assign_branch_scope_allows_in_branch_denies_out_of_branch() {
        assert_eq!(
            assign_verdict(true, "branch", &[], "agt_in", true),
            KeyVerdict::Allow
        );
        assert!(assign_verdict(true, "branch", &[], "agt_out", false).is_deny());
    }

    #[test]
    fn assign_specific_scope_honours_allowlist() {
        let allowed = vec!["agt_a".to_string(), "agt_b".to_string()];
        assert_eq!(
            assign_verdict(true, "specific", &allowed, "agt_b", false),
            KeyVerdict::Allow
        );
        // Branch membership is irrelevant under specific scope.
        assert!(assign_verdict(true, "specific", &allowed, "agt_c", true).is_deny());
    }

    #[test]
    fn unknown_scope_defaults_to_specific() {
        // A garbage scope must not silently widen to "any".
        let allowed = vec!["agt_a".to_string()];
        assert_eq!(
            assign_verdict(true, "garbage", &allowed, "agt_a", true),
            KeyVerdict::Allow
        );
        assert!(assign_verdict(true, "garbage", &allowed, "agt_z", true).is_deny());
    }
}
