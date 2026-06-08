//! `relix release ...` - first-release operator gates.
//!
//! This is intentionally a thin CLI surface over the existing local release
//! scripts. The release gate already lives in `scripts/ci-local.ps1`; this
//! command makes it discoverable from the product CLI and can run it without
//! enabling hosted GitHub workflows or spending model credits.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Print the first-release gate, or run it with --run-local-gate.
    Readiness(ReadinessArgs),
}

#[derive(Args, Debug)]
pub struct ReadinessArgs {
    /// Run the heavyweight local release gate (`scripts/ci-local.ps1`).
    ///
    /// Without this flag, the command only prints what will be checked and how
    /// to run it. The gate can take a long time because it runs clippy, the
    /// workspace tests, cargo-deny, dashboard dist parity, and the live first
    /// release smoke.
    #[arg(long, default_value_t = false)]
    pub run_local_gate: bool,

    /// Fail before running the local gate if the git working tree is dirty.
    ///
    /// Useful immediately before tagging. Left off by default so developers
    /// can still run the gate while iterating.
    #[arg(long, default_value_t = false)]
    pub require_clean: bool,

    /// Do not check `origin` for an existing release tag.
    ///
    /// The remote check is read-only (`git ls-remote`) and normally quick, but
    /// this flag keeps the command useful while offline.
    #[arg(long, default_value_t = false)]
    pub skip_remote_tag_check: bool,

    /// Override the repository root. Defaults to the current directory or one
    /// of its parents containing `Cargo.toml` and `scripts/ci-local.ps1`.
    #[arg(long)]
    pub repo: Option<PathBuf>,
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Readiness(args) => readiness(args),
    }
}

fn readiness(args: ReadinessArgs) -> Result<(), Box<dyn std::error::Error>> {
    let repo = match args.repo {
        Some(path) => canonicalize_repo(&path)?,
        None => find_repo_root(&std::env::current_dir()?)
            .ok_or("could not find repo root; pass --repo <Relix checkout>")?,
    };
    let gate = repo.join("scripts").join("ci-local.ps1");
    if !gate.exists() {
        return Err(format!("missing local release gate: {}", gate.display()).into());
    }

    let version = env!("CARGO_PKG_VERSION");
    let tag = expected_tag_for_version(version);
    let git = inspect_git_readiness(&repo, &tag, !args.skip_remote_tag_check);
    print_readiness_plan(&repo, &gate, version, &tag, &git);
    if args.require_clean && matches!(git.clean, Some(false)) {
        eprintln!(
            "release readiness: FAIL (working tree is dirty; commit or stash before tagging)"
        );
        std::process::exit(1);
    }
    if !args.run_local_gate {
        println!();
        println!("Next command:");
        println!("  relix release readiness --require-clean --run-local-gate");
        println!();
        println!(
            "This does not enable GitHub Actions, does not create a tag, and does not call Claude or any model provider."
        );
        return Ok(());
    }

    println!();
    println!("Running local first-release gate. This may take a while.");
    let mut cmd = powershell_command(&gate);
    cmd.current_dir(&repo)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = cmd.status()?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    println!();
    println!("release readiness: PASS");
    println!(
        "Safe next manual step: create/review tag {tag} only after you also review the final diff and changelog."
    );
    Ok(())
}

fn print_readiness_plan(repo: &Path, gate: &Path, version: &str, tag: &str, git: &GitReadiness) {
    let channel = release_channel_for_version(version);
    println!("relix release readiness");
    println!("version: {version} ({})", channel.label());
    println!("tag: {tag} ({})", channel.tag_note());
    println!("repo: {}", repo.display());
    println!("gate: {}", gate.display());
    println!("git head: {}", git.head.as_deref().unwrap_or("unknown"));
    println!(
        "working tree: {}",
        match git.clean {
            Some(true) => "clean",
            Some(false) => "dirty",
            None => "unknown",
        }
    );
    println!("local tag: {}", git.local_tag.render());
    println!("origin tag: {}", git.remote_tag.render());
    println!();
    println!("First-release gate:");
    println!("  1. boot-policy coverage");
    println!("  2. cargo fmt");
    println!("  3. cargo clippy");
    println!("  4. dashboard dist parity");
    println!("  5. cargo test --workspace (serial)");
    println!("  6. cargo deny check");
    println!("  7. live first-release smoke with echo Rig");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleaseChannel {
    Stable,
    Beta,
}

impl ReleaseChannel {
    fn label(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta/pre-release",
        }
    }

    fn tag_note(self) -> &'static str {
        match self {
            Self::Stable => "stable; GitHub Latest",
            Self::Beta => "beta/pre-release; not GitHub Latest",
        }
    }
}

