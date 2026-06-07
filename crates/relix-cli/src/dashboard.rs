//! `relix dashboard ...` — operator diagnostics + recovery for the product
//! dashboard's auth and the product (spine/prime) loop it drives.
//!
//! Two subcommands, deliberately small and read-only-by-default so an
//! operator can answer "can I boot, log in, and reach the product loop?"
//! without guessing:
//!
//! - `doctor` — read-only health/auth probe. Is the bridge up? Is a dashboard
//!   admin configured? Is the SPA bundle served? Are the spine/prime product
//!   routes WIRED (or is a 401 just missing auth, NOT a broken spine)? It
//!   never mutates state and never needs a logged-in session.
//! - `reset-admin` — thin passthrough to `relix-web-bridge reset-admin`, the
//!   LOCAL forgotten-password recovery. This layer never reads or prints a
//!   password; the bridge binary generates + prints a new one once. There is
//!   NO network / unauthenticated reset path.
//!
//! Honest scope: `doctor` probes from the CLI, which carries no browser
//! cookie, so it CANNOT confirm a logged-in session — it says so plainly
//! and tells the operator to open the dashboard in a browser to log in.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use clap::{Args, Subcommand};
use serde::Deserialize;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Read-only dashboard + product-loop health check. Probes the bridge's
    /// public surfaces (`/health`, `/v1/auth/status`, `/dashboard`) and, when
    /// a bridge token is available, the authenticated spine/prime routes.
    /// Prints an opinionated PASS/WARN/FAIL/INFO report and exits non-zero on
    /// any FAIL so scripts / CI can gate on it. Mutates nothing.
    Doctor(DoctorArgs),
    /// LOCAL forgotten-password recovery: reset (or initialize) the dashboard
    /// admin credential by delegating to `relix-web-bridge reset-admin`.
    /// Rewrites only the admin file (Argon2id hash — never plaintext);
    /// restart the bridge afterward for the new credential to take effect.
    ResetAdmin(ResetAdminArgs),
}

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Bridge HTTP base URL.
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    /// Print the raw probe results as JSON instead of the opinionated
    /// report. Useful for scripts that want to parse the verdicts.
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Bridge bearer token for the authenticated spine/prime probes.
    /// Precedence when omitted: `RELIX_BRIDGE_TOKEN` env, then
    /// `~/.relix/bridge-token`. WITHOUT a token the product routes answer
    /// 401 — which doctor reports as "auth enforced" (healthy), not as a
    /// broken spine.
    #[arg(long)]
    pub token: Option<String>,
}

#[derive(Args, Debug)]
pub struct ResetAdminArgs {
    /// Operate directly on this `dashboard-admin.json` (overrides --config).
    #[arg(long)]
    pub admin_file: Option<PathBuf>,
    /// Resolve the admin file from this bridge config TOML.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// New admin username. Defaults to the existing username, else "admin".
    #[arg(long)]
    pub username: Option<String>,
    /// New admin password (min 8 chars). If omitted, the bridge generates a
    /// strong random one and prints it ONCE.
    #[arg(long)]
    pub password: Option<String>,
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Doctor(args) => doctor(args).await,
        Cmd::ResetAdmin(args) => reset_admin(args),
    }
}

// ── doctor ──────────────────────────────────────────────────────

/// One check verdict in the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    Warn,
    Fail,
    /// Neutral note — neither healthy nor broken, just something the
    /// operator must know (e.g. "the CLI cannot see your browser session").
    Info,
}

impl Verdict {
    fn tag(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
            Self::Info => "INFO",
        }
    }
}

/// One row in the doctor report.
struct Check {
    label: String,
    verdict: Verdict,
    detail: String,
}

impl Check {
    fn new(label: &str, verdict: Verdict, detail: impl Into<String>) -> Self {
        Self {
            label: label.to_string(),
            verdict,
            detail: detail.into(),
        }
    }
}

/// Mirror of the bridge's `dashboard_auth::StatusBody`. Tolerant of unknown
/// fields so a newer bridge doesn't break an older CLI.
#[derive(Debug, Deserialize)]
struct AuthStatus {
    #[serde(default)]
    needs_setup: bool,
    #[serde(default)]
    authenticated: bool,
    #[serde(default)]
    username: Option<String>,
}

