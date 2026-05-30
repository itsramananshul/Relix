//! RELIX-7.30 PART 2 — Credential lifecycle.
//!
//! A SQLite-backed credential vault for API keys + secrets
//! consumed by agents. Every value is encrypted at rest with
//! AES-256-GCM using a 32-byte key derived from a master
//! secret (`[credentials] master_key_env`, default
//! `RELIX_CREDENTIAL_KEY`). The key never lives on disk.
//!
//! Surfaces:
//!
//! - [`store::CredentialStore`] — the SQLite-backed CRUD
//!   surface (store / get / rotate / revoke / list / audit).
//! - [`scheduler::RotationScheduler`] — background task that
//!   checks every `rotation_check_interval_secs` whether a
//!   credential's `next_rotation_at_ms` has elapsed and emits
//!   a `rotation_needed` notification through the registered
//!   sink. Does NOT auto-rotate values; only notifies.
//! - [`caps::register`] — wires the six `credentials.*` caps
//!   onto a `DispatchBridge`.

pub mod caps;
pub mod scheduler;
pub mod store;

pub use scheduler::{
    RotationNotification, RotationNotifier, RotationScheduler, RotationSchedulerConfig,
};
pub use store::{
    AuditEvent, AuditRow, Credential, CredentialError, CredentialKind, CredentialStore,
    CredentialSummary, DecryptedCredential, EncryptedValue, KdfParams, KeyVersionMap, SecretString,
    VAULT_FORMAT_VERSION,
};

/// `[credentials]` config block parsed from the controller TOML.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct CredentialsConfig {
    /// Master switch. `false` (the default) keeps the
    /// controller credential-less.
    #[serde(default)]
    pub enabled: bool,
    /// SQLite path for the credential vault.
    #[serde(default)]
    pub db_path: Option<std::path::PathBuf>,
    /// Env var the controller reads to derive the AES key.
    /// Defaults to `RELIX_CREDENTIAL_KEY`. Used as the version
    /// `v1` key when no `[credentials.key_versions]` block is
    /// supplied — that case keeps single-key deployments
    /// unchanged. When `key_versions` IS configured, the
    /// active version's env var is what supplies the master
    /// secret; `master_key_env` is then unused.
    #[serde(default = "default_master_key_env")]
    pub master_key_env: String,
    /// How often the rotation scheduler wakes up. Defaults to
    /// 60s.
    #[serde(default = "default_rotation_check_interval_secs")]
    pub rotation_check_interval_secs: u64,
    /// SEC PART 1: Argon2id memory cost in KiB. Default
    /// 65_536 (64 MB). Raising lifts the per-derivation cost
    /// linearly; lowering pushes brute-force feasibility back
    /// toward the legacy SHA-256 baseline.
    #[serde(default = "default_argon2_memory_cost")]
    pub argon2_memory_cost: u32,
    /// SEC PART 1: Argon2id time cost (iterations). Default 3.
    #[serde(default = "default_argon2_time_cost")]
    pub argon2_time_cost: u32,
    /// SEC PART 1: Argon2id parallelism (lanes). Default 4.
    #[serde(default = "default_argon2_parallelism")]
    pub argon2_parallelism: u32,
    /// SEC PART 7: `[credentials.key_versions]` map —
    /// version name → env var name. Each value lookup reads
    /// `std::env::var(<value>)` to source the master secret
    /// for that version. The ACTIVE version is the highest-
    /// numbered entry whose env var is set and non-empty;
    /// new credentials are encrypted under it. Existing rows
    /// carry their own `key_version` so they always decrypt
    /// with the matching key.
    ///
    /// Empty (the default) collapses to a single implicit
    /// version `v1` sourced from `master_key_env`.
    #[serde(default)]
    pub key_versions: std::collections::BTreeMap<String, String>,
}

impl Default for CredentialsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            db_path: None,
            master_key_env: default_master_key_env(),
            rotation_check_interval_secs: default_rotation_check_interval_secs(),
            argon2_memory_cost: default_argon2_memory_cost(),
            argon2_time_cost: default_argon2_time_cost(),
            argon2_parallelism: default_argon2_parallelism(),
            key_versions: std::collections::BTreeMap::new(),
        }
    }
}

fn default_master_key_env() -> String {
    "RELIX_CREDENTIAL_KEY".into()
}

fn default_rotation_check_interval_secs() -> u64 {
    60
}

fn default_argon2_memory_cost() -> u32 {
    65_536
}

fn default_argon2_time_cost() -> u32 {
    3
}

fn default_argon2_parallelism() -> u32 {
    4
}

impl CredentialsConfig {
    /// Project the Argon2id-related fields into a
    /// [`KdfParams`] for the store constructors.
    pub fn kdf_params(&self) -> KdfParams {
        KdfParams {
            memory_cost_kib: self.argon2_memory_cost,
            time_cost: self.argon2_time_cost,
            parallelism: self.argon2_parallelism,
        }
    }

    /// Build a [`KeyVersionMap`] from the configured
    /// `key_versions` block. Reads each value's env var at
    /// call time so operators rotating env vars only need to
    /// restart the controller (not re-read TOML).
    ///
    /// When `key_versions` is empty, falls back to a single
    /// implicit `v1 → master_key_env` mapping so existing
    /// single-key deployments keep working without TOML
    /// changes.
    pub fn key_versions_resolved(&self) -> KeyVersionMap {
        let mut map = KeyVersionMap::default();
        if self.key_versions.is_empty() {
            let v = std::env::var(&self.master_key_env).unwrap_or_default();
            if !v.is_empty() {
                map.insert("v1".to_string(), v);
            }
            return map;
        }
        for (name, env_var) in &self.key_versions {
            let v = std::env::var(env_var).unwrap_or_default();
            if !v.is_empty() {
                map.insert(name.clone(), v);
            }
        }
        map
    }
}
