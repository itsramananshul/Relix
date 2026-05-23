//! `relix boot` / `relix stop` / `relix status` — cross-platform mesh
//! control wrappers around the platform-specific boot scripts.
//!
//! `boot` shells out to `scripts/relix-mesh-up.ps1` (Windows) or
//! `scripts/relix-mesh-up.sh` (POSIX), translates the `--with-*` flags
//! into the env vars those scripts already understand, then polls the
//! bridge's `/health` endpoint until it returns 200. Once healthy, it
//! opens `/dashboard` in the operator's default browser unless
//! `--no-browser` is set.
//!
//! `stop` kills `relix-controller` and `relix-web-bridge` by name —
//! `taskkill /F /IM` on Windows, `pkill -x` everywhere else.
//!
//! `status` polls the bridge's `/health` and `/v1/topology` endpoints
//! and prints a one-line-per-peer table. Exits 1 if the bridge is down.

use clap::Args;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Args, Debug)]
pub struct BootArgs {
    /// Also start the Telegram controller. Requires
    /// `RELIX_TELEGRAM_BOT_TOKEN` in the environment.
    #[arg(long)]
    pub with_telegram: bool,

    /// Also start the Discord controller. Requires
    /// `RELIX_DISCORD_BOT_TOKEN` and `RELIX_DISCORD_CHANNEL_ID` in
    /// the environment.
    #[arg(long)]
    pub with_discord: bool,

    /// Also start the Slack controller. Requires
    /// `RELIX_SLACK_BOT_TOKEN` and `RELIX_SLACK_CHANNEL_ID` in the
    /// environment.
    #[arg(long)]
    pub with_slack: bool,

    /// Also start the plugin_host. Loads plugins from `--plugin-dir`.
    #[arg(long)]
    pub with_plugins: bool,

    /// Directory the plugin_host scans for `plugin.toml` files.
    #[arg(long, default_value = "./plugins")]
    pub plugin_dir: PathBuf,

    /// Root directory for runtime data (logs, SQLite DBs, identity
    /// caches). Default: `dev-data`.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// HTTP port the bridge listens on. Default: 19791.
    #[arg(long, default_value_t = 19791)]
    pub bridge_port: u16,

    /// AI provider for the AI node. Defaults to `mock` (no
    /// credentials required).
    #[arg(long, default_value = "mock")]
    pub provider: String,

    /// Don't open the dashboard in a browser when the bridge becomes
    /// healthy.
    #[arg(long)]
    pub no_browser: bool,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Bridge port to poll. Default: 19791.
    #[arg(long, default_value_t = 19791)]
    pub bridge_port: u16,
}

/// Boot the local mesh by shelling out to the platform-specific boot
/// script and waiting for the bridge to become healthy.
pub async fn boot(args: BootArgs) -> Result<(), Box<dyn std::error::Error>> {
    let script = locate_script("relix-mesh-up")?;

    let mut cmd = build_boot_command(&script, &args)?;

    println!("starting mesh via {} ...", script.display());
    let mut child = cmd
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to spawn boot script: {e}"))?;

    let health_url = format!("http://127.0.0.1:{}/health", args.bridge_port);
    let dashboard_url = format!("http://127.0.0.1:{}/dashboard", args.bridge_port);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("boot script exited early with status {status}").into());
        }
        if let Ok(resp) = client.get(&health_url).send().await
            && resp.status().is_success()
        {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err("bridge did not become healthy within 60s".into());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    println!("bridge ready at http://127.0.0.1:{}", args.bridge_port);
    if !args.no_browser
        && let Err(e) = open_browser(&dashboard_url)
    {
        eprintln!("(could not open browser: {e}; visit {dashboard_url})");
    }
    println!("Ctrl-C the boot script's terminal (or run `relix stop`) to shut down.");
    Ok(())
}

