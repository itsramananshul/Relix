//! SQLite-backed encrypted credential store.

use std::path::Path;
use std::sync::{Arc, Mutex};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Kind tag stored alongside the value. Operators read these
/// off the `credentials.list` cap to filter by purpose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    #[default]
    ApiKey,
    Token,
    Secret,
    OAuthRefresh,
    Other,
}

impl CredentialKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::Token => "token",
            Self::Secret => "secret",
            Self::OAuthRefresh => "oauth_refresh",
            Self::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "api_key" => Self::ApiKey,
            "token" => Self::Token,
            "secret" => Self::Secret,
            "oauth_refresh" => Self::OAuthRefresh,
            _ => Self::Other,
        }
    }
}

/// Full credential row. The `value_encrypted` field is never
/// surfaced past the store boundary — operators see
/// `CredentialSummary` instead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    pub id: String,
    pub name: String,
    pub kind: CredentialKind,
    pub owner_agent: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub last_rotated_at_ms: Option<i64>,
    pub rotation_interval_secs: Option<u64>,
    pub next_rotation_at_ms: Option<i64>,
    pub revoked: bool,
    pub revoked_at_ms: Option<i64>,
    pub revoke_reason: Option<String>,
    pub version: u32,
}

/// What `credentials.list` returns. Never carries the
/// encrypted blob.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialSummary {
    pub name: String,
    pub kind: CredentialKind,
    pub owner_agent: Option<String>,
    pub created_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub last_rotated_at_ms: Option<i64>,
    pub next_rotation_at_ms: Option<i64>,
    pub revoked: bool,
    pub version: u32,
}

impl From<&Credential> for CredentialSummary {
    fn from(c: &Credential) -> Self {
        Self {
            name: c.name.clone(),
            kind: c.kind,
            owner_agent: c.owner_agent.clone(),
            created_at_ms: c.created_at_ms,
            expires_at_ms: c.expires_at_ms,
            last_rotated_at_ms: c.last_rotated_at_ms,
            next_rotation_at_ms: c.next_rotation_at_ms,
            revoked: c.revoked,
            version: c.version,
        }
    }
}

/// What `credentials.get` returns to authorised callers. Plain
/// value, never persisted in this form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecryptedCredential {
    pub name: String,
    pub kind: CredentialKind,
    pub owner_agent: Option<String>,
    pub value: String,
    pub version: u32,
}

/// One row in the audit table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRow {
    pub id: String,
    pub credential_id: String,
    pub event: AuditEvent,
    pub actor: Option<String>,
    pub timestamp_ms: i64,
    pub details: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEvent {
    Stored,
    Accessed,
    Rotated,
    Revoked,
}

impl AuditEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stored => "stored",
            Self::Accessed => "accessed",
            Self::Rotated => "rotated",
            Self::Revoked => "revoked",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stored" => Some(Self::Stored),
            "accessed" => Some(Self::Accessed),
            "rotated" => Some(Self::Rotated),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// Encrypted ciphertext + nonce in base64 form. Stored as a
/// single column so the store survives `value` rotation
/// without schema gymnastics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedValue {
    pub nonce_b64: String,
    pub ciphertext_b64: String,
}

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("credentials: sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("credentials: serialization: {0}")]
    Serialization(String),
    #[error("credentials: encryption: {0}")]
    Encryption(String),
    #[error("credentials: lock poisoned")]
    Lock,
    #[error("credentials: credential `{0}` not found")]
    NotFound(String),
    #[error("credentials: credential `{0}` is revoked")]
    Revoked(String),
    #[error("credentials: credential `{0}` is expired")]
    Expired(String),
    #[error("credentials: master key must be 32 bytes; got {0} bytes")]
    InvalidMasterKey(usize),
}

/// SQLite-backed encrypted vault. Cheap to clone.
#[derive(Clone)]
pub struct CredentialStore {
    conn: Arc<Mutex<Connection>>,
    key: Arc<[u8; 32]>,
}

