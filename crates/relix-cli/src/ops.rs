//! `relix-cli ops` — operator-facing CLI snapshots.
//!
//! Subcommands:
//! - `providers-health` (PH-WAVE2L) — hits the bridge's
//!   `/v1/providers/health` and pretty-prints aggregate +
//!   per-provider state.
//! - `capabilities` (PH-DASH3-CLI) — hits `/v1/topology` and
//!   pretty-prints every capability the bridge has discovered,
//!   mirroring the dashboard's PH-DASH3 explorer for terminal
//!   operators.
//!
//! All subcommands are one-shot HTTP-against-bridge — useful
//! for status-line scripts, on-call triage, and tmux dashboards.

use clap::Subcommand;
use serde::Deserialize;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Print the consolidated provider-health snapshot from the
    /// bridge. Shows aggregate counters across the AI stack
    /// (cooldowns active, rate-limit hits in 5min / 1h, lifetime
    /// success / fail counts) plus a per-provider one-liner.
    ProvidersHealth {
        /// Bridge HTTP base URL (e.g. `http://127.0.0.1:19791`).
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Raw JSON instead of the pretty one-screen summary.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List every capability the bridge has discovered across
    /// every peer in the cached topology. Mirrors the dashboard's
    /// PH-DASH3 capability explorer for terminal operators.
    /// Source is `/v1/topology` — each peer's methods[]
    /// aggregated.
    Capabilities {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Filter by capability prefix (e.g. `tool.web`).
        /// Substring match, case-insensitive. Empty = all.
        #[arg(long, default_value = "")]
        filter: String,
        /// Raw JSON instead of the table view.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// H6 stuck-running task projection from /v1/tasks/stuck.
    /// Shows tasks that have been `running` longer than
    /// `--threshold-secs` (default 300) without a deadline.
    Stuck {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Stuck threshold in seconds (passed to the bridge).
        #[arg(long, default_value_t = 300i64)]
        threshold_secs: i64,
        /// Raw JSON instead of the table view.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Preview the HealthAwareRouter's pick for a candidate
    /// list (PH-ROUTER-PREVIEW). Hits POST
    /// /v1/providers/route_test against current cached health.
    /// Does NOT send any chat call.
    RouteTest {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Comma-separated candidate list (e.g.
        /// `openai,anthropic`). Order matters — the router uses
        /// it for stable tie-breaking.
        #[arg(long)]
        candidates: String,
        /// Raw JSON instead of the pretty summary.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// W2-006c mirror: per-capability invocation + latency
    /// counters from a peer's DispatchBridge, fetched via
    /// `GET /v1/dispatch/stats`. Sorted by mean latency desc —
    /// the slowest capability shows first. Lifetime counters,
    /// reset on peer restart.
    DispatchStats {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Target peer alias.
        #[arg(long, default_value = "tool")]
        peer: String,
        /// Raw JSON instead of the table view.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// W2-008b mirror: end-to-end mesh smoke test against
    /// an already-running bridge. Hits liveness, topology,
    /// chat completion, dispatch stats, and policy denials
    /// in sequence. Exit 1 on any failure. Pure Rust port
    /// of `scripts/demo-smoke.sh` so Windows operators
    /// don't need bash.
    Smoke {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Chat model used for the round-trip step. Defaults
        /// to `relix-mock` which works without an API key
        /// regardless of provider configuration.
        #[arg(long, default_value = "relix-mock")]
        provider: String,
    },
    /// W2-007b mirror: ask a peer's PolicyEngine "would this
    /// caller (with these groups) calling this method be
    /// allowed?" without invoking the method. Hits
    /// `GET /v1/policy/simulate`.
    PolicySimulate {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Target peer alias.
        #[arg(long, default_value = "tool")]
        peer: String,
        /// Method to simulate (e.g. `tool.web_fetch`).
        #[arg(long)]
        method: String,
        /// Comma-separated groups list (e.g.
        /// `chat-users,operators`). Empty = inherit caller.
        #[arg(long, default_value = "")]
        groups: String,
        /// Raw JSON instead of the pretty summary.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// W2-007e mirror: recent policy-denied attempts ring
    /// (capacity 256, peer-restart resets). Hits
    /// `GET /v1/policy/denials`.
    PolicyDenials {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Target peer alias.
        #[arg(long, default_value = "tool")]
        peer: String,
        /// Maximum entries (default 100, server caps at 500).
        #[arg(long, default_value_t = 100usize)]
        max: usize,
        /// Raw JSON instead of the table view.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// W2-008i — one-shot snapshot of the bridge's
    /// observable state. Hits health, topology, dispatch
    /// stats, policy denials, and the recent events ring
    /// in parallel and combines them into a single JSON
    /// dump. Useful for incident attachments and offline
    /// triage — engineers without mesh access can answer
    /// "what did the mesh look like at $time" from the
    /// file alone.
    Snapshot {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Target peer alias for per-peer endpoints
        /// (dispatch stats + policy denials).
        #[arg(long, default_value = "tool")]
        peer: String,
        /// Write to a file instead of stdout. `-` is the
        /// stdout sentinel (default). Existing files are
        /// overwritten without prompt.
        #[arg(long, default_value = "-")]
        output: String,
        /// Pretty-print the JSON (indented, easy to diff).
        #[arg(long, default_value_t = false)]
        pretty: bool,
    },
    /// W2-008h — print a copy-paste Open WebUI connection
    /// setup for the current bridge. Hits `/v1/models` and
    /// formats the host:port + advertised model ids into
    /// a block operators can paste into Open WebUI's
    /// Settings → Connections → OpenAI API.
    OpenWebuiSetup {
        /// Bridge HTTP base URL (used to fetch the model list).
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Hostname Open WebUI should dial. Defaults to
        /// `host.docker.internal` (the Docker-on-Mac/Windows
        /// loopback alias). Use `127.0.0.1` when Open WebUI
        /// is native, or your machine's LAN IP when remote.
        #[arg(long, default_value = "host.docker.internal")]
        host: String,
        /// Raw JSON of the bridge's `/v1/models` response.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// W2-008d — live tail of the task firehose. Polls
    /// `/v1/tasks/events/recent?since=<cursor>` on a loop
    /// and prints each new event one-per-line. Ctrl-C
    /// exits cleanly. Lighter than SSE — pure HTTP polling,
    /// works through every proxy / shell / tmux.
    Tail {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Filter by event_type substring (case-insensitive).
        /// Empty = all.
        #[arg(long, default_value = "")]
        filter: String,
        /// Poll interval in milliseconds (default 1000).
        /// Clamped to [200, 60000].
        #[arg(long, default_value_t = 1000u64)]
        interval_ms: u64,
        /// Stop after N total events have been printed
        /// (handy for CI smoke). 0 = no limit.
        #[arg(long, default_value_t = 0usize)]
        max_events: usize,
    },
    /// Recent cross-task events from /v1/tasks/events/recent.
    /// Mirrors the dashboard firehose for terminal operators
    /// — shows the H2 one-line summary projection per row.
    Events {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Page limit (server caps at 500).
        #[arg(long, default_value_t = 50usize)]
        limit: usize,
        /// Filter by event_type substring (e.g.
        /// `task.retry`). Empty = all.
        #[arg(long, default_value = "")]
        filter: String,
        /// Raw JSON instead of the table view.
        #[arg(long, default_value_t = false)]
        json: bool,
        /// W2-008f: CSV output instead of the table — easy
        /// spreadsheet import. Columns:
        /// `event_id,task_id,event_type,ts,summary,payload`.
        /// Quoting matches RFC 4180.
        #[arg(long, default_value_t = false)]
        csv: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::ProvidersHealth { bridge, json } => providers_health(&bridge, json).await,
        Cmd::Capabilities {
            bridge,
            filter,
            json,
        } => capabilities(&bridge, &filter, json).await,
        Cmd::Stuck {
            bridge,
            threshold_secs,
            json,
        } => stuck(&bridge, threshold_secs, json).await,
        Cmd::Events {
            bridge,
            limit,
            filter,
            json,
            csv,
        } => events(&bridge, limit, &filter, json, csv).await,
        Cmd::RouteTest {
            bridge,
            candidates,
            json,
        } => route_test(&bridge, &candidates, json).await,
        Cmd::DispatchStats { bridge, peer, json } => dispatch_stats(&bridge, &peer, json).await,
        Cmd::PolicySimulate {
            bridge,
            peer,
            method,
            groups,
            json,
        } => policy_simulate(&bridge, &peer, &method, &groups, json).await,
        Cmd::PolicyDenials {
            bridge,
            peer,
            max,
            json,
        } => policy_denials(&bridge, &peer, max, json).await,
        Cmd::Smoke { bridge, provider } => smoke(&bridge, &provider).await,
        Cmd::Tail {
            bridge,
            filter,
            interval_ms,
            max_events,
        } => tail(&bridge, &filter, interval_ms, max_events).await,
        Cmd::OpenWebuiSetup { bridge, host, json } => openwebui_setup(&bridge, &host, json).await,
        Cmd::Snapshot {
            bridge,
            peer,
            output,
            pretty,
        } => snapshot(&bridge, &peer, &output, pretty).await,
    }
}

// W2-008i CLI: combined-state snapshot for incident attachments.

async fn snapshot(
    bridge: &str,
    peer: &str,
    output: &str,
    pretty: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    // Bind the URLs first so the format!() temporaries
    // live across the tokio::join! await; passing
    // `&format!(...)` inline races their drop.
    let u_health = format!("{base}/v1/health");
    let u_topology = format!("{base}/v1/topology");
    let u_dispatch = format!("{base}/v1/dispatch/stats?peer={peer}");
    let u_denials = format!("{base}/v1/policy/denials?peer={peer}&max=100");
    let u_events = format!("{base}/v1/tasks/events/recent?limit=100");
    // Run the five fetches concurrently — each is a cheap
    // HTTP GET and incident-response wants the dump fast.
    let (health, topology, dispatch_stats, denials, events) = tokio::join!(
        http_get(&u_health),
        http_get(&u_topology),
        http_get(&u_dispatch),
        http_get(&u_denials),
        http_get(&u_events),
    );
    // Each endpoint's value is either the parsed JSON
    // payload (preferred — pretty-prints cleanly) or an
    // error string when the fetch failed. Operators see
    // partial state instead of a hard fail; one section
    // being missing in a triage dump is still useful.
    let entry = |name: &str,
                 res: Result<String, Box<dyn std::error::Error>>|
     -> (String, serde_json::Value) {
        let v = match res {
            Ok(body) => serde_json::from_str::<serde_json::Value>(&body)
                .unwrap_or(serde_json::Value::String(body)),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        };
        (name.to_string(), v)
    };
    let mut obj = serde_json::Map::new();
    obj.insert(
        "snapshot_at".to_string(),
        serde_json::json!(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ),
    );
    obj.insert("bridge".to_string(), serde_json::json!(base));
    obj.insert("peer".to_string(), serde_json::json!(peer));
    let entries = [
        entry("health", health),
        entry("topology", topology),
        entry("dispatch_stats", dispatch_stats),
        entry("denials", denials),
        entry("events", events),
    ];
    for (k, v) in entries {
        obj.insert(k, v);
    }
    let value = serde_json::Value::Object(obj);
    let text = if pretty {
        serde_json::to_string_pretty(&value)?
    } else {
        serde_json::to_string(&value)?
    };
    if output == "-" {
        println!("{text}");
    } else {
        std::fs::write(output, &text).map_err(|e| format!("write {output}: {e}"))?;
        // Status line on stderr so `> file` redirection
        // remains clean if the operator passes `-` instead.
        eprintln!("wrote snapshot to {output} ({len} bytes)", len = text.len());
    }
    Ok(())
}

// W2-008h CLI: print Open WebUI connection setup.

#[derive(Debug, Deserialize)]
struct ModelsResp {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    description: String,
}

/// W2-008h: derive the bridge's listening port from the
/// `--bridge` URL (`http://127.0.0.1:19791` → `19791`).
/// Falls back to `19791` (the default).
fn port_from_bridge(bridge: &str) -> u16 {
    bridge
        .trim_end_matches('/')
        .rsplit_once(':')
        .and_then(|(_, p)| p.trim_end_matches('/').parse::<u16>().ok())
        .unwrap_or(19791)
}

async fn openwebui_setup(
    bridge: &str,
    host: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/models", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let resp: ModelsResp = serde_json::from_str(&body)
        .map_err(|e| format!("decode /v1/models body: {e} (body={body})"))?;
    let port = port_from_bridge(bridge);
    println!("Open WebUI connection setup");
    println!("Settings → Connections → OpenAI API");
    println!();
    println!("  API Base URL: http://{host}:{port}/v1");
    println!("  API Key:      relix   (any non-empty string works)");
    println!();
    if resp.data.is_empty() {
        println!("  Models:       (none advertised — bridge has no");
        println!("                [openai_compat.models] entries and no");
        println!("                ai.chat-capable peer in the manifest cache)");
    } else {
        println!("  Models:");
        for m in &resp.data {
            let desc = if m.description.is_empty() {
                String::from("(no description)")
            } else {
                m.description.clone()
            };
            println!("    {id:<24}  {desc}", id = m.id, desc = desc);
        }
    }
    println!();
    println!("Note: when running native (no docker), use --host 127.0.0.1.");
    println!("When Open WebUI is on another machine, use this host's LAN IP.");
    Ok(())
}

// W2-008d CLI live-tail: poll /v1/tasks/events/recent on a loop.

async fn tail(
    bridge: &str,
    filter: &str,
    interval_ms: u64,
    max_events: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let interval = std::time::Duration::from_millis(interval_ms.clamp(200, 60_000));
    let needle = filter.trim().to_ascii_lowercase();
    let mut since: i64 = 0;
    let mut printed: usize = 0;
    // Header so operators know what they're looking at —
    // matches the `events` subcommand columns.
    eprintln!(
        "tailing {base}/v1/tasks/events/recent  interval={ms}ms  filter='{f}'  (Ctrl-C to stop)",
        ms = interval.as_millis(),
        f = filter,
    );
    let ev_h = "event_type";
    let tid_h = "task_id";
    let id_h = "id";
    let sum_h = "summary";
    println!("{ev_h:<28}  {tid_h:<10}  {id_h:>6}  {sum_h}");
    loop {
        // Page size is intentionally small per tick — the
        // operator polling cadence is the rate limiter, not
        // the page. Bridge caps internally too.
        let url = if since > 0 {
            format!("{base}/v1/tasks/events/recent?limit=50&since={since}")
        } else {
            format!("{base}/v1/tasks/events/recent?limit=50")
        };
        match http_get(&url).await {
            Ok(body) => {
                let resp: EventsResponse = match serde_json::from_str(&body) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("(decode failed: {e})");
                        tokio::time::sleep(interval).await;
                        continue;
                    }
                };
                // The bridge returns events oldest-first
                // within a since= window — print in that
                // order so operators read top-to-bottom as
                // time flows.
                for r in &resp.items {
                    if !needle.is_empty() && !r.event_type.to_ascii_lowercase().contains(&needle) {
                        continue;
                    }
                    let short = if r.task_id.len() > 8 {
                        &r.task_id[..8]
                    } else {
                        &r.task_id
                    };
                    let sum = if r.summary.is_empty() {
                        r.payload.as_str()
                    } else {
                        r.summary.as_str()
                    };
                    println!(
                        "{et:<28}  {tid:<10}  {id:>6}  {sum}",
                        et = r.event_type,
                        tid = short,
                        id = r.event_id,
                        sum = sum,
                    );
                    printed += 1;
                    if max_events > 0 && printed >= max_events {
                        eprintln!("(reached --max-events={max_events})");
                        return Ok(());
                    }
                }
                // Advance the cursor for the next tick. The
                // bridge guarantees next_cursor monotonic
                // across calls; we trust it.
                if resp.next_cursor > since {
                    since = resp.next_cursor;
                }
            }
            Err(e) => {
                // Don't bail on a transient blip — operators
                // care about tail resilience. Log once per
                // failed poll, sleep, retry.
                eprintln!("(poll failed: {e})");
            }
        }
        tokio::time::sleep(interval).await;
    }
}

// W2-008b CLI mirror: end-to-end mesh smoke test.

async fn smoke(bridge: &str, provider: &str) -> Result<(), Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    println!("Relix smoke (bridge={base})");
    let mut step = 0usize;
    let mut fails = 0usize;
    let mut run = |desc: &str, res: Result<String, Box<dyn std::error::Error>>| {
        step += 1;
        match &res {
            Ok(_) => println!("  step {step} OK   — {desc}"),
            Err(e) => {
                println!("  step {step} FAIL — {desc}");
                eprintln!("         {e}");
                fails += 1;
            }
        }
        res.ok()
    };

    // 1. liveness
    let _ = run("GET /health", http_get(&format!("{base}/health")).await);

    // 2. topology — count peers when we got a body back
    let topo_body = run(
        "GET /v1/topology",
        http_get(&format!("{base}/v1/topology")).await,
    );
    if let Some(body) = topo_body {
        let peer_count = body.matches("\"alias\":").count();
        println!("         peers discovered: {peer_count}");
    }

    // 3. chat completion (mock by default)
    let chat_body = format!(
        r#"{{"model":"{provider}","messages":[{{"role":"user","content":"smoke test ping"}}]}}"#
    );
    let _ = run(
        &format!("POST /v1/chat/completions (model={provider})"),
        http_post_json(&format!("{base}/v1/chat/completions"), &chat_body).await,
    );

    // 4. dispatch stats — observability
    let _ = run(
        "GET /v1/dispatch/stats?peer=tool (W2-006c)",
        http_get(&format!("{base}/v1/dispatch/stats?peer=tool")).await,
    );

    // 5. policy denials — yellow-flag a non-empty ring
    let denials_body = run(
        "GET /v1/policy/denials?peer=tool (W2-007e)",
        http_get(&format!("{base}/v1/policy/denials?peer=tool&max=10")).await,
    );
    if let Some(body) = denials_body {
        // Loose-parse just the count field — keeps the smoke
        // path zero-dependency on the full denials struct.
        let count = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("count").and_then(|c| c.as_u64()))
            .unwrap_or(0);
        if count > 0 {
            println!("         ⚠  {count} recent denial(s) on tool — investigate via:");
            println!("         relix-cli ops policy-denials --peer tool");
        } else {
            println!("         denial ring empty on tool");
        }
    }

    println!();
    if fails == 0 {
        println!("smoke PASS — {step}/{step} steps OK");
        Ok(())
    } else {
        Err(format!("smoke FAIL — {fails}/{step} step(s) failed").into())
    }
}

async fn providers_health(bridge: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/providers/health", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let h: HealthResponse = serde_json::from_str(&body)
        .map_err(|e| format!("bridge returned non-JSON body: {e}\nraw:\n{body}"))?;
    print_pretty(&h);
    Ok(())
}

fn print_pretty(h: &HealthResponse) {
    // Aggregate header.
    let a = &h.aggregate;
    let total_ok = a.success_request_count;
    let total_fail = a.failed_request_count;
    let reliability = (total_ok * 100)
        .checked_div(total_ok + total_fail)
        .unwrap_or(0);
    println!(
        "providers={count}  cooldowns_active={cd}  quarantined={q}",
        count = h.providers.len(),
        cd = a.cooldowns_active,
        q = a.quarantined,
    );
    println!(
        "rate_limit_hits  5min={rl5}  1h={rl1h}",
        rl5 = a.rate_limit_hits_5min,
        rl1h = a.rate_limit_hits_1h,
    );
    println!(
        "lifetime  ok={ok}  fail={fail}  reliability={r}%",
        ok = total_ok,
        fail = total_fail,
        r = reliability,
    );
    println!();
    // Per-provider table.
    let name_h = "provider";
    let cfg_h = "cfg";
    let cd_h = "cooldown";
    let last_h = "last_fail";
    let rl_h = "rl 5m/1h";
    println!("{name_h:<14}  {cfg_h:<5}  {cd_h:<12}  {last_h:<24}  {rl_h}");
    for p in &h.providers {
        let name = &p.name;
        let cfg = if p.configured { "yes" } else { "no" };
        let cd = match p.cooldown_until {
            Some(c) => {
                let rem = c - now_secs();
                if rem > 0 {
                    let auto = if p.quarantined_at.is_none() {
                        "auto"
                    } else {
                        "op"
                    };
                    format!("{rem}s ({auto})")
                } else {
                    "-".to_string()
                }
            }
            None => "-".to_string(),
        };
        let last = match (p.last_failure_reason.as_deref(), p.last_failure_at) {
            (Some(r), Some(t)) => format!("{r} @ {t}"),
            _ => "-".to_string(),
        };
        let rl = format!("{}/{}", p.rate_limit_hits_5min, p.rate_limit_hits_1h);
        println!("{name:<14}  {cfg:<5}  {cd:<12}  {last:<24}  {rl}",);
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn capabilities(
    bridge: &str,
    filter: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/topology", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let topo: TopologyResponse = serde_json::from_str(&body)
        .map_err(|e| format!("bridge returned non-JSON body: {e}\nraw:\n{body}"))?;
    let needle = filter.trim().to_ascii_lowercase();
    let mut rows: Vec<(String, String, String, String)> = Vec::new();
    for p in &topo.peers {
        let alias = p.alias.clone().unwrap_or_else(|| "(none)".to_string());
        for m in &p.methods {
            if !needle.is_empty() && !m.to_ascii_lowercase().contains(&needle) {
                continue;
            }
            rows.push((
                m.clone(),
                alias.clone(),
                p.node_type.clone(),
                p.freshness.clone(),
            ));
        }
    }
    rows.sort();
    let total_methods: usize = topo.peers.iter().map(|p| p.methods.len()).sum();
    println!(
        "capabilities  shown={shown}  total={total}  peers={peers}",
        shown = rows.len(),
        total = total_methods,
        peers = topo.peers.len(),
    );
    if rows.is_empty() {
        if needle.is_empty() {
            println!("(no capabilities discovered yet)");
        } else {
            println!("(no capabilities match filter \"{needle}\")");
        }
        return Ok(());
    }
    println!();
    let m_h = "capability";
    let a_h = "alias";
    let t_h = "node_type";
    let f_h = "freshness";
    println!("{m_h:<36}  {a_h:<14}  {t_h:<14}  {f_h}");
    for (method, alias, node_type, fresh) in &rows {
        println!("{method:<36}  {alias:<14}  {node_type:<14}  {fresh}",);
    }
    Ok(())
}

async fn stuck(
    bridge: &str,
    threshold_secs: i64,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/tasks/stuck?threshold_secs={}",
        bridge.trim_end_matches('/'),
        threshold_secs.max(0),
    );
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let s: StuckResponse = serde_json::from_str(&body)
        .map_err(|e| format!("bridge returned non-JSON body: {e}\nraw:\n{body}"))?;
    println!(
        "stuck={count}  threshold_secs={threshold}",
        count = s.count,
        threshold = s.threshold_secs.unwrap_or(threshold_secs),
    );
    if s.items.is_empty() {
        println!("(no stuck tasks)");
        return Ok(());
    }
    println!();
    let id_h = "task_id";
    let title_h = "title";
    let age_h = "age";
    println!("{id_h:<36}  {title_h:<32}  {age_h}");
    for it in &s.items {
        println!(
            "{id:<36}  {title:<32}  {age}s",
            id = it.task_id,
            title = it.title,
            age = it.age_secs,
        );
    }
    Ok(())
}

async fn events(
    bridge: &str,
    limit: usize,
    filter: &str,
    json: bool,
    csv: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let cap = limit.clamp(1, 500);
    let url = format!(
        "{}/v1/tasks/events/recent?limit={}",
        bridge.trim_end_matches('/'),
        cap,
    );
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let resp: EventsResponse = serde_json::from_str(&body)
        .map_err(|e| format!("bridge returned non-JSON body: {e}\nraw:\n{body}"))?;
    let needle = filter.trim().to_ascii_lowercase();
    let filtered: Vec<&EventRow> = resp
        .items
        .iter()
        .filter(|r| needle.is_empty() || r.event_type.to_ascii_lowercase().contains(&needle))
        .collect();
    // W2-008f: CSV branch — RFC 4180 quoting, no table
    // headers stderr noise so the output pipes cleanly
    // into `> events.csv`.
    if csv {
        println!("event_id,task_id,event_type,ts,summary,payload");
        for r in &filtered {
            println!(
                "{id},{tid},{et},{ts},{sum},{pl}",
                id = r.event_id,
                tid = csv_field(&r.task_id),
                et = csv_field(&r.event_type),
                ts = r.ts,
                sum = csv_field(&r.summary),
                pl = csv_field(&r.payload),
            );
        }
        return Ok(());
    }
    println!(
        "events  shown={shown}  fetched={fetched}  next_cursor={cursor}",
        shown = filtered.len(),
        fetched = resp.items.len(),
        cursor = resp.next_cursor,
    );
    if filtered.is_empty() {
        if needle.is_empty() {
            println!("(no events)");
        } else {
            println!("(no events match filter \"{needle}\")");
        }
        return Ok(());
    }
    println!();
    let ev_h = "event_type";
    let tid_h = "task_id";
    let id_h = "id";
    let sum_h = "summary";
    println!("{ev_h:<28}  {tid_h:<10}  {id_h:>6}  {sum_h}");
    for r in &filtered {
        let short = if r.task_id.len() > 8 {
            &r.task_id[..8]
        } else {
            &r.task_id
        };
        let sum = if r.summary.is_empty() {
            r.payload.as_str()
        } else {
            r.summary.as_str()
        };
        println!(
            "{et:<28}  {tid:<10}  {id:>6}  {sum}",
            et = r.event_type,
            tid = short,
            id = r.event_id,
            sum = sum,
        );
    }
    Ok(())
}

async fn route_test(
    bridge: &str,
    candidates_csv: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let candidates: Vec<String> = candidates_csv
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if candidates.is_empty() {
        return Err("--candidates required (comma-separated, non-empty)".into());
    }
    let url = format!("{}/v1/providers/route_test", bridge.trim_end_matches('/'));
    let body_in = serde_json::json!({ "candidates": candidates }).to_string();
    let body = http_post_json(&url, &body_in).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let resp: RouteTestResp = serde_json::from_str(&body)
        .map_err(|e| format!("bridge returned non-JSON body: {e}\nraw:\n{body}"))?;
    println!("router={r}  chosen={c}", r = resp.router, c = resp.chosen,);
    println!("reasoning: {}", resp.reasoning);
    println!();
    let name_h = "candidate";
    let score_h = "score";
    let elig_h = "eligibility";
    let why_h = "why";
    println!("{name_h:<14}  {score_h:>5}  {elig_h:<12}  {why_h}");
    for c in &resp.candidates {
        println!(
            "{n:<14}  {s:>5.2}  {e:<12}  {w}",
            n = c.name,
            s = c.score,
            e = c.eligibility,
            w = c.why,
        );
    }
    Ok(())
}

async fn http_post_json(url: &str, body: &str) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("bridge returned HTTP {status}: {body}").into());
    }
    Ok(body)
}

#[derive(Debug, Deserialize)]
struct RouteTestResp {
    #[serde(default)]
    router: String,
    #[serde(default)]
    chosen: String,
    #[serde(default)]
    reasoning: String,
    #[serde(default)]
    candidates: Vec<RouteTestCandidate>,
}

#[derive(Debug, Deserialize)]
struct RouteTestCandidate {
    #[serde(default)]
    name: String,
    #[serde(default)]
    score: f32,
    #[serde(default)]
    eligibility: String,
    #[serde(default)]
    why: String,
}

#[derive(Debug, Deserialize)]
struct EventsResponse {
    #[serde(default)]
    items: Vec<EventRow>,
    #[serde(default)]
    next_cursor: i64,
}

#[derive(Debug, Deserialize)]
struct EventRow {
    #[serde(default)]
    task_id: String,
    #[serde(default)]
    event_id: i64,
    #[serde(default)]
    event_type: String,
    #[serde(default)]
    payload: String,
    #[serde(default)]
    summary: String,
    /// W2-008f: unix-seconds timestamp the bridge ships
    /// (defaults to 0 on older bridges that don't surface it).
    #[serde(default)]
    ts: i64,
}

/// W2-008f: RFC 4180 quoting — wrap in double-quotes when
/// the value contains `,` `"` newline or CR; double any
/// embedded `"`.
fn csv_field(s: &str) -> String {
    let needs_quote = s.bytes().any(|b| matches!(b, b',' | b'"' | b'\n' | b'\r'));
    if !needs_quote {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 4);
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push_str("\"\"");
        } else {
            out.push(c);
        }
    }
    out.push('"');
    out
}

