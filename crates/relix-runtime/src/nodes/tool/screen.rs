//! `tool.screen` — cross-platform screen capture.
//!
//! Captures a screenshot of the host's screen and returns it as a
//! base64-encoded PNG. Backends:
//!
//! - **Linux** — `scrot` (preferred) → `import` (ImageMagick fallback).
//!   Returns a clear actionable error when neither is on PATH.
//! - **macOS** — `screencapture` (always present at /usr/sbin).
//! - **Windows** — PowerShell + System.Windows.Forms / System.Drawing.
//!
//! Operators must explicitly enable the cap via `[tool.screen]
//! enabled = true` because it captures the host's screen — opt-in
//! by design.
//!
//! ## Wire format
//!
//! Input args (JSON):
//!
//! ```json
//! { "region": { "x": 0, "y": 0, "width": 1920, "height": 1080 } }
//! ```
//!
//! `region` is optional — absent means full-screen. When present the
//! geometry is forwarded to the backend (`scrot --geometry x,y,w,h`,
//! `screencapture -R x,y,w,h`, or a `Bitmap` of the requested size on
//! Windows).
//!
//! Output (JSON):
//!
//! ```json
//! {
//!     "image_base64": "...",
//!     "width": 1920,
//!     "height": 1080,
//!     "format": "png",
//!     "backend_used": "scrot" | "imagemagick" | "screencapture" | "powershell"
//! }
//! ```

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

/// `[tool.screen]` config block.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ScreenConfig {
    /// Master switch. `false` (the default) makes the cap return a
    /// clear disabled error.
    #[serde(default)]
    pub enabled: bool,
    /// Optional override for the temp-file directory. When `None`
    /// uses `std::env::temp_dir()`.
    #[serde(default)]
    pub temp_dir: Option<PathBuf>,
    /// Per-call deadline for the capture subprocess, in seconds.
    /// Default 15.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            temp_dir: None,
            timeout_secs: default_timeout_secs(),
        }
    }
}

fn default_timeout_secs() -> u64 {
    15
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Default, Deserialize)]
pub struct ScreenRequest {
    #[serde(default)]
    pub region: Option<Region>,
}

/// Wire-shaped response from `tool.screen`.
///
/// SEC PART 1: `image_base64` carries pixel data captured
/// from the host display — its contents can be controlled by
/// whatever the user has on screen. The current handler does
/// NOT run OCR (there is no in-tree OCR pipeline), but any
/// downstream consumer that feeds these bytes (or OCR-derived
/// text from them) into an LLM prompt MUST wrap that text via
/// `relix_core::types::UntrustedText::new(text).wrap_for_prompt()`
/// or route it through `ai.perception_extract` for the
/// two-stage isolation primitive. The boundary is enforced at
/// prompt-construction time, not at the wire layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScreenResponse {
    pub image_base64: String,
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub backend_used: String,
}

#[derive(Error, Debug)]
pub enum ScreenError {
    #[error("tool.screen disabled (set [tool.screen] enabled = true)")]
    Disabled,
    #[error("invalid args: {0}")]
    InvalidArgs(String),
    #[error(
        "screen capture on Linux requires scrot (apt install scrot) \
         or ImageMagick (apt install imagemagick); install one and retry"
    )]
    NoLinuxBackend,
    #[error("screen capture backend ({backend}) failed: {cause}")]
    BackendFailed { backend: String, cause: String },
    #[error("screen capture subprocess timed out after {0}s")]
    Timeout(u64),
    #[error("screen capture: unsupported platform '{0}'")]
    UnsupportedPlatform(&'static str),
    #[error("screen capture: temp file io: {0}")]
    Io(String),
}