impl CredentialStore {
    /// Open (or create) the store at `path` and derive the
    /// AES key from `master_secret`. The secret is passed
    /// through SHA-256 to produce the 32-byte key — operators
    /// can supply any-length entropy.
    pub fn open(path: &Path, master_secret: &str) -> Result<Self, CredentialError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::log_integrity_warning(&conn, "credentials");
        Self::migrate(&conn)?;
        let key = derive_key(master_secret);
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            key: Arc::new(key),
        })
    }

    /// Open an in-memory store. Tests + the dry-run cap path.
    pub fn open_in_memory(master_secret: &str) -> Result<Self, CredentialError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        Self::migrate(&conn)?;
        let key = derive_key(master_secret);
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            key: Arc::new(key),
        })
    }

    fn migrate(conn: &Connection) -> Result<(), CredentialError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS credentials (\
                 id                     TEXT PRIMARY KEY,\
                 name                   TEXT NOT NULL UNIQUE,\
                 value_encrypted        TEXT NOT NULL,\
                 kind                   TEXT NOT NULL DEFAULT 'api_key',\
                 owner_agent            TEXT,\
                 created_at_ms          INTEGER NOT NULL,\
                 updated_at_ms          INTEGER NOT NULL,\
                 expires_at_ms          INTEGER,\
                 last_rotated_at_ms     INTEGER,\
                 rotation_interval_secs INTEGER,\
                 next_rotation_at_ms    INTEGER,\
                 revoked                INTEGER NOT NULL DEFAULT 0,\
                 revoked_at_ms          INTEGER,\
                 revoke_reason          TEXT,\
                 version                INTEGER NOT NULL DEFAULT 1\
             );\
             CREATE TABLE IF NOT EXISTS credential_audit (\
                 id              TEXT PRIMARY KEY,\
                 credential_id   TEXT NOT NULL,\
                 event           TEXT NOT NULL,\
                 actor           TEXT,\
                 timestamp_ms    INTEGER NOT NULL,\
                 details         TEXT\
             );\
             CREATE INDEX IF NOT EXISTS credential_audit_cred_idx \
                 ON credential_audit(credential_id, timestamp_ms);\
             CREATE INDEX IF NOT EXISTS credentials_owner_idx \
                 ON credentials(owner_agent);",
        )?;
        Ok(())
    }

    /// Encrypt + insert a new credential. Returns the row.
    #[allow(clippy::too_many_arguments)]
    pub fn store(
        &self,
        name: &str,
        value: &str,
        kind: CredentialKind,
        owner_agent: Option<&str>,
        expires_at_ms: Option<i64>,
        rotation_interval_secs: Option<u64>,
        actor: Option<&str>,
    ) -> Result<Credential, CredentialError> {
        if name.trim().is_empty() {
            return Err(CredentialError::Serialization(
                "credential name is required".into(),
            ));
        }
        let now = unix_ms();
        let id = format!("cred_{}", uuid::Uuid::new_v4().simple());
        let encrypted = encrypt(&self.key, value)?;
        let encrypted_json = serde_json::to_string(&encrypted)
            .map_err(|e| CredentialError::Serialization(e.to_string()))?;
        let next_rot = rotation_interval_secs.map(|s| now + (s as i64) * 1000);
        {
            let conn = self.lock()?;
            conn.execute(
                "INSERT INTO credentials \
                 (id, name, value_encrypted, kind, owner_agent, created_at_ms, updated_at_ms, \
                  expires_at_ms, last_rotated_at_ms, rotation_interval_secs, next_rotation_at_ms, \
                  revoked, revoked_at_ms, revoke_reason, version) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?10, 0, NULL, NULL, 1)",
                params![
                    id,
                    name,
                    encrypted_json,
                    kind.as_str(),
                    owner_agent,
                    now,
                    now,
                    expires_at_ms,
                    rotation_interval_secs.map(|s| s as i64),
                    next_rot,
                ],
            )?;
        }
        let cred = self
            .get_row_by_id(&id)?
            .ok_or(CredentialError::NotFound(name.to_string()))?;
        self.audit(&id, AuditEvent::Stored, actor, None)?;
        Ok(cred)
    }

    /// Decrypt the value when the credential is active.
    /// Returns `Ok(None)` when revoked or expired so callers
    /// see "credential is gone" rather than "credential
    /// doesn't exist" (the spec contract).
    pub fn get(
        &self,
        name: &str,
        actor: Option<&str>,
    ) -> Result<Option<DecryptedCredential>, CredentialError> {
        let row = match self.get_row_by_name(name)? {
            Some(r) => r,
            None => return Ok(None),
        };
        if row.revoked {
            return Ok(None);
        }
        if let Some(exp) = row.expires_at_ms
            && exp <= unix_ms()
        {
            return Ok(None);
        }
        let encrypted: EncryptedValue = serde_json::from_str(&self.row_encrypted_json(&row.id)?)
            .map_err(|e| CredentialError::Serialization(e.to_string()))?;
        let plaintext = decrypt(&self.key, &encrypted)?;
        self.audit(&row.id, AuditEvent::Accessed, actor, None)?;
        Ok(Some(DecryptedCredential {
            name: row.name.clone(),
            kind: row.kind,
            owner_agent: row.owner_agent.clone(),
            value: plaintext,
            version: row.version,
        }))
    }

    /// Increment version + replace the value.
    pub fn rotate(
        &self,
        name: &str,
        new_value: &str,
        actor: Option<&str>,
    ) -> Result<Credential, CredentialError> {
        let row = self
            .get_row_by_name(name)?
            .ok_or_else(|| CredentialError::NotFound(name.to_string()))?;
        if row.revoked {
            return Err(CredentialError::Revoked(name.to_string()));
        }
        let now = unix_ms();
        let encrypted = encrypt(&self.key, new_value)?;
        let encrypted_json = serde_json::to_string(&encrypted)
            .map_err(|e| CredentialError::Serialization(e.to_string()))?;
        let next_rot = row.rotation_interval_secs.map(|s| now + (s as i64) * 1000);
        {
            let conn = self.lock()?;
            conn.execute(
                "UPDATE credentials \
                 SET value_encrypted = ?1, updated_at_ms = ?2, last_rotated_at_ms = ?2, \
                     next_rotation_at_ms = ?3, version = version + 1 \
                 WHERE id = ?4",
                params![encrypted_json, now, next_rot, row.id],
            )?;
        }
        let cred = self
            .get_row_by_id(&row.id)?
            .ok_or(CredentialError::NotFound(name.to_string()))?;
        self.audit(&row.id, AuditEvent::Rotated, actor, None)?;
        Ok(cred)
    }

    /// Flip the revoked flag + record the reason.
    pub fn revoke(
        &self,
        name: &str,
        reason: Option<&str>,
        actor: Option<&str>,
    ) -> Result<Credential, CredentialError> {
        let row = self
            .get_row_by_name(name)?
            .ok_or_else(|| CredentialError::NotFound(name.to_string()))?;
        if row.revoked {
            return Ok(row);
        }
        let now = unix_ms();
        {
            let conn = self.lock()?;
            conn.execute(
                "UPDATE credentials \
                 SET revoked = 1, revoked_at_ms = ?1, revoke_reason = ?2, updated_at_ms = ?1 \
                 WHERE id = ?3",
                params![now, reason, row.id],
            )?;
        }
        let cred = self
            .get_row_by_id(&row.id)?
            .ok_or(CredentialError::NotFound(name.to_string()))?;
        self.audit(&row.id, AuditEvent::Revoked, actor, reason)?;
        Ok(cred)
    }

    /// List summaries, optionally filtered by owner_agent.
    pub fn list(
        &self,
        owner_agent: Option<&str>,
    ) -> Result<Vec<CredentialSummary>, CredentialError> {
        let conn = self.lock()?;
        let mut stmt = if owner_agent.is_some() {
            conn.prepare(
                "SELECT id, name, kind, owner_agent, created_at_ms, updated_at_ms, \
                        expires_at_ms, last_rotated_at_ms, rotation_interval_secs, \
                        next_rotation_at_ms, revoked, revoked_at_ms, revoke_reason, version \
                 FROM credentials WHERE owner_agent = ?1 \
                 ORDER BY created_at_ms DESC, name ASC",
            )?
        } else {
            conn.prepare(
                "SELECT id, name, kind, owner_agent, created_at_ms, updated_at_ms, \
                        expires_at_ms, last_rotated_at_ms, rotation_interval_secs, \
                        next_rotation_at_ms, revoked, revoked_at_ms, revoke_reason, version \
                 FROM credentials \
                 ORDER BY created_at_ms DESC, name ASC",
            )?
        };
        let rows: Vec<Credential> = if let Some(o) = owner_agent {
            stmt.query_map(params![o], row_to_credential)?
                .collect::<Result<_, _>>()?
        } else {
            stmt.query_map([], row_to_credential)?
                .collect::<Result<_, _>>()?
        };
        Ok(rows.iter().map(CredentialSummary::from).collect())
    }

    /// Return audit rows for one credential, chronological
    /// ascending. `limit = 0` falls back to a sane default.
    pub fn audit_rows(&self, name: &str, limit: usize) -> Result<Vec<AuditRow>, CredentialError> {
        let row = self
            .get_row_by_name(name)?
            .ok_or_else(|| CredentialError::NotFound(name.to_string()))?;
        let limit_i = if limit == 0 {
            100
        } else {
            limit.clamp(1, 5000)
        } as i64;
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, credential_id, event, actor, timestamp_ms, details \
             FROM credential_audit WHERE credential_id = ?1 \
             ORDER BY timestamp_ms ASC, rowid ASC LIMIT ?2",
        )?;
        let rows: Vec<AuditRow> = stmt
            .query_map(params![row.id, limit_i], row_to_audit)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// List every credential whose `next_rotation_at_ms` is at-
    /// or-past `now_ms` AND that isn't revoked. The rotation
    /// scheduler walks this set to emit notifications.
    pub fn due_for_rotation(&self, now_ms: i64) -> Result<Vec<Credential>, CredentialError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, kind, owner_agent, created_at_ms, updated_at_ms, \
                    expires_at_ms, last_rotated_at_ms, rotation_interval_secs, \
                    next_rotation_at_ms, revoked, revoked_at_ms, revoke_reason, version \
             FROM credentials \
             WHERE revoked = 0 AND next_rotation_at_ms IS NOT NULL \
                   AND next_rotation_at_ms <= ?1 \
             ORDER BY next_rotation_at_ms ASC, name ASC",
        )?;
        let rows: Vec<Credential> = stmt
            .query_map(params![now_ms], row_to_credential)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    fn get_row_by_id(&self, id: &str) -> Result<Option<Credential>, CredentialError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, name, kind, owner_agent, created_at_ms, updated_at_ms, \
                    expires_at_ms, last_rotated_at_ms, rotation_interval_secs, \
                    next_rotation_at_ms, revoked, revoked_at_ms, revoke_reason, version \
             FROM credentials WHERE id = ?1",
            params![id],
            row_to_credential,
        )
        .optional()
        .map_err(Into::into)
    }

    fn get_row_by_name(&self, name: &str) -> Result<Option<Credential>, CredentialError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, name, kind, owner_agent, created_at_ms, updated_at_ms, \
                    expires_at_ms, last_rotated_at_ms, rotation_interval_secs, \
                    next_rotation_at_ms, revoked, revoked_at_ms, revoke_reason, version \
             FROM credentials WHERE name = ?1",
            params![name],
            row_to_credential,
        )
        .optional()
        .map_err(Into::into)
    }

    fn row_encrypted_json(&self, id: &str) -> Result<String, CredentialError> {
        let conn = self.lock()?;
        let v: String = conn.query_row(
            "SELECT value_encrypted FROM credentials WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        Ok(v)
    }

    fn audit(
        &self,
        credential_id: &str,
        event: AuditEvent,
        actor: Option<&str>,
        details: Option<&str>,
    ) -> Result<(), CredentialError> {
        let conn = self.lock()?;
        let id = format!("audit_{}", uuid::Uuid::new_v4().simple());
        conn.execute(
            "INSERT INTO credential_audit (id, credential_id, event, actor, timestamp_ms, details) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, credential_id, event.as_str(), actor, unix_ms(), details],
        )?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, CredentialError> {
        self.conn.lock().map_err(|_| CredentialError::Lock)
    }
}

fn row_to_credential(row: &rusqlite::Row<'_>) -> rusqlite::Result<Credential> {
    let kind_str: String = row.get(2)?;
    Ok(Credential {
        id: row.get(0)?,
        name: row.get(1)?,
        kind: CredentialKind::parse(&kind_str),
        owner_agent: row.get(3)?,
        created_at_ms: row.get(4)?,
        updated_at_ms: row.get(5)?,
        expires_at_ms: row.get(6)?,
        last_rotated_at_ms: row.get(7)?,
        rotation_interval_secs: row.get::<_, Option<i64>>(8)?.map(|v| v as u64),
        next_rotation_at_ms: row.get(9)?,
        revoked: row.get::<_, i64>(10)? != 0,
        revoked_at_ms: row.get(11)?,
        revoke_reason: row.get(12)?,
        version: row.get::<_, i64>(13)? as u32,
    })
}

fn row_to_audit(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditRow> {
    let event_str: String = row.get(2)?;
    Ok(AuditRow {
        id: row.get(0)?,
        credential_id: row.get(1)?,
        event: AuditEvent::parse(&event_str).unwrap_or(AuditEvent::Accessed),
        actor: row.get(3)?,
        timestamp_ms: row.get(4)?,
        details: row.get(5)?,
    })
}

fn derive_key(master_secret: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(master_secret.as_bytes());
    let out = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&out[..32]);
    key
}

fn encrypt(key: &[u8; 32], plaintext: &str) -> Result<EncryptedValue, CredentialError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut nonce_bytes = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| CredentialError::Encryption(format!("encrypt: {e}")))?;
    use base64::Engine;
    Ok(EncryptedValue {
        nonce_b64: base64::engine::general_purpose::STANDARD.encode(nonce_bytes),
        ciphertext_b64: base64::engine::general_purpose::STANDARD.encode(ciphertext),
    })
}