async fn doctor(args: DoctorArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/').to_string();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    let mut checks: Vec<Check> = Vec::new();

    // 1) Bridge reachable — the gate for every other probe.
    let health = http_probe(&client, &format!("{base}/health"), None).await;
    match &health {
        Ok((status, _)) if (200..300).contains(status) => {
            checks.push(Check::new(
                "bridge.reachable",
                Verdict::Pass,
                format!("{base}/health responded {status}"),
            ));
        }
        Ok((status, _)) => {
            checks.push(Check::new(
                "bridge.reachable",
                Verdict::Fail,
                format!("{base}/health responded {status} (expected 2xx)"),
            ));
            // No point probing further — the bridge is up but unhealthy.
            return finish(&base, checks, args.json);
        }
        Err(e) => {
            checks.push(Check::new(
                "bridge.reachable",
                Verdict::Fail,
                format!("{base}/health unreachable: {e}. Start it with `relix boot`."),
            ));
            return finish(&base, checks, args.json);
        }
    }

    // 2) Dashboard auth status (public endpoint — no token needed).
    let token = crate::bridge_token::resolve(args.token.as_deref());
    let bearer = token.as_ref().map(|(t, _)| t.as_str());
    match http_probe(&client, &format!("{base}/v1/auth/status"), None).await {
        Ok((status, body)) if (200..300).contains(&status) => {
            match serde_json::from_str::<AuthStatus>(&body) {
                Ok(s) => checks.extend(evaluate_auth_status(&s, &base)),
                Err(e) => checks.push(Check::new(
                    "dashboard.auth",
                    Verdict::Fail,
                    format!("/v1/auth/status returned undecodable body: {e}"),
                )),
            }
        }
        Ok((status, _)) => checks.push(Check::new(
            "dashboard.auth",
            Verdict::Fail,
            format!("/v1/auth/status responded {status} (expected 2xx)"),
        )),
        Err(e) => checks.push(Check::new(
            "dashboard.auth",
            Verdict::Fail,
            format!("/v1/auth/status unreachable: {e}"),
        )),
    }

    // 3) SPA bundle served (public). 200 + an HTML-looking body means the
    // committed dashboard-dist is mounted and reachable.
    match http_probe(&client, &format!("{base}/dashboard"), None).await {
        Ok((status, body)) => checks.push(evaluate_bundle(status, &body)),
        Err(e) => checks.push(Check::new(
            "dashboard.bundle",
            Verdict::Fail,
            format!("/dashboard unreachable: {e}"),
        )),
    }
    // Best-effort local dist presence (the source of the served bundle).
    checks.push(evaluate_local_dist());

    // 4) Product (spine/prime) routes WIRED. The whole point: distinguish a
    // 401 (auth enforced — healthy) from a 404/503 (spine genuinely missing).
    let had_token = bearer.is_some();
    for (label, path) in [
        ("spine.board", "/v1/spine/board"),
        ("prime.proposals", "/v1/spine/prime/proposals"),
    ] {
        let probe = http_probe(&client, &format!("{base}{path}"), bearer).await;
        let status = probe.as_ref().ok().map(|(s, _)| *s);
        checks.push(classify_route_probe(label, path, status, had_token));
    }

    // 5) Honest session note — the CLI has no browser cookie.
    checks.push(Check::new(
        "dashboard.session",
        Verdict::Info,
        format!(
            "the CLI carries no browser cookie, so it cannot confirm a logged-in \
             session — open {base}/dashboard in a browser to log in"
        ),
    ));

    if let Some((_, src)) = &token {
        checks.push(Check::new(
            "auth.token",
            Verdict::Info,
            format!("bridge token resolved from {}", src.label()),
        ));
    } else {
        checks.push(Check::new(
            "auth.token",
            Verdict::Info,
            format!(
                "no bridge token provided — authenticated probes were skipped. {}",
                crate::bridge_token::missing_token_hint()
            ),
        ));
    }

    finish(&base, checks, args.json)
}