/// Register `tool.screen` onto `bridge` when [`ScreenConfig::enabled`]
/// is `true`. When disabled the cap is still registered so the
/// caller gets a clear, structured error rather than `UNKNOWN_METHOD`.
pub fn register(bridge: &mut DispatchBridge, cfg: Arc<ScreenConfig>) {
    bridge.register(
        "tool.screen",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let cfg = cfg.clone();
            async move {
                match handle_screen(&cfg, &ctx).await {
                    Ok(resp) => match serde_json::to_vec(&resp) {
                        Ok(b) => HandlerOutcome::Ok(b),
                        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::RESPONDER_INTERNAL,
                            cause: format!("tool.screen encode: {e}"),
                            retry_hint: 0,
                            retry_after: None,
                        }),
                    },
                    Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                        kind: match e {
                            ScreenError::Disabled | ScreenError::InvalidArgs(_) => {
                                error_kinds::INVALID_ARGS
                            }
                            _ => error_kinds::RESPONDER_INTERNAL,
                        },
                        cause: format!("tool.screen: {e}"),
                        retry_hint: 0,
                        retry_after: None,
                    }),
                }
            }
        })),
    );
}

/// Pure-function handler used by both the bridge wrapper and unit
/// tests. Returns a [`ScreenResponse`] on success or [`ScreenError`]
/// otherwise.
pub async fn handle_screen(
    cfg: &ScreenConfig,
    ctx: &InvocationCtx,
) -> Result<ScreenResponse, ScreenError> {
    if !cfg.enabled {
        return Err(ScreenError::Disabled);
    }
    let req: ScreenRequest = if ctx.args.is_empty() {
        ScreenRequest::default()
    } else {
        serde_json::from_slice(&ctx.args)
            .map_err(|e| ScreenError::InvalidArgs(format!("decode JSON: {e}")))?
    };
    capture(cfg, req.region.as_ref()).await
}

async fn capture(
    cfg: &ScreenConfig,
    region: Option<&Region>,
) -> Result<ScreenResponse, ScreenError> {
    let path = temp_png_path(cfg)?;
    let timeout = Duration::from_secs(cfg.timeout_secs.max(3));
    let result = capture_platform(&path, region, timeout).await;
    let (backend, bytes) = match result {
        Ok((b, bytes)) => (b, bytes),
        Err(e) => {
            // Best-effort cleanup even on failure paths.
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
    };
    let _ = std::fs::remove_file(&path);
    let (w, h) = png_dimensions(&bytes)
        .unwrap_or_else(|| region.map(|r| (r.width, r.height)).unwrap_or((0, 0)));
    let image_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(ScreenResponse {
        image_base64,
        width: w,
        height: h,
        format: "png".into(),
        backend_used: backend,
    })
}

fn temp_png_path(cfg: &ScreenConfig) -> Result<PathBuf, ScreenError> {
    let dir = cfg.temp_dir.clone().unwrap_or_else(std::env::temp_dir);
    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(|e| ScreenError::Io(e.to_string()))?;
    }
    let unique = format!("relix-screen-{}-{}.png", std::process::id(), rand_hex_8(),);
    Ok(dir.join(unique))
}

fn rand_hex_8() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: [u8; 4] = rng.r#gen();
    hex::encode(bytes)
}

// ── Backend impls ─────────────────────────────────────────────

#[cfg(target_os = "linux")]
async fn capture_platform(
    path: &std::path::Path,
    region: Option<&Region>,
    timeout: Duration,
) -> Result<(String, Vec<u8>), ScreenError> {
    capture_linux(path, region, timeout).await
}

#[cfg(target_os = "macos")]
async fn capture_platform(
    path: &std::path::Path,
    region: Option<&Region>,
    timeout: Duration,
) -> Result<(String, Vec<u8>), ScreenError> {
    capture_macos(path, region, timeout).await
}

