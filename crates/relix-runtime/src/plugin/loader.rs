//! Plugin loader — spawns a plugin binary, reads its announced
//! port from stdout, polls /health until ready, and packages the
//! result as a [`LoadedPlugin`].
//!
//! Stdin/stdout/stderr posture:
//! - stdout is piped so we can read the `RELIX_PLUGIN_PORT=<n>`
//!   line. After the port is read, the remaining stdout is
//!   drained and logged at trace level. This keeps the OS
//!   pipe buffer from filling and blocking the plugin.
//! - stderr is piped + forwarded to the host's `tracing::info`
//!   stream prefixed with the plugin name.
//! - stdin is null (the plugin must not require input).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use super::dispatcher::PluginDispatcher;
use super::manifest::PluginManifest;

/// One running plugin.
pub struct LoadedPlugin {
    pub plugin_id: String,
    pub manifest: PluginManifest,
    pub manifest_path: PathBuf,
    pub dispatcher: PluginDispatcher,
    /// Wrapped in a Mutex so reload/disable can kill the
    /// subprocess without ownership conflicts.
    pub child: tokio::sync::Mutex<Option<Child>>,
}

impl LoadedPlugin {
    pub fn capabilities(&self) -> Vec<String> {
        self.manifest
            .plugin
            .capabilities
            .provides
            .iter()
            .map(|c| c.method.clone())
            .collect()
    }

    /// Kill the subprocess. Best-effort; never panics. After
    /// this returns, the dispatcher will return Transport errors
    /// on every invoke.
    pub async fn shutdown(&self) {
        let mut g = self.child.lock().await;
        if let Some(mut child) = g.take() {
            // Try a graceful kill first. tokio::process::Child::kill
            // sends SIGKILL on Unix / TerminateProcess on Windows;
            // there's no portable "ask nicely" surface in the std
            // process API, so we just kill.
            if let Err(e) = child.start_kill() {
                tracing::warn!(error = %e, "plugin: start_kill failed");
            }
            // Reap so we don't leave a zombie. 5-second cap so
            // a wedged kernel doesn't pin the controller.
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("io: {0}")]
    Io(String),
    #[error("spawn {bin}: {cause}")]
    Spawn { bin: String, cause: String },
    #[error(
        "port not announced after {secs}s — plugin did not write `RELIX_PLUGIN_PORT=<n>` to stdout"
    )]
    PortTimeout { secs: u64 },
    #[error("port line malformed: {0}")]
    PortMalformed(String),
    #[error("health probe did not pass after {secs}s")]
    HealthTimeout { secs: u64 },
}

pub struct PluginLoader;

