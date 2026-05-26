//! `tool.audio.transcribe` — speech-to-text via Whisper.
//!
//! Two engines today (only one wired per node config):
//!
//! - **`ollama`** — `POST <base_url>/api/transcribe` against a
//!   local Ollama server with a Whisper-class model. The audio
//!   bytes ride the request body as base64 or as a multipart
//!   form depending on the configured route (Ollama's
//!   transcribe surface is still moving — operators select the
//!   wire shape that matches their installed version).
//! - **`whisper_cpp`** — shells out to a `whisper.cpp` binary
//!   on PATH (or at a configured path). Writes the audio bytes
//!   to a tempfile and reads the transcript from stdout.
//!
//! ## Opt-in posture
//!
//! Missing `[tool.audio]` section ⇒ the capability is not
//! registered. The Telegram controller checks for the
//! capability before transcribing voice messages, so absent
//! config = "voice messages stay as a static error reply".
//! Operators who want voice transcription enable the section
//! explicitly.
//!
//! ## Honest scope
//!
//! Neither engine binary ships with Relix. Ollama operators
//! install `ollama pull whisper` separately; whisper.cpp
//! operators build / install the binary themselves. The
//! `engine_available()` probe at startup logs a loud warning
//! when the configured engine isn't reachable so a
//! misconfiguration surfaces immediately instead of on the
//! first call.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use relix_core::capability::{
    CapabilityDescriptor, CapabilityKind, CostClass, Idempotency, RiskLevel,
};
// `description` lives at module scope, not on the descriptor —
// we set it via the inherent field below.

/// `[tool.audio]` section. Opt-in; absent means the capability
/// isn't registered.
#[derive(Clone, Debug, Deserialize)]
pub struct AudioConfig {
    #[serde(default)]
    pub enabled: bool,
    /// `ollama` (default) or `whisper_cpp`.
    #[serde(default = "default_engine")]
    pub engine: String,
    /// Model identifier the engine will load. Defaults to
    /// `whisper` (Ollama's name); for whisper.cpp this is the
    /// model path / size hint passed via `--model`.
    #[serde(default = "default_model")]
    pub model: String,
    /// Ollama base URL. Ignored when `engine = "whisper_cpp"`.
    #[serde(default = "default_ollama_base_url")]
    pub ollama_base_url: String,
    /// Absolute path to the whisper.cpp binary. When empty,
    /// the runtime probes `PATH` for `whisper` / `whisper.cpp`
    /// / `main` (whisper.cpp's default output name).
    /// Ignored when `engine = "ollama"`.
    #[serde(default)]
    pub whisper_cpp_path: Option<PathBuf>,
    /// Per-call deadline. Default 60s.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_engine() -> String {
    "ollama".to_string()
}
fn default_model() -> String {
    "whisper".to_string()
}
fn default_ollama_base_url() -> String {
    "http://localhost:11434".to_string()
}
fn default_timeout_secs() -> u64 {
    60
}

/// Capability descriptor for `tool.audio.transcribe`.
pub fn descriptor_transcribe() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.audio.transcribe");
    d.major_version = 1;
    d.description = Some(
        "Transcribe audio to text via a local Whisper engine (Ollama or whisper.cpp).".to_string(),
    );
    d.kind = CapabilityKind::Unary;
    d.risk_level = RiskLevel::Medium;
    // Whisper transcription is CPU-bound and can take seconds —
    // `Expensive` is the closest documented bucket short of
    // `ExternalPaid` (which we're not — engine is local).
    d.cost_class = CostClass::Expensive;
    d.idempotency = Idempotency::AtMostOnce;
    d.sensitivity_tags = vec!["audio".into(), "transcribe".into(), "external-cpu".into()];
    d.categories = vec!["transform".into(), "audio".into()];
    d
}

/// What `tool.audio.transcribe` returns on success. The wire
/// format is pipe-delimited like the rest of the tool node:
/// `text=<utf8 transcript>`. Failures surface via the standard
/// `ErrorEnvelope` — invalid args / engine unavailable map to
/// `INVALID_ARGS`; transient ollama / whisper hiccups map to
/// `RESPONDER_OVERLOADED`.
#[derive(Clone, Debug)]
pub struct TranscribeResult {
    pub text: String,
}