#[derive(Debug, Deserialize)]
struct StuckResponse {
    #[serde(default)]
    items: Vec<StuckItem>,
    #[serde(default)]
    count: usize,
    #[serde(default)]
    threshold_secs: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct StuckItem {
    task_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    age_secs: i64,
}

#[derive(Debug, Deserialize)]
struct TopologyResponse {
    #[serde(default)]
    peers: Vec<TopologyPeer>,
}

#[derive(Debug, Deserialize)]
struct TopologyPeer {
    #[serde(default)]
    alias: Option<String>,
    node_type: String,
    #[serde(default)]
    methods: Vec<String>,
    #[serde(default)]
    freshness: String,
}

async fn http_get(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let resp = client.get(url).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("bridge returned HTTP {status}: {body}").into());
    }
    Ok(body)
}

// W2-006c CLI mirror: GET /v1/dispatch/stats?peer=...

#[derive(Debug, Deserialize)]
struct DispatchStatsResp {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    rows: Vec<DispatchStatsRow>,
    #[serde(default)]
    count: usize,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // last_invoked_at / last_error_at preserved for future "stale" detection
struct DispatchStatsRow {
    #[serde(default)]
    method: String,
    #[serde(default)]
    invocations: u64,
    #[serde(default)]
    errors: u64,
    #[serde(default)]
    denied: u64,
    #[serde(default)]
    unknown_method: u64,
    #[serde(default)]
    last_invoked_at: i64,
    #[serde(default)]
    last_error_at: Option<i64>,
    #[serde(default)]
    latency_samples: u64,
    #[serde(default)]
    last_elapsed_ms: u64,
    #[serde(default)]
    max_elapsed_ms: u64,
    #[serde(default)]
    mean_elapsed_ms: u64,
    /// W2-006d: recent per-call latencies ring (oldest-first,
    /// capped at 32 by the runtime). Empty when the responder
    /// is an older peer that doesn't ship the column.
    #[serde(default)]
    recent_latencies: Vec<u32>,
}

/// W2-006d: render a ring of latency samples as a Unicode
/// block-character sparkline. Heights normalize to the ring's
/// own max so a 5ms-mean method and a 2000ms-mean method both
/// render legibly side-by-side.
fn ascii_sparkline(samples: &[u32]) -> String {
    if samples.is_empty() {
        return "-".to_string();
    }
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = (*samples.iter().max().unwrap_or(&1)).max(1);
    samples
        .iter()
        .map(|&v| {
            // Map [0..=max] → BARS index. f64 keeps the
            // mapping stable across the full u32 range
            // without integer overflow.
            let idx = ((v as f64 / max as f64) * (BARS.len() - 1) as f64).round() as usize;
            BARS[idx.min(BARS.len() - 1)]
        })
        .collect()
}

async fn dispatch_stats(
    bridge: &str,
    peer: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/dispatch/stats?peer={peer}",
        bridge.trim_end_matches('/')
    );
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let parsed: DispatchStatsResp = serde_json::from_str(&body)
        .map_err(|e| format!("decode /v1/dispatch/stats body: {e} (body={body})"))?;
    if parsed.rows.is_empty() {
        println!(
            "(no dispatch activity on peer '{p}' — count={c})",
            p = parsed.peer,
            c = parsed.count
        );
        return Ok(());
    }
    // Sort by mean elapsed desc (tied: invocations desc).
    let mut rows = parsed.rows;
    rows.sort_by(|a, b| {
        b.mean_elapsed_ms
            .cmp(&a.mean_elapsed_ms)
            .then_with(|| b.invocations.cmp(&a.invocations))
    });
    let m_h = "method";
    let i_h = "invocs";
    let e_h = "errs";
    let mean_h = "mean";
    let max_h = "max";
    let last_h = "last";
    let samples_h = "samples";
    let trend_h = "trend";
    println!(
        "{m_h:<36}  {i_h:>7}  {e_h:>5}  {mean_h:>6}  {max_h:>6}  {last_h:>6}  {samples_h:>7}  {trend_h}",
    );
    for r in &rows {
        let method = truncate(&r.method, 36);
        let errs = r.errors + r.denied + r.unknown_method;
        let trend = ascii_sparkline(&r.recent_latencies);
        println!(
            "{method:<36}  {invocs:>7}  {errs:>5}  {mean:>5}ms  {max:>5}ms  {last:>5}ms  {samples:>7}  {trend}",
            method = method,
            invocs = r.invocations,
            errs = errs,
            mean = r.mean_elapsed_ms,
            max = r.max_elapsed_ms,
            last = r.last_elapsed_ms,
            samples = r.latency_samples,
            trend = trend,
        );
    }
    println!("count={}", parsed.count);
    Ok(())
}

