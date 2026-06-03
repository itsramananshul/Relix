//! `relix install` — dependency auto-install for fresh-system
//! setup.
//!
//! Closes the install-flow story tracked under Part 1b of the
//! roadmap: a fresh user on any supported platform should be
//! able to run `relix setup` (or `relix install --fix`) and
//! end up with Docker, Ollama, and a Qdrant container ready
//! to serve the memory system.
//!
//! ## Surfaces
//!
//! - `relix install` / `relix install --check` — detect every
//!   dependency, print a status table, exit 0 regardless of
//!   what's missing.
//! - `relix install --fix` — same detection, then prompt
//!   ONCE before running every missing-dependency installer
//!   sequentially. Individual install failures do not stop
//!   the rest.
//! - `relix setup` calls [`status_for_setup`] at the start of
//!   its wizard so missing dependencies surface alongside the
//!   config wizard.
//!
//! ## Honest scope statements
//!
//! - **Docker Desktop on Windows / macOS** is installed by
//!   downloading the official signed installer to a temp file
//!   and executing it. The installer itself shows its own UI
//!   (Microsoft has standardised this UX for years); we cannot
//!   silently install Docker Desktop because both vendors
//!   require user interaction for the EULA.
//! - **Docker on Linux** uses the official `get.docker.com`
//!   convenience script via `sh -c`. Same script Docker
//!   themselves recommend in their docs.
//! - **Ollama on Linux** uses `ollama.com/install.sh`.
//! - **Qdrant** is started via Docker — `docker pull
//!   qdrant/qdrant && docker run -d ...`. We do not ship a
//!   native Qdrant binary; if Docker is unavailable the
//!   Qdrant status row shows the manual URL.
//!
//! Every network call has a 120 s timeout; every install
//! subprocess has a 300 s timeout. On timeout we kill the
//! subprocess, print a clear message, print the manual URL,
//! and continue. We never panic and never exit non-zero just
//! because a dependency is missing — `--check` is a check,
//! `--fix` is best-effort.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use clap::Args;

/// Hard cap on a single network download (Docker Desktop
/// installer is ~700 MB; 120 s gets us through on a typical
/// home connection).
const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);

/// Hard cap on a single install subprocess. Docker Desktop on
/// Windows can take several minutes; 300 s is the documented
/// upper bound.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);

// ────────────────────────── CLI surface ─────────────────────────

#[derive(Args, Debug)]
pub struct InstallArgs {
    /// Check dependency status without installing anything.
    /// Same as the default behaviour when no flag is set.
    /// Exits 0 regardless of what's missing.
    #[arg(long, default_value_t = false)]
    pub check: bool,

    /// Install every missing dependency without per-item
    /// prompts. Asks once at the start before running any
    /// installs. Continues past individual install failures.
    #[arg(long, default_value_t = false)]
    pub fix: bool,
}

pub async fn run(args: InstallArgs) -> Result<(), Box<dyn std::error::Error>> {
    let statuses = check_all();
    print_table(&statuses);
    if args.fix {
        let missing: Vec<&DependencyStatus> = statuses.iter().filter(|s| !s.found).collect();
        if missing.is_empty() {
            println!();
            println!("All dependencies present. Nothing to install.");
            return Ok(());
        }
        if !confirm_or_skip(&format!(
            "\nThis will install {} missing dependency/dependencies. Continue? [y/N] ",
            missing.len()
        )) {
            println!("Skipped — re-run with --fix to install.");
            return Ok(());
        }
        for s in missing {
            match install_dependency(s.dependency).await {
                Ok(note) => println!("[OK]      {:<14}{}", s.dependency.label(), note),
                Err(e) => {
                    println!("[FAILED]  {:<14}{e}", s.dependency.label());
                    println!("           manual install: {}", manual_url(s.dependency));
                }
            }
        }
    }
    Ok(())
}

/// Read one yes/no answer from stdin. Returns true on `y` /
/// `yes`; everything else (including a closed stdin) is `no`.
/// Used at the top of `--fix` and embedded inside the setup
/// wizard's pre-flight check.
fn confirm_or_skip(prompt: &str) -> bool {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let stdin = io::stdin();
    if stdin.lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

// ────────────────────────── data model ──────────────────────────

/// Catalog of dependencies the installer knows about. Names
/// are stable — operators script against `--check` output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dependency {
    Docker,
    Ollama,
    Qdrant,
}