#[cfg(target_os = "windows")]
async fn capture_platform(
    path: &std::path::Path,
    region: Option<&Region>,
    timeout: Duration,
) -> Result<(String, Vec<u8>), ScreenError> {
    capture_windows(path, region, timeout).await
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn capture_platform(
    _path: &std::path::Path,
    _region: Option<&Region>,
    _timeout: Duration,
) -> Result<(String, Vec<u8>), ScreenError> {
    Err(ScreenError::UnsupportedPlatform(std::env::consts::OS))
}

#[cfg(target_os = "linux")]
async fn capture_linux(
    path: &std::path::Path,
    region: Option<&Region>,
    timeout: Duration,
) -> Result<(String, Vec<u8>), ScreenError> {
    if which_exists("scrot").await {
        let mut cmd = Command::new("scrot");
        cmd.arg("--silent").arg("--quality").arg("90");
        if let Some(r) = region {
            cmd.arg("--autoselect")
                .arg(format!("{},{},{},{}", r.x, r.y, r.width, r.height));
        }
        cmd.arg(path);
        run_capture_command(cmd, "scrot", path, timeout).await
    } else if which_exists("import").await {
        let mut cmd = Command::new("import");
        if let Some(r) = region {
            cmd.arg("-window")
                .arg("root")
                .arg("-crop")
                .arg(format!("{}x{}+{}+{}", r.width, r.height, r.x, r.y));
        } else {
            cmd.arg("-window").arg("root");
        }
        cmd.arg(path);
        run_capture_command(cmd, "imagemagick", path, timeout).await
    } else {
        Err(ScreenError::NoLinuxBackend)
    }
}

#[cfg(target_os = "macos")]
async fn capture_macos(
    path: &std::path::Path,
    region: Option<&Region>,
    timeout: Duration,
) -> Result<(String, Vec<u8>), ScreenError> {
    let mut cmd = Command::new("/usr/sbin/screencapture");
    cmd.arg("-x").arg("-t").arg("png");
    if let Some(r) = region {
        cmd.arg("-R")
            .arg(format!("{},{},{},{}", r.x, r.y, r.width, r.height));
    }
    cmd.arg(path);
    run_capture_command(cmd, "screencapture", path, timeout).await
}

#[cfg(target_os = "windows")]
async fn capture_windows(
    path: &std::path::Path,
    region: Option<&Region>,
    timeout: Duration,
) -> Result<(String, Vec<u8>), ScreenError> {
    let path_str = path
        .to_str()
        .ok_or_else(|| ScreenError::Io("temp path not utf-8".into()))?;
    // Single-quoted PowerShell script; replace any single quotes in
    // the path with two single quotes (PS escape rule).
    let escaped_path = path_str.replace('\'', "''");
    let capture_block: String = if let Some(r) = region {
        format!(
            "$bitmap = New-Object System.Drawing.Bitmap({width}, {height}); \
             $graphics = [System.Drawing.Graphics]::FromImage($bitmap); \
             $graphics.CopyFromScreen({x}, {y}, 0, 0, \
             (New-Object System.Drawing.Size({width}, {height})));",
            width = r.width,
            height = r.height,
            x = r.x,
            y = r.y,
        )
    } else {
        "$screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds; \
         $bitmap = New-Object System.Drawing.Bitmap($screen.Width, $screen.Height); \
         $graphics = [System.Drawing.Graphics]::FromImage($bitmap); \
         $graphics.CopyFromScreen($screen.Location, \
         [System.Drawing.Point]::Empty, $screen.Size);"
            .to_string()
    };
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; \
         Add-Type -AssemblyName System.Drawing; \
         {capture_block} \
         $bitmap.Save('{escaped_path}', [System.Drawing.Imaging.ImageFormat]::Png); \
         $bitmap.Dispose(); $graphics.Dispose();",
    );
    let mut cmd = Command::new("powershell");
    cmd.arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(script);
    run_capture_command(cmd, "powershell", path, timeout).await
}

// ── Helpers ───────────────────────────────────────────────────

async fn run_capture_command(
    mut cmd: Command,
    backend: &str,
    path: &std::path::Path,
    timeout: Duration,
) -> Result<(String, Vec<u8>), ScreenError> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn().map_err(|e| ScreenError::BackendFailed {
        backend: backend.into(),
        cause: format!("spawn: {e}"),
    })?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| ScreenError::Timeout(timeout.as_secs()))?
        .map_err(|e| ScreenError::BackendFailed {
            backend: backend.into(),
            cause: format!("wait: {e}"),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(ScreenError::BackendFailed {
            backend: backend.into(),
            cause: format!("exit={:?} stderr={stderr}", output.status.code()),
        });
    }
    let bytes = std::fs::read(path)
        .map_err(|e| ScreenError::Io(format!("read {path}: {e}", path = path.display())))?;
    Ok((backend.to_string(), bytes))
}

