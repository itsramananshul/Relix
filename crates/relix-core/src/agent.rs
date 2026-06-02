//! Agent identity data model: `AgentId`, `AgentRecord`, `AgentToken`,
//! the `AgentRegistry` and `TokenIssuer` traits, and the HMAC-SHA256
//! `HmacTokenIssuer` implementation.
//!
//! Database-backed registry implementations live in `relix-runtime`
//! (no storage dependencies belong in `relix-core`).

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

// ── AgentId ──────────────────────────────────────────────────

/// Opaque string identifier for a registered agent.
///
/// The only invariant enforced at construction time is that the string
/// must not contain `|` (the HMAC token field delimiter) or be empty.
/// Registry implementations choose their own format (e.g. `"agt_{hex8}"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(String);

impl AgentId {
    /// Construct an `AgentId` from a raw string.
    ///
    /// Returns `Err(AgentError::InvalidId)` if `id` is empty or contains `|`.
    pub fn new(id: impl Into<String>) -> Result<Self, AgentError> {
        let id = id.into();
        if id.is_empty() || id.contains('|') {
            return Err(AgentError::InvalidId(id));
        }
        Ok(AgentId(id))
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for AgentId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// ── AgentStatus ───────────────────────────────────────────────

/// Lifecycle status of a registered agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Agent is allowed to operate normally.
    Active,
    /// Temporarily suspended; may be re-activated by an operator.
    Suspended,
    /// Permanently revoked; cannot be re-activated.
    Revoked,
}

impl AgentStatus {
    /// Canonical lowercase string stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Active => "active",
            AgentStatus::Suspended => "suspended",
            AgentStatus::Revoked => "revoked",
        }
    }
}

impl FromStr for AgentStatus {
    type Err = AgentError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(AgentStatus::Active),
            "suspended" => Ok(AgentStatus::Suspended),
            "revoked" => Ok(AgentStatus::Revoked),
            other => Err(AgentError::InvalidStatus(other.to_string())),
        }
    }
}

impl std::fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ── AgentRecord ───────────────────────────────────────────────

/// Persistent record for a registered agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    /// Unique agent identifier.
    pub agent_id: AgentId,
    /// Human-readable display name.
    pub name: String,
    /// Role label (e.g. `"agent"`, `"service"`, `"operator"`).
    pub role: String,
    /// Current lifecycle status.
    pub status: AgentStatus,
    /// Unix timestamp (seconds) when the record was first created.
    pub created_at: i64,
    /// Unix timestamp (seconds) of the most recent status update.
    pub updated_at: i64,
}

impl AgentRecord {
    /// True iff the agent is currently active (not suspended or revoked).
    pub fn is_active(&self) -> bool {
        self.status == AgentStatus::Active
    }
}

// ── AgentToken ────────────────────────────────────────────────

/// An issued authentication token for an agent.
///
/// The `token` field is the opaque, signed credential string that callers
/// exchange over the wire.  It is self-contained: pass it to
/// [`TokenIssuer::verify`] without needing the other fields separately.
///
/// Token wire format (managed entirely by [`HmacTokenIssuer`]):
/// `{agent_id}|{expires_at_secs}|{hmac_hex}`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToken {
    /// The agent this token was issued for.
    pub agent_id: AgentId,
    /// Expiry as a Unix timestamp (seconds).
    pub expires_at: i64,
    /// Opaque signed credential.  Treat as a black box; pass to
    /// [`TokenIssuer::verify`] for authoritative validation.
    pub token: String,
}

// ── AgentError ────────────────────────────────────────────────

/// Errors from agent registry and token operations.
#[derive(Debug, Error)]
pub enum AgentError {
    /// The agent id is empty or contains the reserved `|` delimiter.
    #[error("invalid agent id: {0:?}")]
    InvalidId(String),

    /// Unknown status string (e.g. corrupted database row).
    #[error("unknown agent status: {0:?}")]
    InvalidStatus(String),

    /// No agent found for the given id.
    #[error("agent not found: {0}")]
    NotFound(AgentId),

    /// An agent with this id is already registered.
    #[error("agent already exists: {0}")]
    AlreadyExists(AgentId),

