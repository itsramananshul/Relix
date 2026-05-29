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
    CredentialSummary, DecryptedCredential, EncryptedValue,
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
    /// Defaults to `RELIX_CREDENTIAL_KEY`.
    #[serde(default = "default_master_key_env")]
    pub master_key_env: String,
    /// How often the rotation scheduler wakes up. Defaults to
    /// 60s.
    #[serde(default = "default_rotation_check_interval_secs")]
    pub rotation_check_interval_secs: u64,
}

impl Default for CredentialsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            db_path: None,
            master_key_env: default_master_key_env(),
            rotation_check_interval_secs: default_rotation_check_interval_secs(),
        }
    }
}

fn default_master_key_env() -> String {
    "RELIX_CREDENTIAL_KEY".into()
}

fn default_rotation_check_interval_secs() -> u64 {
    60
}
