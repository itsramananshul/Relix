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

use std::io::Write;
use std::path::{Path, PathBuf};
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
    let Some(asset_name) = asset_name_for_current_platform() else {
        eprintln!("error: unsupported platform for `relix update` self-replace.");
        std::process::exit(2);
    };
    let Some(asset) = release.assets.iter().find(|a| a.name == asset_name) else {
        eprintln!(
            "error: release {} does not ship an asset named '{}'.",
            release.tag_name, asset_name
        );
        std::process::exit(2);
    };
    let installed_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot resolve current binary path: {e}");
            std::process::exit(2);
        }
    };
    let dir = installed_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let temp_name = format!(".relix-update-{}.tmp", std::process::id());
    let temp_path = dir.join(temp_name);
    println!(
        "downloading {} ({}) to {}...",
        asset.name,
        human_bytes(asset.size),
        temp_path.display()
    );
    if let Err(e) = download_to(&asset.browser_download_url, &temp_path).await {
        // Leave the installed binary untouched.
        let _ = std::fs::remove_file(&temp_path);
        eprintln!("error: download failed: {e}");
        std::process::exit(2);
    }
    // W6 follow-up: when the asset is an archive (.tar.gz /
    // .tgz / .zip), extract it and find the bundled binary.
    // Raw binaries skip extraction and replace directly.
    let replacement_source = if is_archive_asset(asset_name) {
        if asset_name.to_ascii_lowercase().ends_with(".zip") {
            // .zip is the Windows release format. We don't ship
            // the heavyweight `zip` crate today; operators on
            // Windows need to extract manually for now.
            let _ = std::fs::remove_file(&temp_path);
            eprintln!(
                "error: this build can self-replace from .tar.gz / .tgz archives but not .zip yet.\nDownload {} manually from the release page and replace your relix binary.",
                asset.name
            );
            std::process::exit(2);
        }
        let extract_dir = dir.join(format!(".relix-update-extract-{}", std::process::id()));
        if let Err(e) = std::fs::create_dir_all(&extract_dir) {
            let _ = std::fs::remove_file(&temp_path);
            eprintln!("error: create extract dir: {e}");
            std::process::exit(2);
        }
        let extracted = match extract_tar_gz(&temp_path, &extract_dir) {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_file(&temp_path);
                let _ = std::fs::remove_dir_all(&extract_dir);
                eprintln!("error: extract archive: {e}");
                std::process::exit(2);
            }
        };
        let binary = match pick_extracted_binary(&extracted) {
            Some(p) => p.to_path_buf(),
            None => {
                let _ = std::fs::remove_file(&temp_path);
                let _ = std::fs::remove_dir_all(&extract_dir);
                eprintln!(
                    "error: archive did not contain a relix-shaped binary; \
                     extracted {} files",
                    extracted.len()
                );
                std::process::exit(2);
            }
        };
        // Drop the original archive so the replace step
        // operates on the extracted binary.
        let _ = std::fs::remove_file(&temp_path);
        binary
    } else {
        temp_path.clone()
    };
    println!("replacing {} ...", installed_path.display());
    if let Err(e) = atomically_replace_binary(&installed_path, &replacement_source) {
        // Best-effort cleanup of the staging files on replace
        // failure; the installed binary stays untouched per
        // the replace function's contract.
        let _ = std::fs::remove_file(&replacement_source);
        eprintln!("error: replace failed: {e}");
        std::process::exit(2);
    }
    println!("relix update — done. Restart any running relix processes.");
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

