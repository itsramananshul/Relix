//! SQLite-backed encrypted credential store.
//!
//! ## Key derivation (SEC PART 1)
//!
//! The AES-256-GCM key is derived from a master secret using
//! Argon2id with a per-vault random salt. The salt + the
//! configured parameters (`memory_cost`, `time_cost`,
//! `parallelism`) live in the `vault_metadata` table so the
//! derivation is deterministic across opens of the same vault.
//!
//! The pre-fix SHA-256 derivation (no salt, no work factor)
//! is GONE from the open path. Legacy vaults that lack a
//! `vault_metadata` table are refused with
//! [`CredentialError::LegacyFormat`]; operators run
//! `relix credentials migrate-kdf` to upgrade them. The
//! legacy KDF survives only inside the migration entry point
//! [`CredentialStore::migrate_kdf`] so a stolen v1 database
//! can be decrypted exactly once during the rebuild step.
//!
//! ## Key versioning (SEC PART 7)
//!
//! Each credential row carries a `key_version` column. The
//! active version is the highest-numbered entry in the
//! configured [`KeyVersionMap`] whose env var is set. New
//! credentials encrypt with the active version; existing rows
//! decrypt with whichever version they carry. The
//! `rotate-vault-key` CLI re-encrypts every row from the old
//! active version to the new one atomically.
//!
//! ## In-memory hygiene (SEC PART 2)
//!
//! Derived AES keys live inside [`Zeroizing<[u8; 32]>`] and
//! decrypted plaintext is returned wrapped in
//! [`SecretString`] (a `Zeroizing<String>` newtype with serde
//! impls). On drop the bytes are wiped — a heap inspector
//! attached to a coredump won't recover the value.

use std::collections::BTreeMap;
use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, Mutex};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde::{Deserializer, Serializer};
use thiserror::Error;
use zeroize::Zeroizing;

/// Wire-format version stamped into `vault_metadata`.
/// `1` was the legacy plain-SHA-256-KDF format (never
/// written by this codebase any more — refused with
/// [`CredentialError::LegacyFormat`]).
/// `2` is the Argon2id KDF + per-row key_version layout.
pub const VAULT_FORMAT_VERSION: u32 = 2;

const KDF_SALT_LEN: usize = 32;
const AES_KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const METADATA_SALT_KEY: &str = "kdf_salt";
const METADATA_PARAMS_KEY: &str = "kdf_params";
const METADATA_VERSION_KEY: &str = "vault_version";

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
    /// Tenant-isolation: per-tenant scoping. `None` on rows
    /// written through the tenant-blind `store` path; set by
    /// `store_for_tenant`.
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// SEC PART 7: which entry in [`KeyVersionMap`] this row
    /// is encrypted under. `None` on rows written before
    /// PART 7 shipped — those are treated as the active
    /// version on open (covered by the post-migration test).
    #[serde(default)]
    pub key_version: Option<String>,
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
    /// SEC PART 7: which key version protects this row.
    /// Surfaced so operators can grep the list for rows that
    /// still need rotation after a vault-key rotate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_version: Option<String>,
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
            key_version: c.key_version.clone(),
        }
    }
}

/// SEC PART 2: Zeroizing wrapper around `String` with serde
/// impls. Heap bytes are wiped when the value is dropped, so
/// a decrypted credential never lingers in a heap inspector's
/// view of the process.
#[derive(Clone, Debug, Default)]
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    pub fn new(s: String) -> Self {
        Self(Zeroizing::new(s))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl Deref for SecretString {
    type Target = String;
    fn deref(&self) -> &String {
        &self.0
    }
}

impl PartialEq for SecretString {
    fn eq(&self, other: &Self) -> bool {
        // Constant-time-ish: rely on String's eq for the test
        // surface (the wire/serde path is the operator-side
        // hot path; equality is rare and primarily a test
        // affordance).
        self.0.as_str() == other.0.as_str()
    }
}

impl Eq for SecretString {}

impl Serialize for SecretString {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.as_str().serialize(s)
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(SecretString(Zeroizing::new(String::deserialize(d)?)))
    }
}

/// What `credentials.get` returns to authorised callers. The
/// plain value lives inside [`SecretString`] (a zeroizing
/// `String` newtype) so memory is wiped on drop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecryptedCredential {
    pub name: String,
    pub kind: CredentialKind,
    pub owner_agent: Option<String>,
    /// SEC PART 2: zeroized on drop.
    pub value: SecretString,
    pub version: u32,
    /// SEC PART 7: which key version decrypted this row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_version: Option<String>,
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
    /// SEC §10: a one-way KDF migration (legacy SHA-256 →
    /// Argon2id) was applied to the vault. Recorded so the
    /// weak-KDF retirement is attributable and observable.
    KdfMigrated,
}

impl AuditEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stored => "stored",
            Self::Accessed => "accessed",
            Self::Rotated => "rotated",
            Self::Revoked => "revoked",
            Self::KdfMigrated => "kdf_migrated",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stored" => Some(Self::Stored),
            "accessed" => Some(Self::Accessed),
            "rotated" => Some(Self::Rotated),
            "revoked" => Some(Self::Revoked),
            "kdf_migrated" => Some(Self::KdfMigrated),
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

/// SEC PART 1: Argon2id parameters. The trio
/// (memory_cost_kib, time_cost, parallelism) is stored in
/// `vault_metadata` at vault creation so reopens derive the
/// exact same 32-byte AES key from the master secret + salt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    pub memory_cost_kib: u32,
    pub time_cost: u32,
    pub parallelism: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self {
            memory_cost_kib: 65_536,
            time_cost: 3,
            parallelism: 4,
        }
    }
}

impl KdfParams {
    /// Cheap parameters for unit-test bootstraps so the
    /// 64 MB / 3-iter default doesn't make `cargo test`
    /// take hours. NOT for production.
    #[doc(hidden)]
    pub fn for_tests() -> Self {
        Self {
            memory_cost_kib: 8,
            time_cost: 1,
            parallelism: 1,
        }
    }

    fn into_argon2_params(self) -> Result<Params, CredentialError> {
        Params::new(
            self.memory_cost_kib,
            self.time_cost,
            self.parallelism,
            Some(AES_KEY_LEN),
        )
        .map_err(|e| CredentialError::Kdf(format!("argon2 params: {e}")))
    }
}

/// SEC PART 7: map of version name → master secret bytes.
/// The active version is `active_version()` — the
/// highest-numbered entry; existing rows decrypt under
/// whichever version they carry.
#[derive(Clone, Debug, Default)]
pub struct KeyVersionMap {
    /// Stored as a `BTreeMap` so ordering is lexicographic.
    /// Operators name versions `v1`, `v2`, … so lexicographic
    /// ordering matches numeric ordering up to v9; for ≥ v10
    /// we sort by the numeric suffix when the prefix matches
    /// (see [`active_version`]).
    entries: BTreeMap<String, Zeroizing<String>>,
}

impl KeyVersionMap {
    pub fn insert(&mut self, name: String, secret: String) {
        self.entries.insert(name, Zeroizing::new(secret));
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries.get(name).map(|s| s.as_str())
    }

    /// Resolve the active version — the highest-numbered
    /// entry. Returns `None` when the map is empty.
    pub fn active_version(&self) -> Option<&str> {
        let mut best: Option<(u64, &str)> = None;
        for name in self.entries.keys() {
            let rank = version_rank(name);
            let take = match best {
                None => true,
                Some((br, _)) => rank > br,
            };
            if take {
                best = Some((rank, name.as_str()));
            }
        }
        best.map(|(_, n)| n)
    }

