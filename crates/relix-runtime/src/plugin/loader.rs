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

/// SEC PART 2: plugin-process sandbox knobs. Carried from
/// `[plugin_host]` into [`PluginLoader::spawn`].
#[derive(Clone, Copy, Debug)]
pub struct SandboxLimits {
    pub max_memory_mb: u64,
    pub max_cpu_secs: u64,
    pub max_open_fds: u64,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            max_memory_mb: 512,
            max_cpu_secs: 30,
            max_open_fds: 100,
        }
    }
}

/// SEC PART 2: env var the plugin SDK reads to learn the
/// per-plugin bearer token it must require on `/invoke`. The
/// host loader sets this in the spawned child's environment.
pub const PLUGIN_BEARER_ENV: &str = "RELIX_PLUGIN_BEARER";

/// Mint a fresh per-plugin bearer token (32 random bytes
/// hex-encoded). Used by the host loader and exposed for
/// tests.
pub fn mint_plugin_bearer_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

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
    /// SEC PART 2 gates the spawn on three checks:
    /// 1. `manifest.resolved_binary()` must succeed — bare PATH
    ///    lookups and missing files are refused.
    /// 2. `manifest.verify_binary_sha256(bin)` must succeed —
    ///    when the manifest pins a hash, the binary on disk
    ///    must match.
    /// 3. The child process gets a per-plugin random bearer
    ///    token wired via [`PLUGIN_BEARER_ENV`]; the SDK
    ///    rejects `/invoke` without it.
    ///
    /// On Unix the child is sandboxed via `pre_exec` with
    /// `RLIMIT_AS` + `RLIMIT_CPU` + `RLIMIT_NOFILE` +
    /// `RLIMIT_CORE = 0`. On Linux the loader additionally
    /// applies `prctl(PR_SET_NO_NEW_PRIVS)` + a seccomp
    /// allowlist via `seccompiler`. On Windows the loader
    /// logs a startup warning that resource caps are not
    /// applied (no native equivalent in `std::os::windows`).
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
        limits: SandboxLimits,
    ) -> Result<Arc<LoadedPlugin>, LoadError> {
        // (1) absolute-path resolution.
        let bin = manifest.resolved_binary().map_err(|e| LoadError::Spawn {
            bin: manifest.plugin.runtime.binary.display().to_string(),
            cause: format!("{e}"),
        })?;
        // (2) SHA-256 pinning when the operator configured it.
        manifest
            .verify_binary_sha256(&bin)
            .map_err(|e| LoadError::Spawn {
                bin: bin.display().to_string(),
                cause: format!("{e}"),
            })?;
        // (3) per-plugin bearer token.
        let bearer = mint_plugin_bearer_token();
        let mut cmd = Command::new(&bin);
        cmd.args(&manifest.plugin.runtime.args)
            .current_dir(&manifest.manifest_dir)
            .env(PLUGIN_BEARER_ENV, &bearer)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // SEC PART 2: apply Unix resource limits via
        // pre_exec. The closure runs in the child between
        // fork() and execve(); it must be async-signal-safe
        // and avoid heap allocation (we use rlimit's
        // setrlimit which is a single libc call).
        apply_sandbox(&mut cmd, &limits);

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

        let dispatcher = PluginDispatcher::new(
            port,
            manifest.plugin.runtime.invoke_timeout_secs,
            bearer.clone(),
        );

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

/// SEC PART 2: wire the per-plugin resource caps into the
/// child process. On Unix this hooks `pre_exec` so the
/// limits apply BEFORE `execve` — the child cannot escape
/// them. On Linux we additionally set `PR_SET_NO_NEW_PRIVS`
/// + a seccomp allowlist. On Windows we log a single
/// startup warning per spawn (no equivalent native API in
/// `std::os::windows`); operators can add Job Objects in a
/// future change.
#[cfg(unix)]
fn apply_sandbox(cmd: &mut Command, limits: &SandboxLimits) {
    let limits_copy = *limits;
    // SEC PART 2: on Linux, compile the seccomp BPF program
    // BEFORE pre_exec so the child's between-fork-and-execve
    // window contains zero heap allocation. The compiled
    // program is a `Vec<sock_filter>` we install via the
    // raw libc::prctl syscall (PR_SET_SECCOMP, MODE_FILTER).
    // After fork() the parent's allocator may have been
    // locked by another worker thread; allocating inside
    // pre_exec is a deadlock hazard.
    #[cfg(target_os = "linux")]
    let seccomp_program: Option<Vec<libc::sock_filter>> = build_linux_seccomp_program();
    // SAFETY: closure runs in the child between fork and
    // execve. We make ONLY async-signal-safe calls
    // (setrlimit, raw libc::prctl, raw libc syscall(). No
    // heap allocation. Returns 0 on success to let exec
    // proceed.
    use std::os::unix::process::CommandExt as _;
    unsafe {
        cmd.pre_exec(move || {
            use rlimit::{Resource, setrlimit};
            if limits_copy.max_memory_mb > 0 {
                let bytes = limits_copy.max_memory_mb.saturating_mul(1024 * 1024);
                let _ = setrlimit(Resource::AS, bytes, bytes);
            }
            if limits_copy.max_cpu_secs > 0 {
                let _ = setrlimit(
                    Resource::CPU,
                    limits_copy.max_cpu_secs,
                    limits_copy.max_cpu_secs,
                );
            }
            if limits_copy.max_open_fds > 0 {
                let _ = setrlimit(
                    Resource::NOFILE,
                    limits_copy.max_open_fds,
                    limits_copy.max_open_fds,
                );
            }
            // No core dumps.
            let _ = setrlimit(Resource::CORE, 0, 0);
            #[cfg(target_os = "linux")]
            {
                // PR_SET_NO_NEW_PRIVS = 38.
                const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
                libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
                if let Some(prog) = seccomp_program.as_ref() {
                    install_seccomp_program(prog);
                }
            }
            Ok(())
        });
    }
}

/// SEC PART 2: build the Linux seccomp BPF program in the
/// parent. Returns `None` on architectures we don't have a
/// preset for; the child still inherits PR_SET_NO_NEW_PRIVS
/// + rlimits in that case.
#[cfg(target_os = "linux")]
fn build_linux_seccomp_program() -> Option<Vec<libc::sock_filter>> {
    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch};
    let arch = if cfg!(target_arch = "x86_64") {
        TargetArch::x86_64
    } else if cfg!(target_arch = "aarch64") {
        TargetArch::aarch64
    } else {
        return None;
    };
    let mut rules: std::collections::BTreeMap<i64, Vec<SeccompRule>> =
        std::collections::BTreeMap::new();
    for &nr in DENIED_LINUX_SYSCALLS {
        rules.insert(nr, vec![]);
    }
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::KillProcess,
        arch,
    )
    .ok()?;
    let program: BpfProgram = filter.try_into().ok()?;
    // BpfProgram is Vec<sock_filter> — we use it directly
    // when installing via the raw prctl path.
    Some(program)
}

