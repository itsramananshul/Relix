//! Plugin host — loads external plugins over the `relix-plugin-v1`
//! HTTP/JSON protocol and registers their capabilities on the
//! local dispatch bridge.
//!
//! Architecture:
//!
//! ```text
//!   plugin.toml + ./binary
//!         │
//!         ▼
//!   PluginLoader.spawn()
//!         │           (pipes stdout, waits for `RELIX_PLUGIN_PORT=<n>`,
//!         │            polls /health)
//!         ▼
//!   PluginRecord (in-memory + registry)
//!         │
//!         ▼
//!   DispatchBridge.register("method.name", FnHandler → PluginDispatcher.invoke)
//!         │
//!         ▼
//!   Remote mesh peers call the method like any other capability —
//!   their request flows through the bridge, into a plugin /invoke,
//!   and the reply rides back out.
//! ```
//!
//! See `docs/plugins.md` for the full operator-facing reference
//! and the wire shape.

pub mod dispatcher;
pub mod loader;
pub mod manifest;
pub mod registry;

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;

pub use dispatcher::{InvokeRequest, PluginDispatcher, PluginEndpoint, PluginInvokeError};
pub use loader::{LoadError, LoadedPlugin, PluginLoader, SandboxLimits};
pub use manifest::{ManifestError, PluginCapability, PluginManifest, PluginRuntime};
pub use registry::{PluginRegistry, PluginStatus, RegistryError, StoredPlugin};

/// Per-node `[plugin_host]` config (used when
/// `node_type = "plugin_host"`).
#[derive(Clone, Debug, Deserialize)]
pub struct PluginHostConfig {
    /// Directory the loader scans for `plugin.toml` files.
    /// May contain one plugin or a directory of plugin
    /// directories — the loader walks at depth 1.
    pub plugin_dir: PathBuf,
    /// Hard cap on simultaneously-loaded plugins. Exceeding it
    /// at startup logs a warning and stops scanning — the
    /// already-loaded plugins remain active.
    #[serde(default = "default_max_plugins")]
    pub max_plugins: usize,
    /// Path to the SQLite database the registry persists into.
    /// Defaults to `./plugin-registry.db` under the plugin
    /// host's `RELIX_DATA_DIR`.
    #[serde(default)]
    pub registry_db_path: Option<PathBuf>,
    /// SEC PART 2: per-plugin virtual-memory cap, applied on
    /// Unix via `RLIMIT_AS`. 0 means "do not apply" — used by
    /// tests. Default 512 MiB.
    #[serde(default = "default_max_memory_mb")]
    pub max_memory_mb: u64,
    /// SEC PART 2: per-plugin CPU-time cap (RLIMIT_CPU), in
    /// seconds. 0 means "do not apply". Default 30.
    #[serde(default = "default_max_cpu_secs")]
    pub max_cpu_secs: u64,
}

fn default_max_plugins() -> usize {
    20
}

fn default_max_memory_mb() -> u64 {
    512
}

fn default_max_cpu_secs() -> u64 {
    30
}

/// Errors emerging from plugin operations that need a stable
/// public surface.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("manifest: {0}")]
    Manifest(#[from] ManifestError),
    #[error("load: {0}")]
    Load(#[from] LoadError),
    #[error("invoke: {0}")]
    Invoke(#[from] PluginInvokeError),
    #[error("registry: {0}")]
    Registry(#[from] RegistryError),
}

/// Bag of state the plugin_host carries — registry + the live
/// dispatcher map keyed by plugin_id. Wrapped in `Arc` and
/// `tokio::sync::RwLock` so the management capabilities can
/// list / disable / reload safely.
#[derive(Clone)]
pub struct PluginHostState {
    pub registry: Arc<PluginRegistry>,
    /// Live in-memory map. Populated at boot by the loader and
    /// kept in sync with the registry by reload / disable.
    pub plugins: Arc<tokio::sync::RwLock<std::collections::HashMap<String, Arc<LoadedPlugin>>>>,
}

impl PluginHostState {
    pub fn new(registry: Arc<PluginRegistry>) -> Self {
        Self {
            registry,
            plugins: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        }
    }
}
