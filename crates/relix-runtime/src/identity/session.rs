//! Per-session token issuance / verify / revocation.
//!
//! Tokens are CBOR-encoded `SessionToken` structs signed with
//! HMAC-SHA256 over the serialised body. Operators configure
//! the HMAC key via `signing_key_env`. The on-the-wire form
//! is base64url(cbor(body) || hmac_sha256_tag).

use std::path::Path;
use std::sync::{Arc, Mutex};

use hmac::{Hmac, Mac};
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// `[identity.session]` config block.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SessionIdentityConfig {
    /// Master switch. `false` (the default) keeps the
    /// DispatchBridge token-less.
    #[serde(default)]
    pub enabled: bool,
    /// Env var the runtime reads to source the HMAC key.
    /// Defaults to `RELIX_SESSION_SIGNING_KEY`.
    #[serde(default = "default_signing_key_env")]
    pub signing_key_env: String,
    /// Token TTL in seconds. Defaults to 3600 (1h).
    #[serde(default = "default_session_ttl_secs")]
    pub session_ttl_secs: u64,
    /// Idle timeout in seconds. When a token hasn't been
    /// `last_seen_ms`-touched for this long, the background
    /// sweeper revokes it. Defaults to 1800 (30m).
    #[serde(default = "default_session_idle_timeout_secs")]
    pub session_idle_timeout_secs: u64,
    /// When `true`, every `DispatchBridge` call checks the
    /// caller's bundle for a valid token. When `false`
    /// (the default) the bridge runs without verification —
    /// existing deployments stay byte-identical.
    #[serde(default)]
    pub verify_on_dispatch: bool,
    /// SQLite path for the token vault.
    #[serde(default)]
    pub db_path: Option<std::path::PathBuf>,
    /// How often the idle-timeout sweeper wakes up. Defaults
    /// to 60s.
    #[serde(default = "default_sweep_interval_secs")]
    pub sweep_interval_secs: u64,
}

impl Default for SessionIdentityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            signing_key_env: default_signing_key_env(),
            session_ttl_secs: default_session_ttl_secs(),
            session_idle_timeout_secs: default_session_idle_timeout_secs(),
            verify_on_dispatch: false,
            db_path: None,
            sweep_interval_secs: default_sweep_interval_secs(),
        }
    }
}

fn default_signing_key_env() -> String {
    "RELIX_SESSION_SIGNING_KEY".into()
}

fn default_session_ttl_secs() -> u64 {
    3600
}

fn default_session_idle_timeout_secs() -> u64 {
    1800
}

fn default_sweep_interval_secs() -> u64 {
    60
}

/// One signed session token. The HMAC tag is over the CBOR
/// serialisation of every field EXCEPT `signature`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionToken {
    pub session_id: String,
    pub agent_name: String,
    pub tenant_id: String,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub scopes: Vec<String>,
    /// 16-byte hex random — defeats replay across deployments
    /// with the same HMAC key.
    pub nonce: String,
    /// HMAC-SHA256 hex over the CBOR body without this field.
    pub signature: String,
}

impl SessionToken {
    /// Build the signature payload — the CBOR encoding of the
    /// struct with `signature = ""`. Tests + verify_token both
    /// call this to compute the canonical pre-image.
    fn canonical_bytes(&self) -> Result<Vec<u8>, TokenError> {
        let mut clone = self.clone();
        clone.signature.clear();
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&clone, &mut buf)
            .map_err(|e| TokenError::Serialization(e.to_string()))?;
        Ok(buf)
    }

    /// Wire-format encoder: base64url(cbor(body)). Operators
    /// pass the resulting string to `identity.verify_token`.
    pub fn to_wire(&self) -> Result<String, TokenError> {
        use base64::Engine;
        let mut buf = Vec::new();
        ciborium::ser::into_writer(self, &mut buf)
            .map_err(|e| TokenError::Serialization(e.to_string()))?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf))
    }

    /// Decode the wire format. Does NOT verify the signature.
    pub fn from_wire(s: &str) -> Result<Self, TokenError> {
        use base64::Engine;
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s.trim())
            .map_err(|e| TokenError::Serialization(format!("decode base64url: {e}")))?;
        let tok: SessionToken = ciborium::de::from_reader(raw.as_slice())
            .map_err(|e| TokenError::Serialization(format!("decode cbor: {e}")))?;
        Ok(tok)
    }
}