// W2-007b CLI mirror: GET /v1/policy/simulate

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PolicySimulateResp {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    method: String,
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    decision: String,
    #[serde(default)]
    matched_rule: Option<String>,
    #[serde(default)]
    reason: String,
}

async fn policy_simulate(
    bridge: &str,
    peer: &str,
    method: &str,
    groups: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let method_trim = method.trim();
    if method_trim.is_empty() {
        return Err("--method required (e.g. `--method tool.web_fetch`)".into());
    }
    let mut url = format!(
        "{}/v1/policy/simulate?peer={}&method={}",
        bridge.trim_end_matches('/'),
        urlencoding(peer),
        urlencoding(method_trim),
    );
    let groups_trim = groups.trim();
    if !groups_trim.is_empty() {
        url.push_str("&groups=");
        url.push_str(&urlencoding(groups_trim));
    }
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let r: PolicySimulateResp = serde_json::from_str(&body)
        .map_err(|e| format!("decode /v1/policy/simulate body: {e} (body={body})"))?;
    let groups_label = if r.groups.is_empty() {
        "(no groups)".to_string()
    } else {
        r.groups.join(",")
    };
    println!("peer={p}  method={m}", p = r.peer, m = r.method);
    println!("groups={g}", g = groups_label);
    println!("decision={d}", d = r.decision);
    println!(
        "matched_rule={r}",
        r = r.matched_rule.as_deref().unwrap_or("-")
    );
    if !r.reason.is_empty() {
        println!("reason={r}", r = r.reason);
    }
    Ok(())
}

