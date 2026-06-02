//! `relix-cli doctor` — W2-008a one-command operator health
//! check. Hits the bridge's `GET /v1/health` and prints an
//! opinionated PASS/WARN/FAIL report. Exits non-zero on any
//! FAIL so CI / shell scripts can gate on it.
//!
//! Honest scope: this probes the BRIDGE process, not the
//! controller binary itself. If the bridge is down, doctor
//! prints "bridge unreachable" and exits non-zero. For
//! controller-side health an operator runs `relix-cli ping
//! --peer <addr> --identity <bundle>`; doctor is the
//! bridge-side counterpart.

use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use serde::Deserialize;

use crate::os_secure::{PermVerdict, inspect_permissions};

/// `doctor` arguments. Distinct from `Cmd` because doctor is
/// flat (no subcommands).
#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Bridge HTTP base URL.
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    /// Print the raw `/v1/health` JSON instead of the
    /// opinionated report. Useful for scripts that want to
    /// jq-parse the response.
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Bridge bearer token for the auth-gated `/v1/health` probe.
    /// Precedence when omitted: `RELIX_BRIDGE_TOKEN` env, then
    /// `~/.relix/bridge-token`. Without a token an auth-enabled
    /// bridge answers 401 and doctor reports a healthy mesh as
    /// broken.
    #[arg(long)]
    pub token: Option<String>,
}

/// Mirror of the bridge's `topology::HealthResponse`. We
/// accept unknown fields (the bridge may grow new ones)
/// and default missing fields to safe values.
#[derive(Debug, Deserialize)]
struct HealthResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    started_at: i64,
    #[serde(default)]
    now: i64,
    #[serde(default)]
    uptime_secs: i64,
    #[serde(default)]
    coordinator_configured: bool,
    #[serde(default)]
    peer_count: usize,
    #[serde(default)]
    peers_fresh: usize,
    #[serde(default)]
    peers_stale: usize,
    #[serde(default)]
    peers_expired: usize,
    #[serde(default)]
    reconnect: Option<ReconnectCounters>,
}

#[derive(Debug, Deserialize)]
struct ReconnectCounters {
    #[serde(default)]
    attempts: u64,
    #[serde(default)]
    successes: u64,
}

/// One check verdict in the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    Warn,
    Fail,
}

impl Verdict {
    fn tag(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }
}

/// One row in the doctor report.
struct Check {
    label: String,
    verdict: Verdict,
    detail: String,
}

pub async fn run(args: DoctorArgs) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/health", args.bridge.trim_end_matches('/'));
    let mut local_checks = evaluate_local_readiness();
    // Resolve the bridge bearer once. `/v1/health` is auth-gated, so
    // without it an auth-enabled bridge answers 401 and every check
    // below would read as a broken mesh.
    let token = crate::bridge_token::resolve(args.token.as_deref());
    let bearer = token.as_ref().map(|(t, _)| t.as_str());
    let body = match http_get(&url, bearer).await {
        Ok(b) => b,
        Err(e) => {
            // Probe failed — single FAIL row, exit 1. The error text
            // already names the token locations on a 401/403.
            local_checks.push(Check {
                label: "bridge.reachable".into(),
                verdict: Verdict::Fail,
                detail: format!("{url}: {e}"),
            });
            render_offline(&args.bridge, &local_checks);
            std::process::exit(1);
        }
    };
    if args.json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let resp: HealthResponse = serde_json::from_str(&body)
        .map_err(|e| format!("decode /v1/health body: {e} (body={body})"))?;
    let mut checks = local_checks;
    checks.extend(evaluate(&resp));
    let perm_checks = evaluate_perms();
    checks.extend(perm_checks);
    render(&args.bridge, &resp, &checks);
    if let Some((_, src)) = &token {
        println!("token: {}", src.label());
    }
    let any_fail = checks.iter().any(|c| c.verdict == Verdict::Fail);
    if any_fail {
        std::process::exit(1);
    }
    Ok(())
}