/// The lightweight summary surfaced by `identity.active_tokens`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSummary {
    pub token_id: String,
    pub session_id: String,
    pub agent_name: String,
    pub tenant_id: String,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub last_seen_ms: Option<i64>,
    pub revoked: bool,
    pub revoked_at_ms: Option<i64>,
    pub scopes: Vec<String>,
}

/// What `identity.verify_token` returns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenVerification {
    pub valid: bool,
    pub session_id: Option<String>,
    pub agent_name: Option<String>,
    pub tenant_id: Option<String>,
    pub scopes: Vec<String>,
    pub expires_at_ms: Option<i64>,
    pub reason: Option<String>,
}

impl TokenVerification {
    pub fn invalid(reason: impl Into<String>) -> Self {
        Self {
            valid: false,
            session_id: None,
            agent_name: None,
            tenant_id: None,
            scopes: Vec::new(),
            expires_at_ms: None,
            reason: Some(reason.into()),
        }
    }

    pub fn ok(token: &SessionToken) -> Self {
        Self {
            valid: true,
            session_id: Some(token.session_id.clone()),
            agent_name: Some(token.agent_name.clone()),
            tenant_id: Some(token.tenant_id.clone()),
            scopes: token.scopes.clone(),
            expires_at_ms: Some(token.expires_at_ms),
            reason: None,
        }
    }
}

/// Operator-supplied issuance request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IssueRequest {
    pub session_id: String,
    pub agent_name: String,
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Override the configured TTL. `None` honours
    /// `SessionIdentityConfig::session_ttl_secs`.
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Error)]
pub enum TokenError {
    #[error("identity: sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("identity: serialization: {0}")]
    Serialization(String),
    #[error("identity: signing key must be at least 32 bytes; got {0}")]
    InvalidSigningKey(usize),
    #[error("identity: token not found")]
    NotFound,
    #[error("identity: lock poisoned")]
    Lock,
}

/// SQLite-backed token + blocklist store.
#[derive(Clone)]
pub struct TokenStore {
    conn: Arc<Mutex<Connection>>,
}

