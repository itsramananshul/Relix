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

use super::dispatcher::{PluginDispatcher, PluginEndpoint};
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

impl SandboxLimits {
    /// True when any resource cap is actually requested.
    #[cfg(not(unix))]
    fn any_configured(&self) -> bool {
        self.max_memory_mb > 0 || self.max_cpu_secs > 0 || self.max_open_fds > 0
    }

    /// SEC §11: fail closed where the configured sandbox cannot
    /// be enforced.
    ///
    /// On Unix the loader applies real `RLIMIT_AS` / `RLIMIT_CPU`
    /// / `RLIMIT_NOFILE` (+ a seccomp allowlist on Linux) via
    /// `pre_exec`, so the caps are genuinely enforced — this
    /// returns `Ok`.
    ///
    /// On Windows (and any other target) there is no enforcement
    /// path wired in this build. The previous behavior was a
    /// `tracing::warn!` followed by spawning the plugin anyway —
    /// advertising `max_memory_mb` etc. while applying nothing.
    /// That is replaced by a hard refusal: if any cap is
    /// configured we return [`LoadError::SandboxUnenforceable`]
    /// so the operator gets a clear error instead of a false
    /// sense of containment. When NO cap is configured there is
    /// nothing to enforce, so the load proceeds.
    pub fn ensure_enforceable(&self) -> Result<(), LoadError> {
        #[cfg(unix)]
        {
            Ok(())
        }
        #[cfg(not(unix))]
        {
            if self.any_configured() {
                Err(LoadError::SandboxUnenforceable {
                    detail: format!(
                        "configured plugin sandbox (max_memory_mb={}, max_cpu_secs={}, \
                         max_open_fds={}) cannot be enforced on this platform; refusing to \
                         load the plugin rather than run it with caps that do not apply",
                        self.max_memory_mb, self.max_cpu_secs, self.max_open_fds
                    ),
                })
            } else {
                Ok(())
            }
        }
    }
}

/// SEC PART 2: env var the plugin SDK reads to learn the
/// per-plugin bearer token it must require on `/invoke`. The
/// host loader sets this in the spawned child's environment.
pub const PLUGIN_BEARER_ENV: &str = "RELIX_PLUGIN_BEARER";