fn release_channel_for_version(version: &str) -> ReleaseChannel {
    if version.contains('-') {
        ReleaseChannel::Beta
    } else {
        ReleaseChannel::Stable
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitReadiness {
    head: Option<String>,
    clean: Option<bool>,
    local_tag: TagPresence,
    remote_tag: TagPresence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TagPresence {
    Exists,
    Missing,
    Skipped,
    Unknown(String),
}

impl TagPresence {
    fn render(&self) -> String {
        match self {
            Self::Exists => "exists (do not reuse)".to_string(),
            Self::Missing => "missing (safe to create)".to_string(),
            Self::Skipped => "skipped".to_string(),
            Self::Unknown(e) => format!("unknown ({e})"),
        }
    }
}

fn expected_tag_for_version(version: &str) -> String {
    format!("v{}", version.trim_start_matches('v'))
}

fn inspect_git_readiness(repo: &Path, tag: &str, check_remote: bool) -> GitReadiness {
    GitReadiness {
        head: git_stdout(repo, &["rev-parse", "--short", "HEAD"]).ok(),
        clean: git_stdout(repo, &["status", "--porcelain"])
            .ok()
            .map(|s| s.trim().is_empty()),
        local_tag: local_tag_presence(repo, tag),
        remote_tag: if check_remote {
            remote_tag_presence(repo, tag)
        } else {
            TagPresence::Skipped
        },
    }
}

fn local_tag_presence(repo: &Path, tag: &str) -> TagPresence {
    let tag_ref = format!("refs/tags/{tag}");
    match Command::new("git")
        .args(["rev-parse", "-q", "--verify", &tag_ref])
        .current_dir(repo)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
    {
        Ok(status) if status.success() => TagPresence::Exists,
        Ok(status) if status.code() == Some(1) => TagPresence::Missing,
        Ok(status) => TagPresence::Unknown(format!("git rev-parse exited {status}")),
        Err(e) => TagPresence::Unknown(e.to_string()),
    }
}

fn remote_tag_presence(repo: &Path, tag: &str) -> TagPresence {
    let tag_ref = format!("refs/tags/{tag}");
    match Command::new("git")
        .args(["ls-remote", "--exit-code", "--tags", "origin", &tag_ref])
        .current_dir(repo)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
    {
        Ok(status) if status.success() => TagPresence::Exists,
        Ok(status) if status.code() == Some(2) => TagPresence::Missing,
        Ok(status) => TagPresence::Unknown(format!("git ls-remote exited {status}")),
        Err(e) => TagPresence::Unknown(e.to_string()),
    }
}

fn git_stdout(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if err.is_empty() {
            format!("git {} exited {}", args.join(" "), output.status)
        } else {
            err
        })
    }
}

fn canonicalize_repo(path: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let repo = normalize_for_shell(path.canonicalize()?);
    if repo.join("Cargo.toml").exists() && repo.join("scripts").join("ci-local.ps1").exists() {
        Ok(repo)
    } else {
        Err(format!(
            "{} is not a Relix repo root (missing Cargo.toml or scripts/ci-local.ps1)",
            repo.display()
        )
        .into())
    }
}

fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut cur = normalize_for_shell(start.canonicalize().ok()?);
    loop {
        if cur.join("Cargo.toml").exists() && cur.join("scripts").join("ci-local.ps1").exists() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

fn normalize_for_shell(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

fn powershell_command(script: &Path) -> Command {
    let exe = if cfg!(windows) { "powershell" } else { "pwsh" };
    let mut cmd = Command::new(exe);
    cmd.arg("-NoProfile");
    if cfg!(windows) {
        cmd.arg("-ExecutionPolicy").arg("Bypass");
    }
    cmd.arg("-File").arg(script);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_repo_root_walks_up_to_ci_local_gate() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("scripts")).unwrap();
        std::fs::write(tmp.path().join("scripts").join("ci-local.ps1"), "").unwrap();
        let nested = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();

        let found = find_repo_root(&nested).unwrap();
        assert_eq!(
            found,
            normalize_for_shell(tmp.path().canonicalize().unwrap())
        );
    }

    #[test]
    fn canonicalize_repo_rejects_non_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        let err = canonicalize_repo(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("not a Relix repo root"));
    }

    #[test]
    fn release_channel_tracks_semver_suffix() {
        assert_eq!(release_channel_for_version("0.4.3"), ReleaseChannel::Stable);
        assert_eq!(
            release_channel_for_version("0.4.3-beta.2"),
            ReleaseChannel::Beta
        );
        assert_eq!(
            ReleaseChannel::Beta.tag_note(),
            "beta/pre-release; not GitHub Latest"
        );
    }

    #[test]
    fn expected_tag_for_version_adds_one_v_prefix() {
        assert_eq!(expected_tag_for_version("0.4.3"), "v0.4.3");
        assert_eq!(expected_tag_for_version("v0.4.3-beta.1"), "v0.4.3-beta.1");
    }

    #[test]
    fn tag_presence_rendering_is_operator_facing() {
        assert_eq!(TagPresence::Exists.render(), "exists (do not reuse)");
        assert_eq!(TagPresence::Missing.render(), "missing (safe to create)");
        assert_eq!(TagPresence::Skipped.render(), "skipped");
        assert_eq!(
            TagPresence::Unknown("offline".to_string()).render(),
            "unknown (offline)"
        );
    }

    #[cfg(windows)]
    #[test]
    fn normalize_for_shell_strips_windows_verbatim_prefix() {
        assert_eq!(
            normalize_for_shell(PathBuf::from(r"\\?\D:\repo\Relix")),
            PathBuf::from(r"D:\repo\Relix")
        );
        assert_eq!(
            normalize_for_shell(PathBuf::from(r"\\?\UNC\server\share\Relix")),
            PathBuf::from(r"\\server\share\Relix")
        );
    }
}