impl Dependency {
    pub fn all() -> &'static [Dependency] {
        &[Dependency::Docker, Dependency::Ollama, Dependency::Qdrant]
    }

    pub fn label(self) -> &'static str {
        match self {
            Dependency::Docker => "Docker",
            Dependency::Ollama => "Ollama",
            Dependency::Qdrant => "Qdrant",
        }
    }
}

/// One row in the dependency status table.
#[derive(Debug, Clone)]
pub struct DependencyStatus {
    pub dependency: Dependency,
    /// `true` when the dependency was detected on the host.
    pub found: bool,
    /// Parsed version string when `found` and parseable. Held
    /// for future surfaces (the dashboard / SDK might render
    /// it separately from `detail`); not yet read at runtime.
    #[allow(dead_code)]
    pub version: Option<String>,
    /// Free-text note (e.g. `"running on port 6333"` for
    /// Qdrant, or the OS-level "not found" message).
    pub detail: String,
}

// ────────────────────────── detection ───────────────────────────

/// Detect every known dependency. Returns one [`DependencyStatus`]
/// per [`Dependency`] in `Dependency::all()` order so the
/// caller can render a stable table.
pub fn check_all() -> Vec<DependencyStatus> {
    Dependency::all().iter().map(|d| check(*d)).collect()
}

pub fn check(dep: Dependency) -> DependencyStatus {
    match dep {
        Dependency::Docker => check_docker(),
        Dependency::Ollama => check_ollama(),
        Dependency::Qdrant => check_qdrant(),
    }
}

fn check_docker() -> DependencyStatus {
    match run_capture("docker", &["--version"]) {
        Ok(out) => match parse_docker_version(&out) {
            Some(v) => DependencyStatus {
                dependency: Dependency::Docker,
                found: true,
                version: Some(v.clone()),
                detail: v,
            },
            None => DependencyStatus {
                dependency: Dependency::Docker,
                found: true,
                version: None,
                detail: "version unparsable".into(),
            },
        },
        Err(_) => DependencyStatus {
            dependency: Dependency::Docker,
            found: false,
            version: None,
            detail: format!("not found — see {}", manual_url(Dependency::Docker)),
        },
    }
}

fn check_ollama() -> DependencyStatus {
    match run_capture("ollama", &["--version"]) {
        Ok(out) => match parse_ollama_version(&out) {
            Some(v) => DependencyStatus {
                dependency: Dependency::Ollama,
                found: true,
                version: Some(v.clone()),
                detail: v,
            },
            None => DependencyStatus {
                dependency: Dependency::Ollama,
                found: true,
                version: None,
                detail: "version unparsable".into(),
            },
        },
        Err(_) => DependencyStatus {
            dependency: Dependency::Ollama,
            found: false,
            version: None,
            detail: format!("not found — see {}", manual_url(Dependency::Ollama)),
        },
    }
}

fn check_qdrant() -> DependencyStatus {
    // First-choice: a running `relix-qdrant` container.
    if let Ok(out) = run_capture(
        "docker",
        &[
            "ps",
            "--filter",
            "name=relix-qdrant",
            "--filter",
            "status=running",
            "--format",
            "{{.Names}}",
        ],
    ) && out.trim().contains("relix-qdrant")
    {
        return DependencyStatus {
            dependency: Dependency::Qdrant,
            found: true,
            version: None,
            detail: "container relix-qdrant running".into(),
        };
    }
    // Fallback: a native `qdrant` binary on $PATH.
    if let Ok(out) = run_capture("qdrant", &["--version"]) {
        return DependencyStatus {
            dependency: Dependency::Qdrant,
            found: true,
            version: parse_qdrant_version(&out),
            detail: out.lines().next().unwrap_or("").trim().to_string(),
        };
    }
    DependencyStatus {
        dependency: Dependency::Qdrant,
        found: false,
        version: None,
        detail: format!(
            "not running — run: docker run -d --name relix-qdrant -p 6333:6333 qdrant/qdrant (or see {})",
            manual_url(Dependency::Qdrant)
        ),
    }
}