    /// Iterate every (name, secret) pair. The secret is a
    /// short-lived `&str` borrowed from the zeroizing storage.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// Rank a version label so `v10` > `v9`. Falls back to a
/// stable hash-by-name for non-`v<digits>` labels.
fn version_rank(name: &str) -> u64 {
    if let Some(suffix) = name.strip_prefix('v')
        && let Ok(n) = suffix.parse::<u64>()
    {
        return n;
    }
    // Non-numeric labels rank by the first byte so the order is
    // stable but always behind any `v<n>` label.
    name.as_bytes().first().copied().unwrap_or(0) as u64
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
    #[error("credentials: tenant_id required in multi-tenant mode")]
    MissingTenant,
    /// SEC PART 1: opening a v1-format vault is refused. Operators
    /// run `relix credentials migrate-kdf` to upgrade.
    #[error("credentials: {message}")]
    LegacyFormat { message: String },
    /// SEC PART 1: Argon2id derivation failure (param decode,
    /// param construction, or the hash call itself). Always
    /// fail-closed at open time.
    #[error("credentials: kdf: {0}")]
    Kdf(String),
    /// SEC PART 7: configured key_versions block is empty AND
    /// `master_key_env` is unset, OR the active version's env
    /// var resolved to empty. The vault refuses to open without
    /// at least one usable key.
    #[error("credentials: no usable key version (set the active env var or `master_key_env`)")]
    NoActiveKeyVersion,
    /// SEC PART 7: a credential row references a `key_version`
    /// that isn't present in the configured map. Operator must
    /// re-add the env var or rotate the row.
    #[error("credentials: row references unknown key_version `{0}`")]
    UnknownKeyVersion(String),
    /// SEC PART 1: migrate-kdf invoked but the vault is already
    /// in the v2 format. No-op + error so the CLI exits non-zero
    /// and the operator notices.
    #[error("credentials: vault already uses Argon2id (no migration needed)")]
    NotLegacyFormat,
    /// SEC PART 1: post-migration verification failed for at
    /// least one credential. The migration is rolled back; the
    /// vault remains in its pre-migration state.
    #[error("credentials: migration verification failed: {0}")]
    MigrationVerifyFailed(String),
}

/// SQLite-backed encrypted vault. Cheap to clone.
#[derive(Clone)]
pub struct CredentialStore {
    conn: Arc<Mutex<Connection>>,
    /// SEC PART 7: every configured key version's derived AES
    /// key, indexed by version name. The active version is the
    /// highest-numbered entry; new credentials encrypt under
    /// it. Existing rows decrypt under whichever version they
    /// carry.
    keys: Arc<BTreeMap<String, Zeroizing<[u8; AES_KEY_LEN]>>>,
    /// Active version name — the value `keys` is indexed by
    /// for fresh writes. Always present (constructor fails
    /// closed when empty).
    active_version: String,
    tenant_isolation: bool,
}

impl CredentialStore {
    /// Open (or create) the store at `path`. Uses the default
    /// [`KdfParams`]; production callers should usually go
    /// through [`Self::open_with_params`] so operator-supplied
    /// `[credentials] argon2_*` values apply.
    pub fn open(path: &Path, master_secret: &str) -> Result<Self, CredentialError> {
        Self::open_with_params(
            path,
            single_version_map(master_secret)?,
            KdfParams::default(),
            false,
        )
    }

    /// Tenant-isolation variant of [`Self::open`].
    pub fn open_with_tenant_isolation(
        path: &Path,
        master_secret: &str,
        tenant_isolation: bool,
    ) -> Result<Self, CredentialError> {
        Self::open_with_params(
            path,
            single_version_map(master_secret)?,
            KdfParams::default(),
            tenant_isolation,
        )
    }

    /// Primary open entrypoint. The controller wires the
    /// [`KeyVersionMap`] from `[credentials.key_versions]`
    /// and the [`KdfParams`] from `[credentials] argon2_*`.
    pub fn open_with_params(
        path: &Path,
        keys: KeyVersionMap,
        kdf_params: KdfParams,
        tenant_isolation: bool,
    ) -> Result<Self, CredentialError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::log_integrity_warning(&conn, "credentials");
        Self::build(conn, keys, kdf_params, tenant_isolation)
    }

    /// In-memory store with default Argon2id params. Tests that
    /// don't want the 64 MB derivation cost should use
    /// [`Self::open_in_memory_with_params`] with
    /// [`KdfParams::for_tests`].
    pub fn open_in_memory(master_secret: &str) -> Result<Self, CredentialError> {
        Self::open_in_memory_with_params(
            single_version_map(master_secret)?,
            KdfParams::for_tests(),
            false,
        )
    }

    /// Tenant-isolation in-memory variant. Uses
    /// [`KdfParams::for_tests`] so cargo test stays fast.
    pub fn open_in_memory_with_tenant_isolation(
        master_secret: &str,
        tenant_isolation: bool,
    ) -> Result<Self, CredentialError> {
        Self::open_in_memory_with_params(
            single_version_map(master_secret)?,
            KdfParams::for_tests(),
            tenant_isolation,
        )
    }