    /// The token has passed its `expires_at` timestamp.
    #[error("token expired")]
    TokenExpired,

    /// Token structure or HMAC signature is invalid.
    #[error("invalid token")]
    InvalidToken,

    /// The agent record exists but its status is `Revoked`.
    #[error("agent revoked: {0}")]
    Revoked(AgentId),

    /// Backing storage failure.
    #[error("storage: {0}")]
    Storage(String),
}

// ── AgentRegistry trait ───────────────────────────────────────

/// CRUD operations for the agent registry.
///
/// Implementations must be `Send + Sync` so they can be shared
/// across async tasks via `Arc`.
pub trait AgentRegistry: Send + Sync {
    /// Register a new agent; returns the newly created [`AgentRecord`].
    ///
    /// Fails with [`AgentError::AlreadyExists`] if the id is already taken.
    fn create(&self, agent_id: AgentId, name: &str, role: &str) -> Result<AgentRecord, AgentError>;

    /// Look up an agent by id.
    ///
    /// Returns `Ok(None)` when the agent does not exist (distinct from
    /// `Err`, which signals a storage failure).
    fn get(&self, agent_id: &AgentId) -> Result<Option<AgentRecord>, AgentError>;

    /// Return all registered agents, including suspended and revoked records.
    fn list(&self) -> Result<Vec<AgentRecord>, AgentError>;

    /// Permanently revoke an agent.  Idempotent if already revoked.
    ///
    /// Fails with [`AgentError::NotFound`] if the id is unknown.
    fn revoke(&self, agent_id: &AgentId) -> Result<(), AgentError>;
}

// ── TokenIssuer trait ─────────────────────────────────────────

/// Issue and verify agent authentication tokens.
pub trait TokenIssuer: Send + Sync {
    /// Issue a signed token for `agent_id` that expires after `ttl_secs` seconds.
    fn issue(&self, agent_id: &AgentId, ttl_secs: u64) -> Result<AgentToken, AgentError>;

    /// Verify an opaque token string (the `token` field of [`AgentToken`]).
    ///
    /// Returns the [`AgentId`] encoded in the token on success.
    ///
    /// # Errors
    /// - [`AgentError::TokenExpired`] if the token is past its expiry.
    /// - [`AgentError::InvalidToken`] if the structure or signature is wrong.
    fn verify(&self, token: &str) -> Result<AgentId, AgentError>;
}

// ── HmacTokenIssuer ───────────────────────────────────────────

/// HMAC-SHA256 token issuer.
///
/// Signs `"{agent_id}|{expires_at}"` with a symmetric key; the resulting
/// signature is appended to produce a self-contained token string:
///
/// ```text
/// {agent_id}|{expires_at}|{hmac_hex}
/// ```
///
/// The signing key must be kept secret.  Load it from an environment
/// variable (e.g. `RELIX_AGENT_TOKEN_KEY`) at startup; do not embed it
/// in source code or configs.
pub struct HmacTokenIssuer {
    key: Vec<u8>,
}

impl HmacTokenIssuer {
    /// Construct from raw key bytes (any length is accepted by HMAC-SHA256).
    pub fn new(key: Vec<u8>) -> Self {
        HmacTokenIssuer { key }
    }

    /// Construct from a hex-encoded key string.
    pub fn from_hex(hex_key: &str) -> Result<Self, AgentError> {
        let key = hex::decode(hex_key)
            .map_err(|e| AgentError::Storage(format!("invalid key hex: {e}")))?;
        Ok(HmacTokenIssuer { key })
    }

    /// Compute HMAC-SHA256 over `message` and return the hex digest.
    ///
    /// `pub(crate)` so inline test modules can call it when constructing
    /// synthetic tokens (e.g. already-expired ones with valid sigs).
    pub(crate) fn sign(&self, message: &[u8]) -> Result<String, AgentError> {
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .map_err(|e| AgentError::Storage(format!("hmac init: {e}")))?;
        mac.update(message);
        Ok(hex::encode(mac.finalize().into_bytes()))
    }
}

fn now_unix_secs() -> Result<i64, AgentError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|e| AgentError::Storage(e.to_string()))
}