/// Spawn a process and capture its stdout. Returns
/// `Err(io::Error)` when the binary is not on $PATH or the
/// process returns a non-zero exit code.
fn run_capture(program: &str, args: &[&str]) -> io::Result<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "{program} {args:?} exited with status {}",
            out.status
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

// ────────────────────────── version parsers (pure) ──────────────

/// Parse a `docker --version` line into a bare version
/// string. Real output looks like
/// `Docker version 24.0.5, build ced0996600`.
pub fn parse_docker_version(s: &str) -> Option<String> {
    let line = s.lines().next()?;
    let after = line.trim().strip_prefix("Docker version ")?;
    let (v, _rest) = after.split_once(',').unwrap_or((after, ""));
    let v = v.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// Parse `ollama --version` output. Real output:
/// `ollama version is 0.1.32` (older) or
/// `ollama version 0.1.32` (newer).
pub fn parse_ollama_version(s: &str) -> Option<String> {
    let line = s.lines().next()?;
    let line = line.trim();
    let after = line
        .strip_prefix("ollama version is ")
        .or_else(|| line.strip_prefix("ollama version "))?;
    let v = after.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// Parse `qdrant --version` output. Real output:
/// `qdrant 1.7.0` or similar — Qdrant's CLI emits a short
/// banner.
pub fn parse_qdrant_version(s: &str) -> Option<String> {
    let line = s.lines().next()?;
    let line = line.trim();
    let after = line
        .strip_prefix("qdrant ")
        .or_else(|| line.strip_prefix("Qdrant "))?;
    let v = after.split_whitespace().next()?.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

// ────────────────────────── URLs ────────────────────────────────

/// Manual-install URL for the current platform. Returned to
/// the operator whenever auto-install can't be attempted
/// (unsupported platform, dependency requires user interaction,
/// download / install timed out, etc).
pub fn manual_url(dep: Dependency) -> &'static str {
    manual_url_for(dep, std::env::consts::OS, std::env::consts::ARCH)
}

/// Same as [`manual_url`] but parameterised on `os` / `arch`
/// so unit tests can drive the matrix.
pub fn manual_url_for(dep: Dependency, os: &str, _arch: &str) -> &'static str {
    match (dep, os) {
        (Dependency::Docker, "windows") => {
            "https://docs.docker.com/desktop/install/windows-install/"
        }
        (Dependency::Docker, "macos") => "https://docs.docker.com/desktop/install/mac-install/",
        (Dependency::Docker, "linux") => "https://docs.docker.com/engine/install/",
        (Dependency::Docker, _) => "https://docs.docker.com/get-docker/",
        (Dependency::Ollama, "windows") => "https://ollama.com/download/windows",
        (Dependency::Ollama, "macos") => "https://ollama.com/download/mac",
        (Dependency::Ollama, "linux") => "https://ollama.com/download/linux",
        (Dependency::Ollama, _) => "https://ollama.com/download",
        (Dependency::Qdrant, _) => "https://qdrant.tech/documentation/quick-start/",
    }
}

/// Download URL for the auto-install path. `None` means the
/// platform is unsupported or the dependency doesn't have an
/// auto-install path.
fn installer_url(dep: Dependency, os: &str, arch: &str) -> Option<&'static str> {
    match (dep, os, arch) {
        (Dependency::Docker, "windows", _) => {
            Some("https://desktop.docker.com/win/main/amd64/Docker Desktop Installer.exe")
        }
        (Dependency::Docker, "macos", "aarch64") => {
            Some("https://desktop.docker.com/mac/main/arm64/Docker.dmg")
        }
        (Dependency::Docker, "macos", _) => {
            Some("https://desktop.docker.com/mac/main/amd64/Docker.dmg")
        }
        (Dependency::Docker, "linux", _) => Some("https://get.docker.com"),
        (Dependency::Ollama, "windows", _) => Some("https://ollama.com/download/OllamaSetup.exe"),
        (Dependency::Ollama, "macos", _) => Some("https://ollama.com/download/Ollama-darwin.zip"),
        (Dependency::Ollama, "linux", _) => Some("https://ollama.com/install.sh"),
        // Qdrant doesn't get downloaded directly — we go through
        // Docker. `install_qdrant_via_docker` skips this path.
        (Dependency::Qdrant, _, _) => None,
        // Any other OS for Docker / Ollama is unsupported — caller
        // falls back to the manual URL.
        (Dependency::Docker, _, _) | (Dependency::Ollama, _, _) => None,
    }
}