/// Turn `/v1/auth/status` into operator-facing rows + a recovery hint.
fn evaluate_auth_status(s: &AuthStatus, base: &str) -> Vec<Check> {
    let mut out = Vec::new();
    if s.needs_setup {
        out.push(Check::new(
            "dashboard.admin",
            Verdict::Warn,
            format!(
                "no dashboard admin configured yet — create one on first load of \
                 {base}/dashboard (username + password), or pre-set it with \
                 `relix dashboard reset-admin`"
            ),
        ));
    } else {
        let who = s.username.as_deref().unwrap_or("(set)");
        out.push(Check::new(
            "dashboard.admin",
            Verdict::Pass,
            format!(
                "admin configured (username '{who}') — log in at {base}/dashboard. \
                 Forgot the password? run `relix dashboard reset-admin`"
            ),
        ));
    }
    // `authenticated` is always false for a cookie-less CLI request; report
    // it as a neutral note rather than a failure.
    out.push(Check::new(
        "dashboard.login_required",
        Verdict::Info,
        if s.authenticated {
            "the bridge reports an authenticated caller (unexpected from the CLI)".to_string()
        } else {
            "login is required for protected APIs (expected — a fresh request is unauthenticated)"
                .to_string()
        },
    ));
    out
}

/// Verdict for the served `/dashboard` bundle. 200 + HTML → present.
fn evaluate_bundle(status: u16, body: &str) -> Check {
    if !(200..300).contains(&status) {
        return Check::new(
            "dashboard.bundle",
            Verdict::Fail,
            format!("/dashboard responded {status} (expected 2xx) — bundle not served"),
        );
    }
    let looks_html = {
        let head = body.trim_start();
        head.starts_with("<!") || head.starts_with("<html") || head.contains("<div id=\"root\"")
    };
    if looks_html {
        Check::new(
            "dashboard.bundle",
            Verdict::Pass,
            "dashboard SPA bundle is served (run scripts/check-dashboard-dist to verify it is current)",
        )
    } else {
        Check::new(
            "dashboard.bundle",
            Verdict::Warn,
            "/dashboard responded 2xx but the body does not look like the SPA HTML",
        )
    }
}

/// Best-effort: is the committed dashboard-dist directory present on disk?
/// A served-200 (above) is the runtime truth; this catches a repo-local
/// build that was never committed. Missing-and-not-locatable → quiet INFO.
fn evaluate_local_dist() -> Check {
    let candidates = dist_candidates();
    for dir in &candidates {
        if dir.join("index.html").is_file() {
            return Check::new(
                "dashboard.dist",
                Verdict::Pass,
                format!("committed bundle present at {}", dir.display()),
            );
        }
    }
    Check::new(
        "dashboard.dist",
        Verdict::Info,
        "dashboard-dist not found relative to cwd (fine for a binary-only install; \
         the served bundle above is the runtime truth)",
    )
}

fn dist_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        for ancestor in cwd.ancestors().take(6) {
            out.push(
                ancestor
                    .join("crates")
                    .join("relix-web-bridge")
                    .join("dashboard-dist"),
            );
        }
    }
    out
}