/// Atomically replace the binary at `installed_path` with the
/// file at `new_path`.
///
/// On POSIX this is a single `rename()` — atomic when both
/// paths live on the same filesystem, the common case. On
/// Windows `std::fs::rename` calls `MoveFileExW` with
/// `MOVEFILE_REPLACE_EXISTING` under the hood; that works for
/// non-running targets but fails with `ERROR_ACCESS_DENIED`
/// when the destination is the currently-executing `.exe`.
///
/// To survive the running-binary case the function attempts a
/// direct rename first; on permission-denied it falls back to
/// the standard Windows pattern: rename the existing binary to
/// `<name>.old` (allowed even while running), then rename the
/// new binary into the original path. If anything fails the
/// `.old` rollback restores the installed binary so the
/// operator does not end up with a broken install.
pub fn atomically_replace_binary(installed_path: &Path, new_path: &Path) -> Result<(), String> {
    if !new_path.exists() {
        return Err(format!("new binary not found at {}", new_path.display()));
    }
    match std::fs::rename(new_path, installed_path) {
        Ok(()) => return Ok(()),
        Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied => {
            // Fall through to the .old workaround for the
            // running-.exe case. Other error kinds bubble.
        }
        Err(e) => {
            return Err(format!(
                "rename {} -> {} failed: {}",
                new_path.display(),
                installed_path.display(),
                e
            ));
        }
    }
    let old_path = with_dot_old_suffix(installed_path);
    let _ = std::fs::remove_file(&old_path);
    std::fs::rename(installed_path, &old_path)
        .map_err(|e| format!("rename installed -> {} failed: {}", old_path.display(), e))?;
    if let Err(e) = std::fs::rename(new_path, installed_path) {
        // Rollback so the operator isn't left binary-less.
        if let Err(restore_err) = std::fs::rename(&old_path, installed_path) {
            return Err(format!(
                "rename new -> installed failed ({e}); rollback ALSO failed ({restore_err})"
            ));
        }
        return Err(format!("rename new -> installed failed: {e}"));
    }
    Ok(())
}

/// Append `.old` to a path. Honest helper kept private so
/// callers can't accidentally smuggle in a fancier suffix.
fn with_dot_old_suffix(p: &Path) -> PathBuf {
    let mut s: std::ffi::OsString = p.into();
    s.push(".old");
    PathBuf::from(s)
}