// W2-007e CLI mirror: GET /v1/policy/denials

#[derive(Debug, Deserialize)]
struct PolicyDenialsResp {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    denials: Vec<PolicyDenialRow>,
    #[serde(default)]
    count: usize,
}

#[derive(Debug, Deserialize)]
// caller_subject_id is preserved for forensic identity and
// future "--show-subject" mode; the default table is too
// narrow to display the full 32-byte fingerprint.
#[allow(dead_code)]
struct PolicyDenialRow {
    #[serde(default)]
    at: i64,
    #[serde(default)]
    method: String,
    #[serde(default)]
    caller_subject_id: String,
    #[serde(default)]
    caller_name: String,
    #[serde(default)]
    rule: String,
    #[serde(default)]
    reason: String,
}

async fn policy_denials(
    bridge: &str,
    peer: &str,
    max: usize,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/policy/denials?peer={}&max={}",
        bridge.trim_end_matches('/'),
        urlencoding(peer),
        max,
    );
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let r: PolicyDenialsResp = serde_json::from_str(&body)
        .map_err(|e| format!("decode /v1/policy/denials body: {e} (body={body})"))?;
    if r.denials.is_empty() {
        println!(
            "(no denials in ring on peer '{p}' — count={c})",
            p = r.peer,
            c = r.count
        );
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let when_h = "when";
    let method_h = "method";
    let caller_h = "caller";
    let rule_h = "rule";
    let reason_h = "reason";
    println!("{when_h:<10}  {method_h:<28}  {caller_h:<16}  {rule_h:<24}  {reason_h}");
    for d in &r.denials {
        let age = (now - d.at).max(0);
        let when = format_age(age);
        let method = truncate(&d.method, 28);
        let caller = truncate(&d.caller_name, 16);
        let rule = truncate(&d.rule, 24);
        println!(
            "{when:<10}  {method:<28}  {caller:<16}  {rule:<24}  {reason}",
            when = when,
            method = method,
            caller = caller,
            rule = rule,
            reason = d.reason,
        );
    }
    println!("count={}", r.count);
    Ok(())
}