/// The crux of avoiding the old "spine unavailable" confusion. Maps an HTTP
/// status on a product route to an HONEST verdict:
///   - 2xx              → PASS (route wired + answered)
///   - 401/403, no token→ WARN (auth ENFORCED, not a broken spine; provide --token)
///   - 401/403, w/ token→ FAIL (token rejected — auth misconfig)
///   - 404              → FAIL (route not mounted — wrong/old bridge build)
///   - 503              → WARN (route mounted, backend unavailable)
///   - other / None     → WARN (unexpected; details in row)
fn classify_route_probe(label: &str, path: &str, status: Option<u16>, had_token: bool) -> Check {
    match status {
        Some(s) if (200..300).contains(&s) => Check::new(
            label,
            Verdict::Pass,
            format!("{path} responded {s} — product route wired and reachable"),
        ),
        Some(401) | Some(403) => {
            if had_token {
                Check::new(
                    label,
                    Verdict::Fail,
                    format!(
                        "{path} responded {} despite a bridge token — the token was \
                         rejected (wrong token or CSRF origin). Auth, not the spine, \
                         is the problem.",
                        status.unwrap()
                    ),
                )
            } else {
                Check::new(
                    label,
                    Verdict::Warn,
                    format!(
                        "{path} responded {} — this means auth is ENFORCED, not that \
                         the spine is down. Pass --token (or log in via the dashboard) \
                         to confirm the route answers.",
                        status.unwrap()
                    ),
                )
            }
        }
        Some(404) => Check::new(
            label,
            Verdict::Fail,
            format!("{path} responded 404 — product route not mounted (old/wrong bridge build)"),
        ),
        Some(503) => Check::new(
            label,
            Verdict::Warn,
            format!("{path} responded 503 — route mounted but its backend is unavailable"),
        ),
        Some(s) => Check::new(
            label,
            Verdict::Warn,
            format!("{path} responded {s} (unexpected)"),
        ),
        None => Check::new(
            label,
            Verdict::Warn,
            format!("{path} could not be probed (request failed)"),
        ),
    }
}

fn finish(base: &str, checks: Vec<Check>, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        let rows: Vec<serde_json::Value> = checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "label": c.label,
                    "verdict": c.verdict.tag(),
                    "detail": c.detail,
                })
            })
            .collect();
        let doc = serde_json::json!({ "bridge": base, "checks": rows });
        println!("{}", serde_json::to_string_pretty(&doc)?);
    } else {
        render(base, &checks);
    }
    if checks.iter().any(|c| c.verdict == Verdict::Fail) {
        std::process::exit(1);
    }
    Ok(())
}

fn render(base: &str, checks: &[Check]) {
    println!("relix dashboard doctor — bridge={base}");
    println!();
    for c in checks {
        println!("{:<5} {:<26}  {}", c.verdict.tag(), c.label, c.detail);
    }
    let n = |v: Verdict| checks.iter().filter(|c| c.verdict == v).count();
    println!();
    println!(
        "{} pass, {} warn, {} fail, {} info",
        n(Verdict::Pass),
        n(Verdict::Warn),
        n(Verdict::Fail),
        n(Verdict::Info),
    );
}

/// GET `url` with an optional bearer. Returns `(status, body)` for any HTTP
/// reply (including 4xx/5xx — those are data, not errors here); only a
/// transport failure (bridge down) is an `Err`.
async fn http_probe(
    client: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
) -> Result<(u16, String), reqwest::Error> {
    let mut req = client.get(url);
    if let Some(t) = bearer {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Ok((status, body))
}

// ── reset-admin (passthrough) ───────────────────────────────────

/// Delegate to `relix-web-bridge reset-admin`. We do NOT reimplement the
/// Argon2id reset here — that single source of truth lives in the bridge
/// crate so the on-disk format can never drift. This wrapper just locates
/// the binary (built alongside this CLI) and forwards the operator's flags.
fn reset_admin(args: ResetAdminArgs) -> Result<(), Box<dyn std::error::Error>> {
    let bin = locate_bridge_binary();
    let mut cmd = Command::new(&bin);
    cmd.arg("reset-admin");
    if let Some(p) = &args.admin_file {
        cmd.arg("--admin-file").arg(p);
    } else if let Some(p) = &args.config {
        cmd.arg("--config").arg(p);
    }
    if let Some(u) = &args.username {
        cmd.arg("--username").arg(u);
    }
    if let Some(p) = &args.password {
        cmd.arg("--password").arg(p);
    }
    let status = cmd
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            format!(
                "could not run `{} reset-admin`: {e}. Build it with \
                 `cargo build -p relix-web-bridge`, or run the binary directly: \
                 `relix-web-bridge reset-admin`.",
                bin.display()
            )
        })?;
    if !status.success() {
        return Err(format!("reset-admin exited with status {status}").into());
    }
    Ok(())
}