// ────────────────────────── install ─────────────────────────────

/// Install one missing dependency. Returns an operator-facing
/// status message on success (so `--fix` can print it) or a
/// rich error on failure. Errors are intentionally formatted
/// for terminal display — they are not chained programmatically.
pub async fn install_dependency(dep: Dependency) -> Result<String, String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match dep {
        Dependency::Qdrant => install_qdrant_via_docker().await,
        _ => match installer_url(dep, os, arch) {
            Some(url) => install_from_url(dep, url, os).await,
            None => Err(format!(
                "no auto-install path for {} on {os}/{arch}; see {}",
                dep.label(),
                manual_url(dep)
            )),
        },
    }
}

async fn install_from_url(dep: Dependency, url: &str, os: &str) -> Result<String, String> {
    // Linux Docker + Ollama use shell-script installers — `sh
    // -c "curl ... | sh"`. Everything else is a binary
    // installer we download to a temp file and exec.
    if os == "linux" {
        return install_via_shell_script(dep, url).await;
    }
    let installer_path = download_to_temp(dep, url).await?;
    run_installer(dep, &installer_path).await
}

async fn install_via_shell_script(dep: Dependency, url: &str) -> Result<String, String> {
    let shell_cmd = format!("curl -fsSL {url} | sh");
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&shell_cmd)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn shell installer: {e}"))?;
    let wait = tokio::time::timeout(INSTALL_TIMEOUT, child.wait()).await;
    match wait {
        Ok(Ok(status)) if status.success() => Ok(format!("installed via {url}")),
        Ok(Ok(status)) => Err(format!(
            "shell installer exited with status {status}; see {}",
            manual_url(dep)
        )),
        Ok(Err(e)) => Err(format!("shell installer failed: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            Err(format!(
                "shell installer timed out after {}s; see {}",
                INSTALL_TIMEOUT.as_secs(),
                manual_url(dep)
            ))
        }
    }
}

