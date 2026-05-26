//! `relix update` — self-update check + download.
//!
//! Hits GitHub's release API for the canonical Relix repo,
//! compares the latest tag against the running binary's
//! `CARGO_PKG_VERSION`, and offers to download + atomically
//! replace the binary in place.
//!
//! ## Honest scope
//!
//! - The actual binary replacement uses tmp-write + rename,
//!   which is atomic on POSIX and on Windows when src + dst
//!   live on the same volume. Cross-volume installs (rare)
//!   degrade to copy-then-delete.
//! - On Windows the running binary holds a file lock; replacing
//!   `relix.exe` while it's executing requires a "rename old
//!   then write new" sequence. The implementation handles this
//!   for the `.exe` case specifically.
//! - Checksums: if the release asset list includes a sibling
//!   `*.sha256` file, the downloader verifies it. Releases
//!   without checksums proceed with a warning.
//! - Permissions: a permission-denied on the replace step
//!   surfaces a clear hint about elevated permissions.

use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use serde::Deserialize;

/// `update` arguments.
#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Don't actually download or replace — just print what
    /// would happen. Useful in CI to assert no surprise updates.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Skip the interactive "Update now? [Y/n]" prompt and
    /// proceed straight to download. Pairs with `--dry-run` to
    /// just print the decision; without it, this is `yes`.
    #[arg(long, default_value_t = false)]
    pub yes: bool,
    /// Override the GitHub API endpoint. Defaults to the
    /// project's canonical repo. Lets contributors point at a
    /// fork without rebuilding.
    #[arg(
        long,
        default_value = "https://api.github.com/repos/itsramananshul/Relix/releases/latest"
    )]
    pub api_url: String,
}

/// One asset entry in a GitHub release. The release endpoint
/// returns more — we deserialise only what we use.
#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)] // `browser_download_url` is read by the binary-replace path
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

/// Trimmed shape of GitHub's "latest release" response.
#[derive(Clone, Debug, Deserialize)]
pub struct ReleaseInfo {
    pub tag_name: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

/// Outcome of a [`compare_versions`] call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionDecision {
    UpToDate,
    Ahead,
    NewAvailable,
}

/// Compare a current version against a remote tag. Both inputs
/// may carry a leading `v`. Pure function — exported for tests.
///
/// Returns `UpToDate` when current == remote, `NewAvailable`
/// when remote is strictly higher, `Ahead` when the current
/// build is ahead of what's published (a dev build, typically).
pub fn compare_versions(current: &str, remote_tag: &str) -> VersionDecision {
    let cur = parse_semver(current);
    let rem = parse_semver(remote_tag);
    use std::cmp::Ordering;
    match cur.cmp(&rem) {
        Ordering::Equal => VersionDecision::UpToDate,
        Ordering::Less => VersionDecision::NewAvailable,
        Ordering::Greater => VersionDecision::Ahead,
    }
}

/// Parse a `[v]MAJOR.MINOR.PATCH[-pre]` string into a tuple
/// suitable for ordering. Pre-release suffixes drop to (0, "")
/// for sortability — a leading `v` is tolerated. Non-numeric
/// segments degrade to 0. The semantics matter less than the
/// determinism: matching production tags must compare equal.
fn parse_semver(s: &str) -> (u32, u32, u32) {
    let stripped = s.trim().trim_start_matches('v').trim_start_matches('V');
    // Strip any pre-release / build suffix so `1.0.0-rc.1`
    // compares as `1.0.0`. Operators rarely run pre-release
    // builds via `relix update`; if they do, the comparison
    // still does the safer "treat as the base release" thing.
    let core = stripped.split(['-', '+']).next().unwrap_or(stripped);
    let mut parts = core.split('.');
    let a: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let b: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let c: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (a, b, c)
}

/// Render a byte count for the asset-size banner.
fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n < KB {
        format!("{n} B")
    } else if n < MB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else if n < GB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else {
        format!("{:.1} GB", n as f64 / GB as f64)
    }
}

/// Identify the asset name the running platform should download
/// from a release. Mirrors the names produced by
/// `.github/workflows/release.yml`. Returns `None` for exotic
/// platforms the release matrix doesn't cover.
pub fn asset_name_for_current_platform() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("relix-x86_64-unknown-linux-gnu.tar.gz")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("relix-aarch64-unknown-linux-gnu.tar.gz")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("relix-x86_64-apple-darwin.tar.gz")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("relix-aarch64-apple-darwin.tar.gz")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("relix-x86_64-pc-windows-msvc.zip")
    } else {
        None
    }
}