/// Inspect the on-disk secrets files an operator might own.
/// Returns one row per known secret file. PASS = restrictive
/// (POSIX 0600 or Windows non-inheriting current-user-only
/// ACL). WARN = looser than recommended. Missing files emit a
/// quiet PASS — a fresh install has nothing to leak.
fn evaluate_perms() -> Vec<Check> {
    let mut out = Vec::new();
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = std::env::var_os(home_var).map(PathBuf::from);

    let mut watch: Vec<(&'static str, PathBuf)> = Vec::new();
    if let Some(h) = home.as_ref() {
        watch.push((
            "secrets.bridge_token",
            h.join(".relix").join("bridge-token"),
        ));
        watch.push(("secrets.config_toml", h.join(".relix").join("config.toml")));
    }
    // The bridge-secrets file is under the data_dir, which can
    // vary by deployment. Probe the conventional `dev-data/`
    // location with the cwd as a fall-through. Missing in both
    // → PASS with a "not present" note.
    let dev_data_secrets = PathBuf::from("dev-data").join("bridge-secrets.toml");
    let chosen = if dev_data_secrets.exists() {
        dev_data_secrets
    } else {
        PathBuf::from("bridge-secrets.toml")
    };
    watch.push(("secrets.bridge_secrets", chosen));

    for (label, path) in watch {
        let row = match inspect_permissions(&path) {
            PermVerdict::Strict => Check {
                label: label.into(),
                verdict: Verdict::Pass,
                detail: format!("{} — permissions restrictive", path.display()),
            },
            PermVerdict::Loose => Check {
                label: label.into(),
                verdict: Verdict::Warn,
                detail: format!(
                    "{} is readable by other users; \
                     re-harden with `chmod 600` on POSIX or \
                     `icacls <path> /inheritance:r /grant:r %USERNAME%:F` on Windows",
                    path.display()
                ),
            },
            PermVerdict::Unknown => Check {
                label: label.into(),
                verdict: Verdict::Pass,
                detail: format!("{} — not present (no secrets to leak)", path.display()),
            },
        };
        out.push(row);
    }
    out
}

fn evaluate_local_readiness() -> Vec<Check> {
    let mut out = Vec::new();
    let config_path = crate::config::RelixConfig::default_path();
    match crate::config::RelixConfig::load_from(&config_path) {
        Ok(Some(cfg)) => {
            let errs = cfg.validate();
            if errs.is_empty() {
                out.push(Check {
                    label: "setup.config".into(),
                    verdict: Verdict::Pass,
                    detail: format!("{} is valid", config_path.display()),
                });
            } else {
                out.push(Check {
                    label: "setup.config".into(),
                    verdict: Verdict::Fail,
                    detail: format!(
                        "{} has {} problem(s): {}",
                        config_path.display(),
                        errs.len(),
                        errs.join("; ")
                    ),
                });
            }
            out.push(if cfg.mesh.data_dir.trim().is_empty() {
                Check {
                    label: "setup.data_dir".into(),
                    verdict: Verdict::Fail,
                    detail: "mesh.data_dir is empty; set it in ~/.relix/config.toml".into(),
                }
            } else {
                Check {
                    label: "setup.data_dir".into(),
                    verdict: Verdict::Pass,
                    detail: format!("mesh.data_dir={}", cfg.mesh.data_dir),
                }
            });
        }
        Ok(None) => out.push(Check {
            label: "setup.config".into(),
            verdict: Verdict::Warn,
            detail: format!(
                "{} missing; run `relix setup` before `relix boot`",
                config_path.display()
            ),
        }),
        Err(e) => out.push(Check {
            label: "setup.config".into(),
            verdict: Verdict::Fail,
            detail: format!("{} cannot be read: {e}", config_path.display()),
        }),
    }

    out.push(if script_exists("relix-mesh-up") {
        Check {
            label: "setup.boot_script".into(),
            verdict: Verdict::Pass,
            detail: "relix-mesh-up script found".into(),
        }
    } else {
        Check {
            label: "setup.boot_script".into(),
            verdict: Verdict::Fail,
            detail: "relix-mesh-up script missing; re-run the installer or run from the repo root"
                .into(),
        }
    });

    out.push(if crate::bridge_token::resolve(None).is_some() {
        Check {
            label: "setup.bridge_token".into(),
            verdict: Verdict::Pass,
            detail: "bridge token resolved".into(),
        }
    } else {
        Check {
            label: "setup.bridge_token".into(),
            verdict: Verdict::Warn,
            detail: crate::bridge_token::missing_token_hint(),
        }
    });

    out
}

/// Apply the doctor's opinions to a HealthResponse. Pure
/// function so the rule set is testable without touching
/// the network.
fn evaluate(h: &HealthResponse) -> Vec<Check> {
    let mut out = Vec::new();

    // bridge.status
    out.push(if h.status == "ok" {
        Check {
            label: "bridge.status".into(),
            verdict: Verdict::Pass,
            detail: format!("status={} uptime={}s", h.status, h.uptime_secs),
        }
    } else {
        Check {
            label: "bridge.status".into(),
            verdict: Verdict::Fail,
            detail: format!("status='{}' (expected 'ok')", h.status),
        }
    });

    // coordinator_configured — WARN (chat still works without it).
    out.push(if h.coordinator_configured {
        Check {
            label: "coordinator.configured".into(),
            verdict: Verdict::Pass,
            detail: "task.* endpoints active".into(),
        }
    } else {
        Check {
            label: "coordinator.configured".into(),
            verdict: Verdict::Warn,
            detail: "no [coordinator] alias — task.* endpoints return 503; chat still works".into(),
        }
    });

    // peer_count — FAIL when zero (no peers means nothing the
    // bridge can dispatch to).
    out.push(if h.peer_count == 0 {
        Check {
            label: "mesh.peers".into(),
            verdict: Verdict::Fail,
            detail: "no peers in manifest cache — start a controller and configure [peers]".into(),
        }
    } else {
        Check {
            label: "mesh.peers".into(),
            verdict: Verdict::Pass,
            detail: format!(
                "{} total ({} fresh, {} stale, {} expired)",
                h.peer_count, h.peers_fresh, h.peers_stale, h.peers_expired,
            ),
        }
    });

    // expired peers — FAIL even if there are non-expired ones
    // alongside. An expired peer = a configured peer that the
    // bridge has lost contact with; operator should know.
    if h.peers_expired > 0 {
        out.push(Check {
            label: "mesh.expired".into(),
            verdict: Verdict::Fail,
            detail: format!(
                "{} peer(s) expired — their controllers stopped sending heartbeats",
                h.peers_expired
            ),
        });
    }

    // reconnect flapping — WARN.
    if let Some(r) = &h.reconnect {
        let failures = r.attempts.saturating_sub(r.successes);
        if failures > 0 {
            out.push(Check {
                label: "mesh.reconnect".into(),
                verdict: Verdict::Warn,
                detail: format!(
                    "{failures} reconnect attempt(s) failed (attempts={}, successes={}) — possible flapping",
                    r.attempts, r.successes
                ),
            });
        }
    }

    out
}

fn render(bridge: &str, h: &HealthResponse, checks: &[Check]) {
    println!("relix-cli doctor — bridge={bridge}");
    println!(
        "started_at={} now={} uptime={}s",
        h.started_at, h.now, h.uptime_secs
    );
    println!();
    for c in checks {
        println!("{:<5} {:<24}  {}", c.verdict.tag(), c.label, c.detail);
    }
    let n_fail = checks.iter().filter(|c| c.verdict == Verdict::Fail).count();
    let n_warn = checks.iter().filter(|c| c.verdict == Verdict::Warn).count();
    let n_pass = checks.iter().filter(|c| c.verdict == Verdict::Pass).count();
    println!();
    println!("{n_pass} pass, {n_warn} warn, {n_fail} fail");
}

fn render_offline(bridge: &str, checks: &[Check]) {
    println!("relix-cli doctor — bridge={bridge}");
    println!("bridge health: unreachable");
    println!();
    for c in checks {
        println!("{:<5} {:<24}  {}", c.verdict.tag(), c.label, c.detail);
    }
    let n_fail = checks.iter().filter(|c| c.verdict == Verdict::Fail).count();
    let n_warn = checks.iter().filter(|c| c.verdict == Verdict::Warn).count();
    let n_pass = checks.iter().filter(|c| c.verdict == Verdict::Pass).count();
    println!();
    println!("{n_pass} pass, {n_warn} warn, {n_fail} fail");
}

fn script_exists(stem: &str) -> bool {
    script_candidates(stem).iter().any(|p| p.is_file())
}

fn script_candidates(stem: &str) -> Vec<PathBuf> {
    let (ps_name, sh_name) = (format!("{stem}.ps1"), format!("{stem}.sh"));
    let want_ps = cfg!(windows);
    let leaf = if want_ps { &ps_name } else { &sh_name };
    let mut out = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        for ancestor in cwd.ancestors().take(6) {
            out.push(ancestor.join("scripts").join(leaf));
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        out.push(dir.join("scripts").join(leaf));
    }
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    if let Some(home) = std::env::var_os(home_var).map(PathBuf::from) {
        out.push(home.join(".local").join("scripts").join(leaf));
    }
    out
}

async fn http_get(url: &str, bearer: Option<&str>) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let mut req = client.get(url);
    if let Some(t) = bearer {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        // Don't dump a raw 401 — name the token locations so the
        // operator knows the mesh may be healthy and the probe just
        // lacked credentials.
        return Err(format!(
            "bridge returned HTTP {status}. {}",
            crate::bridge_token::missing_token_hint()
        )
        .into());
    }
    if !status.is_success() {
        return Err(format!("bridge returned HTTP {status}: {body}").into());
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h_ok() -> HealthResponse {
        HealthResponse {
            status: "ok".into(),
            started_at: 1,
            now: 100,
            uptime_secs: 99,
            coordinator_configured: true,
            peer_count: 2,
            peers_fresh: 2,
            peers_stale: 0,
            peers_expired: 0,
            reconnect: None,
        }
    }

    #[test]
    fn healthy_bridge_passes_every_check() {
        let checks = evaluate(&h_ok());
        assert!(checks.iter().all(|c| c.verdict == Verdict::Pass));
    }

    #[test]
    fn missing_coordinator_is_warn_not_fail() {
        let mut h = h_ok();
        h.coordinator_configured = false;
        let checks = evaluate(&h);
        let row = checks
            .iter()
            .find(|c| c.label == "coordinator.configured")
            .unwrap();
        assert_eq!(row.verdict, Verdict::Warn);
    }

    #[test]
    fn zero_peers_is_fail() {
        let mut h = h_ok();
        h.peer_count = 0;
        h.peers_fresh = 0;
        let checks = evaluate(&h);
        let row = checks.iter().find(|c| c.label == "mesh.peers").unwrap();
        assert_eq!(row.verdict, Verdict::Fail);
    }

    #[test]
    fn expired_peers_emit_dedicated_fail_row() {
        let mut h = h_ok();
        h.peer_count = 3;
        h.peers_fresh = 2;
        h.peers_expired = 1;
        let checks = evaluate(&h);
        assert!(
            checks
                .iter()
                .any(|c| c.label == "mesh.expired" && c.verdict == Verdict::Fail)
        );
    }

    #[test]
    fn reconnect_flapping_emits_warn() {
        let mut h = h_ok();
        h.reconnect = Some(ReconnectCounters {
            attempts: 10,
            successes: 7,
        });
        let checks = evaluate(&h);
        assert!(
            checks
                .iter()
                .any(|c| c.label == "mesh.reconnect" && c.verdict == Verdict::Warn)
        );
    }

    #[test]
    fn reconnect_perfect_no_warn() {
        let mut h = h_ok();
        h.reconnect = Some(ReconnectCounters {
            attempts: 10,
            successes: 10,
        });
        let checks = evaluate(&h);
        assert!(!checks.iter().any(|c| c.label == "mesh.reconnect"));
    }

    #[test]
    fn unknown_status_is_fail() {
        let mut h = h_ok();
        h.status = "degraded".into();
        let checks = evaluate(&h);
        let row = checks.iter().find(|c| c.label == "bridge.status").unwrap();
        assert_eq!(row.verdict, Verdict::Fail);
    }

    /// Minimal mock bridge: records every request's headers and
    /// answers `/v1/health` with a healthy body. Lets a test assert
    /// the probe carries `Authorization: Bearer <token>`.
    async fn spawn_mock_bridge() -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<std::collections::HashMap<String, String>>>>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let seen: std::sync::Arc<std::sync::Mutex<Vec<std::collections::HashMap<String, String>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let seen = seen2.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    loop {
                        let Ok(n) = sock.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&buf);
                    let mut headers = std::collections::HashMap::new();
                    for l in text.split("\r\n").skip(1) {
                        if l.is_empty() {
                            break;
                        }
                        if let Some((k, v)) = l.split_once(':') {
                            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                        }
                    }
                    seen.lock().unwrap().push(headers);
                    let body = r#"{"status":"ok","peer_count":1,"peers_fresh":1,"coordinator_configured":true}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        (addr, seen)
    }

    #[tokio::test]
    async fn health_probe_attaches_bearer_token() {
        // AC: the auth-gated `/v1/health` probe must carry
        // `Authorization: Bearer <token>` so a healthy auth-enabled
        // mesh is not reported as broken.
        let (addr, seen) = spawn_mock_bridge().await;
        let url = format!("{addr}/v1/health");
        let body = http_get(&url, Some("smoke-tok")).await.unwrap();
        assert!(body.contains("\"status\":\"ok\""));
        let reqs = seen.lock().unwrap();
        assert!(
            reqs.iter().any(|h| h
                .get("authorization")
                .map(|v| v.eq_ignore_ascii_case("Bearer smoke-tok"))
                .unwrap_or(false)),
            "doctor /v1/health probe must send Authorization: Bearer"
        );
    }

    #[test]
    fn unknown_json_fields_tolerated() {
        // Forward-compat: a future bridge field shouldn't
        // break doctor.
        let json = r#"{
            "status": "ok",
            "uptime_secs": 5,
            "peer_count": 1,
            "peers_fresh": 1,
            "coordinator_configured": true,
            "future_field": 99
        }"#;
        let h: HealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(h.status, "ok");
        assert_eq!(h.peer_count, 1);
    }
}