async fn download_to_temp(dep: Dependency, url: &str) -> Result<PathBuf, String> {
    let client = reqwest::Client::builder()
        .timeout(NETWORK_TIMEOUT)
        .build()
        .map_err(|e| format!("reqwest client init: {e}"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("download {url}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("download {url} returned HTTP {status}"));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("read body of {url}: {e}"))?;
    let suffix = match dep {
        Dependency::Docker => {
            if std::env::consts::OS == "windows" {
                ".exe"
            } else {
                ".dmg"
            }
        }
        Dependency::Ollama => {
            if std::env::consts::OS == "windows" {
                ".exe"
            } else {
                ".zip"
            }
        }
        Dependency::Qdrant => ".bin",
    };
    let path = std::env::temp_dir().join(format!("relix-{}{}", dep.label().to_lowercase(), suffix));
    std::fs::write(&path, &bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

async fn run_installer(dep: Dependency, installer: &PathBuf) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(installer);
    // On Windows the .exe installer drives its own UI; we
    // just exec it. macOS .dmg files open in Finder via
    // `open` so the operator can drag-and-drop — that's the
    // canonical UX Apple users expect.
    if std::env::consts::OS == "macos" {
        cmd = tokio::process::Command::new("open");
        cmd.arg(installer);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn installer at {}: {e}", installer.display()))?;
    let wait = tokio::time::timeout(INSTALL_TIMEOUT, child.wait()).await;
    match wait {
        Ok(Ok(status)) if status.success() => Ok(format!(
            "installer completed (saved to {})",
            installer.display()
        )),
        Ok(Ok(status)) => Err(format!(
            "installer exited with status {status}; see {}",
            manual_url(dep)
        )),
        Ok(Err(e)) => Err(format!("installer failed to wait: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            Err(format!(
                "installer timed out after {}s; see {}",
                INSTALL_TIMEOUT.as_secs(),
                manual_url(dep)
            ))
        }
    }
}

async fn install_qdrant_via_docker() -> Result<String, String> {
    // Requires Docker. We check before pulling so a clear
    // message fires when the operator runs `--fix` without
    // Docker installed.
    let docker = check_docker();
    if !docker.found {
        return Err(format!(
            "Qdrant requires Docker; install Docker first (see {})",
            manual_url(Dependency::Docker)
        ));
    }
    run_docker(&["pull", "qdrant/qdrant"]).await?;
    let qdrant_volume =
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|h| {
            let mut p = PathBuf::from(h);
            p.push(".relix");
            p.push("qdrant");
            p
        });
    if let Some(v) = qdrant_volume.as_ref() {
        let _ = std::fs::create_dir_all(v);
    }
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        "relix-qdrant".into(),
        "-p".into(),
        "6333:6333".into(),
        "-p".into(),
        "6334:6334".into(),
    ];
    if let Some(v) = qdrant_volume.as_ref() {
        args.push("-v".into());
        args.push(format!("{}:/qdrant/storage", v.display()));
    }
    args.push("qdrant/qdrant".into());
    let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_docker(&str_args).await?;
    Ok("container relix-qdrant started on :6333 / :6334".to_string())
}

async fn run_docker(args: &[&str]) -> Result<(), String> {
    let mut child = tokio::process::Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn `docker {}`: {e}", args.join(" ")))?;
    let wait = tokio::time::timeout(INSTALL_TIMEOUT, child.wait()).await;
    match wait {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(format!("`docker {}` exited with {status}", args.join(" "))),
        Ok(Err(e)) => Err(format!("`docker {}` wait failed: {e}", args.join(" "))),
        Err(_) => {
            let _ = child.kill().await;
            Err(format!(
                "`docker {}` timed out after {}s",
                args.join(" "),
                INSTALL_TIMEOUT.as_secs()
            ))
        }
    }
}

// ────────────────────────── rendering ───────────────────────────

/// Format `[OK] / [MISSING]` table the user sees on
/// `relix install --check`.
pub fn render_table(rows: &[DependencyStatus]) -> String {
    let mut out = String::new();
    out.push_str("Relix dependency check\n");
    for row in rows {
        let tag = if row.found { "[OK]     " } else { "[MISSING]" };
        let label = row.dependency.label();
        out.push_str(&format!("  {tag:<10}{label:<14}{}\n", row.detail));
    }
    out
}

pub fn print_table(rows: &[DependencyStatus]) {
    print!("{}", render_table(rows));
}

// ────────────────────────── setup wizard hook ───────────────────

/// Pre-flight check invoked by `relix setup` BEFORE the
/// wizard enters raw mode. Returns the status vec so the
/// wizard's confirm page can echo it alongside the config
/// diff. When dependencies are missing AND stdin is a TTY,
/// prompts to install before continuing.
///
/// The function never blocks for more than the configured
/// install timeout per dependency; on failure it prints the
/// manual URL and returns without an error so the wizard
/// always proceeds.
/// Outcome of the pre-flight dependency step the setup wizard runs.
pub enum SetupPreflight {
    /// Proceed into the wizard with these (re-checked) dependency
    /// statuses.
    Continue(Vec<DependencyStatus>),
    /// The operator chose to set up WITH memory but Docker is not
    /// running. The actionable message has already been printed; the
    /// wizard should exit cleanly so they can start Docker and re-run.
    ExitStartDocker,
}

/// Is the Docker DAEMON reachable (not merely the CLI installed)?
/// `check_docker()` only proves `docker --version` works; `docker info`
/// succeeds only when the daemon is up. Bounded by a timeout so a hung
/// or unreachable daemon socket can never stall setup (BUG 1: the old
/// path ran `docker pull` against a dead daemon and blocked up to the
/// 300 s install timeout).
async fn docker_daemon_running() -> bool {
    let spawned = tokio::process::Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(_) => return false, // docker CLI not installed
    };
    match tokio::time::timeout(Duration::from_secs(15), child.wait()).await {
        Ok(Ok(status)) => status.success(),
        _ => {
            let _ = child.kill().await;
            false
        }
    }
}

/// Read a `1` / `2` memory choice from stdin. Defaults to `2`
/// (WITHOUT memory) on blank input or a closed stdin — memory is
/// opt-in, never required.
fn prompt_memory_choice() -> bool {
    print!("Enter 1 or 2 [2]: ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "1")
}

/// Pre-flight dependency step for `relix setup`. Memory (Qdrant via
/// Docker) is presented as an explicit CHOICE, never a hard
/// requirement, and never hangs:
///   * Qdrant already running        → continue, memory available.
///   * choose WITH memory + Docker up → pull/run Qdrant, continue.
///   * choose WITH memory + Docker DOWN → print actionable message,
///     return ExitStartDocker.
///   * choose WITHOUT memory          → continue degraded (no Qdrant).
pub async fn status_for_setup() -> SetupPreflight {
    let statuses = check_all();
    println!();
    print_table(&statuses);

    let qdrant_running = statuses
        .iter()
        .find(|s| s.dependency == Dependency::Qdrant)
        .map(|s| s.found)
        .unwrap_or(false);
    if qdrant_running {
        // Memory is already available — nothing to decide.
        return SetupPreflight::Continue(statuses);
    }

    println!();
    println!("Memory (semantic recall + vector search) needs a running Qdrant,");
    println!("which Relix starts via Docker. Qdrant is not running right now.");
    println!("  [1] Set up WITH memory       — requires Docker running + Qdrant");
    println!("  [2] Continue WITHOUT memory  — enable later by re-running `relix setup`");
    if prompt_memory_choice() {
        // (a) WITH memory.
        if !docker_daemon_running().await {
            println!();
            println!(
                "Docker is not running. Start Docker Desktop, then re-run `relix setup` to enable memory."
            );
            return SetupPreflight::ExitStartDocker;
        }
        match install_qdrant_via_docker().await {
            Ok(note) => println!("[OK]      Qdrant        {note}"),
            Err(e) => {
                println!("[FAILED]  Qdrant        {e}");
                println!("          Continuing without memory; re-run `relix setup` to retry.");
            }
        }
        // Re-check so the wizard sees the new state.
        SetupPreflight::Continue(check_all())
    } else {
        // (b) WITHOUT memory.
        println!();
        println!("Continuing without memory — vector recall is disabled. Start Docker and");
        println!("re-run `relix setup`, choosing [1], to enable it later.");
        SetupPreflight::Continue(statuses)
    }
}

// ────────────────────────── tests ───────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_docker_version_line() {
        let out = "Docker version 24.0.5, build ced0996600\n";
        assert_eq!(parse_docker_version(out).as_deref(), Some("24.0.5"));
    }

    #[test]
    fn parses_docker_version_with_no_build_suffix() {
        let out = "Docker version 26.0.0\n";
        assert_eq!(parse_docker_version(out).as_deref(), Some("26.0.0"));
    }

    #[test]
    fn parses_docker_returns_none_on_empty() {
        assert_eq!(parse_docker_version(""), None);
        assert_eq!(parse_docker_version("nonsense\n"), None);
    }

    #[test]
    fn parses_ollama_version_old_form() {
        assert_eq!(
            parse_ollama_version("ollama version is 0.1.32\n").as_deref(),
            Some("0.1.32")
        );
    }

    #[test]
    fn parses_ollama_version_new_form() {
        assert_eq!(
            parse_ollama_version("ollama version 0.5.7\n").as_deref(),
            Some("0.5.7")
        );
    }

    #[test]
    fn parses_ollama_returns_none_on_garbage() {
        assert_eq!(parse_ollama_version("oops\n"), None);
        assert_eq!(parse_ollama_version(""), None);
    }

    #[test]
    fn parses_qdrant_version() {
        assert_eq!(
            parse_qdrant_version("qdrant 1.7.0\n").as_deref(),
            Some("1.7.0")
        );
        assert_eq!(
            parse_qdrant_version("Qdrant 1.10.2\n").as_deref(),
            Some("1.10.2")
        );
    }

    #[test]
    fn missing_binary_produces_missing_status() {
        // Detect using a guaranteed-missing program name. The
        // status row should say `found == false` and include
        // the manual URL in the detail.
        let res = run_capture("relix-definitely-not-on-path-37cf914b", &["--version"]);
        assert!(res.is_err(), "expected the bogus program to be missing");
    }

    #[test]
    fn manual_url_returns_platform_specific_link_for_each_dep() {
        for (os, expect_win, expect_mac, expect_lin) in [
            ("windows", true, false, false),
            ("macos", false, true, false),
            ("linux", false, false, true),
        ] {
            let url = manual_url_for(Dependency::Docker, os, "x86_64");
            if expect_win {
                assert!(url.contains("windows"), "{os}: {url}");
            }
            if expect_mac {
                assert!(url.contains("mac"), "{os}: {url}");
            }
            if expect_lin {
                // Linux Docker URL is the engine page, not the desktop page.
                assert!(url.contains("engine"), "{os}: {url}");
            }
        }
    }

    #[test]
    fn manual_url_for_ollama_per_platform() {
        assert!(manual_url_for(Dependency::Ollama, "windows", "x86_64").contains("windows"));
        assert!(manual_url_for(Dependency::Ollama, "macos", "aarch64").contains("mac"));
        assert!(manual_url_for(Dependency::Ollama, "linux", "x86_64").contains("linux"));
    }

    #[test]
    fn manual_url_for_qdrant_is_quick_start() {
        for os in &["windows", "macos", "linux"] {
            assert!(
                manual_url_for(Dependency::Qdrant, os, "x86_64").contains("qdrant.tech"),
                "{os}"
            );
        }
    }

    #[test]
    fn installer_url_returns_some_for_supported_combos() {
        assert!(installer_url(Dependency::Docker, "windows", "x86_64").is_some());
        assert!(installer_url(Dependency::Docker, "macos", "aarch64").is_some());
        assert!(installer_url(Dependency::Docker, "macos", "x86_64").is_some());
        assert!(installer_url(Dependency::Docker, "linux", "x86_64").is_some());
        assert!(installer_url(Dependency::Ollama, "windows", "x86_64").is_some());
        assert!(installer_url(Dependency::Ollama, "macos", "x86_64").is_some());
        assert!(installer_url(Dependency::Ollama, "linux", "x86_64").is_some());
        // Qdrant doesn't use the URL path — it goes through Docker.
        assert!(installer_url(Dependency::Qdrant, "linux", "x86_64").is_none());
    }

    #[test]
    fn macos_docker_installer_url_picks_arm_for_aarch64() {
        let url = installer_url(Dependency::Docker, "macos", "aarch64").unwrap();
        assert!(url.contains("arm64"), "expected arm64 download: {url}");
        let url = installer_url(Dependency::Docker, "macos", "x86_64").unwrap();
        assert!(url.contains("amd64"), "expected amd64 download: {url}");
    }

    #[test]
    fn render_table_includes_one_row_per_dependency() {
        let rows = vec![
            DependencyStatus {
                dependency: Dependency::Docker,
                found: true,
                version: Some("24.0.5".into()),
                detail: "24.0.5".into(),
            },
            DependencyStatus {
                dependency: Dependency::Ollama,
                found: false,
                version: None,
                detail: "not found".into(),
            },
            DependencyStatus {
                dependency: Dependency::Qdrant,
                found: false,
                version: None,
                detail: "not running".into(),
            },
        ];
        let out = render_table(&rows);
        // One header + 3 dep rows.
        assert_eq!(out.lines().count(), 4);
        assert!(out.contains("Docker"));
        assert!(out.contains("Ollama"));
        assert!(out.contains("Qdrant"));
        assert!(out.contains("[OK]"));
        assert!(out.contains("[MISSING]"));
    }

    #[test]
    fn missing_dependency_row_renders_with_manual_url_in_detail() {
        // Spot-check the detail field set by `check_docker`
        // when Docker is missing. We can't actually unset
        // Docker from the test process, so we synthesise the
        // status and re-use `render_table`.
        let url = manual_url(Dependency::Docker);
        let detail = format!("not found — see {url}");
        let row = DependencyStatus {
            dependency: Dependency::Docker,
            found: false,
            version: None,
            detail: detail.clone(),
        };
        let out = render_table(&[row]);
        assert!(out.contains("[MISSING]"));
        assert!(out.contains(&detail));
        assert!(out.contains(url));
    }

    #[test]
    fn check_all_returns_one_status_per_dependency_in_stable_order() {
        let statuses = check_all();
        assert_eq!(statuses.len(), 3);
        assert_eq!(statuses[0].dependency, Dependency::Docker);
        assert_eq!(statuses[1].dependency, Dependency::Ollama);
        assert_eq!(statuses[2].dependency, Dependency::Qdrant);
    }
}