    /// In-memory store with caller-supplied key map + KDF
    /// params. Tests that need to exercise the key-rotation
    /// path use this directly.
    pub fn open_in_memory_with_params(
        keys: KeyVersionMap,
        kdf_params: KdfParams,
        tenant_isolation: bool,
    ) -> Result<Self, CredentialError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        Self::build(conn, keys, kdf_params, tenant_isolation)
    }

    fn build(
        conn: Connection,
        keys: KeyVersionMap,
        kdf_params: KdfParams,
        tenant_isolation: bool,
    ) -> Result<Self, CredentialError> {
        if keys.is_empty() {
            return Err(CredentialError::NoActiveKeyVersion);
        }
        let active_version = keys
            .active_version()
            .ok_or(CredentialError::NoActiveKeyVersion)?
            .to_string();
        crate::db::ensure_migration_table(&conn)?;
        // SEC PART 1: vault_metadata presence drives legacy
        // refusal. Bootstrap path (no vault_metadata table AND
        // empty `credentials` table) writes fresh salt + params.
        let bootstrap = decide_bootstrap(&conn)?;
        ensure_vault_metadata_table(&conn)?;
        // Migrate credential schema (idempotent).
        Self::migrate(&conn)?;
        let salt = match bootstrap {
            BootstrapMode::FreshVault => {
                let salt = generate_salt();
                write_vault_metadata(&conn, &salt, &kdf_params)?;
                salt
            }
            BootstrapMode::ExistingV2 => read_vault_metadata_salt(&conn)?,
        };
        let derived_params = match bootstrap {
            BootstrapMode::FreshVault => kdf_params,
            BootstrapMode::ExistingV2 => read_vault_metadata_params(&conn)?,
        };
        // SEC PART 7: derive an AES key for every configured
        // version. Fail-closed if the active version isn't in
        // the map (already checked above) — and surface every
        // version so the row reader can find the right key.
        let mut derived: BTreeMap<String, Zeroizing<[u8; AES_KEY_LEN]>> = BTreeMap::new();
        for (name, secret) in keys.entries() {
            let key = derive_aes_key_argon2id(secret, &salt, &derived_params)?;
            derived.insert(name.to_string(), key);
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            keys: Arc::new(derived),
            active_version,
            tenant_isolation,
        })
    }

    pub fn tenant_isolation_enabled(&self) -> bool {
        self.tenant_isolation
    }

    /// SEC PART 7: the active key version's name. Exposed for
    /// the rotate-vault-key CLI's pre-flight check.
    pub fn active_key_version(&self) -> &str {
        &self.active_version
    }

    /// SEC PART 7: every loaded key version's name. Surfaced
    /// for operator inspection (`credentials list` shows each
    /// row's `key_version`).
    pub fn known_key_versions(&self) -> Vec<String> {
        self.keys.keys().cloned().collect()
    }

    fn migrate(conn: &Connection) -> Result<(), CredentialError> {
        let current = crate::db::current_migration_version(conn)?;
        if current < 1 {
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
            crate::db::record_migration_applied(conn, 1)?;
        }
        if current < 2 {
            // Tenant-isolation: tenant_id column + partial index.
            if !column_exists(conn, "credentials", "tenant_id")? {
                conn.execute("ALTER TABLE credentials ADD COLUMN tenant_id TEXT", [])?;
            }
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_credentials_tenant \
                     ON credentials(tenant_id) WHERE tenant_id IS NOT NULL;",
            )?;
            crate::db::record_migration_applied(conn, 2)?;
        }
        if current < 3 {
            // SEC PART 7: key_version column + partial index.
            if !column_exists(conn, "credentials", "key_version")? {
                conn.execute("ALTER TABLE credentials ADD COLUMN key_version TEXT", [])?;
            }
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_credentials_key_version \
                     ON credentials(key_version) WHERE key_version IS NOT NULL;",
            )?;
            crate::db::record_migration_applied(conn, 3)?;
        }
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
        self.store_inner(
            name,
            value,
            kind,
            owner_agent,
            expires_at_ms,
            rotation_interval_secs,
            actor,
            None,
        )
    }

    /// Tenant-aware insert. Falls through to [`Self::store`]
    /// when isolation is disabled.
    #[allow(clippy::too_many_arguments)]
    pub fn store_for_tenant(
        &self,
        name: &str,
        value: &str,
        kind: CredentialKind,
        owner_agent: Option<&str>,
        expires_at_ms: Option<i64>,
        rotation_interval_secs: Option<u64>,
        actor: Option<&str>,
        tenant_id: Option<&str>,
    ) -> Result<Credential, CredentialError> {
        if self.tenant_isolation {
            match tenant_id {
                Some(t) if !t.trim().is_empty() => {}
                _ => return Err(CredentialError::MissingTenant),
            }
        }
        self.store_inner(
            name,
            value,
            kind,
            owner_agent,
            expires_at_ms,
            rotation_interval_secs,
            actor,
            tenant_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn store_inner(
        &self,
        name: &str,
        value: &str,
        kind: CredentialKind,
        owner_agent: Option<&str>,
        expires_at_ms: Option<i64>,
        rotation_interval_secs: Option<u64>,
        actor: Option<&str>,
        tenant_id: Option<&str>,
    ) -> Result<Credential, CredentialError> {
        if name.trim().is_empty() {
            return Err(CredentialError::Serialization(
                "credential name is required".into(),
            ));
        }
        let now = unix_ms();
        let id = format!("cred_{}", uuid::Uuid::new_v4().simple());
        let active_key = self.active_key_bytes()?;
        let encrypted = encrypt(active_key, value)?;
        let encrypted_json = serde_json::to_string(&encrypted)
            .map_err(|e| CredentialError::Serialization(e.to_string()))?;
        let next_rot = rotation_interval_secs.map(|s| now + (s as i64) * 1000);
        {
            let conn = self.lock()?;
            conn.execute(
                "INSERT INTO credentials \
                 (id, name, value_encrypted, kind, owner_agent, created_at_ms, updated_at_ms, \
                  expires_at_ms, last_rotated_at_ms, rotation_interval_secs, next_rotation_at_ms, \
                  revoked, revoked_at_ms, revoke_reason, version, tenant_id, key_version) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?10, 0, NULL, NULL, 1, ?11, ?12)",
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
                    tenant_id,
                    self.active_version,
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
        let key = self.key_for_row(&row)?;
        let plaintext = decrypt(key, &encrypted)?;
        self.audit(&row.id, AuditEvent::Accessed, actor, None)?;
        Ok(Some(DecryptedCredential {
            name: row.name.clone(),
            kind: row.kind,
            owner_agent: row.owner_agent.clone(),
            value: plaintext,
            version: row.version,
            key_version: row.key_version.clone(),
        }))
    }

    /// Tenant-aware variant of [`Self::get`].
    pub fn get_for_tenant(
        &self,
        name: &str,
        actor: Option<&str>,
        tenant_id: Option<&str>,
    ) -> Result<Option<DecryptedCredential>, CredentialError> {
        if !self.tenant_isolation {
            return self.get(name, actor);
        }
        let tenant = match tenant_id {
            Some(t) if !t.trim().is_empty() => t,
            _ => return Err(CredentialError::MissingTenant),
        };
        let row = match self.get_row_by_name_for_tenant(name, tenant)? {
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
        let key = self.key_for_row(&row)?;
        let plaintext = decrypt(key, &encrypted)?;
        self.audit(&row.id, AuditEvent::Accessed, actor, None)?;
        Ok(Some(DecryptedCredential {
            name: row.name.clone(),
            kind: row.kind,
            owner_agent: row.owner_agent.clone(),
            value: plaintext,
            version: row.version,
            key_version: row.key_version.clone(),
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
        let active_key = self.active_key_bytes()?;
        let encrypted = encrypt(active_key, new_value)?;
        let encrypted_json = serde_json::to_string(&encrypted)
            .map_err(|e| CredentialError::Serialization(e.to_string()))?;
        let next_rot = row.rotation_interval_secs.map(|s| now + (s as i64) * 1000);
        {
            let conn = self.lock()?;
            conn.execute(
                "UPDATE credentials \
                 SET value_encrypted = ?1, updated_at_ms = ?2, last_rotated_at_ms = ?2, \
                     next_rotation_at_ms = ?3, version = version + 1, key_version = ?5 \
                 WHERE id = ?4",
                params![encrypted_json, now, next_rot, row.id, self.active_version],
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
        let rows: Vec<Credential> = if let Some(o) = owner_agent {
            let mut stmt = conn.prepare(
                "SELECT id, name, kind, owner_agent, created_at_ms, updated_at_ms, \
                        expires_at_ms, last_rotated_at_ms, rotation_interval_secs, \
                        next_rotation_at_ms, revoked, revoked_at_ms, revoke_reason, version, \
                        tenant_id, key_version \
                 FROM credentials WHERE owner_agent = ?1 \
                 ORDER BY created_at_ms DESC, name ASC",
            )?;
            stmt.query_map(params![o], row_to_credential_full)?
                .collect::<Result<_, _>>()?
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, name, kind, owner_agent, created_at_ms, updated_at_ms, \
                        expires_at_ms, last_rotated_at_ms, rotation_interval_secs, \
                        next_rotation_at_ms, revoked, revoked_at_ms, revoke_reason, version, \
                        tenant_id, key_version \
                 FROM credentials \
                 ORDER BY created_at_ms DESC, name ASC",
            )?;
            stmt.query_map([], row_to_credential_full)?
                .collect::<Result<_, _>>()?
        };
        Ok(rows.iter().map(CredentialSummary::from).collect())
    }

    /// Tenant-aware variant of [`Self::list`].
    pub fn list_for_tenant(
        &self,
        owner_agent: Option<&str>,
        tenant_id: Option<&str>,
    ) -> Result<Vec<CredentialSummary>, CredentialError> {
        if !self.tenant_isolation {
            return self.list(owner_agent);
        }
        let tenant = match tenant_id {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => return Err(CredentialError::MissingTenant),
        };
        let conn = self.lock()?;
        let rows: Vec<Credential> = if let Some(o) = owner_agent {
            let mut stmt = conn.prepare(
                "SELECT id, name, kind, owner_agent, created_at_ms, updated_at_ms, \
                        expires_at_ms, last_rotated_at_ms, rotation_interval_secs, \
                        next_rotation_at_ms, revoked, revoked_at_ms, revoke_reason, version, \
                        tenant_id, key_version \
                 FROM credentials WHERE tenant_id = ?1 AND owner_agent = ?2 \
                 ORDER BY created_at_ms DESC, name ASC",
            )?;
            stmt.query_map(params![tenant, o], row_to_credential_full)?
                .collect::<Result<_, _>>()?
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, name, kind, owner_agent, created_at_ms, updated_at_ms, \
                        expires_at_ms, last_rotated_at_ms, rotation_interval_secs, \
                        next_rotation_at_ms, revoked, revoked_at_ms, revoke_reason, version, \
                        tenant_id, key_version \
                 FROM credentials WHERE tenant_id = ?1 \
                 ORDER BY created_at_ms DESC, name ASC",
            )?;
            stmt.query_map(params![tenant], row_to_credential_full)?
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
                    next_rotation_at_ms, revoked, revoked_at_ms, revoke_reason, version, \
                    tenant_id, key_version \
             FROM credentials \
             WHERE revoked = 0 AND next_rotation_at_ms IS NOT NULL \
                   AND next_rotation_at_ms <= ?1 \
             ORDER BY next_rotation_at_ms ASC, name ASC",
        )?;
        let rows: Vec<Credential> = stmt
            .query_map(params![now_ms], row_to_credential_full)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// SEC PART 7: re-encrypt every credential under the new
    /// active key version. Atomic — single SQLite tx; rolled
    /// back if any post-rotate decryption fails.
    pub fn rotate_vault_key(
        &self,
        actor: Option<&str>,
    ) -> Result<RotateVaultKeyReport, CredentialError> {
        // Snapshot every row + decrypt under its existing
        // key version BEFORE entering the tx, so the tx only
        // covers the writes (keeping the lock window small).
        let prior_rows = {
            let conn = self.lock()?;
            let mut stmt = conn.prepare(
                "SELECT id, name, kind, owner_agent, created_at_ms, updated_at_ms, \
                        expires_at_ms, last_rotated_at_ms, rotation_interval_secs, \
                        next_rotation_at_ms, revoked, revoked_at_ms, revoke_reason, version, \
                        tenant_id, key_version, value_encrypted \
                 FROM credentials",
            )?;
            let rows: Vec<(Credential, String)> = stmt
                .query_map([], |r| {
                    let cred = row_to_credential_full(r)?;
                    let enc: String = r.get(16)?;
                    Ok((cred, enc))
                })?
                .collect::<Result<_, _>>()?;
            rows
        };
        let active_key = self.active_key_bytes()?;
        let active_version = self.active_version.clone();
        let mut decrypted: Vec<(String, Zeroizing<String>)> = Vec::with_capacity(prior_rows.len());
        for (cred, enc_json) in &prior_rows {
            let enc: EncryptedValue = serde_json::from_str(enc_json)
                .map_err(|e| CredentialError::Serialization(e.to_string()))?;
            let key = self.key_for_row(cred)?;
            let plain = decrypt(key, &enc)?;
            decrypted.push((cred.id.clone(), plain.0));
        }
        // Re-encrypt with fresh nonces under the active key
        // version. Single tx so a mid-rotation failure rolls
        // back; verify each row decrypts before COMMIT.
        let mut updated = 0usize;
        {
            let mut conn = self.lock()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            for (id, plain) in &decrypted {
                let encrypted = encrypt(active_key, plain.as_str())?;
                // Verify the round trip BEFORE writing so we
                // catch a broken active key (e.g. wrong env var
                // length) before any row is mutated.
                let verified = decrypt(active_key, &encrypted)?;
                if verified.as_str() != plain.as_str() {
                    return Err(CredentialError::MigrationVerifyFailed(format!(
                        "row {id}: encrypt/decrypt round-trip mismatch"
                    )));
                }
                let encrypted_json = serde_json::to_string(&encrypted)
                    .map_err(|e| CredentialError::Serialization(e.to_string()))?;
                tx.execute(
                    "UPDATE credentials \
                     SET value_encrypted = ?1, key_version = ?2 \
                     WHERE id = ?3",
                    params![encrypted_json, active_version, id],
                )?;
                updated += 1;
            }
            tx.commit()?;
        }
        // Audit each row's rotation outside the tx (audit
        // failures are operator-visible but don't roll back
        // the rotate — same contract as `rotate`).
        for (id, _) in &decrypted {
            self.audit(
                id,
                AuditEvent::Rotated,
                actor,
                Some(&format!("vault-key rotated to {active_version}")),
            )?;
        }
        Ok(RotateVaultKeyReport {
            rows_rotated: updated,
            active_version,
        })
    }

    fn get_row_by_id(&self, id: &str) -> Result<Option<Credential>, CredentialError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, name, kind, owner_agent, created_at_ms, updated_at_ms, \
                    expires_at_ms, last_rotated_at_ms, rotation_interval_secs, \
                    next_rotation_at_ms, revoked, revoked_at_ms, revoke_reason, version, \
                    tenant_id, key_version \
             FROM credentials WHERE id = ?1",
            params![id],
            row_to_credential_full,
        )
        .optional()
        .map_err(Into::into)
    }

    fn get_row_by_name(&self, name: &str) -> Result<Option<Credential>, CredentialError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, name, kind, owner_agent, created_at_ms, updated_at_ms, \
                    expires_at_ms, last_rotated_at_ms, rotation_interval_secs, \
                    next_rotation_at_ms, revoked, revoked_at_ms, revoke_reason, version, \
                    tenant_id, key_version \
             FROM credentials WHERE name = ?1",
            params![name],
            row_to_credential_full,
        )
        .optional()
        .map_err(Into::into)
    }

    fn get_row_by_name_for_tenant(
        &self,
        name: &str,
        tenant: &str,
    ) -> Result<Option<Credential>, CredentialError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, name, kind, owner_agent, created_at_ms, updated_at_ms, \
                    expires_at_ms, last_rotated_at_ms, rotation_interval_secs, \
                    next_rotation_at_ms, revoked, revoked_at_ms, revoke_reason, version, \
                    tenant_id, key_version \
             FROM credentials WHERE name = ?1 AND tenant_id = ?2",
            params![name, tenant],
            row_to_credential_full,
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

    fn active_key_bytes(&self) -> Result<&[u8; AES_KEY_LEN], CredentialError> {
        self.keys
            .get(&self.active_version)
            .map(|k| {
                let r: &[u8; AES_KEY_LEN] = k;
                r
            })
            .ok_or_else(|| CredentialError::UnknownKeyVersion(self.active_version.clone()))
    }

    fn key_for_row(&self, row: &Credential) -> Result<&[u8; AES_KEY_LEN], CredentialError> {
        let version = row
            .key_version
            .clone()
            .unwrap_or_else(|| self.active_version.clone());
        self.keys
            .get(&version)
            .map(|k| {
                let r: &[u8; AES_KEY_LEN] = k;
                r
            })
            .ok_or(CredentialError::UnknownKeyVersion(version))
    }
}

/// SEC PART 7: post-rotation report for the
/// rotate-vault-key CLI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RotateVaultKeyReport {
    pub rows_rotated: usize,
    pub active_version: String,
}

/// SEC PART 1: migrate a legacy v1 vault (SHA-256 KDF, no
/// salt, no work factor) to the v2 Argon2id KDF.
///
/// 1. Open the v1 DB read-only-style (we still need writes
///    for the rebuild).
/// 2. Decrypt every credential under the SHA-256-derived key.
/// 3. Generate a fresh 32-byte salt + write `vault_metadata`
///    with the supplied params + the version constant.
/// 4. Derive the new Argon2id key under the supplied
///    `KeyVersionMap` active version.
/// 5. Re-encrypt every credential with a fresh nonce under
///    the new key + stamp `key_version` to the new active.
/// 6. Verify every row decrypts before COMMIT.
/// 7. Single SQLite tx — any failure rolls back.
pub fn migrate_kdf(
    path: &Path,
    legacy_master_secret: &str,
    keys: KeyVersionMap,
    new_kdf_params: KdfParams,
) -> Result<MigrateKdfReport, CredentialError> {
    if keys.is_empty() {
        return Err(CredentialError::NoActiveKeyVersion);
    }
    let active_version = keys
        .active_version()
        .ok_or(CredentialError::NoActiveKeyVersion)?
        .to_string();
    let mut conn = Connection::open(path)?;
    crate::db::apply_pragmas(&conn)?;
    crate::db::ensure_migration_table(&conn)?;
    // Refuse if the vault already carries v2 metadata.
    if vault_metadata_table_exists(&conn)? && read_vault_metadata_salt(&conn).is_ok() {
        return Err(CredentialError::NotLegacyFormat);
    }
    // The legacy DB lacks the v2 columns; run the schema
    // migrations first so the column writes below succeed.
    CredentialStore::migrate(&conn)?;
    ensure_vault_metadata_table(&conn)?;

    // Phase 1 — decrypt every row under the legacy SHA-256 key.
    let legacy_key = derive_legacy_sha256_key(legacy_master_secret);
    let prior_rows: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT id, value_encrypted FROM credentials")?;
        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<_, _>>()?
    };
    let mut decrypted: Vec<(String, Zeroizing<String>)> = Vec::with_capacity(prior_rows.len());
    for (id, enc_json) in &prior_rows {
        let enc: EncryptedValue = serde_json::from_str(enc_json)
            .map_err(|e| CredentialError::Serialization(e.to_string()))?;
        let plain = decrypt(&legacy_key, &enc).map_err(|e| {
            CredentialError::MigrationVerifyFailed(format!("legacy decrypt for row {id}: {e}"))
        })?;
        decrypted.push((id.clone(), plain.0));
    }

    // Phase 2 — derive new salt + key + write metadata + re-encrypt + verify, all in one tx.
    let salt = generate_salt();
    let active_secret = keys
        .get(&active_version)
        .ok_or_else(|| CredentialError::UnknownKeyVersion(active_version.clone()))?;
    let active_key = derive_aes_key_argon2id(active_secret, &salt, &new_kdf_params)?;

    let mut rows_rotated = 0usize;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    write_vault_metadata_inside_tx(&tx, &salt, &new_kdf_params)?;
    for (id, plain) in &decrypted {
        let encrypted = encrypt(&active_key, plain.as_str())?;
        let verified = decrypt(&active_key, &encrypted)?;
        if verified.as_str() != plain.as_str() {
            return Err(CredentialError::MigrationVerifyFailed(format!(
                "row {id}: encrypt/decrypt round-trip mismatch"
            )));
        }
        let encrypted_json = serde_json::to_string(&encrypted)
            .map_err(|e| CredentialError::Serialization(e.to_string()))?;
        tx.execute(
            "UPDATE credentials \
             SET value_encrypted = ?1, key_version = ?2 \
             WHERE id = ?3",
            params![encrypted_json, active_version, id],
        )?;
        rows_rotated += 1;
    }
    // SEC §10: record the one-way KDF migration in the credential
    // audit log, inside the same tx so it is atomic with the
    // re-encryption (no audit row exists for a rolled-back
    // migration, and a committed migration always carries one).
    // `credential_id` uses the `__vault__` sentinel because this
    // is a vault-level event, not tied to a single credential.
    let audit_id = format!("audit_{}", uuid::Uuid::new_v4().simple());
    let audit_details = format!(
        "kdf migration sha256->argon2id: {rows_rotated} rows re-encrypted under active version {active_version}"
    );
    tx.execute(
        "INSERT INTO credential_audit (id, credential_id, event, actor, timestamp_ms, details) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            audit_id,
            "__vault__",
            AuditEvent::KdfMigrated.as_str(),
            Option::<&str>::None,
            unix_ms(),
            audit_details
        ],
    )?;
    tx.commit()?;
    Ok(MigrateKdfReport {
        rows_rotated,
        active_version,
    })
}

/// Migration outcome surfaced to the CLI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrateKdfReport {
    pub rows_rotated: usize,
    pub active_version: String,
}