impl PluginLoader {
    /// Walk a plugin directory (depth 1) and return the list of
    /// `plugin.toml` paths found. The host scans each at boot.
    /// A plugin can be either `plugin_dir/foo/plugin.toml` (one
    /// directory per plugin — the common shape) OR
    /// `plugin_dir/plugin.toml` (single-plugin dir).
    pub fn find_manifests(plugin_dir: &Path) -> Result<Vec<PathBuf>, LoadError> {
        let mut out = Vec::new();
        if !plugin_dir.exists() {
            return Ok(out);
        }
        // Single-file case first.
        let single = plugin_dir.join("plugin.toml");
        if single.is_file() {
            out.push(single);
        }
        // Then per-subdir case.
        let entries = std::fs::read_dir(plugin_dir)
            .map_err(|e| LoadError::Io(format!("read_dir {}: {e}", plugin_dir.display())))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let m = path.join("plugin.toml");
                if m.is_file() {
                    out.push(m);
                }
            }
        }
        // De-dup (in case plugin_dir contains both a plugin.toml
        // AND a subdir matching the canonicalised path).
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Spawn one plugin subprocess and wait for it to become
    /// healthy. The caller (the plugin_host node) then registers
    /// the plugin's capabilities on the dispatch bridge.
    ///
    /// Timeouts:
    /// - `port_announce_secs` waits for the
    ///   `RELIX_PLUGIN_PORT=<n>` line on stdout. Default 10s.
    /// - `health_probe_secs` polls /health every 200ms until it
    ///   returns 200. Default 30s.
    pub async fn spawn(
        manifest: PluginManifest,
        manifest_path: PathBuf,
        port_announce_secs: u64,
        health_probe_secs: u64,
    ) -> Result<Arc<LoadedPlugin>, LoadError> {
        let bin = manifest.resolved_binary();
        let mut cmd = Command::new(&bin);
        cmd.args(&manifest.plugin.runtime.args)
            .current_dir(&manifest.manifest_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| LoadError::Spawn {
            bin: bin.display().to_string(),
            cause: format!("{e}"),
        })?;

        // Read stdout until we either see RELIX_PLUGIN_PORT=<n>
        // or the timeout fires. After we've captured the port,
        // spawn a draining task that logs further stdout lines
        // at trace level so the OS pipe buffer never fills.
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LoadError::Io("stdout pipe missing".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| LoadError::Io("stderr pipe missing".into()))?;

        let plugin_name = manifest.plugin.name.clone();
        let plugin_name_for_drain = plugin_name.clone();
        let stderr_name = plugin_name.clone();
        let mut reader = BufReader::new(stdout).lines();
        let port_result = tokio::time::timeout(Duration::from_secs(port_announce_secs), async {
            loop {
                let line = reader.next_line().await;
                match line {
                    Ok(Some(l)) => {
                        if let Some(n) = l.trim().strip_prefix("RELIX_PLUGIN_PORT=") {
                            return n
                                .parse::<u16>()
                                .map_err(|e| LoadError::PortMalformed(format!("`{l}`: {e}")));
                        }
                        tracing::debug!(
                            plugin = %plugin_name,
                            "plugin pre-port stdout: {l}"
                        );
                    }
                    Ok(None) => {
                        return Err(LoadError::Io(
                            "plugin closed stdout before announcing port".into(),
                        ));
                    }
                    Err(e) => {
                        return Err(LoadError::Io(format!("read stdout: {e}")));
                    }
                }
            }
        })
        .await;
        let port = match port_result {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                let _ = child.start_kill();
                return Err(e);
            }
            Err(_elapsed) => {
                let _ = child.start_kill();
                return Err(LoadError::PortTimeout {
                    secs: port_announce_secs,
                });
            }
        };

        // Drain remaining stdout/stderr so the pipes don't fill.
        tokio::spawn(async move {
            let mut reader = reader;
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::debug!(plugin = %plugin_name_for_drain, "stdout: {line}");
            }
        });
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::info!(plugin = %stderr_name, "stderr: {line}");
            }
        });

        let dispatcher = PluginDispatcher::new(port, manifest.plugin.runtime.invoke_timeout_secs);

        // Poll /health until 200 or timeout.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(health_probe_secs);
        loop {
            if tokio::time::Instant::now() >= deadline {
                let _ = child.start_kill();
                return Err(LoadError::HealthTimeout {
                    secs: health_probe_secs,
                });
            }
            if let Ok(true) = dispatcher.health().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        let plugin_id = super::registry::PluginRegistry::plugin_id_for(&manifest, &manifest_path);
        Ok(Arc::new(LoadedPlugin {
            plugin_id,
            manifest,
            manifest_path,
            dispatcher,
            child: tokio::sync::Mutex::new(Some(child)),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn find_manifests_empty_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let v = PluginLoader::find_manifests(dir.path()).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn find_manifests_finds_single_plugin_in_root() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("plugin.toml");
        std::fs::File::create(&p).unwrap();
        let v = PluginLoader::find_manifests(dir.path()).unwrap();
        assert_eq!(v.len(), 1);
        assert!(v[0].ends_with("plugin.toml"));
    }

    #[test]
    fn find_manifests_finds_per_subdir() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["foo", "bar"] {
            let sub = dir.path().join(name);
            std::fs::create_dir(&sub).unwrap();
            let mut f = std::fs::File::create(sub.join("plugin.toml")).unwrap();
            f.write_all(b"# plugin").unwrap();
        }
        let v = PluginLoader::find_manifests(dir.path()).unwrap();
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn find_manifests_skips_non_directories_without_manifest() {
        let dir = tempfile::tempdir().unwrap();
        // Random file in plugin_dir, not a subdir, no plugin.toml.
        let mut f = std::fs::File::create(dir.path().join("README.md")).unwrap();
        f.write_all(b"hi").unwrap();
        let v = PluginLoader::find_manifests(dir.path()).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn find_manifests_returns_missing_dir_as_empty() {
        let v = PluginLoader::find_manifests(Path::new("./no-such-dir-zxcv")).unwrap();
        assert!(v.is_empty());
    }
}