// Only the Linux capture path probes for tools; macOS calls screencapture
// directly, so this would be dead code there.
#[cfg(target_os = "linux")]
async fn which_exists(name: &str) -> bool {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(format!("command -v {name}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.status().await.map(|s| s.success()).unwrap_or(false)
}

/// Parse the PNG IHDR chunk to extract image dimensions. Returns
/// `None` when the input isn't a valid PNG. Used to populate the
/// response's `width` + `height` honestly — backends like
/// `screencapture` and `scrot` capture whatever the display reports,
/// which may differ from the operator-supplied region.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // PNG signature: 8 bytes, then a 4-byte length, then "IHDR".
    // After IHDR comes 4 bytes width + 4 bytes height (big-endian).
    if bytes.len() < 24 {
        return None;
    }
    if &bytes[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    if &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};

    fn ctx_for(body: &[u8]) -> InvocationCtx {
        InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"alice"),
                name: "alice".into(),
                org_id: NodeId::from_pubkey(b"org"),
                groups: vec!["chat-users".into()],
                role: "agent".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: body.to_vec(),
            tenant_id: None,
        }
    }

    #[tokio::test]
    async fn disabled_returns_clear_error() {
        let cfg = ScreenConfig::default();
        let err = handle_screen(&cfg, &ctx_for(b"")).await.unwrap_err();
        assert!(matches!(err, ScreenError::Disabled));
    }

    #[tokio::test]
    async fn invalid_json_rejects() {
        let cfg = ScreenConfig {
            enabled: true,
            ..ScreenConfig::default()
        };
        let err = handle_screen(&cfg, &ctx_for(b"not json"))
            .await
            .unwrap_err();
        assert!(matches!(err, ScreenError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn enabled_either_succeeds_or_returns_clear_unavailable_error() {
        // On the current test platform we either have a working backend
        // (real CI box with scrot/screencapture/PowerShell) or we don't
        // (headless container without DISPLAY). The contract: never
        // panic; always either Ok(valid PNG) or a clear, structured
        // error mentioning the missing piece.
        let cfg = ScreenConfig {
            enabled: true,
            timeout_secs: 5,
            ..ScreenConfig::default()
        };
        match handle_screen(&cfg, &ctx_for(b"")).await {
            Ok(resp) => {
                assert_eq!(resp.format, "png");
                assert!(!resp.image_base64.is_empty());
                assert!(!resp.backend_used.is_empty());
            }
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    matches!(
                        e,
                        ScreenError::NoLinuxBackend
                            | ScreenError::BackendFailed { .. }
                            | ScreenError::UnsupportedPlatform(_)
                            | ScreenError::Timeout(_)
                            | ScreenError::Io(_)
                    ),
                    "unexpected error shape: {msg}",
                );
            }
        }
    }

    #[tokio::test]
    async fn region_arg_is_forwarded_into_request_decode() {
        let body = serde_json::to_vec(&serde_json::json!({
            "region": { "x": 10, "y": 20, "width": 100, "height": 50 }
        }))
        .unwrap();
        let req: ScreenRequest = serde_json::from_slice(&body).unwrap();
        let r = req.region.unwrap();
        assert_eq!(r.x, 10);
        assert_eq!(r.y, 20);
        assert_eq!(r.width, 100);
        assert_eq!(r.height, 50);
    }

    #[test]
    fn png_dimensions_reads_ihdr() {
        // 1x1 PNG signature + IHDR + width=1, height=1.
        let mut bytes = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&[0u8, 0u8, 0u8, 13]); // length
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]); // bit depth + colour type + ...
        let dims = png_dimensions(&bytes);
        assert_eq!(dims, Some((1, 1)));
    }

    #[test]
    fn png_dimensions_rejects_non_png() {
        let bytes = vec![0u8; 100];
        assert!(png_dimensions(&bytes).is_none());
    }

    #[test]
    fn screen_response_round_trips_through_serde() {
        let r = ScreenResponse {
            image_base64: "aGk=".into(),
            width: 100,
            height: 50,
            format: "png".into(),
            backend_used: "scrot".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: ScreenResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }
}
