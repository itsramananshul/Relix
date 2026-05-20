//! Bridge config + shared `AppState`.
//!
//! Parsed once at startup; the resulting [`AppState`] is cloned into each
//! axum handler. Identity bundle, client key, peers file, and the SOL flow
//! template are all loaded here so the request path stays I/O-free.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_runtime::flow_runner::PeersFile;
use relix_runtime::manifest::{ManifestCache, MeshClient};

use crate::BridgeError;

/// Top-level bridge config (TOML-loaded).
#[derive(Clone, Debug, Deserialize)]
pub struct BridgeConfig {
    pub bridge: BridgeSection,
    pub identity: IdentitySection,
    pub transport: TransportSection,
    pub flow: FlowSection,
    /// Optional OpenAI-compatible shim. Absent ⇒ `/v1/*` routes are 404.
    #[serde(default)]
    pub openai_compat: Option<OpenAiCompatSection>,
    /// Optional SSE settings shared by `/chat/stream` and the streaming
    /// variant of `/v1/chat/completions`.
    #[serde(default)]
    pub sse: SseSection,
    /// Optional Coordinator integration. When present, every chat request
    /// is persisted as a Task on the named peer. When absent or when the
    /// peer is unreachable, the bridge degrades gracefully (warn + skip
    /// persistence; the user's request still executes).
    #[serde(default)]
    pub coordinator: Option<CoordinatorSection>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CoordinatorSection {
    /// Peer alias in `peers.toml` (e.g. `"coordinator"`). Bridge uses
    /// `MeshClient::call(alias, ...)` to send `task.*` calls, so the
    /// reconnect-on-drop behaviour applies for free.
    pub alias: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BridgeSection {
    /// `127.0.0.1:9100` by default. Always loopback in alpha.
    pub listen_addr: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct IdentitySection {
    /// The signed IdentityBundle this bridge presents to mesh responders.
    pub bundle_path: PathBuf,
    /// 32-byte secret used as the local libp2p PeerId AND as the signer for
    /// the per-flow event log records.
    pub client_key_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TransportSection {
    /// Path to the peer alias map (`relix_runtime::flow_runner::PeersFile`).
    pub peers_path: PathBuf,
    /// Per-call deadline. Default 30 s.
    #[serde(default = "default_deadline")]
    pub deadline_secs: i64,
    /// Per-flow event log directory; defaults to `RELIX_DATA_DIR` discovery.
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
}

fn default_deadline() -> i64 {
    30
}

#[derive(Clone, Debug, Deserialize)]
pub struct FlowSection {
    /// Path to the SOL chat template file. Two placeholders are substituted
    /// at request time: `{{SESSION}}` and `{{MESSAGE}}`.
    pub template_path: PathBuf,
    /// Optional second template that adds a `tool.web_fetch` step before the
    /// AI call (M9). Three placeholders: `{{SESSION}}`, `{{MESSAGE}}`,
    /// `{{TOOL_URL}}`. When unset, `/chat_with_tool` is 404 and the OpenAI
    /// shim never auto-routes to it.
    #[serde(default)]
    pub tool_template_path: Option<PathBuf>,
}

/// Bridge-level SSE knobs. See `docs/streaming-and-openai-shim.md`.
#[derive(Clone, Debug, Deserialize)]
pub struct SseSection {
    /// Bytes per SSE chunk when slicing the final reply. Default 32.
    #[serde(default = "default_chunk_bytes")]
    pub chunk_bytes: usize,
    /// Inter-chunk delay in milliseconds, simulating progressive delivery.
    /// Default 25 ms. Set to 0 for an immediate flush.
    #[serde(default = "default_chunk_delay_ms")]
    pub chunk_delay_ms: u64,
}

impl Default for SseSection {
    fn default() -> Self {
        Self {
            chunk_bytes: default_chunk_bytes(),
            chunk_delay_ms: default_chunk_delay_ms(),
        }
    }
}

fn default_chunk_bytes() -> usize {
    32
}

fn default_chunk_delay_ms() -> u64 {
    25
}

/// OpenAI-compatible shim configuration.
///
/// The shim translates `POST /v1/chat/completions` requests into the same
/// SOL chat flow the native `/chat` endpoint uses. Provider keys never live
/// here — provider selection still happens inside the AI node.
#[derive(Clone, Debug, Deserialize)]
pub struct OpenAiCompatSection {
    /// Models advertised by `GET /v1/models`. Each entry maps a client-facing
    /// id (e.g. `relix-mock`) to a free-form description. The bridge does NOT
    /// route based on the chosen id — provider selection is on the AI node.
    /// The list is purely advisory so OpenAI-compatible clients see something
    /// in their model picker.
    #[serde(default)]
    pub models: Vec<OpenAiModelEntry>,
    /// Default model id returned in responses when the client did not supply
    /// one. Empty ⇒ falls back to the first `models` entry, then to `"relix"`.
    #[serde(default)]
    pub default_model: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OpenAiModelEntry {
    pub id: String,
    #[serde(default)]
    pub description: String,
}

/// In-memory app state: validated config + preloaded identity bundle + peers.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<BridgeConfig>,
    pub identity_bundle: Bundle,
    pub client_key: [u8; 32],
    pub peers: PeersFile,
    pub template: String,
    /// Pre-validated tool-flow template when `[flow] tool_template_path` is
    /// set. `None` ⇒ `/chat_with_tool` returns 404 and the OpenAI shim
    /// never auto-routes to the tool flow.
    pub tool_template: Option<String>,
    /// Capability discovery cache populated at bridge startup (M10). Empty
    /// when discovery failed; the bridge stays up and static aliases continue
    /// to work. Read by `/v1/models` and (optionally) by the flow runner's
    /// `capability:` resolver.
    pub manifest_cache: Arc<ManifestCache>,
    /// Long-lived libp2p client with peers pre-dialled. `Some` after a
    /// successful discovery pass; `None` if discovery failed (in which case
    /// FlowRunner falls back to the per-request ephemeral peer path).
    pub mesh_client: Option<Arc<MeshClient>>,
    /// Coordinator integration. `Some` when `[coordinator] alias` is set
    /// in bridge config AND the mesh client is up. Used fail-soft from
    /// flow.rs — every method on `TaskRecorder` warns-and-skips on
    /// failure so chat requests never block on coordinator availability.
    pub task_recorder: Option<crate::task_recorder::TaskRecorder>,
}

impl AppState {
    pub fn try_new(cfg: BridgeConfig) -> Result<Self, BridgeError> {
        let bundle_bytes = std::fs::read(&cfg.identity.bundle_path).map_err(|e| {
            BridgeError::Config(format!(
                "read identity bundle {}: {e}",
                cfg.identity.bundle_path.display()
            ))
        })?;
        let identity_bundle: Bundle = codec::decode(&bundle_bytes)
            .map_err(|e| BridgeError::Config(format!("decode identity bundle: {e}")))?;

        let client_key = load_or_generate_client_key(&cfg.identity.client_key_path)?;

        let peers = PeersFile::from_path(&cfg.transport.peers_path)
            .map_err(|e| BridgeError::Config(format!("peers: {e}")))?;

        let template = std::fs::read_to_string(&cfg.flow.template_path).map_err(|e| {
            BridgeError::Config(format!(
                "read flow template {}: {e}",
                cfg.flow.template_path.display()
            ))
        })?;
        if !template.contains("{{SESSION}}") || !template.contains("{{MESSAGE}}") {
            return Err(BridgeError::Config(
                "flow template must contain {{SESSION}} and {{MESSAGE}} placeholders".to_string(),
            ));
        }

        let tool_template = if let Some(path) = cfg.flow.tool_template_path.as_ref() {
            let text = std::fs::read_to_string(path).map_err(|e| {
                BridgeError::Config(format!("read tool flow template {}: {e}", path.display()))
            })?;
            if !text.contains("{{SESSION}}")
                || !text.contains("{{MESSAGE}}")
                || !text.contains("{{TOOL_URL}}")
            {
                return Err(BridgeError::Config(
                    "tool flow template must contain {{SESSION}}, {{MESSAGE}} and {{TOOL_URL}} placeholders"
                        .to_string(),
                ));
            }
            Some(text)
        } else {
            None
        };

        Ok(Self {
            cfg: Arc::new(cfg),
            identity_bundle,
            client_key,
            peers,
            template,
            tool_template,
            manifest_cache: Arc::new(ManifestCache::new()),
            mesh_client: None,
            task_recorder: None,
        })
    }
}

/// Load the bridge's local libp2p secret from disk, or generate a new one if
/// the file does not exist. Mirrors `controller_runtime::load_or_generate_key`
/// so operators do not have to remember a manual `relix-cli` step before
/// first start. The file is gitignored (`dev-keys/*.key`).
pub fn load_or_generate_client_key(path: &Path) -> Result<[u8; 32], BridgeError> {
    if path.exists() {
        let bytes = std::fs::read(path)
            .map_err(|e| BridgeError::Config(format!("read client key {}: {e}", path.display())))?;
        if bytes.len() != 32 {
            return Err(BridgeError::Config(format!(
                "{}: expected 32-byte secret key, got {}",
                path.display(),
                bytes.len()
            )));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(out)
    } else {
        use rand::RngCore;
        let mut out = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut out);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| BridgeError::Config(format!("mkdir {}: {e}", parent.display())))?;
        }
        std::fs::write(path, out).map_err(|e| {
            BridgeError::Config(format!("write client key {}: {e}", path.display()))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(mut p) = std::fs::metadata(path).map(|m| m.permissions()) {
                p.set_mode(0o600);
                let _ = std::fs::set_permissions(path, p);
            }
        }
        tracing::info!(path = %path.display(), "generated new bridge client key");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_config_parses_minimal() {
        let toml_str = r#"
            [bridge]
            listen_addr = "127.0.0.1:9100"

            [identity]
            bundle_path     = "dev-keys/bridge.aic"
            client_key_path = "dev-keys/bridge.key"

            [transport]
            peers_path    = "configs/peers-chained.toml"
            deadline_secs = 30

            [flow]
            template_path = "flows/chat_template.sol"
        "#;
        let cfg: BridgeConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.bridge.listen_addr, "127.0.0.1:9100");
        assert_eq!(cfg.transport.deadline_secs, 30);
        assert!(cfg.openai_compat.is_none());
        assert_eq!(cfg.sse.chunk_bytes, 32);
    }

    #[test]
    fn bridge_config_parses_openai_compat_and_sse() {
        let toml_str = r#"
            [bridge]
            listen_addr = "127.0.0.1:9100"

            [identity]
            bundle_path     = "dev-keys/bridge.aic"
            client_key_path = "dev-keys/bridge.key"

            [transport]
            peers_path = "configs/peers-chained.toml"

            [flow]
            template_path = "flows/chat_template.sol"

            [sse]
            chunk_bytes    = 16
            chunk_delay_ms = 5

            [openai_compat]
            default_model = "relix-mock"
            [[openai_compat.models]]
            id          = "relix-mock"
            description = "Deterministic mock through the Relix mesh"
            [[openai_compat.models]]
            id          = "relix-anthropic"
        "#;
        let cfg: BridgeConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.sse.chunk_bytes, 16);
        assert_eq!(cfg.sse.chunk_delay_ms, 5);
        let oa = cfg.openai_compat.expect("openai_compat section");
        assert_eq!(oa.default_model, "relix-mock");
        assert_eq!(oa.models.len(), 2);
        assert_eq!(oa.models[0].id, "relix-mock");
    }
}