/// W2-007e: minimal "Xs ago" / "Xm ago" / "Xh ago" formatter.
fn format_age(secs: i64) -> String {
    if secs < 60 {
        return format!("{secs}s ago");
    }
    if secs < 3600 {
        return format!("{}m ago", secs / 60);
    }
    format!("{}h ago", secs / 3600)
}

/// Tiny URL-encoding helper. `urlencoding` crate isn't in
/// the workspace and the operator-facing values here are
/// short identifiers — manual escaping is fine.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let is_safe = matches!(
            b,
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
                | b'-' | b'_' | b'.' | b'~'
                | b',' | b'/'
        );
        if is_safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

// Loose deserializers — we accept whatever the bridge sends and
// don't fail on additional fields. Same posture as topology.rs's
// HealthResponse.

#[derive(Debug, Deserialize)]
struct HealthResponse {
    providers: Vec<ProviderStatus>,
    aggregate: Aggregate,
}

#[derive(Debug, Deserialize, Default)]
struct Aggregate {
    #[serde(default)]
    cooldowns_active: u64,
    #[serde(default)]
    quarantined: u64,
    #[serde(default)]
    rate_limit_hits_5min: u64,
    #[serde(default)]
    rate_limit_hits_1h: u64,
    #[serde(default)]
    failed_request_count: u64,
    #[serde(default)]
    success_request_count: u64,
}