fn decrypt(key: &[u8; 32], enc: &EncryptedValue) -> Result<String, CredentialError> {
    use base64::Engine;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce_bytes = base64::engine::general_purpose::STANDARD
        .decode(&enc.nonce_b64)
        .map_err(|e| CredentialError::Encryption(format!("decode nonce: {e}")))?;
    if nonce_bytes.len() != 12 {
        return Err(CredentialError::Encryption(format!(
            "nonce length {} != 12",
            nonce_bytes.len()
        )));
    }
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(&enc.ciphertext_b64)
        .map_err(|e| CredentialError::Encryption(format!("decode ciphertext: {e}")))?;
    let plaintext = cipher
        .decrypt(nonce, ciphertext.as_slice())
        .map_err(|e| CredentialError::Encryption(format!("decrypt: {e}")))?;
    String::from_utf8(plaintext)
        .map_err(|e| CredentialError::Encryption(format!("plaintext utf-8: {e}")))
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_store() -> CredentialStore {
        CredentialStore::open_in_memory("test-master-secret").unwrap()
    }

    #[test]
    fn round_trip_encrypts_and_decrypts() {
        let s = fresh_store();
        let cred = s
            .store(
                "github_token",
                "ghp_abc",
                CredentialKind::Token,
                Some("alice"),
                None,
                None,
                Some("alice"),
            )
            .unwrap();
        assert_eq!(cred.name, "github_token");
        let plain = s.get("github_token", Some("alice")).unwrap().unwrap();
        assert_eq!(plain.value, "ghp_abc");
    }

    #[test]
    fn store_writes_no_plaintext_to_database() {
        let s = fresh_store();
        s.store(
            "api",
            "supersecret-plain",
            CredentialKind::ApiKey,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let conn = s.lock().unwrap();
        let raw: String = conn
            .query_row(
                "SELECT value_encrypted FROM credentials WHERE name = 'api'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !raw.contains("supersecret-plain"),
            "plaintext leaked into stored row: {raw}"
        );
    }

    #[test]
    fn get_returns_none_for_revoked_credential() {
        let s = fresh_store();
        s.store("k", "v", CredentialKind::ApiKey, None, None, None, None)
            .unwrap();
        s.revoke("k", Some("compromised"), None).unwrap();
        assert!(s.get("k", None).unwrap().is_none());
    }

    #[test]
    fn get_returns_none_for_expired_credential() {
        let s = fresh_store();
        let past = unix_ms() - 1_000;
        s.store(
            "e",
            "v",
            CredentialKind::ApiKey,
            None,
            Some(past),
            None,
            None,
        )
        .unwrap();
        assert!(s.get("e", None).unwrap().is_none());
    }

    #[test]
    fn rotate_increments_version_and_updates_timestamps() {
        let s = fresh_store();
        s.store(
            "r",
            "v1",
            CredentialKind::ApiKey,
            None,
            None,
            Some(3600),
            None,
        )
        .unwrap();
        let r = s.rotate("r", "v2", Some("alice")).unwrap();
        assert_eq!(r.version, 2);
        assert!(r.last_rotated_at_ms.is_some());
        assert!(r.next_rotation_at_ms.is_some());
        let v = s.get("r", None).unwrap().unwrap();
        assert_eq!(v.value, "v2");
        assert_eq!(v.version, 2);
    }

    #[test]
    fn rotate_fails_on_revoked_credential() {
        let s = fresh_store();
        s.store("r", "v1", CredentialKind::ApiKey, None, None, None, None)
            .unwrap();
        s.revoke("r", None, None).unwrap();
        let err = s.rotate("r", "v2", None).unwrap_err();
        assert!(matches!(err, CredentialError::Revoked(_)), "{err}");
    }

    #[test]
    fn list_never_returns_encrypted_blob() {
        let s = fresh_store();
        s.store("k", "v", CredentialKind::ApiKey, None, None, None, None)
            .unwrap();
        let list = s.list(None).unwrap();
        assert_eq!(list.len(), 1);
        // CredentialSummary type doesn't have a value/encrypted
        // field at all — compile-time guarantee. Sanity-check
        // serialisation just in case.
        let json = serde_json::to_string(&list[0]).unwrap();
        assert!(!json.contains("value"), "summary serialised value: {json}");
        assert!(!json.contains("encrypted"));
    }

    #[test]
    fn list_filters_by_owner_agent() {
        let s = fresh_store();
        s.store(
            "a",
            "v",
            CredentialKind::ApiKey,
            Some("alice"),
            None,
            None,
            None,
        )
        .unwrap();
        s.store(
            "b",
            "v",
            CredentialKind::ApiKey,
            Some("bob"),
            None,
            None,
            None,
        )
        .unwrap();
        let alice = s.list(Some("alice")).unwrap();
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].name, "a");
    }

    #[test]
    fn audit_returns_events_in_chronological_order() {
        let s = fresh_store();
        s.store(
            "a",
            "v",
            CredentialKind::ApiKey,
            None,
            None,
            None,
            Some("alice"),
        )
        .unwrap();
        s.get("a", Some("alice")).unwrap();
        s.rotate("a", "v2", Some("alice")).unwrap();
        let rows = s.audit_rows("a", 50).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].event, AuditEvent::Stored);
        assert_eq!(rows[1].event, AuditEvent::Accessed);
        assert_eq!(rows[2].event, AuditEvent::Rotated);
    }

    #[test]
    fn due_for_rotation_returns_only_eligible_rows() {
        let s = fresh_store();
        s.store(
            "on_schedule",
            "v",
            CredentialKind::ApiKey,
            None,
            None,
            Some(60),
            None,
        )
        .unwrap();
        s.store(
            "no_schedule",
            "v",
            CredentialKind::Secret,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let now = unix_ms();
        let due_now = s.due_for_rotation(now - 1).unwrap();
        // The stored credential's next_rotation_at_ms is now+60s,
        // so it is NOT due yet.
        assert!(due_now.is_empty());
        // Fast-forward virtually: ask for time AFTER the next
        // rotation should fire. The on_schedule credential is
        // due; the no_schedule one is not.
        let later = now + 120_000;
        let due_later = s.due_for_rotation(later).unwrap();
        assert_eq!(due_later.len(), 1);
        assert_eq!(due_later[0].name, "on_schedule");
    }

    #[test]
    fn derive_key_is_deterministic_for_same_secret() {
        let a = derive_key("alpha");
        let b = derive_key("alpha");
        assert_eq!(a, b);
        let c = derive_key("beta");
        assert_ne!(a, c);
    }
}