impl TokenIssuer for HmacTokenIssuer {
    fn issue(&self, agent_id: &AgentId, ttl_secs: u64) -> Result<AgentToken, AgentError> {
        let now = now_unix_secs()?;
        let expires_at = now.saturating_add(ttl_secs as i64);
        let message = format!("{}|{}", agent_id.as_str(), expires_at);
        let sig = self.sign(message.as_bytes())?;
        let token = format!("{}|{}|{}", agent_id.as_str(), expires_at, sig);
        Ok(AgentToken {
            agent_id: agent_id.clone(),
            expires_at,
            token,
        })
    }

    fn verify(&self, token: &str) -> Result<AgentId, AgentError> {
        // token = "{agent_id}|{expires_at}|{hmac_hex}"
        // agent_id cannot contain `|` (enforced at construction), so splitn(3)
        // gives exactly the three parts.
        let mut parts = token.splitn(3, '|');
        let raw_id = parts.next().ok_or(AgentError::InvalidToken)?;
        let raw_expires = parts.next().ok_or(AgentError::InvalidToken)?;
        let provided_sig = parts.next().ok_or(AgentError::InvalidToken)?;

        if raw_id.is_empty() || raw_expires.is_empty() || provided_sig.is_empty() {
            return Err(AgentError::InvalidToken);
        }

        let expires_at: i64 = raw_expires.parse().map_err(|_| AgentError::InvalidToken)?;

        // Verify HMAC in constant time *before* returning any structural error
        // that would distinguish a bad-sig from a well-formed-but-stale token.
        let message = format!("{}|{}", raw_id, expires_at);
        let expected_sig = self.sign(message.as_bytes())?;

        let sig_valid: bool = provided_sig
            .as_bytes()
            .ct_eq(expected_sig.as_bytes())
            .into();

        if !sig_valid {
            return Err(AgentError::InvalidToken);
        }

        // Only reveal expiry after signature passes — prevents an attacker
        // from probing token structure without a valid key.
        let now = now_unix_secs()?;
        if now > expires_at {
            return Err(AgentError::TokenExpired);
        }

        AgentId::new(raw_id).map_err(|_| AgentError::InvalidToken)
    }
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn issuer() -> HmacTokenIssuer {
        HmacTokenIssuer::new(b"test-secret-key-32-bytes-padpadp".to_vec())
    }

    // ── AgentId ───────────────────────────────────────────────

    #[test]
    fn agent_id_rejects_pipe() {
        assert!(matches!(
            AgentId::new("agt|bad"),
            Err(AgentError::InvalidId(_))
        ));
    }

    #[test]
    fn agent_id_rejects_empty() {
        assert!(matches!(AgentId::new(""), Err(AgentError::InvalidId(_))));
    }

    #[test]
    fn agent_id_roundtrip_display() {
        let id = AgentId::new("agt_abc123").unwrap();
        assert_eq!(id.to_string(), "agt_abc123");
        assert_eq!(id.as_str(), "agt_abc123");
        assert_eq!(id.as_ref(), "agt_abc123");
    }

    // ── AgentStatus ───────────────────────────────────────────

    #[test]
    fn agent_status_roundtrip() {
        for s in &["active", "suspended", "revoked"] {
            let status: AgentStatus = s.parse().unwrap();
            assert_eq!(status.as_str(), *s);
            assert_eq!(status.to_string(), *s);
        }
    }

    #[test]
    fn agent_status_unknown_rejected() {
        assert!(matches!(
            "unknown".parse::<AgentStatus>(),
            Err(AgentError::InvalidStatus(_))
        ));
    }

    // ── AgentRecord ───────────────────────────────────────────

    #[test]
    fn agent_record_is_active() {
        let id = AgentId::new("agt_x").unwrap();
        let rec = AgentRecord {
            agent_id: id,
            name: "x".into(),
            role: "agent".into(),
            status: AgentStatus::Active,
            created_at: 0,
            updated_at: 0,
        };
        assert!(rec.is_active());
        let rec2 = AgentRecord {
            status: AgentStatus::Revoked,
            ..rec
        };
        assert!(!rec2.is_active());
    }

    // ── HmacTokenIssuer — happy path ──────────────────────────