/// Stop every running `relix-controller` and `relix-web-bridge`.
pub fn stop() -> Result<(), Box<dyn std::error::Error>> {
    let targets = ["relix-controller", "relix-web-bridge"];
    let mut killed = 0;
    let mut errors: Vec<String> = Vec::new();

    for name in &targets {
        match kill_by_name(name) {
            Ok(count) => killed += count,
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }

    if killed == 0 && errors.is_empty() {
        println!("no relix-controller / relix-web-bridge processes were running.");
    }
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("warning: {e}");
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct TopologyResp {
    peers: Vec<TopologyPeer>,
}
#[derive(Deserialize)]
struct TopologyPeer {
    #[serde(default)]
    alias: String,
    #[serde(default)]
    node_type: String,
    #[serde(default)]
    addr: String,
    #[serde(default)]
    freshness: String,
    #[serde(default)]
    capability_count: u32,
}

/// Poll the bridge's `/health` + `/v1/topology` and print a status
/// summary. Exits 1 if the bridge is unreachable.
pub async fn status(args: StatusArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = format!("http://127.0.0.1:{}", args.bridge_port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;

    if client.get(format!("{base}/health")).send().await.is_err() {
        println!("Relix is not running. Start with: relix boot");
        std::process::exit(1);
    }

    println!("bridge: up  ({})", base);

    let topo = match client.get(format!("{base}/v1/topology")).send().await {
        Ok(r) if r.status().is_success() => r.json::<TopologyResp>().await.ok(),
        _ => None,
    };

    match topo {
        Some(t) if !t.peers.is_empty() => {
            println!();
            println!(
                "{:<14}  {:<14}  {:<32}  {:<10}  CAPS",
                "ALIAS", "NODE_TYPE", "ADDR", "FRESHNESS"
            );
            for p in &t.peers {
                let alias = truncate(&p.alias, 14);
                let node_type = truncate(&p.node_type, 14);
                let addr = truncate(&p.addr, 32);
                let freshness = truncate(&p.freshness, 10);
                let count = p.capability_count;
                println!("{alias:<14}  {node_type:<14}  {addr:<32}  {freshness:<10}  {count}");
            }
        }
        _ => {
            println!("(no peer topology reported)");
        }
    }

    Ok(())
}

// ---- helpers ----

fn locate_script(stem: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let (ps_name, sh_name) = (format!("{stem}.ps1"), format!("{stem}.sh"));
    let want_ps = cfg!(windows);

    let cwd = std::env::current_dir()?;
    for ancestor in cwd.ancestors().take(6) {
        let candidate = if want_ps {
            ancestor.join("scripts").join(&ps_name)
        } else {
            ancestor.join("scripts").join(&sh_name)
        };
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = if want_ps {
            dir.join("scripts").join(&ps_name)
        } else {
            dir.join("scripts").join(&sh_name)
        };
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(format!(
        "could not find {} in any ./scripts directory (cwd: {})",
        if want_ps { &ps_name } else { &sh_name },
        cwd.display()
    )
    .into())
}

fn build_boot_command(
    script: &Path,
    args: &BootArgs,
) -> Result<Command, Box<dyn std::error::Error>> {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("powershell");
        c.arg("-NoProfile").arg("-ExecutionPolicy").arg("Bypass");
        c.arg("-File").arg(script);
        c
    } else {
        let mut c = Command::new("bash");
        c.arg(script);
        c
    };

    cmd.arg("--provider").arg(&args.provider);
    cmd.arg("--bridge-port").arg(args.bridge_port.to_string());
    if let Some(data_dir) = &args.data_dir {
        cmd.env("RELIX_DATA_DIR", data_dir);
    }

    if args.with_telegram {
        cmd.env("RELIX_TELEGRAM", "1");
    }
    if args.with_discord {
        cmd.env("RELIX_DISCORD", "1");
    }
    if args.with_slack {
        cmd.env("RELIX_SLACK", "1");
    }
    if args.with_plugins {
        cmd.env("RELIX_PLUGINS", "1");
        cmd.env("RELIX_PLUGIN_DIR", &args.plugin_dir);
    }

    Ok(cmd)
}

fn open_browser(url: &str) -> Result<(), String> {
    let result = if cfg!(target_os = "windows") {
        Command::new("cmd").args(["/C", "start", "", url]).status()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(url).status()
    } else {
        Command::new("xdg-open").arg(url).status()
    };
    match result {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("browser command exited {s}")),
        Err(e) => Err(e.to_string()),
    }
}

fn kill_by_name(name: &str) -> Result<usize, String> {
    if cfg!(windows) {
        let exe_name = format!("{name}.exe");
        let out = Command::new("taskkill")
            .args(["/F", "/IM", &exe_name])
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let count = stdout.matches("SUCCESS").count();
            for line in stdout.lines() {
                if line.starts_with("SUCCESS") {
                    println!("  {line}");
                }
            }
            Ok(count)
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("not found") || stderr.contains("ERROR: The process") {
                Ok(0)
            } else {
                Err(stderr.trim().to_string())
            }
        }
    } else {
        // Use pkill -x for an exact match. Returns 0 on a kill, 1 if no
        // processes matched, 2+ on error.
        let status = Command::new("pkill")
            .args(["-x", name])
            .status()
            .map_err(|e| e.to_string())?;
        match status.code() {
            Some(0) => {
                println!("  stopped {name}");
                Ok(1)
            }
            Some(1) => Ok(0),
            Some(c) => Err(format!("pkill exited {c}")),
            None => Err("pkill terminated by signal".into()),
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}