#[derive(Debug, Deserialize)]
struct ProviderStatus {
    name: String,
    #[serde(default)]
    configured: bool,
    #[serde(default)]
    cooldown_until: Option<i64>,
    #[serde(default)]
    quarantined_at: Option<i64>,
    #[serde(default)]
    last_failure_reason: Option<String>,
    #[serde(default)]
    last_failure_at: Option<i64>,
    #[serde(default)]
    rate_limit_hits_5min: u64,
    #[serde(default)]
    rate_limit_hits_1h: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_typical_health_body() {
        let body = r#"{
            "providers": [
                {
                    "name": "openai",
                    "configured": true,
                    "rate_limit_hits_5min": 2,
                    "rate_limit_hits_1h": 5,
                    "last_failure_reason": "rate-limit",
                    "last_failure_at": 1700000000
                },
                {"name": "anthropic", "configured": false}
            ],
            "aggregate": {
                "cooldowns_active": 1,
                "quarantined": 0,
                "rate_limit_hits_5min": 2,
                "rate_limit_hits_1h": 5,
                "failed_request_count": 9,
                "success_request_count": 41
            }
        }"#;
        let h: HealthResponse = serde_json::from_str(body).unwrap();
        assert_eq!(h.providers.len(), 2);
        assert_eq!(h.providers[0].rate_limit_hits_5min, 2);
        assert_eq!(h.aggregate.cooldowns_active, 1);
        assert_eq!(h.aggregate.failed_request_count, 9);
    }

    #[test]
    fn parse_empty_providers() {
        let body = r#"{"providers":[], "aggregate":{}}"#;
        let h: HealthResponse = serde_json::from_str(body).unwrap();
        assert!(h.providers.is_empty());
        assert_eq!(h.aggregate.cooldowns_active, 0);
    }

    #[test]
    fn parse_events_response() {
        let body = r#"{
            "items": [
                {"task_id": "abc123",
                 "event_id": 5,
                 "event_type": "task.retry_requested",
                 "payload": "raw payload",
                 "summary": "[retry] requested (#2/5)"}
            ],
            "next_cursor": 5
        }"#;
        let r: EventsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.next_cursor, 5);
        assert_eq!(r.items[0].summary, "[retry] requested (#2/5)");
    }

    #[test]
    fn parse_route_test_response() {
        let body = r#"{
            "router": "health-aware",
            "chosen": "openai",
            "reasoning": "health-aware: chose openai (success_ratio=0.95) from 2 eligible of 2 total",
            "chosen_at": 1700000000,
            "candidates": [
                {"name": "openai", "score": 0.95, "eligibility": "eligible", "why": "chosen (healthy success_ratio=0.95)"},
                {"name": "anthropic", "score": 0.42, "eligibility": "eligible", "why": "considered (healthy success_ratio=0.84)"}
            ]
        }"#;
        let r: RouteTestResp = serde_json::from_str(body).unwrap();
        assert_eq!(r.router, "health-aware");
        assert_eq!(r.chosen, "openai");
        assert_eq!(r.candidates.len(), 2);
        assert!((r.candidates[0].score - 0.95).abs() < 1e-3);
        assert_eq!(r.candidates[1].eligibility, "eligible");
    }

    #[test]
    fn parse_stuck_response() {
        let body = r#"{
            "items": [
                {"task_id": "abcd1234abcd1234abcd1234abcd1234",
                 "title": "long-running task",
                 "started_at": 1700000000,
                 "age_secs": 1234}
            ],
            "count": 1,
            "threshold_secs": 300
        }"#;
        let s: StuckResponse = serde_json::from_str(body).unwrap();
        assert_eq!(s.count, 1);
        assert_eq!(s.items.len(), 1);
        assert_eq!(s.items[0].age_secs, 1234);
    }

    #[test]
    fn parse_topology_for_capabilities() {
        // PH-DASH3-CLI: minimal topology body the capabilities
        // subcommand needs. Aliases optional; methods required;
        // freshness propagated.
        let body = r#"{
            "peers": [
                {
                    "alias": "tool",
                    "node_id": "abc",
                    "node_type": "tool",
                    "node_name": "t",
                    "manifest_version": 1,
                    "capability_count": 2,
                    "methods": ["tool.web_fetch", "tool.web_search"],
                    "last_refreshed_at": 1,
                    "last_refreshed_secs_ago": 5,
                    "freshness": "fresh"
                }
            ],
            "generated_at": 0
        }"#;
        let t: TopologyResponse = serde_json::from_str(body).unwrap();
        assert_eq!(t.peers.len(), 1);
        assert_eq!(t.peers[0].methods.len(), 2);
        assert_eq!(t.peers[0].freshness, "fresh");
    }

    // ── W2-006d: dispatch_stats CLI sparkline ──────────────────

    #[test]
    fn ascii_sparkline_empty_returns_dash() {
        assert_eq!(ascii_sparkline(&[]), "-");
    }

    #[test]
    fn ascii_sparkline_flat_renders_low_bars() {
        // All-equal samples normalize to the max bar.
        let s = ascii_sparkline(&[5, 5, 5, 5]);
        assert_eq!(s.chars().count(), 4);
        // Every bar at full height since v == max for all.
        assert!(s.chars().all(|c| c == '█'));
    }

    #[test]
    fn ascii_sparkline_renders_one_char_per_sample() {
        let s = ascii_sparkline(&[1, 10, 5, 20, 8]);
        assert_eq!(s.chars().count(), 5);
        // The peak (20) should map to the tallest bar.
        assert!(s.contains('█'));
    }

    #[test]
    fn format_age_renders_buckets() {
        assert_eq!(format_age(0), "0s ago");
        assert_eq!(format_age(59), "59s ago");
        assert_eq!(format_age(60), "1m ago");
        assert_eq!(format_age(3599), "59m ago");
        assert_eq!(format_age(3600), "1h ago");
        assert_eq!(format_age(36000), "10h ago");
    }

    #[test]
    fn urlencoding_passes_safe_chars() {
        assert_eq!(urlencoding("tool.web_fetch"), "tool.web_fetch");
        assert_eq!(urlencoding("a,b/c-d.e_f~g"), "a,b/c-d.e_f~g");
    }

    #[test]
    fn urlencoding_escapes_specials() {
        assert_eq!(urlencoding(" "), "%20");
        assert_eq!(urlencoding("?"), "%3F");
        assert_eq!(urlencoding("="), "%3D");
        assert_eq!(urlencoding("&"), "%26");
    }

    #[test]
    fn policy_simulate_resp_parses() {
        let body = r#"{
            "peer": "tool",
            "method": "tool.web_fetch",
            "groups": ["chat-users", "operators"],
            "decision": "allow",
            "matched_rule": "web_fetch_chat",
            "reason": "explicit allow"
        }"#;
        let r: PolicySimulateResp = serde_json::from_str(body).unwrap();
        assert_eq!(r.decision, "allow");
        assert_eq!(r.matched_rule.as_deref(), Some("web_fetch_chat"));
        assert_eq!(r.groups.len(), 2);
    }

    #[test]
    fn policy_simulate_resp_handles_missing_rule() {
        let body = r#"{
            "peer": "tool",
            "method": "tool.unknown",
            "groups": [],
            "decision": "deny",
            "matched_rule": null,
            "reason": "default deny"
        }"#;
        let r: PolicySimulateResp = serde_json::from_str(body).unwrap();
        assert_eq!(r.decision, "deny");
        assert!(r.matched_rule.is_none());
    }

    #[test]
    fn port_from_bridge_default() {
        assert_eq!(port_from_bridge("http://127.0.0.1:19791"), 19791);
    }

    #[test]
    fn port_from_bridge_custom() {
        assert_eq!(port_from_bridge("http://localhost:8080"), 8080);
        assert_eq!(port_from_bridge("https://example.com:443"), 443);
    }

    #[test]
    fn port_from_bridge_with_trailing_slash() {
        assert_eq!(port_from_bridge("http://127.0.0.1:19791/"), 19791);
    }

    #[test]
    fn port_from_bridge_falls_back_when_unparseable() {
        // No port → default.
        assert_eq!(port_from_bridge("http://example.com"), 19791);
        // Garbage → default.
        assert_eq!(port_from_bridge("not a url"), 19791);
    }

    #[test]
    fn models_resp_parses() {
        let body = r#"{
            "object": "list",
            "data": [
                {"id":"relix-mock", "object":"model", "created":0,
                 "owned_by":"relix", "description":"mock route"},
                {"id":"relix-openai", "object":"model", "created":0,
                 "owned_by":"relix", "description":"openai route"}
            ]
        }"#;
        let r: ModelsResp = serde_json::from_str(body).unwrap();
        assert_eq!(r.data.len(), 2);
        assert_eq!(r.data[0].id, "relix-mock");
        assert_eq!(r.data[1].description, "openai route");
    }

    #[test]
    fn csv_field_passthrough_for_safe_strings() {
        assert_eq!(csv_field("task.created"), "task.created");
        assert_eq!(csv_field(""), "");
        assert_eq!(csv_field("simple summary"), "simple summary");
    }

    #[test]
    fn csv_field_quotes_commas() {
        assert_eq!(csv_field("a,b,c"), "\"a,b,c\"");
    }

    #[test]
    fn csv_field_doubles_internal_quotes() {
        assert_eq!(csv_field("he said \"hi\""), "\"he said \"\"hi\"\"\"");
    }

    #[test]
    fn csv_field_quotes_newlines_and_crs() {
        assert_eq!(csv_field("line one\nline two"), "\"line one\nline two\"");
        assert_eq!(
            csv_field("line one\r\nline two"),
            "\"line one\r\nline two\""
        );
    }

    #[test]
    fn policy_denials_resp_parses() {
        let body = r#"{
            "peer": "tool",
            "denials": [
                {"at": 1716, "method": "tool.web_fetch",
                 "caller_subject_id": "abcd", "caller_name": "bob",
                 "rule": "default_deny", "reason": "no rule matched"}
            ],
            "count": 1
        }"#;
        let r: PolicyDenialsResp = serde_json::from_str(body).unwrap();
        assert_eq!(r.count, 1);
        assert_eq!(r.denials.len(), 1);
        assert_eq!(r.denials[0].method, "tool.web_fetch");
        assert_eq!(r.denials[0].caller_name, "bob");
    }

    #[test]
    fn dispatch_stats_row_parses_recent_latencies() {
        // Forward-compat: the JSON may or may not include
        // recent_latencies. Both shapes parse.
        let with_field = r#"{
            "method": "tool.web_fetch",
            "invocations": 5, "errors": 0, "denied": 0, "unknown_method": 0,
            "last_invoked_at": 100, "latency_samples": 5,
            "last_elapsed_ms": 10, "max_elapsed_ms": 25, "mean_elapsed_ms": 12,
            "recent_latencies": [10, 15, 12, 25, 8]
        }"#;
        let r: DispatchStatsRow = serde_json::from_str(with_field).unwrap();
        assert_eq!(r.recent_latencies, vec![10, 15, 12, 25, 8]);

        let without_field = r#"{
            "method": "tool.web_fetch",
            "invocations": 5, "errors": 0, "denied": 0, "unknown_method": 0,
            "last_invoked_at": 100, "latency_samples": 5,
            "last_elapsed_ms": 10, "max_elapsed_ms": 25, "mean_elapsed_ms": 12
        }"#;
        let r2: DispatchStatsRow = serde_json::from_str(without_field).unwrap();
        assert!(r2.recent_latencies.is_empty());
    }
}