    #[test]
    fn issue_and_verify_roundtrip() {
        let iss = issuer();
        let id = AgentId::new("agt_abc123").unwrap();
        let token = iss.issue(&id, 3600).unwrap();
        assert_eq!(token.agent_id, id);
        assert!(token.expires_at > 0);

        let verified = iss.verify(&token.token).unwrap();
        assert_eq!(verified, id);
    }

    #[test]
    fn from_hex_key_roundtrip() {
        let raw = b"test-secret-key-32-bytes-padpadp";
        let hex_key = hex::encode(raw);
        let iss = HmacTokenIssuer::from_hex(&hex_key).unwrap();
        let id = AgentId::new("agt_test").unwrap();
        let token = iss.issue(&id, 60).unwrap();
        let verified = iss.verify(&token.token).unwrap();
        assert_eq!(verified, id);
    }

    // ── HmacTokenIssuer — revocation / bad token ──────────────

    #[test]
    fn wrong_key_rejected() {
        let iss1 = issuer();
        let iss2 = HmacTokenIssuer::new(b"different-key-32-bytes-padpadpad".to_vec());
        let id = AgentId::new("agt_abc123").unwrap();
        let token = iss1.issue(&id, 3600).unwrap();
        let err = iss2.verify(&token.token).unwrap_err();
        assert!(matches!(err, AgentError::InvalidToken));
    }

    #[test]
    fn bad_signature_directly_rejected() {
        let iss = issuer();
        let id = AgentId::new("agt_abc123").unwrap();
        let token = iss.issue(&id, 3600).unwrap();
        // Replace signature component with 64 zeros.
        let parts: Vec<&str> = token.token.splitn(3, '|').collect();
        let bad = format!("{}|{}|{}", parts[0], parts[1], "0".repeat(64));
        let err = iss.verify(&bad).unwrap_err();
        assert!(matches!(err, AgentError::InvalidToken));
    }

    #[test]
    fn expired_token_rejected() {
        let iss = issuer();
        // Construct a token with a past expiry but a valid HMAC over it.
        let expires_at: i64 = 1; // Unix epoch + 1s — definitively in the past.
        let msg = format!("agt_abc123|{}", expires_at);
        let sig = iss.sign(msg.as_bytes()).unwrap();
        let expired = format!("agt_abc123|{}|{}", expires_at, sig);
        let err = iss.verify(&expired).unwrap_err();
        assert!(matches!(err, AgentError::TokenExpired));
    }

    #[test]
    fn tampered_agent_id_rejected() {
        let iss = issuer();
        let id = AgentId::new("agt_alice").unwrap();
        let token = iss.issue(&id, 3600).unwrap();
        // Change agent_id in the token but keep the original sig.
        let parts: Vec<&str> = token.token.splitn(3, '|').collect();
        let tampered = format!("agt_bob|{}|{}", parts[1], parts[2]);
        let err = iss.verify(&tampered).unwrap_err();
        assert!(matches!(err, AgentError::InvalidToken));
    }

    #[test]
    fn tampered_expiry_rejected() {
        let iss = issuer();
        let id = AgentId::new("agt_abc").unwrap();
        let token = iss.issue(&id, 3600).unwrap();
        // Extend the expiry by swapping in a larger number.
        let parts: Vec<&str> = token.token.splitn(3, '|').collect();
        let tampered = format!("{}|99999999999|{}", parts[0], parts[2]);
        let err = iss.verify(&tampered).unwrap_err();
        assert!(matches!(err, AgentError::InvalidToken));
    }

    // ── HmacTokenIssuer — malformed inputs ────────────────────

    #[test]
    fn empty_token_rejected() {
        let err = issuer().verify("").unwrap_err();
        assert!(matches!(err, AgentError::InvalidToken));
    }

    #[test]
    fn token_missing_expiry_rejected() {
        // Only one `|`, so splitn gives two parts instead of three.
        let err = issuer().verify("agt_abc|").unwrap_err();
        assert!(matches!(err, AgentError::InvalidToken));
    }

    #[test]
    fn non_numeric_expiry_rejected() {
        let err = issuer()
            .verify("agt_abc|not-a-number|deadbeef")
            .unwrap_err();
        assert!(matches!(err, AgentError::InvalidToken));
    }
}
