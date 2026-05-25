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
    //   * `relix stop` from another terminal taskkill's the
    //     controllers — the boot script's HasExited / wait loop
    //     catches the early exit, runs cleanup, and exits. Our
    //     `child.wait()` returns and we follow it out.
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

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}
