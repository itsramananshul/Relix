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
//! `stop` shells out to `scripts/relix-mesh-down.ps1` (Windows) or
//! `scripts/relix-mesh-down.sh` (POSIX), which read the pidfile mesh-up
//! wrote and signal only those PIDs. It never matches by process name,
//! so an unrelated mesh on the same machine survives.
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
    #[arg(long, default_value_t = crate::defaults::DEFAULT_BRIDGE_PORT)]
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
pub struct StopArgs {
    /// Runtime data root the mesh was booted with. Must match the
    /// `relix boot --data-dir` value so the down script finds the right
    /// pidfile. Defaults to the boot script's own default (`dev-data`).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Bridge port to poll. Default: 19791.
    #[arg(long, default_value_t = crate::defaults::DEFAULT_BRIDGE_PORT)]
    pub bridge_port: u16,
}

/// Boot the local mesh by shelling out to the platform-specific boot
/// script and waiting for the bridge to become healthy.
pub async fn boot(args: BootArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Pull persistent config from `~/.relix/config.toml`. The setup
    // wizard writes this; without it, only the explicit CLI flags
    // matter. Config-supplied values override the BootArgs defaults
    // (e.g. provider) but explicit `--with-telegram` style flags
    // still stack on top of config-driven channels.
    let cfg_opt = crate::config::RelixConfig::load_default().ok().flatten();
    let mut effective = args;
    if let Some(cfg) = &cfg_opt {
        if effective.provider == "mock" && !cfg.provider.name.is_empty() {
            effective.provider = cfg.provider.name.clone();
        }
        if cfg.channels.telegram {
            effective.with_telegram = true;
        }
        if cfg.channels.discord {
            effective.with_discord = true;
        }
        if cfg.channels.slack {
            effective.with_slack = true;
        }
    } else if std::env::var_os("RELIX_SUPPRESS_NO_CONFIG_HINT").is_none() {
        eprintln!(
            "note: no `~/.relix/config.toml` found — using defaults. \
             Run `relix setup` for guided configuration."
        );
    }

    let script = locate_script("relix-mesh-up")?;
    let mut cmd = build_boot_command(&script, &effective)?;
    if let Some(cfg) = &cfg_opt {
        apply_config_env(&mut cmd, cfg);
    }

    println!("starting mesh via {} ...", script.display());
    let mut child = cmd
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to spawn boot script: {e}"))?;

    let health_url = format!("http://127.0.0.1:{}/health", effective.bridge_port);
    let dashboard_url = format!("http://127.0.0.1:{}/dashboard", effective.bridge_port);

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

    println!("bridge ready at http://127.0.0.1:{}", effective.bridge_port);

    // Surface the bridge auth token so operators have a single
    // place to copy it from. The bridge writes it to
    // `~/.relix/bridge-token` on first boot. The dashboard picks
    // it up automatically via the bootstrap endpoint; scripts /
    // curl invocations paste this string into the
    // `Authorization: Bearer <token>` header.
    if let Some((path, value)) = read_bridge_token() {
        println!("bridge token: {value}  (stored in {})", path.display());
    } else {
        eprintln!(
            "(could not read bridge-token file from ~/.relix/bridge-token — \
             curl invocations will need to read it from the bridge log)"
        );
    }

    // Tell the operator how to actually get INTO the dashboard: whether this
    // is a first-run admin setup or a normal login, where to reset a
    // forgotten password, and how to verify the product loop is reachable.
    // This closes the gap where `relix boot` printed a URL but never the
    // login/setup path, leaving operators staring at a 401 dashboard shell.
    for line in dashboard_login_lines(admin_configured(), &dashboard_url) {
        println!("{line}");
    }

    if !effective.no_browser
        && let Err(e) = open_browser(&dashboard_url)
    {
        eprintln!("(could not open browser: {e}; visit {dashboard_url})");
    }
    println!("Ctrl-C this terminal (or run `relix stop` from another) to shut down.");

    // Block until the boot script exits. Two paths get there:
    //
    //   * Operator Ctrl-Cs this terminal — the OS forwards the
    //     CTRL_C_EVENT / SIGINT to both `relix boot` and the spawned
    //     boot script (they share the console process group /
    //     foreground pgrp). The script's own try/finally tears down
    //     every controller it started. We install a tokio Ctrl-C
    //     handler so this process stays alive long enough to observe
    //     the script's exit, instead of dying first and leaving the
    //     script's cleanup output racing the returned shell prompt.
    //
    //   * `relix stop` from another terminal signals the controllers by
    //     their recorded PID (via the mesh-down script). The boot
    //     script's HasExited / wait loop catches the early exit, runs
    //     cleanup, and exits. Our `child.wait()` returns and we follow
    //     it out.
    //
    // `child.wait()` is a blocking syscall, so it goes through
    // spawn_blocking to keep the tokio runtime healthy.
    let wait_handle = tokio::task::spawn_blocking(move || child.wait());
    tokio::pin!(wait_handle);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    tokio::select! {
        res = &mut wait_handle => report_wait_result(res),
        _ = &mut ctrl_c => {
            println!();
            println!("shutting down ...");
            // Drain the script's cleanup so its final messages don't
            // trail past our return to the prompt.
            let res = (&mut wait_handle).await;
            report_wait_result(res);
        }
    }
    Ok(())
}