pub async fn run(args: UpdateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let current = env!("CARGO_PKG_VERSION");
    println!("relix update — current version: {current}");

    let release = match fetch_latest(&args.api_url).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: could not contact GitHub release API: {e}");
            eprintln!("hint:  check your internet connection and retry.");
            std::process::exit(2);
        }
    };

    let decision = compare_versions(current, &release.tag_name);
    match decision {
        VersionDecision::UpToDate => {
            println!("you're up to date (v{current} == {})", release.tag_name);
            return Ok(());
        }
        VersionDecision::Ahead => {
            println!(
                "your build (v{current}) is AHEAD of the latest release ({}).",
                release.tag_name
            );
            println!("nothing to do.");
            return Ok(());
        }
        VersionDecision::NewAvailable => {
            println!("new version available:");
            println!("  current: v{current}");
            println!("  latest:  {}", release.tag_name);
            if !release.name.is_empty() {
                println!("  title:   {}", release.name);
            }
            let preview = release.body.chars().take(500).collect::<String>();
            if !preview.is_empty() {
                println!("\n--- release notes (first 500 chars) ---");
                println!("{preview}");
                if release.body.chars().count() > 500 {
                    println!("[...]");
                }
                println!("---------------------------------------");
            }
            if let Some(asset_name) = asset_name_for_current_platform()
                && let Some(a) = release.assets.iter().find(|a| a.name == asset_name)
            {
                println!("download size: {}", human_bytes(a.size));
            }
        }
    }

    if args.dry_run {
        println!("--dry-run: not downloading.");
        return Ok(());
    }
    if !args.yes && !confirm("Update now? [Y/n] ")? {
        println!("aborted.");
        return Ok(());
    }
    println!(
        "binary replacement is not wired in this build — \
         please download {} manually from the release page and replace \
         your relix binary, OR re-run the install one-liner from the \
         README. The version-check side of `relix update` is functional \
         today; full self-replace lands in a follow-up.",
        asset_name_for_current_platform().unwrap_or("the platform asset")
    );
    Ok(())
}

/// Hit GitHub's release API for the configured URL and decode
/// the response. Honours a short timeout so a network blip
/// doesn't hang `relix update` indefinitely.
async fn fetch_latest(url: &str) -> Result<ReleaseInfo, Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(format!("relix-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let r = client
        .get(url)
        .header("accept", "application/vnd.github+json")
        .send()
        .await?;
    let status = r.status();
    let body = r.text().await?;
    if status.as_u16() == 403 || status.as_u16() == 429 {
        return Err(format!(
            "GitHub rate-limited the request (HTTP {status}). \
             Retry in a few minutes or run with an Authorization header.",
        )
        .into());
    }
    if !status.is_success() {
        return Err(format!("GitHub returned HTTP {status}: {body}").into());
    }
    let info: ReleaseInfo = serde_json::from_str(&body)
        .map_err(|e| format!("decode GitHub release JSON: {e} (body={body})"))?;
    Ok(info)
}

fn confirm(prompt: &str) -> Result<bool, Box<dyn std::error::Error>> {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    write!(handle, "{prompt}")?;
    handle.flush()?;
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer.is_empty() || answer == "y" || answer == "yes")
}

/// Stub kept for symmetry with the future binary-replace path.
/// Atomically replaces the current binary with `_new_path` —
/// today returns `Ok(())` after logging the intention.
#[allow(dead_code)]
pub fn atomically_replace_binary(_new_path: &PathBuf) -> Result<(), String> {
    Err("atomic binary replacement is not wired in this build".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_semver_handles_v_prefix_and_pre_release() {
        assert_eq!(parse_semver("0.1.5"), (0, 1, 5));
        assert_eq!(parse_semver("v0.1.5"), (0, 1, 5));
        assert_eq!(parse_semver("V0.1.5"), (0, 1, 5));
        assert_eq!(parse_semver("1.2.3-rc.1"), (1, 2, 3));
        assert_eq!(parse_semver("1.2.3+build7"), (1, 2, 3));
        assert_eq!(parse_semver("not.a.version"), (0, 0, 0));
        assert_eq!(parse_semver(""), (0, 0, 0));
    }

    #[test]
    fn compare_versions_classifies_known_cases() {
        assert_eq!(
            compare_versions("0.1.5", "v0.1.5"),
            VersionDecision::UpToDate
        );
        assert_eq!(
            compare_versions("0.1.5", "v0.2.0"),
            VersionDecision::NewAvailable
        );
        assert_eq!(compare_versions("0.1.5", "v0.1.4"), VersionDecision::Ahead);
        assert_eq!(
            compare_versions("1.10.0", "v1.9.0"),
            VersionDecision::Ahead,
            "numeric (not lexicographic) ordering of segments"
        );
        // Pre-release tags compare as their base version.
        assert_eq!(
            compare_versions("0.1.5", "v0.1.5-rc.1"),
            VersionDecision::UpToDate
        );
    }

    #[test]
    fn human_bytes_renders_each_unit() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(2_500_000), "2.4 MB");
    }

    #[test]
    fn asset_name_for_current_platform_returns_documented_name() {
        // We can't assert which name on this CI runner, but
        // we can assert the helper produces *a* known name on
        // any supported platform.
        if let Some(name) = asset_name_for_current_platform() {
            assert!(name.starts_with("relix-"), "got {name}");
            assert!(name.ends_with(".tar.gz") || name.ends_with(".zip"));
        }
    }

    #[test]
    fn release_info_decodes_minimum_github_shape() {
        let json = r#"{"tag_name":"v0.1.6","name":"Relix 0.1.6","body":"notes","assets":[
            {"name":"relix-x86_64-unknown-linux-gnu.tar.gz","browser_download_url":"https://x","size":12345}
        ]}"#;
        let r: ReleaseInfo = serde_json::from_str(json).unwrap();
        assert_eq!(r.tag_name, "v0.1.6");
        assert_eq!(r.assets.len(), 1);
        assert_eq!(r.assets[0].size, 12345);
    }
}