// ─────────────────────────── private helpers ───────────────────────────

fn row_to_credential_full(row: &rusqlite::Row<'_>) -> rusqlite::Result<Credential> {
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
        tenant_id: row.get(14)?,
        key_version: row.get(15)?,
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

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, CredentialError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, CredentialError> {
    let v: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name = ?1",
            params![table],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.is_some())
}

fn vault_metadata_table_exists(conn: &Connection) -> Result<bool, CredentialError> {
    table_exists(conn, "vault_metadata")
}

fn ensure_vault_metadata_table(conn: &Connection) -> Result<(), CredentialError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS vault_metadata (\
             key TEXT PRIMARY KEY,\
             value BLOB NOT NULL\
         );",
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BootstrapMode {
    /// New DB: no `vault_metadata` table AND no rows in
    /// `credentials` (whether or not the table exists). Safe
    /// to write a fresh salt + params.
    FreshVault,
    /// `vault_metadata` table is present AND carries the salt.
    /// Read the salt + params from it.
    ExistingV2,
}

fn decide_bootstrap(conn: &Connection) -> Result<BootstrapMode, CredentialError> {
    let has_metadata = vault_metadata_table_exists(conn)?;
    let has_credentials_with_rows = if table_exists(conn, "credentials")? {
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM credentials", [], |r| r.get(0))?;
        n > 0
    } else {
        false
    };
    if has_metadata {
        // Even if the table exists, salt + params must be
        // present. read_vault_metadata_salt below will error
        // out otherwise. Treat any inconsistency that has
        // credentials in it as legacy → refuse.
        let salt_present = conn
            .query_row(
                "SELECT 1 FROM vault_metadata WHERE key = ?1",
                params![METADATA_SALT_KEY],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if salt_present {
            return Ok(BootstrapMode::ExistingV2);
        }
        if has_credentials_with_rows {
            return Err(CredentialError::LegacyFormat {
                message: legacy_message(),
            });
        }
        return Ok(BootstrapMode::FreshVault);
    }
    if has_credentials_with_rows {
        Err(CredentialError::LegacyFormat {
            message: legacy_message(),
        })
    } else {
        Ok(BootstrapMode::FreshVault)
    }
}

fn legacy_message() -> String {
    "This vault uses an insecure SHA-256 key derivation. Run \
     `relix credentials migrate-kdf` to upgrade."
        .to_string()
}

fn write_vault_metadata(
    conn: &Connection,
    salt: &[u8; KDF_SALT_LEN],
    params: &KdfParams,
) -> Result<(), CredentialError> {
    let params_json = serde_json::to_vec(&PersistedKdfParams::from(*params))
        .expect("serde to_vec on plain struct");
    conn.execute(
        "INSERT OR REPLACE INTO vault_metadata (key, value) VALUES (?1, ?2)",
        params![METADATA_SALT_KEY, salt.as_slice()],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO vault_metadata (key, value) VALUES (?1, ?2)",
        params![METADATA_PARAMS_KEY, params_json.as_slice()],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO vault_metadata (key, value) VALUES (?1, ?2)",
        params![
            METADATA_VERSION_KEY,
            VAULT_FORMAT_VERSION.to_le_bytes().to_vec()
        ],
    )?;
    Ok(())
}

fn write_vault_metadata_inside_tx(
    tx: &rusqlite::Transaction<'_>,
    salt: &[u8; KDF_SALT_LEN],
    params: &KdfParams,
) -> Result<(), CredentialError> {
    let params_json = serde_json::to_vec(&PersistedKdfParams::from(*params))
        .expect("serde to_vec on plain struct");
    tx.execute(
        "INSERT OR REPLACE INTO vault_metadata (key, value) VALUES (?1, ?2)",
        params![METADATA_SALT_KEY, salt.as_slice()],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO vault_metadata (key, value) VALUES (?1, ?2)",
        params![METADATA_PARAMS_KEY, params_json.as_slice()],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO vault_metadata (key, value) VALUES (?1, ?2)",
        params![
            METADATA_VERSION_KEY,
            VAULT_FORMAT_VERSION.to_le_bytes().to_vec()
        ],
    )?;
    Ok(())
}

fn read_vault_metadata_salt(conn: &Connection) -> Result<[u8; KDF_SALT_LEN], CredentialError> {
    let bytes: Vec<u8> = conn
        .query_row(
            "SELECT value FROM vault_metadata WHERE key = ?1",
            params![METADATA_SALT_KEY],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| CredentialError::LegacyFormat {
            message: legacy_message(),
        })?;
    if bytes.len() != KDF_SALT_LEN {
        return Err(CredentialError::Kdf(format!(
            "vault salt has length {} (expected {KDF_SALT_LEN})",
            bytes.len()
        )));
    }
    let mut out = [0u8; KDF_SALT_LEN];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn read_vault_metadata_params(conn: &Connection) -> Result<KdfParams, CredentialError> {
    let bytes: Vec<u8> = conn
        .query_row(
            "SELECT value FROM vault_metadata WHERE key = ?1",
            params![METADATA_PARAMS_KEY],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| CredentialError::LegacyFormat {
            message: legacy_message(),
        })?;
    let persisted: PersistedKdfParams = serde_json::from_slice(&bytes)
        .map_err(|e| CredentialError::Kdf(format!("params decode: {e}")))?;
    Ok(persisted.into())
}

/// Wire-format projection of [`KdfParams`] so the JSON blob
/// stored in `vault_metadata` is stable + self-describing
/// (algorithm tag included so a future Argon2 variant swap is
/// detectable on open).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct PersistedKdfParams {
    memory_cost: u32,
    time_cost: u32,
    parallelism: u32,
    algorithm: PersistedAlgorithm,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PersistedAlgorithm {
    Argon2id,
}

impl From<KdfParams> for PersistedKdfParams {
    fn from(p: KdfParams) -> Self {
        Self {
            memory_cost: p.memory_cost_kib,
            time_cost: p.time_cost,
            parallelism: p.parallelism,
            algorithm: PersistedAlgorithm::Argon2id,
        }
    }
}

impl From<PersistedKdfParams> for KdfParams {
    fn from(p: PersistedKdfParams) -> Self {
        Self {
            memory_cost_kib: p.memory_cost,
            time_cost: p.time_cost,
            parallelism: p.parallelism,
        }
    }
}

fn generate_salt() -> [u8; KDF_SALT_LEN] {
    let mut salt = [0u8; KDF_SALT_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    salt
}

fn single_version_map(secret: &str) -> Result<KeyVersionMap, CredentialError> {
    if secret.is_empty() {
        return Err(CredentialError::NoActiveKeyVersion);
    }
    let mut map = KeyVersionMap::default();
    map.insert("v1".to_string(), secret.to_string());
    Ok(map)
}

fn derive_aes_key_argon2id(
    master: &str,
    salt: &[u8; KDF_SALT_LEN],
    params: &KdfParams,
) -> Result<Zeroizing<[u8; AES_KEY_LEN]>, CredentialError> {
    let argon2 = Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        params.into_argon2_params()?,
    );
    let mut out = Zeroizing::new([0u8; AES_KEY_LEN]);
    argon2
        .hash_password_into(master.as_bytes(), salt, &mut out[..])
        .map_err(|e| CredentialError::Kdf(format!("argon2 derive: {e}")))?;
    Ok(out)
}

/// SEC §10 / SEC PART 1: legacy SHA-256 derivation. This fn is
/// PRIVATE (never `pub`) and has exactly one production call
/// site — [`migrate_kdf`], the audited one-way (legacy →
/// Argon2id) migration path. It is unreachable from the normal
/// open path and uncallable from outside this module, so no
/// in-process caller can derive a key with SHA-256. Removing the
/// legacy path entirely would leave v1 vaults unrecoverable,
/// defeating the migration.
fn derive_legacy_sha256_key(master_secret: &str) -> Zeroizing<[u8; AES_KEY_LEN]> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(master_secret.as_bytes());
    let out = hasher.finalize();
    let mut key = Zeroizing::new([0u8; AES_KEY_LEN]);
    key.copy_from_slice(&out[..AES_KEY_LEN]);
    key
}

fn encrypt(key: &[u8; AES_KEY_LEN], plaintext: &str) -> Result<EncryptedValue, CredentialError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
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

fn decrypt(key: &[u8; AES_KEY_LEN], enc: &EncryptedValue) -> Result<SecretString, CredentialError> {
    use base64::Engine;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce_bytes = base64::engine::general_purpose::STANDARD
        .decode(&enc.nonce_b64)
        .map_err(|e| CredentialError::Encryption(format!("decode nonce: {e}")))?;
    if nonce_bytes.len() != NONCE_LEN {
        return Err(CredentialError::Encryption(format!(
            "nonce length {} != {NONCE_LEN}",
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
    let s = String::from_utf8(plaintext)
        .map_err(|e| CredentialError::Encryption(format!("plaintext utf-8: {e}")))?;
    Ok(SecretString::new(s))
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

    fn isolated_store() -> CredentialStore {
        CredentialStore::open_in_memory_with_tenant_isolation("test-master-secret", true).unwrap()
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
        assert_eq!(plain.value.as_str(), "ghp_abc");
        // SEC PART 2: the decrypted plaintext type is SecretString,
        // a Zeroizing<String> newtype. Equality compares the
        // underlying string.
        assert_eq!(plain.value, SecretString::new("ghp_abc".into()));
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
        assert_eq!(v.value.as_str(), "v2");
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
        assert!(due_now.is_empty());
        let later = now + 120_000;
        let due_later = s.due_for_rotation(later).unwrap();
        assert_eq!(due_later.len(), 1);
        assert_eq!(due_later[0].name, "on_schedule");
    }

    // ── SEC PART 1: Argon2id KDF tests ─────────────────────────

    #[test]
    fn new_vault_writes_vault_metadata_with_argon2id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.db");
        let store = CredentialStore::open_with_params(
            &path,
            single_version_map("alpha").unwrap(),
            KdfParams::for_tests(),
            false,
        )
        .unwrap();
        // store + read back; the round-trip exercises encrypt/decrypt.
        store
            .store("a", "v", CredentialKind::ApiKey, None, None, None, None)
            .unwrap();
        drop(store);
        // Inspect the metadata table directly.
        let conn = Connection::open(&path).unwrap();
        let v_bytes: Vec<u8> = conn
            .query_row(
                "SELECT value FROM vault_metadata WHERE key = ?1",
                params![METADATA_VERSION_KEY],
                |r| r.get(0),
            )
            .unwrap();
        let version = u32::from_le_bytes([v_bytes[0], v_bytes[1], v_bytes[2], v_bytes[3]]);
        assert_eq!(version, VAULT_FORMAT_VERSION);
        let params_bytes: Vec<u8> = conn
            .query_row(
                "SELECT value FROM vault_metadata WHERE key = ?1",
                params![METADATA_PARAMS_KEY],
                |r| r.get(0),
            )
            .unwrap();
        let parsed: PersistedKdfParams = serde_json::from_slice(&params_bytes).unwrap();
        assert!(matches!(parsed.algorithm, PersistedAlgorithm::Argon2id));
        let salt: Vec<u8> = conn
            .query_row(
                "SELECT value FROM vault_metadata WHERE key = ?1",
                params![METADATA_SALT_KEY],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(salt.len(), KDF_SALT_LEN);
    }

    #[test]
    fn two_opens_of_same_vault_derive_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.db");
        {
            let s = CredentialStore::open_with_params(
                &path,
                single_version_map("alpha").unwrap(),
                KdfParams::for_tests(),
                false,
            )
            .unwrap();
            s.store(
                "a",
                "supersecret-plain",
                CredentialKind::ApiKey,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        }
        {
            // Second open must derive the same AES key — proves
            // the salt + params were faithfully persisted.
            let s = CredentialStore::open_with_params(
                &path,
                single_version_map("alpha").unwrap(),
                KdfParams::for_tests(),
                false,
            )
            .unwrap();
            let v = s.get("a", None).unwrap().unwrap();
            assert_eq!(v.value.as_str(), "supersecret-plain");
        }
    }

    #[test]
    fn legacy_v1_vault_is_refused_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.db");
        // Build a legacy-shaped vault: credentials table + one
        // SHA-256-encrypted row + NO vault_metadata.
        legacy_seed_vault(&path, "alpha", "github_token", "ghp_abc");
        let result = CredentialStore::open_with_params(
            &path,
            single_version_map("alpha").unwrap(),
            KdfParams::for_tests(),
            false,
        );
        match result {
            Ok(_) => panic!("expected LegacyFormat, got Ok"),
            Err(CredentialError::LegacyFormat { message }) => {
                assert!(message.contains("migrate-kdf"), "msg: {message}");
            }
            Err(other) => panic!("expected LegacyFormat, got {other:?}"),
        }
    }

    #[test]
    fn migrate_kdf_re_encrypts_and_then_vault_opens_with_argon2id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.db");
        legacy_seed_vault(&path, "alpha", "k1", "secret-1");
        legacy_append_to_vault(&path, "alpha", "k2", "secret-2");
        let report = migrate_kdf(
            &path,
            "alpha",
            single_version_map("alpha").unwrap(),
            KdfParams::for_tests(),
        )
        .unwrap();
        assert_eq!(report.rows_rotated, 2);
        assert_eq!(report.active_version, "v1");
        // Reopen as the normal v2 path → reads vault_metadata,
        // Argon2id-derives the key, decrypts both rows.
        let s = CredentialStore::open_with_params(
            &path,
            single_version_map("alpha").unwrap(),
            KdfParams::for_tests(),
            false,
        )
        .unwrap();
        let v1 = s.get("k1", None).unwrap().unwrap();
        let v2 = s.get("k2", None).unwrap().unwrap();
        assert_eq!(v1.value.as_str(), "secret-1");
        assert_eq!(v2.value.as_str(), "secret-2");
        // Repeating migrate-kdf is rejected: vault is already v2.
        let err = migrate_kdf(
            &path,
            "alpha",
            single_version_map("alpha").unwrap(),
            KdfParams::for_tests(),
        )
        .unwrap_err();
        assert!(matches!(err, CredentialError::NotLegacyFormat));
    }

    #[test]
    fn migrate_kdf_is_atomic_on_decrypt_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.db");
        legacy_seed_vault(&path, "alpha", "k1", "secret-1");
        // Use the WRONG legacy master secret → decrypt fails →
        // migration aborts BEFORE any write to vault_metadata.
        let err = migrate_kdf(
            &path,
            "wrong-master",
            single_version_map("alpha").unwrap(),
            KdfParams::for_tests(),
        )
        .unwrap_err();
        assert!(
            matches!(err, CredentialError::MigrationVerifyFailed(_)),
            "unexpected: {err:?}"
        );
        // The vault must STILL refuse to open with the v2 path
        // (no metadata written) — proves no partial migration.
        let result = CredentialStore::open_with_params(
            &path,
            single_version_map("alpha").unwrap(),
            KdfParams::for_tests(),
            false,
        );
        match result {
            Ok(_) => panic!("expected LegacyFormat after failed migration, got Ok"),
            Err(CredentialError::LegacyFormat { .. }) => {}
            Err(other) => panic!("expected LegacyFormat, got {other:?}"),
        }
    }

    /// SEC §10 criterion 2: the one-way migration path converts a
    /// legacy-KDF store to Argon2id AND is audited. A `kdf_migrated`
    /// audit row is written atomically with the migration, and the
    /// path is one-way (a second migrate is rejected). The legacy
    /// SHA-256 derivation cannot be invoked directly from outside
    /// the module — `derive_legacy_sha256_key` is a private fn (see
    /// the grep in the section transcript); attempting to call it
    /// from another crate/module would not compile.
    #[test]
    fn migrate_kdf_is_audited_and_one_way() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.db");
        legacy_seed_vault(&path, "alpha", "k1", "secret-1");
        legacy_append_to_vault(&path, "alpha", "k2", "secret-2");

        let report = migrate_kdf(
            &path,
            "alpha",
            single_version_map("alpha").unwrap(),
            KdfParams::for_tests(),
        )
        .unwrap();
        assert_eq!(report.rows_rotated, 2);

        // The migration must have left an audit trail: exactly one
        // `kdf_migrated` row, recorded against the `__vault__`
        // sentinel, naming the rows migrated.
        let conn = Connection::open(&path).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT credential_id, event, details FROM credential_audit \
                 WHERE event = ?1",
            )
            .unwrap();
        let rows: Vec<(String, String, Option<String>)> = stmt
            .query_map(params![AuditEvent::KdfMigrated.as_str()], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 1, "expected exactly one kdf_migrated audit row");
        assert_eq!(rows[0].0, "__vault__");
        assert_eq!(rows[0].1, "kdf_migrated");
        assert_eq!(AuditEvent::parse(&rows[0].1), Some(AuditEvent::KdfMigrated));
        assert!(
            rows[0]
                .2
                .as_deref()
                .unwrap_or_default()
                .contains("sha256->argon2id"),
            "audit details should describe the KDF transition: {:?}",
            rows[0].2
        );

        // One-way: a second migrate against the now-v2 vault is
        // rejected — there is no path back to the legacy KDF.
        let err = migrate_kdf(
            &path,
            "alpha",
            single_version_map("alpha").unwrap(),
            KdfParams::for_tests(),
        )
        .unwrap_err();
        assert!(matches!(err, CredentialError::NotLegacyFormat));
    }

    /// SEC §10 criterion 3: normal credential open/store on the
    /// Argon2id path (the only KDF a fresh vault ever uses) still
    /// works end to end — store, then read back the plaintext.
    #[test]
    fn argon2id_open_store_path_still_works() {
        let s = fresh_store();
        s.store(
            "argon_cred",
            "argon-secret",
            CredentialKind::ApiKey,
            Some("alice"),
            None,
            None,
            Some("alice"),
        )
        .unwrap();
        let got = s.get("argon_cred", Some("alice")).unwrap().unwrap();
        assert_eq!(got.value.as_str(), "argon-secret");
    }

    /// SEC PART 2: Zeroizing wipes the derived key on drop.
    /// We can't directly observe the destination memory after
    /// a free, but we CAN observe that the type carries the
    /// Zeroizing wrapper. The Default impl initialises a zero
    /// buffer; the post-drop state of any heap copy is also
    /// zeros — exercising the wrapper is enough to lock the
    /// contract.
    #[test]
    fn derived_aes_key_is_zeroizing_on_drop() {
        let salt = [0u8; KDF_SALT_LEN];
        let params = KdfParams::for_tests();
        let derived: Zeroizing<[u8; AES_KEY_LEN]> =
            derive_aes_key_argon2id("master", &salt, &params).unwrap();
        // Type is Zeroizing — Drop wipes the bytes.
        let _seen = *derived.as_slice().first().unwrap();
        // The drop happens at end of scope; the test would
        // fail to compile if the type were a bare `[u8; 32]`.
    }

    #[test]
    fn decrypted_credential_value_is_secret_string() {
        let s = fresh_store();
        s.store(
            "k",
            "deep-secret-12345",
            CredentialKind::ApiKey,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let plain = s.get("k", None).unwrap().unwrap();
        // SEC PART 2 lock: the `value` field is SecretString
        // (a Zeroizing<String> newtype). Compile-time check —
        // a String there would fail this assertion.
        let _value: &SecretString = &plain.value;
        assert_eq!(plain.value.as_str(), "deep-secret-12345");
    }

    // ── Tenant-isolation tests carried forward ────────────

    #[test]
    fn tenant_isolation_flag_defaults_to_false() {
        let s = fresh_store();
        assert!(!s.tenant_isolation_enabled());
    }

    #[test]
    fn tenant_isolation_opt_in_enables_flag() {
        let s = isolated_store();
        assert!(s.tenant_isolation_enabled());
    }

    #[test]
    fn list_for_tenant_hides_cross_tenant_rows() {
        let s = isolated_store();
        s.store_for_tenant(
            "a",
            "v",
            CredentialKind::ApiKey,
            None,
            None,
            None,
            None,
            Some("tenant-a"),
        )
        .unwrap();
        s.store_for_tenant(
            "b",
            "v",
            CredentialKind::ApiKey,
            None,
            None,
            None,
            None,
            Some("tenant-b"),
        )
        .unwrap();
        let only_a = s.list_for_tenant(None, Some("tenant-a")).unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].name, "a");
    }

    #[test]
    fn list_for_tenant_fails_closed_on_missing_tenant() {
        let s = isolated_store();
        let err = s.list_for_tenant(None, None).unwrap_err();
        assert!(matches!(err, CredentialError::MissingTenant));
        let err = s.list_for_tenant(None, Some("   ")).unwrap_err();
        assert!(matches!(err, CredentialError::MissingTenant));
    }

    #[test]
    fn list_for_tenant_falls_through_when_isolation_disabled() {
        let s = fresh_store();
        s.store("a", "v", CredentialKind::ApiKey, None, None, None, None)
            .unwrap();
        let rows = s.list_for_tenant(None, None).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn get_for_tenant_returns_none_on_cross_tenant_name() {
        let s = isolated_store();
        s.store_for_tenant(
            "shared_name",
            "alpha-secret",
            CredentialKind::ApiKey,
            None,
            None,
            None,
            None,
            Some("tenant-a"),
        )
        .unwrap();
        let absent = s
            .get_for_tenant("shared_name", None, Some("tenant-b"))
            .unwrap();
        assert!(absent.is_none());
        let present = s
            .get_for_tenant("shared_name", None, Some("tenant-a"))
            .unwrap();
        assert_eq!(present.unwrap().value.as_str(), "alpha-secret");
    }

    #[test]
    fn get_for_tenant_fails_closed_on_missing_tenant() {
        let s = isolated_store();
        let err = s.get_for_tenant("anything", None, None).unwrap_err();
        assert!(matches!(err, CredentialError::MissingTenant));
    }

    #[test]
    fn store_for_tenant_fails_closed_on_missing_tenant() {
        let s = isolated_store();
        let err = s
            .store_for_tenant(
                "a",
                "v",
                CredentialKind::ApiKey,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap_err();
        assert!(matches!(err, CredentialError::MissingTenant));
    }

    // ── SEC PART 7: key-version rotation ──────────────────

    #[test]
    fn credential_encrypted_with_v1_decrypts_when_v1_key_configured() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.db");
        let mut v1_only = KeyVersionMap::default();
        v1_only.insert("v1".into(), "alpha".into());
        let s = CredentialStore::open_with_params(&path, v1_only, KdfParams::for_tests(), false)
            .unwrap();
        s.store(
            "k",
            "secret-x",
            CredentialKind::ApiKey,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        // Round trip immediately.
        let v = s.get("k", None).unwrap().unwrap();
        assert_eq!(v.value.as_str(), "secret-x");
        assert_eq!(v.key_version.as_deref(), Some("v1"));
    }

    #[test]
    fn rotate_vault_key_re_encrypts_every_row_under_new_active_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.db");
        let mut v1_only = KeyVersionMap::default();
        v1_only.insert("v1".into(), "alpha".into());
        let s1 = CredentialStore::open_with_params(&path, v1_only, KdfParams::for_tests(), false)
            .unwrap();
        s1.store(
            "k1",
            "secret-1",
            CredentialKind::ApiKey,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        s1.store(
            "k2",
            "secret-2",
            CredentialKind::ApiKey,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        drop(s1);
        // Reopen with v1 + v2 wired; rotate-vault-key uses v2.
        let mut both = KeyVersionMap::default();
        both.insert("v1".into(), "alpha".into());
        both.insert("v2".into(), "beta-new-master".into());
        let s2 =
            CredentialStore::open_with_params(&path, both, KdfParams::for_tests(), false).unwrap();
        assert_eq!(s2.active_key_version(), "v2");
        let report = s2.rotate_vault_key(Some("operator")).unwrap();
        assert_eq!(report.rows_rotated, 2);
        assert_eq!(report.active_version, "v2");
        // Both rows now decrypt under v2 (the key_for_row
        // path consults the row's stamped key_version).
        let v1 = s2.get("k1", None).unwrap().unwrap();
        let v2 = s2.get("k2", None).unwrap().unwrap();
        assert_eq!(v1.value.as_str(), "secret-1");
        assert_eq!(v2.value.as_str(), "secret-2");
        assert_eq!(v1.key_version.as_deref(), Some("v2"));
        assert_eq!(v2.key_version.as_deref(), Some("v2"));
        drop(s2);
        // Confirm even without v1 configured the vault now opens
        // (every row is under v2).
        let mut v2_only = KeyVersionMap::default();
        v2_only.insert("v2".into(), "beta-new-master".into());
        let s3 = CredentialStore::open_with_params(&path, v2_only, KdfParams::for_tests(), false)
            .unwrap();
        let v1 = s3.get("k1", None).unwrap().unwrap();
        assert_eq!(v1.value.as_str(), "secret-1");
    }

    #[test]
    fn version_rank_orders_v10_above_v9() {
        // BTreeMap lexicographic order would put v10 before v9
        // — version_rank uses the numeric suffix to fix that.
        assert!(version_rank("v10") > version_rank("v9"));
        assert!(version_rank("v2") > version_rank("v1"));
        assert!(version_rank("v100") > version_rank("v99"));
    }

    // ── helpers ───────────────────────────────────────────

    /// Build a legacy v1-format vault on disk: credentials
    /// table + one row encrypted under the SHA-256-derived
    /// key + NO vault_metadata table. Used by the migration
    /// + legacy-refusal tests.
    fn legacy_seed_vault(path: &std::path::Path, master: &str, name: &str, value: &str) {
        let conn = Connection::open(path).unwrap();
        crate::db::apply_pragmas(&conn).unwrap();
        crate::db::ensure_migration_table(&conn).unwrap();
        // Run only migration 1 — the v1 schema didn't yet
        // include tenant_id or key_version columns.
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
             );",
        )
        .unwrap();
        let legacy_key = derive_legacy_sha256_key(master);
        let enc = encrypt(&legacy_key, value).unwrap();
        let enc_json = serde_json::to_string(&enc).unwrap();
        let now = unix_ms();
        let id = format!("cred_{}", uuid::Uuid::new_v4().simple());
        conn.execute(
            "INSERT INTO credentials \
             (id, name, value_encrypted, kind, owner_agent, created_at_ms, updated_at_ms, \
              expires_at_ms, last_rotated_at_ms, rotation_interval_secs, next_rotation_at_ms, \
              revoked, revoked_at_ms, revoke_reason, version) \
             VALUES (?1, ?2, ?3, 'api_key', NULL, ?4, ?4, NULL, NULL, NULL, NULL, 0, NULL, NULL, 1)",
            params![id, name, enc_json, now],
        )
        .unwrap();
    }

    fn legacy_append_to_vault(path: &std::path::Path, master: &str, name: &str, value: &str) {
        let conn = Connection::open(path).unwrap();
        let legacy_key = derive_legacy_sha256_key(master);
        let enc = encrypt(&legacy_key, value).unwrap();
        let enc_json = serde_json::to_string(&enc).unwrap();
        let now = unix_ms();
        let id = format!("cred_{}", uuid::Uuid::new_v4().simple());
        conn.execute(
            "INSERT INTO credentials \
             (id, name, value_encrypted, kind, owner_agent, created_at_ms, updated_at_ms, \
              expires_at_ms, last_rotated_at_ms, rotation_interval_secs, next_rotation_at_ms, \
              revoked, revoked_at_ms, revoke_reason, version) \
             VALUES (?1, ?2, ?3, 'api_key', NULL, ?4, ?4, NULL, NULL, NULL, NULL, 0, NULL, NULL, 1)",
            params![id, name, enc_json, now],
        )
        .unwrap();
    }
}
