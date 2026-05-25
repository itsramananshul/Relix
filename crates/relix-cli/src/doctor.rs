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
    #[arg(long, default_value = "http://127.0.0.1:19791")]
    pub bridge: String,
    /// Print the raw `/v1/health` JSON instead of the
    /// opinionated report. Useful for scripts that want to
    /// jq-parse the response.
    #[arg(long, default_value_t = false)]
    pub json: bool,
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
    let body = match http_get(&url).await {
        Ok(b) => b,
        Err(e) => {
            // Bridge unreachable — single FAIL row, exit 1.
            eprintln!("FAIL  bridge.reachable  could not reach {url}: {e}");
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
    let mut checks = evaluate(&resp);
    let perm_checks = evaluate_perms();
    checks.extend(perm_checks);
    render(&args.bridge, &resp, &checks);
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

async fn http_get(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let resp = client.get(url).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
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