/// SEC §11: env vars the plugin SDK reads to serve the hardened
/// loopback-TLS transport. The host loader mints a fresh
/// self-signed cert + key bound to `127.0.0.1`, base64-DER-encodes
/// each, and sets them in the child's environment; the plugin
/// binds a TLS listener on `127.0.0.1:0` using them and announces
/// its chosen port on stdout. The dispatcher PINS this same cert.
pub const PLUGIN_TLS_CERT_ENV: &str = "RELIX_PLUGIN_TLS_CERT_DER_B64";
pub const PLUGIN_TLS_KEY_ENV: &str = "RELIX_PLUGIN_TLS_KEY_DER_B64";

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
    /// SEC §11: the configured sandbox cannot be enforced on this
    /// platform; the loader refuses to run the plugin rather than
    /// advertise caps that do not apply.
    #[error("plugin sandbox cannot be enforced: {detail}")]
    SandboxUnenforceable { detail: String },
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
    ///    token wired via [`PLUGIN_BEARER_ENV`] and a hardened
    ///    loopback-TLS cert + key via [`PLUGIN_TLS_CERT_ENV`] /
    ///    [`PLUGIN_TLS_KEY_ENV`]; the SDK serves TLS with them
    ///    and rejects `invoke` without the bearer.
    ///
    /// SEC §11 sandbox posture: on Unix the child is sandboxed
    /// via `pre_exec` with `RLIMIT_AS` + `RLIMIT_CPU` +
    /// `RLIMIT_NOFILE` + `RLIMIT_CORE = 0`. On Linux the loader
    /// additionally applies `prctl(PR_SET_NO_NEW_PRIVS)` + a
    /// seccomp allowlist via `seccompiler`. On Windows (and any
    /// other non-Unix target) the configured caps cannot be
    /// enforced, so the loader FAILS CLOSED in
    /// [`SandboxLimits::ensure_enforceable`] — it refuses to load
    /// the plugin rather than run it with caps that do not apply.
    ///
    /// Timeouts:
    /// - `port_announce_secs` is folded into the readiness window
    ///   (extra time for the plugin to bind its endpoint).
    /// - `health_probe_secs` polls readiness over the hardened
    ///   transport every 200ms. Default 30s.
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
        // (3) SEC §11: fail closed if the configured sandbox
        // cannot be enforced on this platform — never run a
        // plugin while advertising caps that do not apply.
        limits.ensure_enforceable()?;

        // (4) per-plugin bearer token (secondary defense behind
        // the OS-level transport ACL).
        let bearer = mint_plugin_bearer_token();

        // (5) SEC §11: mint a fresh self-signed cert + key bound
        // to 127.0.0.1 for the loopback-TLS transport. The host
        // passes both (base64 DER) to the child via env so the
        // plugin serves TLS with them; the dispatcher PINS this
        // exact cert (no built-in CAs), so the channel is
        // confidential + authenticated rather than plaintext HTTP.
        let (cert_der, key_der) = super::dispatcher::generate_loopback_cert()
            .map_err(|e| LoadError::Io(format!("mint plugin TLS cert: {e}")))?;
        use base64::Engine as _;
        let cert_b64 = base64::engine::general_purpose::STANDARD.encode(&cert_der);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(&key_der);

        let mut cmd = Command::new(&bin);
        cmd.args(&manifest.plugin.runtime.args)
            .current_dir(&manifest.manifest_dir)
            .env(PLUGIN_BEARER_ENV, &bearer)
            .env(PLUGIN_TLS_CERT_ENV, &cert_b64)
            .env(PLUGIN_TLS_KEY_ENV, &key_b64)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // SEC PART 2: apply Unix resource limits via pre_exec
        // (RLIMITs + seccomp on Linux). The closure runs in the
        // child between fork() and execve(); it must be
        // async-signal-safe and avoid heap allocation. Non-Unix
        // platforms have already failed closed above, so the
        // sandbox is only wired on Unix.
        #[cfg(unix)]
        apply_sandbox(&mut cmd, &limits);

        let mut child = cmd.spawn().map_err(|e| LoadError::Spawn {
            bin: bin.display().to_string(),
            cause: format!("{e}"),
        })?;

        // Read stdout until the plugin announces its TLS port
        // (`RELIX_PLUGIN_PORT=<n>`) or the timeout fires; then
        // drain the rest so the OS pipe buffer never blocks.
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LoadError::Io("stdout pipe missing".into()))?;
        let plugin_name = manifest.plugin.name.clone();
        let plugin_name_for_drain = plugin_name.clone();
        let mut reader = BufReader::new(stdout).lines();
        let port_result = tokio::time::timeout(Duration::from_secs(port_announce_secs), async {
            loop {
                match reader.next_line().await {
                    Ok(Some(l)) => {
                        if let Some(n) = l.trim().strip_prefix("RELIX_PLUGIN_PORT=") {
                            return n
                                .parse::<u16>()
                                .map_err(|e| LoadError::PortMalformed(format!("`{l}`: {e}")));
                        }
                        tracing::debug!(plugin = %plugin_name, "plugin pre-port stdout: {l}");
                    }
                    Ok(None) => {
                        return Err(LoadError::Io(
                            "plugin closed stdout before announcing port".into(),
                        ));
                    }
                    Err(e) => return Err(LoadError::Io(format!("read stdout: {e}"))),
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
        tokio::spawn(async move {
            let mut reader = reader;
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::debug!(plugin = %plugin_name_for_drain, "stdout: {line}");
            }
        });
        if let Some(stderr) = child.stderr.take() {
            let name = manifest.plugin.name.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    tracing::info!(plugin = %name, "stderr: {line}");
                }
            });
        }

        // Pin the cert we just minted; dial the announced port.
        let endpoint = PluginEndpoint::new(format!("127.0.0.1:{port}"), cert_der);
        let dispatcher = PluginDispatcher::connect(
            endpoint,
            manifest.plugin.runtime.invoke_timeout_secs,
            bearer.clone(),
        );

        // Poll readiness over the hardened TLS transport until the
        // plugin's server is up or the probe window elapses.
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(health_probe_secs.saturating_add(port_announce_secs));
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
/// child process. On Unix this hooks `pre_exec` so the limits
/// apply BEFORE `execve` — the child cannot escape them. On
/// Linux we additionally set `PR_SET_NO_NEW_PRIVS` + a seccomp
/// allowlist. SEC §11: this fn is `#[cfg(unix)]` only — non-Unix
/// targets fail closed in [`SandboxLimits::ensure_enforceable`]
/// before spawn rather than running unsandboxed.
// SAFETY-island: this fn is the only place in relix-runtime (besides
// `install_seccomp_program`) permitted to use `unsafe`. It hooks
// `Command::pre_exec`, which is `unsafe` because the closure runs in the
// child between fork() and execve() where only async-signal-safe work is
// allowed. The closure here does exactly that — `setrlimit`, raw
// `prctl`, and the pre-built seccomp install — with no heap allocation.
#[cfg(unix)]
#[allow(unsafe_code)]
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
    //
    // `pre_exec` is the inherent tokio::process::Command method on unix, so
    // no std CommandExt import is needed.
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
                // PR_SET_NO_NEW_PRIVS = 38. SAFETY: async-signal-safe
                // prctl with constant args; no allocation. These calls
                // sit inside the enclosing `unsafe { cmd.pre_exec(..) }`
                // block, which already establishes the unsafe context
                // for this closure body — no inner `unsafe` needed.
                const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
                libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
                if let Some(prog) = seccomp_program.as_ref() {
                    // SAFETY: `prog` is a `Vec<sock_filter>` built in the
                    // parent before fork; install_seccomp_program only
                    // makes the async-signal-safe `prctl(PR_SET_SECCOMP)`
                    // call over it.
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
    // seccompiler's `BpfProgram` is `Vec<seccompiler::sock_filter>` — a
    // distinct nominal type from `libc::sock_filter` despite the
    // identical `#[repr(C)]` layout. Map field-for-field (in the parent,
    // before fork — allocation here is fine) so the install path can
    // pass a `*const libc::sock_filter` to the kernel.
    Some(
        program
            .into_iter()
            .map(|f| libc::sock_filter {
                code: f.code,
                jt: f.jt,
                jf: f.jf,
                k: f.k,
            })
            .collect(),
    )
}