impl TokenStore {
    pub fn open(path: &Path) -> Result<Self, TokenError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::log_integrity_warning(&conn, "session_tokens");
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Self, TokenError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn migrate(conn: &Connection) -> Result<(), TokenError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_tokens (\
                 token_id      TEXT PRIMARY KEY,\
                 session_id    TEXT NOT NULL,\
                 agent_name    TEXT NOT NULL,\
                 tenant_id     TEXT NOT NULL DEFAULT '',\
                 issued_at_ms  INTEGER NOT NULL,\
                 expires_at_ms INTEGER NOT NULL,\
                 scopes_json   TEXT NOT NULL DEFAULT '[]',\
                 revoked       INTEGER NOT NULL DEFAULT 0,\
                 revoked_at_ms INTEGER,\
                 last_seen_ms  INTEGER\
             );\
             CREATE INDEX IF NOT EXISTS session_tokens_session_idx \
                 ON session_tokens(session_id);\
             CREATE INDEX IF NOT EXISTS session_tokens_agent_idx \
                 ON session_tokens(agent_name);",
        )?;
        Ok(())
    }

    pub fn insert(&self, token: &SessionToken, token_id: &str) -> Result<(), TokenError> {
        let conn = self.lock()?;
        let scopes_json = serde_json::to_string(&token.scopes)
            .map_err(|e| TokenError::Serialization(e.to_string()))?;
        conn.execute(
            "INSERT INTO session_tokens \
             (token_id, session_id, agent_name, tenant_id, issued_at_ms, expires_at_ms, \
              scopes_json, revoked, revoked_at_ms, last_seen_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, NULL, NULL)",
            params![
                token_id,
                token.session_id,
                token.agent_name,
                token.tenant_id,
                token.issued_at_ms,
                token.expires_at_ms,
                scopes_json,
            ],
        )?;
        Ok(())
    }

    pub fn revoke(&self, session_id: &str, revoked_at_ms: i64) -> Result<usize, TokenError> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE session_tokens SET revoked = 1, revoked_at_ms = ?1 \
             WHERE session_id = ?2 AND revoked = 0",
            params![revoked_at_ms, session_id],
        )?;
        Ok(n)
    }

    pub fn touch(&self, token_id: &str, last_seen_ms: i64) -> Result<(), TokenError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE session_tokens SET last_seen_ms = ?1 WHERE token_id = ?2 AND revoked = 0",
            params![last_seen_ms, token_id],
        )?;
        Ok(())
    }

    pub fn is_revoked(&self, token_id: &str) -> Result<bool, TokenError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT revoked FROM session_tokens WHERE token_id = ?1",
            params![token_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(TokenError::from)
        .map(|opt| opt.map(|v| v != 0).unwrap_or(true))
    }

    pub fn list(&self, agent_name_filter: Option<&str>) -> Result<Vec<TokenSummary>, TokenError> {
        let conn = self.lock()?;
        let mut stmt = if agent_name_filter.is_some() {
            conn.prepare(
                "SELECT token_id, session_id, agent_name, tenant_id, issued_at_ms, \
                        expires_at_ms, scopes_json, revoked, revoked_at_ms, last_seen_ms \
                 FROM session_tokens WHERE agent_name = ?1 \
                 ORDER BY issued_at_ms DESC, token_id ASC",
            )?
        } else {
            conn.prepare(
                "SELECT token_id, session_id, agent_name, tenant_id, issued_at_ms, \
                        expires_at_ms, scopes_json, revoked, revoked_at_ms, last_seen_ms \
                 FROM session_tokens ORDER BY issued_at_ms DESC, token_id ASC",
            )?
        };
        let rows: Vec<TokenSummary> = if let Some(a) = agent_name_filter {
            stmt.query_map(params![a], row_to_summary)?
                .collect::<Result<_, _>>()?
        } else {
            stmt.query_map([], row_to_summary)?
                .collect::<Result<_, _>>()?
        };
        Ok(rows)
    }

    /// Revoke every active token whose `last_seen_ms` is older
    /// than `idle_cutoff_ms`. Tokens that have never been
    /// `touch()`-ed are compared against their `issued_at_ms`
    /// so a token nobody ever used still ages out.
    pub fn revoke_idle(
        &self,
        idle_cutoff_ms: i64,
        revoked_at_ms: i64,
    ) -> Result<Vec<String>, TokenError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT token_id FROM session_tokens \
             WHERE revoked = 0 \
                   AND COALESCE(last_seen_ms, issued_at_ms) <= ?1",
        )?;
        let to_revoke: Vec<String> = stmt
            .query_map(params![idle_cutoff_ms], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        for id in &to_revoke {
            conn.execute(
                "UPDATE session_tokens SET revoked = 1, revoked_at_ms = ?1 \
                 WHERE token_id = ?2 AND revoked = 0",
                params![revoked_at_ms, id],
            )?;
        }
        Ok(to_revoke)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, TokenError> {
        self.conn.lock().map_err(|_| TokenError::Lock)
    }
}

fn row_to_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<TokenSummary> {
    let scopes_json: String = row.get(6)?;
    let scopes: Vec<String> = serde_json::from_str(&scopes_json).unwrap_or_default();
    Ok(TokenSummary {
        token_id: row.get(0)?,
        session_id: row.get(1)?,
        agent_name: row.get(2)?,
        tenant_id: row.get(3)?,
        issued_at_ms: row.get(4)?,
        expires_at_ms: row.get(5)?,
        scopes,
        revoked: row.get::<_, i64>(7)? != 0,
        revoked_at_ms: row.get(8)?,
        last_seen_ms: row.get(9)?,
    })
}

/// The service — cheap to clone.
#[derive(Clone)]
pub struct SessionIdentityService {
    store: TokenStore,
    cfg: Arc<SessionIdentityConfig>,
    signing_key: Arc<Vec<u8>>,
}

impl SessionIdentityService {
    pub fn new(
        store: TokenStore,
        cfg: SessionIdentityConfig,
        signing_key: Vec<u8>,
    ) -> Result<Self, TokenError> {
        if signing_key.len() < 32 {
            return Err(TokenError::InvalidSigningKey(signing_key.len()));
        }
        Ok(Self {
            store,
            cfg: Arc::new(cfg),
            signing_key: Arc::new(signing_key),
        })
    }

    pub fn store(&self) -> &TokenStore {
        &self.store
    }

    pub fn config(&self) -> &SessionIdentityConfig {
        &self.cfg
    }