/// Probe whether the configured engine looks usable at
/// startup. Logs structured tracing lines but does NOT fail
/// startup — operators can fix the engine after boot without
/// restarting Relix.
pub async fn probe_engine_at_startup(cfg: &AudioConfig) {
    if !cfg.enabled {
        return;
    }
    match cfg.engine.as_str() {
        "ollama" => {
            let url = format!("{}/api/tags", cfg.ollama_base_url.trim_end_matches('/'));
            let client = match reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "tool.audio: probe failed to build http client");
                    return;
                }
            };
            match client.get(&url).send().await {
                Ok(r) if r.status().is_success() => {
                    tracing::info!(
                        engine = "ollama",
                        base_url = %cfg.ollama_base_url,
                        "tool.audio: ollama reachable"
                    );
                }
                Ok(r) => {
                    tracing::warn!(
                        engine = "ollama",
                        http.status = r.status().as_u16(),
                        base_url = %cfg.ollama_base_url,
                        "tool.audio: ollama responded non-2xx; transcribe will fail"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        engine = "ollama",
                        base_url = %cfg.ollama_base_url,
                        error = %e,
                        "tool.audio: ollama unreachable; transcribe will fail until ollama \
                         is up. Install Ollama from https://ollama.com and run \
                         `ollama pull whisper`."
                    );
                }
            }
        }
        "whisper_cpp" => {
            let path = resolve_whisper_cpp(cfg);
            match path {
                Some(p) => tracing::info!(
                    engine = "whisper_cpp",
                    path = %p.display(),
                    "tool.audio: whisper.cpp binary found"
                ),
                None => tracing::warn!(
                    engine = "whisper_cpp",
                    "tool.audio: whisper.cpp binary not found on PATH; transcribe will fail. \
                     Build whisper.cpp from https://github.com/ggerganov/whisper.cpp and place \
                     the binary on PATH or set [tool.audio] whisper_cpp_path."
                ),
            }
        }
        other => {
            tracing::error!(
                engine = other,
                "tool.audio: unknown engine; supported: \"ollama\", \"whisper_cpp\""
            );
        }
    }
}

/// Locate a usable whisper.cpp binary. Honours the explicit
/// `whisper_cpp_path` first; falls back to `PATH` discovery
/// for `whisper.cpp` / `whisper` / `main` (the default
/// whisper.cpp build artefact name).
pub fn resolve_whisper_cpp(cfg: &AudioConfig) -> Option<PathBuf> {
    if let Some(p) = &cfg.whisper_cpp_path {
        if p.is_file() {
            return Some(p.clone());
        }
        return None;
    }
    let candidates = [
        "whisper.cpp",
        "whisper-cpp",
        "whisper",
        if cfg!(windows) { "main.exe" } else { "main" },
    ];
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in candidates {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Render a clean "engine not available" error message for
/// operator-facing surfaces. Used by the handler when the
/// configured engine fails to respond.
pub fn engine_unavailable_message(cfg: &AudioConfig) -> String {
    match cfg.engine.as_str() {
        "ollama" => format!(
            "tool.audio: ollama at {} is unreachable. Install Ollama from \
             https://ollama.com and run `ollama pull whisper`, then retry.",
            cfg.ollama_base_url
        ),
        "whisper_cpp" => "tool.audio: whisper.cpp binary not found. Build it from \
             https://github.com/ggerganov/whisper.cpp and place the binary on PATH, or set \
             [tool.audio] whisper_cpp_path."
            .into(),
        other => format!("tool.audio: unknown engine `{other}` (supported: ollama, whisper_cpp)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_carries_documented_metadata() {
        let d = descriptor_transcribe();
        assert_eq!(d.method_name, "tool.audio.transcribe");
        assert_eq!(d.major_version, 1);
        assert!(matches!(d.kind, CapabilityKind::Unary));
        assert!(matches!(d.risk_level, RiskLevel::Medium));
        assert!(matches!(d.cost_class, CostClass::Expensive));
        assert!(d.sensitivity_tags.iter().any(|t| t == "audio"));
        assert!(d.categories.iter().any(|c| c == "audio"));
    }

    #[test]
    fn default_engine_and_model_match_docs() {
        let cfg = AudioConfig {
            enabled: true,
            engine: default_engine(),
            model: default_model(),
            ollama_base_url: default_ollama_base_url(),
            whisper_cpp_path: None,
            timeout_secs: default_timeout_secs(),
        };
        assert_eq!(cfg.engine, "ollama");
        assert_eq!(cfg.model, "whisper");
        assert_eq!(cfg.ollama_base_url, "http://localhost:11434");
        assert_eq!(cfg.timeout_secs, 60);
    }

    #[test]
    fn audio_config_parses_from_minimal_toml() {
        let s = r#"
            enabled = true
        "#;
        let cfg: AudioConfig = toml::from_str(s).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.engine, "ollama");
        assert_eq!(cfg.model, "whisper");
    }

    #[test]
    fn resolve_whisper_cpp_returns_none_when_explicit_path_missing() {
        let cfg = AudioConfig {
            enabled: true,
            engine: "whisper_cpp".into(),
            model: "base".into(),
            ollama_base_url: default_ollama_base_url(),
            whisper_cpp_path: Some(PathBuf::from("/definitely/not/here/whisper")),
            timeout_secs: 60,
        };
        assert!(resolve_whisper_cpp(&cfg).is_none());
    }

    #[test]
    fn engine_unavailable_message_for_each_engine() {
        let mut cfg = AudioConfig {
            enabled: true,
            engine: "ollama".into(),
            model: "whisper".into(),
            ollama_base_url: "http://localhost:11434".into(),
            whisper_cpp_path: None,
            timeout_secs: 60,
        };
        let m = engine_unavailable_message(&cfg);
        assert!(m.contains("ollama"));
        assert!(m.contains("11434"));

        cfg.engine = "whisper_cpp".into();
        let m = engine_unavailable_message(&cfg);
        assert!(m.contains("whisper.cpp"));

        cfg.engine = "garbage".into();
        let m = engine_unavailable_message(&cfg);
        assert!(m.contains("unknown engine"));
    }
}
