//! `relix tool` — surface for tool-node caps that don't fit into
//! the existing per-cap subcommand trees (`fs`, `web`, `browser`).
//!
//! Today: `relix tool screen [--region "x,y,width,height"] [--out <file.png>]`
//! captures the host's screen via the bridge's `/v1/tools/screen`
//! proxy onto `tool.screen`.

use base64::Engine;
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// GAP 10 PART 3: capture the host's screen via `tool.screen`.
    /// Default writes the base64 string to stdout; `--out` saves the
    /// decoded PNG to disk.
    Screen {
        /// Optional region crop in `x,y,width,height` form.
        #[arg(long)]
        region: Option<String>,
        /// Where to save the decoded PNG. When absent, prints the
        /// base64 image to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Screen {
            region,
            out,
            bridge,
            raw,
        } => screen(&bridge, region.as_deref(), out.as_deref(), raw).await,
    }
}

async fn screen(
    bridge: &str,
    region: Option<&str>,
    out: Option<&std::path::Path>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/tools/screen", bridge.trim_end_matches('/'));
    let mut payload = serde_json::Map::new();
    if let Some(r) = region {
        let parts: Vec<&str> = r.split(',').map(|p| p.trim()).collect();
        if parts.len() != 4 {
            return Err(format!(
                "--region expects 'x,y,width,height'; got '{r}' ({} parts)",
                parts.len()
            )
            .into());
        }
        let x: i32 = parts[0].parse().map_err(|e| format!("region x: {e}"))?;
        let y: i32 = parts[1].parse().map_err(|e| format!("region y: {e}"))?;
        let w: u32 = parts[2].parse().map_err(|e| format!("region width: {e}"))?;
        let h: u32 = parts[3]
            .parse()
            .map_err(|e| format!("region height: {e}"))?;
        payload.insert(
            "region".into(),
            serde_json::json!({"x": x, "y": y, "width": w, "height": h}),
        );
    }
    let body = http_post_json(&url, &serde_json::Value::Object(payload)).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("decode tool.screen: {e} (body={body})"))?;
    let backend = v
        .get("backend_used")
        .and_then(|x| x.as_str())
        .unwrap_or("?");
    let w = v.get("width").and_then(|x| x.as_u64()).unwrap_or(0);
    let h = v.get("height").and_then(|x| x.as_u64()).unwrap_or(0);
    let b64 = v
        .get("image_base64")
        .and_then(|x| x.as_str())
        .ok_or("response missing image_base64")?;
    println!("backend_used: {backend}");
    println!("dimensions:   {w}x{h}");
    println!("format:       png");
    println!("base64_bytes: {}", b64.len());
    if let Some(path) = out {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| format!("decode png: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &bytes)?;
        println!("saved:        {} ({} bytes)", path.display(), bytes.len());
    } else {
        println!("\n--- image_base64 ---");
        println!("{b64}");
    }
    Ok(())
}

async fn http_post_json(
    url: &str,
    payload: &serde_json::Value,
) -> Result<String, Box<dyn std::error::Error>> {
    // Tool.screen subprocess can take a few seconds; give the client
    // a comfortable 60s envelope.
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?
        .post(url)
        .json(payload)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}").into());
    }
    Ok(body)
}