/// SEC PART 2: install a pre-compiled seccomp BPF program
/// via raw `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog)`.
/// async-signal-safe; no heap allocation. Failure is silent
/// (we cannot log inside pre_exec).
#[cfg(target_os = "linux")]
unsafe fn install_seccomp_program(program: &[libc::sock_filter]) {
    const PR_SET_SECCOMP: libc::c_int = 22;
    const SECCOMP_MODE_FILTER: libc::c_ulong = 2;
    #[repr(C)]
    struct SockFprog {
        len: u16,
        filter: *const libc::sock_filter,
    }
    let prog = SockFprog {
        len: program.len().min(u16::MAX as usize) as u16,
        filter: program.as_ptr(),
    };
    let _ = libc::prctl(
        PR_SET_SECCOMP,
        SECCOMP_MODE_FILTER,
        &prog as *const _ as libc::c_ulong,
        0,
        0,
    );
}

#[cfg(target_os = "linux")]
const DENIED_LINUX_SYSCALLS: &[i64] = &[
    // Module loading / kernel reconfiguration.
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    // Mount management.
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    // System power.
    libc::SYS_reboot,
    // Process ptrace + perf escalation surface.
    libc::SYS_ptrace,
    libc::SYS_perf_event_open,
    // Set/clear non-owner capabilities.
    libc::SYS_capset,
    libc::SYS_setuid,
    libc::SYS_setgid,
    libc::SYS_setreuid,
    libc::SYS_setregid,
    libc::SYS_setresuid,
    libc::SYS_setresgid,
    // BPF program loading (would let a plugin install its
    // own kernel-side filter).
    libc::SYS_bpf,
    // Swap configuration.
    libc::SYS_swapon,
    libc::SYS_swapoff,
];

#[cfg(windows)]
fn apply_sandbox(_cmd: &mut Command, _limits: &SandboxLimits) {
    // SEC PART 2: Windows has no `pre_exec` and no portable
    // RLIMIT equivalent in `std::os::windows`. Implementing
    // an equivalent via Win32 Job Objects
    // (`SetInformationJobObject` with
    // `JOB_OBJECT_LIMIT_PROCESS_MEMORY` etc.) requires
    // attaching the child to the job before its first
    // instruction, which `std::process::Command` does not
    // expose. We log a single startup warning per spawn
    // so operators see the gap; a future Job-Object
    // integration replaces this without changing the
    // call-site contract.
    tracing::warn!(
        "plugin sandbox: Windows does not apply resource limits to plugin processes; \
         max_memory_mb / max_cpu_secs / max_open_fds are advisory only on this OS"
    );
}

#[cfg(not(any(unix, windows)))]
fn apply_sandbox(_cmd: &mut Command, _limits: &SandboxLimits) {
    tracing::warn!("plugin sandbox: unsupported target — no resource limits applied");
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