fn report_wait_result(
    res: Result<std::io::Result<std::process::ExitStatus>, tokio::task::JoinError>,
) {
    match res {
        Ok(Ok(status)) if status.success() => {}
        Ok(Ok(status)) => eprintln!("boot script exited with status {status}"),
        Ok(Err(e)) => eprintln!("wait failed: {e}"),
        Err(e) => eprintln!("wait task join error: {e}"),
    }
}

/// Stop the local mesh by terminating only the PIDs the boot script
/// recorded. Shells out to `scripts/relix-mesh-down.ps1` (Windows) or
/// `scripts/relix-mesh-down.sh` (POSIX), which read the pidfile mesh-up
/// wrote and signal exactly those processes. A relix-controller or
/// relix-web-bridge belonging to another mesh is never touched.
pub fn stop(args: StopArgs) -> Result<(), Box<dyn std::error::Error>> {
    let script = locate_script("relix-mesh-down")?;

    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("powershell");
        c.arg("-NoProfile").arg("-ExecutionPolicy").arg("Bypass");
        c.arg("-File").arg(&script);
        c
    } else {
        let mut c = Command::new("bash");
        c.arg(&script);
        c
    };

    // Point the down script at the same data root the boot script used so
    // it resolves the right pidfile. PascalCase param on Windows,
    // kebab-case long option on POSIX, same split as build_boot_command.
    if let Some(data_dir) = &args.data_dir {
        if cfg!(windows) {
            cmd.arg("-DataDir").arg(data_dir);
        } else {
            cmd.arg("--data-dir").arg(data_dir);
        }
    }

    println!("stopping mesh via {} ...", script.display());
    let status = cmd
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("failed to spawn mesh-down script: {e}"))?;

    if !status.success() {
        return Err(format!("mesh-down script exited with status {status}").into());
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

    print_database_sizes();

    Ok(())
}

/// Walk the conventional Relix data directories and print one
/// line per SQLite file the bridge / coordinator / memory nodes
/// might have written. Honest about scope: we look in the
/// well-known locations (`~/.relix/data`, `./dev-data`); a
/// custom `data_dir` set by the operator goes uncovered today
/// — `relix doctor` is the place for an exhaustive sweep.
fn print_database_sizes() {
    let candidates = collect_database_paths();
    if candidates.is_empty() {
        return;
    }
    println!();
    println!("databases:");
    for path in candidates {
        match std::fs::metadata(&path) {
            Ok(meta) => {
                println!("  {:<60}  {}", path.display(), human_bytes(meta.len()));
            }
            Err(_) => {
                // File disappeared between the walk + the stat —
                // skip silently. Common when a controller is
                // restarting concurrently with `relix status`.
            }
        }
    }
}

/// Collect candidate SQLite file paths under `~/.relix/data` and
/// `./dev-data`. Filters to files ending in `.db` or `.sqlite`
/// so the WAL / SHM siblings don't double-count.
fn collect_database_paths() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    if let Some(h) = std::env::var_os(home_var) {
        roots.push(PathBuf::from(h).join(".relix").join("data"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join("dev-data"));
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        walk_sqlite_files(&root, &mut out, 0);
    }
    // Deduplicate by canonical path so a symlink doesn't show
    // twice.
    out.sort();
    out.dedup();
    out
}

/// Recursively visit `dir` and append any `*.db` / `*.sqlite`
/// file found, bounded to `depth ≤ 6` so a misconfigured
/// `data_dir` pointing at `/` doesn't take down `relix status`.
fn walk_sqlite_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 6 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            walk_sqlite_files(&p, out, depth + 1);
        } else if ft.is_file() {
            let name = p
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if name == "db" || name == "sqlite" {
                out.push(p);
            }
        }
    }
}