/// Best-effort path to the `relix-web-bridge` binary: prefer the sibling next
/// to this CLI (they are built into the same `target/<profile>/` dir), then
/// the conventional `target/debug|release`, else the bare name so the OS
/// resolves it on `PATH`.
fn locate_bridge_binary() -> PathBuf {
    let exe_name = if cfg!(windows) {
        "relix-web-bridge.exe"
    } else {
        "relix-web-bridge"
    };
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(exe_name);
        if sibling.is_file() {
            return sibling;
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        for profile in ["debug", "release"] {
            let candidate = cwd.join("target").join(profile).join(exe_name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    // Not found locally — fall back to the bare (platform-suffixed) name so
    // the OS resolves it on `PATH`.
    PathBuf::from(exe_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_setup_warns_with_create_hint() {
        let s = AuthStatus {
            needs_setup: true,
            authenticated: false,
            username: None,
        };
        let rows = evaluate_auth_status(&s, "http://127.0.0.1:19791");
        let admin = rows.iter().find(|c| c.label == "dashboard.admin").unwrap();
        assert_eq!(admin.verdict, Verdict::Warn);
        assert!(admin.detail.contains("reset-admin"));
    }

    #[test]
    fn configured_admin_passes_with_login_hint() {
        let s = AuthStatus {
            needs_setup: false,
            authenticated: false,
            username: Some("ops".into()),
        };
        let rows = evaluate_auth_status(&s, "http://127.0.0.1:19791");
        let admin = rows.iter().find(|c| c.label == "dashboard.admin").unwrap();
        assert_eq!(admin.verdict, Verdict::Pass);
        assert!(admin.detail.contains("ops"));
        // The login-required note is informational, never a failure.
        let login = rows
            .iter()
            .find(|c| c.label == "dashboard.login_required")
            .unwrap();
        assert_eq!(login.verdict, Verdict::Info);
    }

    #[test]
    fn route_401_without_token_is_warn_not_fail() {
        // THE key behavior: a 401 with no token means auth is enforced, NOT
        // that the spine is unavailable. It must never read as FAIL.
        let c = classify_route_probe("spine.board", "/v1/spine/board", Some(401), false);
        assert_eq!(c.verdict, Verdict::Warn);
        assert!(c.detail.contains("auth is ENFORCED"));
    }

    #[test]
    fn route_401_with_token_is_fail() {
        let c = classify_route_probe("spine.board", "/v1/spine/board", Some(403), true);
        assert_eq!(c.verdict, Verdict::Fail);
    }

    #[test]
    fn route_200_passes() {
        let c = classify_route_probe("spine.board", "/v1/spine/board", Some(200), false);
        assert_eq!(c.verdict, Verdict::Pass);
    }

    #[test]
    fn route_404_is_fail_missing_route() {
        let c = classify_route_probe(
            "prime.proposals",
            "/v1/spine/prime/proposals",
            Some(404),
            true,
        );
        assert_eq!(c.verdict, Verdict::Fail);
        assert!(c.detail.contains("not mounted"));
    }

    #[test]
    fn route_503_is_warn_backend_down() {
        let c = classify_route_probe("spine.board", "/v1/spine/board", Some(503), true);
        assert_eq!(c.verdict, Verdict::Warn);
    }

    #[test]
    fn bundle_html_passes_non_html_warns() {
        let ok = evaluate_bundle(200, "<!doctype html><div id=\"root\"></div>");
        assert_eq!(ok.verdict, Verdict::Pass);
        let weird = evaluate_bundle(200, "not html at all");
        assert_eq!(weird.verdict, Verdict::Warn);
        let missing = evaluate_bundle(404, "");
        assert_eq!(missing.verdict, Verdict::Fail);
    }

    #[test]
    fn bridge_binary_name_is_platform_correct() {
        let p = locate_bridge_binary();
        let name = p.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("relix-web-bridge"));
        if cfg!(windows) {
            assert!(name.ends_with(".exe"));
        }
    }

    #[test]
    fn auth_status_tolerates_unknown_fields() {
        let s: AuthStatus = serde_json::from_str(
            r#"{"needs_setup":true,"authenticated":false,"username":null,"future":1}"#,
        )
        .unwrap();
        assert!(s.needs_setup);
        assert!(!s.authenticated);
    }
}