    /// Issue a fresh signed token + persist a row to the
    /// vault. The wire form returned by `to_wire()` is what
    /// the caller hands to verify.
    pub fn issue(&self, req: &IssueRequest) -> Result<SessionToken, TokenError> {
        let now = unix_ms();
        let ttl = req.ttl_secs.unwrap_or(self.cfg.session_ttl_secs).max(1);
        let mut nonce_bytes = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = hex::encode(nonce_bytes);
        let mut token = SessionToken {
            session_id: req.session_id.clone(),
            agent_name: req.agent_name.clone(),
            tenant_id: req.tenant_id.clone().unwrap_or_default(),
            issued_at_ms: now,
            expires_at_ms: now + (ttl as i64) * 1000,
            scopes: req.scopes.clone(),
            nonce,
            signature: String::new(),
        };
        let canonical = token.canonical_bytes()?;
        token.signature = self.sign(&canonical);
        let token_id = format!("tok_{}", uuid::Uuid::new_v4().simple());
        self.store.insert(&token, &token_id)?;
        Ok(token)
    }

    /// Verify a wire-encoded token. Returns a structured
    /// verdict carrying the failure reason on the unhappy
    /// path so the cap surface can return something
    /// operator-actionable.
    pub fn verify(&self, wire: &str) -> TokenVerification {
        let tok = match SessionToken::from_wire(wire) {
            Ok(t) => t,
            Err(e) => return TokenVerification::invalid(format!("decode: {e}")),
        };
        let canonical = match tok.canonical_bytes() {
            Ok(b) => b,
            Err(e) => return TokenVerification::invalid(format!("canonical: {e}")),
        };
        if !self.verify_signature(&canonical, &tok.signature) {
            return TokenVerification::invalid("signature mismatch");
        }
        let now = unix_ms();
        if now >= tok.expires_at_ms {
            return TokenVerification::invalid("token expired");
        }
        // Blocklist check — every token whose session is on
        // the revoked list (or never inserted) fails verify.
        let token_id_match = self
            .store
            .list(Some(&tok.agent_name))
            .ok()
            .and_then(|rows| rows.into_iter().find(|r| r.session_id == tok.session_id));
        let Some(row) = token_id_match else {
            return TokenVerification::invalid("token id unknown");
        };
        if row.revoked {
            return TokenVerification::invalid("token revoked");
        }
        // Touch last_seen so the idle-timeout sweeper can
        // identify dormant tokens.
        let _ = self.store.touch(&row.token_id, now);
        TokenVerification::ok(&tok)
    }

    pub fn revoke(&self, session_id: &str) -> Result<usize, TokenError> {
        self.store.revoke(session_id, unix_ms())
    }

    pub fn list_active(
        &self,
        agent_name_filter: Option<&str>,
    ) -> Result<Vec<TokenSummary>, TokenError> {
        self.store.list(agent_name_filter)
    }

    /// Spawn the idle-timeout sweeper. Returns immediately;
    /// the background task wakes every `sweep_interval_secs`
    /// and revokes tokens whose `last_seen_ms` is older than
    /// `now - session_idle_timeout_secs * 1000`.
    pub fn spawn_idle_sweeper(self) {
        let interval = std::time::Duration::from_secs(self.cfg.sweep_interval_secs.max(5));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let cutoff = unix_ms() - (self.cfg.session_idle_timeout_secs as i64) * 1000;
                let now = unix_ms();
                match self.store.revoke_idle(cutoff, now) {
                    Ok(revoked) if !revoked.is_empty() => {
                        tracing::info!(
                            count = revoked.len(),
                            "identity: idle-timeout sweep revoked stale tokens"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "identity: idle-timeout sweep failed");
                    }
                }
            }
        });
    }

    fn sign(&self, payload: &[u8]) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.signing_key).expect("HMAC accepts any key length");
        mac.update(payload);
        hex::encode(mac.finalize().into_bytes())
    }

    fn verify_signature(&self, payload: &[u8], sig_hex: &str) -> bool {
        let Ok(sig) = hex::decode(sig_hex) else {
            return false;
        };
        let mut mac =
            HmacSha256::new_from_slice(&self.signing_key).expect("HMAC accepts any key length");
        mac.update(payload);
        mac.verify_slice(&sig).is_ok()
    }
}