/// Render a byte count as `123 B`, `4.2 KB`, `1.5 MB`, `2.1 GB`.
/// Pure function so the tests can exercise it without disk I/O.
pub(crate) fn human_bytes(n: u64) -> String {
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

    // ~/.local/scripts/<name> — the canonical curl|bash / irm|iex
    // layout. The installer drops the mesh scripts here so a
    // binary-only install (no repo checkout) still has something
    // for `relix boot` to spawn.
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    if let Some(home) = std::env::var_os(home_var).map(PathBuf::from) {
        let leaf = if want_ps { &ps_name } else { &sh_name };
        let candidate = home.join(".local").join("scripts").join(leaf);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(format!(
        "could not find {} in any scripts directory (looked in ./scripts, \
         the install dir, and ~/.local/scripts). If you installed via \
         curl|bash / irm|iex, re-run the installer — newer versions drop \
         the mesh scripts in ~/.local/scripts. cwd: {}",
        if want_ps { &ps_name } else { &sh_name },
        cwd.display()
    )
    .into())
}

/// Layer config-driven secrets onto the boot command's environment.
/// The mesh-up script reads these via `$env:VAR` / `$VAR` — we set
/// them here rather than asking the operator to export them.
fn apply_config_env(cmd: &mut Command, cfg: &crate::config::RelixConfig) {
    // AI provider API key. The AI-node config emitted by mesh-up
    // points at provider-specific env vars (OPENAI_API_KEY,
    // OPENROUTER_API_KEY, ...) via `api_key_env`. Set the right one
    // so the provider actually authenticates.
    if !cfg.provider.api_key.is_empty()
        && let Some(var) = provider_api_key_env(&cfg.provider.name)
    {
        cmd.env(var, &cfg.provider.api_key);
    }
    // AI model selection (RELA-45). mesh-up reads RELIX_AI_MODEL and
    // writes it as the active provider's `default_model`. An empty
    // value leaves the provider's baked-in default in place, so the
    // OpenRouter free-model fallback still applies to a fresh setup.
    if !cfg.provider.model.is_empty() {
        cmd.env("RELIX_AI_MODEL", &cfg.provider.model);
    }
    // Channel secrets. mesh-up only emits the channel TOML when the
    // matching `RELIX_*` flag is set (handled in build_boot_command);
    // here we just supply the tokens it will reference.
    if cfg.channels.telegram && !cfg.channels.telegram_token.is_empty() {
        cmd.env("RELIX_TELEGRAM_BOT_TOKEN", &cfg.channels.telegram_token);
    }
    if cfg.channels.discord {
        if !cfg.channels.discord_token.is_empty() {
            cmd.env("RELIX_DISCORD_BOT_TOKEN", &cfg.channels.discord_token);
        }
        if !cfg.channels.discord_channel.is_empty() {
            cmd.env("RELIX_DISCORD_CHANNEL_ID", &cfg.channels.discord_channel);
        }
    }
    if cfg.channels.slack {
        if !cfg.channels.slack_token.is_empty() {
            cmd.env("RELIX_SLACK_BOT_TOKEN", &cfg.channels.slack_token);
        }
        if !cfg.channels.slack_channel.is_empty() {
            cmd.env("RELIX_SLACK_CHANNEL_ID", &cfg.channels.slack_channel);
        }
    }
    // Credential vault. mesh-up emits `[credentials] enabled` when
    // RELIX_CREDENTIAL_VAULT=1; the coordinator reads the master key
    // from RELIX_CREDENTIAL_KEY (the vault's default `master_key_env`).
    // Only forward when a real key is present — an empty key keeps the
    // vault disabled (never a hardcoded default).
    if cfg.credentials.enabled && !cfg.credentials.master_key.is_empty() {
        cmd.env("RELIX_CREDENTIAL_VAULT", "1");
        cmd.env("RELIX_CREDENTIAL_KEY", &cfg.credentials.master_key);
    }
    // Approval delivery. mesh-up emits `[approval]` + `[approval.delivery]`
    // when RELIX_APPROVALS=1; the channel defaults to the in-process
    // dashboard (no external secret).
    if cfg.approvals.enabled {
        cmd.env("RELIX_APPROVALS", "1");
        let channel = if cfg.approvals.channel.is_empty() {
            "dashboard"
        } else {
            cfg.approvals.channel.as_str()
        };
        cmd.env("RELIX_APPROVAL_CHANNEL", channel);
    }
}

/// Map a provider name to the env var the AI node's TOML references
/// via `api_key_env`. Returns `None` for providers that have no key
/// (mock, local Ollama-style endpoints).
fn provider_api_key_env(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "openai" => Some("OPENAI_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "xai" => Some("XAI_API_KEY"),
        "gemini" => Some("GEMINI_API_KEY"),
        _ => None,
    }
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

    // The two scripts declare their parameters differently: PowerShell
    // uses PascalCase (`-Provider`, `-BridgePort`) while the bash script
    // uses kebab-case long options. Mixing them produces a hard parser
    // error on Windows ("A parameter cannot be found that matches
    // parameter name '-bridge-port'."). Everything else the scripts
    // care about flows through env vars (RELIX_DATA_DIR, RELIX_TELEGRAM,
    // …) which both shells read identically.
    if cfg!(windows) {
        cmd.arg("-Provider").arg(&args.provider);
        cmd.arg("-BridgePort").arg(args.bridge_port.to_string());
    } else {
        cmd.arg("--provider").arg(&args.provider);
        cmd.arg("--bridge-port").arg(args.bridge_port.to_string());
    }
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

/// Try to read the bridge token from `~/.relix/bridge-token`.
/// Returns `(path, value)` on success, `None` when the file is
/// missing or unreadable. Trims the value so a trailing newline
/// doesn't show up in the printed banner.
fn read_bridge_token() -> Option<(PathBuf, String)> {
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = std::env::var_os(home_var)?;
    let path = PathBuf::from(home).join(".relix").join("bridge-token");
    let raw = std::fs::read_to_string(&path).ok()?;
    let v = raw.trim().to_string();
    if v.is_empty() { None } else { Some((path, v)) }
}

/// Whether a dashboard admin credential already exists. The bridge stores it
/// as `dashboard-admin.json` next to the bridge token (`~/.relix/`). Used by
/// `relix boot` to print the right first-run vs. login hint. Best-effort: an
/// unreadable home dir reports "not configured" so the banner errs toward
/// showing the setup path.
fn admin_configured() -> bool {
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let Some(home) = std::env::var_os(home_var) else {
        return false;
    };
    PathBuf::from(home)
        .join(".relix")
        .join("dashboard-admin.json")
        .is_file()
}

/// The login/setup hint block `relix boot` prints once the bridge is healthy.
/// Pure function over `(admin_exists, dashboard_url)` so it is unit-testable
/// without touching disk. First run (no admin) points at the in-dashboard
/// setup form; an existing admin points at login + the reset path. Always
/// names `relix dashboard doctor` so an operator can verify the product loop.
fn dashboard_login_lines(admin_exists: bool, dashboard_url: &str) -> Vec<String> {
    let mut out = vec![format!("dashboard:    {dashboard_url}")];
    if admin_exists {
        out.push("  log in with your dashboard admin username + password.".to_string());
        out.push(
            "  forgot it? run `relix dashboard reset-admin` (local recovery, no secret printed twice)."
                .to_string(),
        );
    } else {
        out.push(
            "  first run: open the dashboard and create the admin account (username + password)."
                .to_string(),
        );
        out.push(
            "  prefer the CLI? `relix dashboard reset-admin` pre-creates/sets the admin locally."
                .to_string(),
        );
    }
    out.push("  verify the product loop: `relix dashboard doctor`.".to_string());
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_renders_each_unit() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(human_bytes(2_500_000), "2.4 MB");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(human_bytes(2_500_000_000), "2.3 GB");
    }

    #[test]
    fn apply_config_env_forwards_model_as_relix_ai_model() {
        use std::ffi::OsStr;
        let mut cfg = crate::config::RelixConfig::default();
        cfg.provider.model = "openai/gpt-oss-120b:free".to_string();
        let mut cmd = Command::new("noop");
        apply_config_env(&mut cmd, &cfg);
        let found = cmd.get_envs().any(|(k, v)| {
            k == OsStr::new("RELIX_AI_MODEL") && v == Some(OsStr::new("openai/gpt-oss-120b:free"))
        });
        assert!(found, "expected RELIX_AI_MODEL to be forwarded to mesh-up");
    }

    #[test]
    fn boot_hint_first_run_points_at_setup() {
        let lines = dashboard_login_lines(false, "http://127.0.0.1:19791/dashboard");
        let joined = lines.join("\n");
        assert!(joined.contains("http://127.0.0.1:19791/dashboard"));
        assert!(joined.contains("first run"));
        assert!(joined.contains("create the admin"));
        // Always surfaces the verification command.
        assert!(joined.contains("relix dashboard doctor"));
    }

    #[test]
    fn boot_hint_existing_admin_points_at_login_and_reset() {
        let lines = dashboard_login_lines(true, "http://127.0.0.1:19791/dashboard");
        let joined = lines.join("\n");
        assert!(joined.contains("log in"));
        assert!(joined.contains("reset-admin"));
        assert!(!joined.contains("first run"));
        assert!(joined.contains("relix dashboard doctor"));
    }

    #[test]
    fn apply_config_env_omits_model_when_empty() {
        use std::ffi::OsStr;
        let cfg = crate::config::RelixConfig::default();
        let mut cmd = Command::new("noop");
        apply_config_env(&mut cmd, &cfg);
        let present = cmd
            .get_envs()
            .any(|(k, _)| k == OsStr::new("RELIX_AI_MODEL"));
        assert!(
            !present,
            "RELIX_AI_MODEL must stay unset when no model is configured \
             so the provider's baked-in default (free for OpenRouter) wins"
        );
    }
}
