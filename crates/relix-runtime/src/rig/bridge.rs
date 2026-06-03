//! The **bridge-back token** (Pillar 2) — the scoped, per-run
//! credential a Rig hands its agent so the agent can call *Relix's*
//! own API back (comment on its Brief, create Sub-briefs, request a
//! Clearance). The universal "talk back to Relix" mechanism that
//! turns any plugged-in agent into a real Operative.
//!
//! A token is **not** a model credential — it grants only the right
//! to act on *this run*: this Brief, by this Operative, in this
//! Guild, until it expires. The dispatcher mints one per Shift and
//! injects it into the Rig's environment; Relix's API handlers
//! [`BridgeTokenStore::authorize`] every inbound call against it.
//!
//! Tokens are recorded server-side (opaque random ids), so a token
//! is revocable and naturally expires — no signing key needed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// What a bridge token authorizes — the scope of one run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeClaims {
    pub brief_id: String,
    pub agent_id: String,
    pub tenant_id: String,
    /// Unix seconds after which the token is dead.
    pub expires_at: i64,
}

/// A process-wide store of live bridge tokens. Cheap to clone (an
/// `Arc` handle to the shared map).
#[derive(Clone, Default)]
pub struct BridgeTokenStore {
    inner: Arc<Mutex<HashMap<String, BridgeClaims>>>,
}

impl BridgeTokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a token scoped to one run, valid for `ttl_secs`. Returns
    /// the opaque token string to inject into the Rig.
    pub fn mint(
        &self,
        brief_id: impl Into<String>,
        agent_id: impl Into<String>,
        tenant_id: impl Into<String>,
        ttl_secs: i64,
    ) -> String {
        let token = new_token();
        let claims = BridgeClaims {
            brief_id: brief_id.into(),
            agent_id: agent_id.into(),
            tenant_id: tenant_id.into(),
            expires_at: unix_now().saturating_add(ttl_secs.max(0)),
        };
        self.lock().insert(token.clone(), claims);
        token
    }

    /// Resolve a token to its claims, or `None` if unknown or
    /// expired (an expired token is reaped on lookup).
    pub fn verify(&self, token: &str) -> Option<BridgeClaims> {
        let now = unix_now();
        let mut guard = self.lock();
        let claims = guard.get(token).cloned()?;
        if now >= claims.expires_at {
            guard.remove(token);
            return None;
        }
        Some(claims)
    }

    /// Authorize an inbound call: the token must be live AND scoped
    /// to exactly this Brief + Operative. This is the check every
    /// bridge-back API handler runs.
    pub fn authorize(&self, token: &str, brief_id: &str, agent_id: &str) -> bool {
        match self.verify(token) {
            Some(c) => c.brief_id == brief_id && c.agent_id == agent_id,
            None => false,
        }
    }

    /// Revoke a token (e.g. when the Shift ends).
    pub fn revoke(&self, token: &str) {
        self.lock().remove(token);
    }

    /// Drop all expired tokens; returns how many were reaped. Cheap
    /// background hygiene the dispatcher can run periodically.
    pub fn sweep_expired(&self) -> usize {
        let now = unix_now();
        let mut guard = self.lock();
        let before = guard.len();
        guard.retain(|_, c| now < c.expires_at);
        before - guard.len()
    }

    /// Number of live (not yet reaped) tokens.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, BridgeClaims>> {
        // Recover from a poisoned lock rather than panic — a dropped
        // token map is never worth taking the node down.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn new_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    format!("brt_{}", hex::encode(b))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_verify_and_scope() {
        let store = BridgeTokenStore::new();
        let t = store.mint("brief_1", "agt_a", "guild_x", 300);
        assert!(t.starts_with("brt_"));

        let c = store.verify(&t).unwrap();
        assert_eq!(c.brief_id, "brief_1");
        assert_eq!(c.agent_id, "agt_a");
        assert_eq!(c.tenant_id, "guild_x");

        // Correct scope authorizes; wrong Brief / Operative does not.
        assert!(store.authorize(&t, "brief_1", "agt_a"));
        assert!(!store.authorize(&t, "brief_2", "agt_a"));
        assert!(!store.authorize(&t, "brief_1", "agt_b"));

        // Unknown token is never authorized.
        assert!(store.verify("brt_nope").is_none());
        assert!(!store.authorize("brt_nope", "brief_1", "agt_a"));
    }

    #[test]
    fn revoke_and_expiry() {
        let store = BridgeTokenStore::new();

        // Revoked token stops authorizing.
        let t = store.mint("b", "a", "g", 300);
        assert!(store.verify(&t).is_some());
        store.revoke(&t);
        assert!(store.verify(&t).is_none());

        // A ttl=0 token is already expired and is reaped on lookup.
        let expired = store.mint("b", "a", "g", 0);
        assert!(store.verify(&expired).is_none());
        assert!(store.is_empty(), "expired token reaped on verify");

        // sweep_expired clears stale tokens.
        let live = store.mint("b", "a", "g", 300);
        let _stale = store.mint("b2", "a", "g", 0);
        let reaped = store.sweep_expired();
        assert_eq!(reaped, 1);
        assert_eq!(store.len(), 1);
        assert!(store.verify(&live).is_some());
    }
}