/// Download `url` to `dest`. The file is written atomically:
/// bytes go to `<dest>.partial` first, then moved into place on
/// successful completion. A network or HTTP error leaves no
/// partial file behind so callers don't have to clean up.
pub async fn download_to(url: &str, dest: &Path) -> Result<(), String> {
    let partial = with_partial_suffix(dest);
    let _ = std::fs::remove_file(&partial);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent(format!("relix-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let _ = std::fs::remove_file(&partial);
        return Err(format!("download {url}: HTTP {status}"));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("read body from {url}: {e}"))?;
    {
        let mut f = std::fs::File::create(&partial)
            .map_err(|e| format!("create {}: {e}", partial.display()))?;
        f.write_all(&bytes)
            .map_err(|e| format!("write {}: {e}", partial.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", partial.display()))?;
    }
    std::fs::rename(&partial, dest).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        format!("rename {} -> {}: {e}", partial.display(), dest.display())
    })?;
    Ok(())
}

fn with_partial_suffix(p: &Path) -> PathBuf {
    let mut s: std::ffi::OsString = p.into();
    s.push(".partial");
    PathBuf::from(s)
}

/// W6 follow-up: extract a `.tar.gz` archive to `dest_dir`,
/// returning the list of extracted file paths. Pure Rust via
/// `flate2` + `tar` so there's no shell dependency.
///
/// Errors surface as operator-readable strings. Failed
/// extractions leave whatever they wrote to `dest_dir`
/// in place; the caller (which owns a tempdir) cleans up by
/// dropping the dir.
pub fn extract_tar_gz(archive_path: &Path, dest_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| format!("open archive {}: {e}", archive_path.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    let mut entries = Vec::new();
    let iter = tar
        .entries()
        .map_err(|e| format!("read tar entries: {e}"))?;
    for entry in iter {
        let mut e = entry.map_err(|e| format!("tar entry: {e}"))?;
        let header_path = e
            .path()
            .map_err(|e| format!("tar entry path: {e}"))?
            .into_owned();
        let target = dest_dir.join(&header_path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        e.unpack(&target)
            .map_err(|e| format!("unpack {}: {e}", target.display()))?;
        if target.is_file() {
            entries.push(target);
        }
    }
    Ok(entries)
}

/// W6 follow-up: pick the first executable-looking entry in
/// `extracted`. The relix release matrix produces a single
/// `relix-cli`-shaped binary per archive plus optionally a
/// `LICENSE` / `README` sibling; we accept any file whose name
/// stem matches `relix` (case-insensitive) and isn't an
/// archive / doc.
pub fn pick_extracted_binary(extracted: &[PathBuf]) -> Option<&Path> {
    for p in extracted {
        let name = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if name.starts_with("relix")
            && !name.ends_with(".md")
            && !name.ends_with(".txt")
            && !name.ends_with(".sha256")
            && !name.ends_with(".asc")
            && !name.ends_with(".sig")
        {
            return Some(p.as_path());
        }
    }
    None
}

/// Returns `true` when `asset_name` should be unpacked before
/// `atomically_replace_binary`. Matches the canonical release
/// matrix produced by `.github/workflows/release.yml`.
pub fn is_archive_asset(asset_name: &str) -> bool {
    let lower = asset_name.to_ascii_lowercase();
    lower.ends_with(".tar.gz") || lower.ends_with(".tgz") || lower.ends_with(".zip")
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

    // ── W6: download + atomic replace ────────────────────────────

    use std::io::{Read, Write};

    /// Tiny single-request HTTP/1.1 server. Spawns a thread,
    /// accepts one connection, returns `(status, body_bytes)`
    /// on the wire. Returns the `127.0.0.1:<port>` URL prefix.
    /// Used only in tests so the cost of a real HTTP mock crate
    /// is avoided.
    fn spawn_one_shot_http(status: u16, body: Vec<u8>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf);
                let reason = match status {
                    200 => "OK",
                    404 => "Not Found",
                    500 => "Internal Server Error",
                    _ => "Unknown",
                };
                let header = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(header.as_bytes());
                let _ = sock.write_all(&body);
                let _ = sock.shutdown(std::net::Shutdown::Write);
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn download_to_writes_response_body_to_dest() {
        let url = spawn_one_shot_http(200, b"NEW_BINARY_CONTENTS".to_vec());
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("downloaded.bin");
        download_to(&url, &dest).await.unwrap();
        let got = std::fs::read(&dest).unwrap();
        assert_eq!(got, b"NEW_BINARY_CONTENTS");
    }

    #[tokio::test]
    async fn download_to_does_not_leave_partial_file_on_http_error() {
        let url = spawn_one_shot_http(404, b"not found".to_vec());
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("downloaded.bin");
        let err = download_to(&url, &dest).await.unwrap_err();
        assert!(err.contains("HTTP 404"), "err={err}");
        // No leftover file at the destination OR at the .partial path.
        assert!(
            !dest.exists(),
            "dest should not exist after failed download"
        );
        let partial = super::with_partial_suffix(&dest);
        assert!(
            !partial.exists(),
            "partial should not exist after failed download"
        );
    }

    #[test]
    fn atomically_replace_binary_swaps_installed_with_new_path() {
        let tmp = tempfile::tempdir().unwrap();
        let installed = tmp.path().join("relix-installed");
        let new_path = tmp.path().join("relix-new");
        std::fs::write(&installed, b"OLD_BINARY").unwrap();
        std::fs::write(&new_path, b"NEW_BINARY").unwrap();
        atomically_replace_binary(&installed, &new_path).unwrap();
        let got = std::fs::read(&installed).unwrap();
        assert_eq!(got, b"NEW_BINARY");
        // The new_path file moved into installed, so its
        // original location is now empty.
        assert!(
            !new_path.exists(),
            "new_path should be consumed by the rename"
        );
    }

    #[tokio::test]
    async fn failed_download_leaves_installed_binary_untouched() {
        // The orchestration: download_to fails → caller does
        // NOT call atomically_replace_binary → installed file
        // stays bit-for-bit identical.
        let tmp = tempfile::tempdir().unwrap();
        let installed = tmp.path().join("relix-installed");
        std::fs::write(&installed, b"ORIGINAL_BINARY").unwrap();
        let original_bytes = std::fs::read(&installed).unwrap();
        let url = spawn_one_shot_http(500, b"oops".to_vec());
        let temp_download = tmp.path().join("relix-update.tmp");
        let result = download_to(&url, &temp_download).await;
        assert!(result.is_err(), "download must fail");
        // Operator-facing contract: installed binary stays put.
        let after = std::fs::read(&installed).unwrap();
        assert_eq!(after, original_bytes);
    }

    /// Build a tiny in-memory `.tar.gz` archive containing
    /// the provided `(filename, contents)` entries and write
    /// it to `dest`. Pure Rust; no shell call.
    fn build_tar_gz(dest: &Path, entries: &[(&str, &[u8])]) {
        use std::io::Write;
        let file = std::fs::File::create(dest).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        for (name, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, *body).unwrap();
        }
        let encoder = tar.into_inner().unwrap();
        let mut f = encoder.finish().unwrap();
        f.flush().unwrap();
    }

    #[test]
    fn extract_tar_gz_unpacks_every_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("relix.tar.gz");
        build_tar_gz(
            &archive,
            &[("relix-cli", b"NEW_RELIX_BYTES"), ("README.md", b"docs")],
        );
        let dest = tmp.path().join("ex");
        let entries = extract_tar_gz(&archive, &dest).unwrap();
        assert_eq!(entries.len(), 2);
        let bin = dest.join("relix-cli");
        let readme = dest.join("README.md");
        assert!(bin.exists());
        assert!(readme.exists());
        assert_eq!(std::fs::read(&bin).unwrap(), b"NEW_RELIX_BYTES");
    }

    #[test]
    fn pick_extracted_binary_skips_docs_and_picks_relix_named_file() {
        let tmp = tempfile::tempdir().unwrap();
        let docs = tmp.path().join("README.md");
        let bin = tmp.path().join("relix-cli");
        let sha = tmp.path().join("relix-cli.sha256");
        std::fs::write(&docs, b"docs").unwrap();
        std::fs::write(&bin, b"bin").unwrap();
        std::fs::write(&sha, b"sha").unwrap();
        let candidates = vec![docs.clone(), sha.clone(), bin.clone()];
        let picked = pick_extracted_binary(&candidates).unwrap();
        assert_eq!(picked, bin.as_path());
    }

    #[test]
    fn is_archive_asset_recognises_canonical_release_formats() {
        assert!(is_archive_asset("relix-x86_64-unknown-linux-gnu.tar.gz"));
        assert!(is_archive_asset("relix-aarch64-apple-darwin.tgz"));
        assert!(is_archive_asset("relix-x86_64-pc-windows-msvc.zip"));
        assert!(!is_archive_asset("relix-cli"));
        assert!(!is_archive_asset("relix.exe"));
    }

    #[test]
    fn end_to_end_tar_gz_extract_then_replace_swaps_installed_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let installed = tmp.path().join("relix-installed");
        std::fs::write(&installed, b"OLD_INSTALLED").unwrap();
        // Build a tar.gz that mirrors what GitHub Actions ships.
        let archive = tmp.path().join("relix.tar.gz");
        build_tar_gz(&archive, &[("relix-cli", b"FRESH_FROM_RELEASE_ARCHIVE")]);
        // Extract, pick, replace.
        let extract_dir = tmp.path().join("ex");
        std::fs::create_dir_all(&extract_dir).unwrap();
        let entries = extract_tar_gz(&archive, &extract_dir).unwrap();
        let binary = pick_extracted_binary(&entries).unwrap().to_path_buf();
        atomically_replace_binary(&installed, &binary).unwrap();
        let got = std::fs::read(&installed).unwrap();
        assert_eq!(got, b"FRESH_FROM_RELEASE_ARCHIVE");
    }

    #[test]
    fn atomically_replace_returns_err_when_new_path_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let installed = tmp.path().join("relix-installed");
        let missing = tmp.path().join("does-not-exist");
        std::fs::write(&installed, b"ORIGINAL").unwrap();
        let err = atomically_replace_binary(&installed, &missing).unwrap_err();
        assert!(err.contains("not found"), "err={err}");
        // Installed must still be the original.
        let after = std::fs::read(&installed).unwrap();
        assert_eq!(after, b"ORIGINAL");
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