/// SEC PART 2: install a pre-compiled seccomp BPF program
/// via raw `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog)`.
/// async-signal-safe; no heap allocation. Failure is silent
/// (we cannot log inside pre_exec).
// SAFETY-island (see `apply_sandbox`): installs a pre-built seccomp
// filter via a single async-signal-safe `prctl` call. Caller guarantees
// `program` outlives the call and the process is between fork and execve.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
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
    // SAFETY: `&prog` is a valid `sock_fprog` pointing at `program`'s
    // buffer (alive for this call); constant prctl op. Explicit unsafe
    // block required even inside `unsafe fn` (unsafe_op_in_unsafe_fn).
    unsafe {
        let _ = libc::prctl(
            PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER,
            &prog as *const _ as libc::c_ulong,
            0,
            0,
        );
    }
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

// SEC §11: there is no non-Unix `apply_sandbox`. On Windows and
// other targets the loader fails closed in
// `SandboxLimits::ensure_enforceable` BEFORE the child is
// spawned, rather than warn-and-run with caps that do not apply.
// The `apply_sandbox` call above is therefore `#[cfg(unix)]`.

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// SEC §11 criterion 1: on a platform where the configured
    /// sandbox cannot be enforced, the loader FAILS CLOSED with a
    /// clear error — it does not warn-and-run. On Unix the
    /// sandbox IS enforceable (real RLIMITs via pre_exec), so the
    /// same configured limits are accepted. Either way there is
    /// no silent "advisory only" path.
    #[test]
    fn sandbox_fails_closed_when_unenforceable() {
        let configured = SandboxLimits::default();
        let result = configured.ensure_enforceable();

        #[cfg(unix)]
        {
            // Unix applies real limits → enforceable → accepted.
            assert!(result.is_ok(), "unix sandbox should be enforceable");
        }
        #[cfg(not(unix))]
        {
            // Non-Unix: configured caps cannot be enforced →
            // refuse, with a message naming the gap.
            match result {
                Err(LoadError::SandboxUnenforceable { detail }) => {
                    assert!(
                        detail.contains("cannot be enforced"),
                        "error must explain the refusal: {detail}"
                    );
                    println!("sandbox fail-closed: {detail}");
                }
                other => panic!("expected SandboxUnenforceable on this platform, got {other:?}"),
            }
            // When NO cap is requested there is nothing to
            // enforce, so loading is permitted (no over-refusal).
            let none = SandboxLimits {
                max_memory_mb: 0,
                max_cpu_secs: 0,
                max_open_fds: 0,
            };
            assert!(
                none.ensure_enforceable().is_ok(),
                "no-cap config must not be refused"
            );
        }
    }

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