pub(crate) fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_service() -> SessionIdentityService {
        let store = TokenStore::open_in_memory().unwrap();
        let cfg = SessionIdentityConfig {
            enabled: true,
            session_ttl_secs: 60,
            session_idle_timeout_secs: 5,
            sweep_interval_secs: 60,
            verify_on_dispatch: false,
            ..Default::default()
        };
        SessionIdentityService::new(store, cfg, vec![7u8; 32]).unwrap()
    }

    fn fixture_request() -> IssueRequest {
        IssueRequest {
            session_id: "sess1".into(),
            agent_name: "alice".into(),
            tenant_id: Some("acme".into()),
            scopes: vec!["ai.chat".into(), "tool.fs.read".into()],
            ttl_secs: None,
        }
    }

    #[test]
    fn issue_returns_token_with_correct_fields_and_valid_hmac() {
        let svc = fresh_service();
        let tok = svc.issue(&fixture_request()).unwrap();
        assert_eq!(tok.session_id, "sess1");
        assert_eq!(tok.agent_name, "alice");
        assert_eq!(tok.tenant_id, "acme");
        assert_eq!(tok.scopes, vec!["ai.chat", "tool.fs.read"]);
        assert!(tok.expires_at_ms > tok.issued_at_ms);
        assert_eq!(tok.nonce.len(), 32);
        assert_eq!(tok.signature.len(), 64);
        // Signature must round-trip the canonical encoding.
        let canonical = tok.canonical_bytes().unwrap();
        assert!(svc.verify_signature(&canonical, &tok.signature));
    }

    #[test]
    fn verify_token_returns_valid_for_fresh_token() {
        let svc = fresh_service();
        let tok = svc.issue(&fixture_request()).unwrap();
        let wire = tok.to_wire().unwrap();
        let v = svc.verify(&wire);
        assert!(v.valid, "expected valid; got {v:?}");
        assert_eq!(v.session_id.as_deref(), Some("sess1"));
        assert_eq!(v.agent_name.as_deref(), Some("alice"));
    }

    #[test]
    fn verify_token_returns_invalid_for_expired_token() {
        let svc = fresh_service();
        let mut tok = svc.issue(&fixture_request()).unwrap();
        // Forge the expiry into the past + re-sign so the
        // signature itself is valid; only the timestamp is
        // stale.
        tok.expires_at_ms = tok.issued_at_ms - 1;
        let canonical = tok.canonical_bytes().unwrap();
        tok.signature = svc.sign(&canonical);
        let wire = tok.to_wire().unwrap();
        let v = svc.verify(&wire);
        assert!(!v.valid);
        assert!(v.reason.as_deref().unwrap().contains("expired"));
    }

    #[test]
    fn verify_token_returns_invalid_for_revoked_token() {
        let svc = fresh_service();
        let tok = svc.issue(&fixture_request()).unwrap();
        svc.revoke("sess1").unwrap();
        let wire = tok.to_wire().unwrap();
        let v = svc.verify(&wire);
        assert!(!v.valid);
        assert!(v.reason.as_deref().unwrap().contains("revoked"));
    }

    #[test]
    fn verify_token_returns_invalid_for_tampered_signature() {
        let svc = fresh_service();
        let mut tok = svc.issue(&fixture_request()).unwrap();
        // Flip one hex digit of the signature.
        let mut chars: Vec<char> = tok.signature.chars().collect();
        chars[0] = if chars[0] == '0' { '1' } else { '0' };
        tok.signature = chars.into_iter().collect();
        let wire = tok.to_wire().unwrap();
        let v = svc.verify(&wire);
        assert!(!v.valid);
        assert!(v.reason.as_deref().unwrap().contains("signature"));
    }

    #[test]
    fn revoke_marks_blocklist_idempotently() {
        let svc = fresh_service();
        let _ = svc.issue(&fixture_request()).unwrap();
        let n = svc.revoke("sess1").unwrap();
        assert_eq!(n, 1);
        let n2 = svc.revoke("sess1").unwrap();
        assert_eq!(n2, 0, "second revoke is a no-op");
    }

    #[test]
    fn list_active_filters_by_agent() {
        let svc = fresh_service();
        let _ = svc.issue(&fixture_request()).unwrap();
        let _ = svc
            .issue(&IssueRequest {
                session_id: "sess2".into(),
                agent_name: "bob".into(),
                tenant_id: None,
                scopes: vec![],
                ttl_secs: None,
            })
            .unwrap();
        let alice = svc.list_active(Some("alice")).unwrap();
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].agent_name, "alice");
        let all = svc.list_active(None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn revoke_idle_revokes_old_tokens() {
        let svc = fresh_service();
        let _ = svc.issue(&fixture_request()).unwrap();
        // Issued at now — last_seen NULL → COALESCE picks
        // issued_at. Pass cutoff = now + 1ms so the token
        // qualifies.
        let cutoff = unix_ms() + 1;
        let now_for_revoke = unix_ms() + 2;
        let revoked = svc.store.revoke_idle(cutoff, now_for_revoke).unwrap();
        assert_eq!(revoked.len(), 1);
    }
}
