//! Controller runtime — what `relix-controller`'s `main()` calls to spin up a
//! node. Loads identity + policy, builds dispatch bridge, starts libp2p,
//! registers built-in `node.health` capability, dispatches inbound RPCs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::Deserialize;
use tokio::sync::mpsc;

use relix_core::policy::PolicyEngine;
use relix_core::types::NodeId;

use crate::dispatch::{DispatchBridge, FnHandler, Handler, HandlerOutcome, InvocationCtx};
use crate::transport::rpc::{self, Event as TransportEvent, Multiaddr};

/// Controller config (per-binary). Matches the TOML in `configs/`.
#[derive(Clone, Debug, Deserialize)]
pub struct ControllerConfig {
    /// `[controller]` section.
    pub controller: ControllerSection,
    /// `[identity]`.
    pub identity: IdentitySection,
    /// `[trust]`.
    pub trust: TrustSection,
    /// `[policy]`.
    pub policy: PolicySection,
    /// Optional per-node sections (memory/ai/tool/bridge). The runtime ignores
    /// unknown sections so each node-type's main can read its own typed view.
    #[serde(default)]
    #[allow(dead_code)]
    pub memory: Option<toml::Value>,
    /// AI node options.
    #[serde(default)]
    #[allow(dead_code)]
    pub ai: Option<toml::Value>,
    /// Tool node options.
    #[serde(default)]
    #[allow(dead_code)]
    pub tool: Option<toml::Value>,
    /// Web-bridge node options.
    #[serde(default)]
    #[allow(dead_code)]
    pub bridge: Option<toml::Value>,
    /// `[peers]` — alias → endpoint info.
    #[serde(default)]
    pub peers: std::collections::BTreeMap<String, PeerConfig>,
    /// SOL session declarations (M6).
    #[serde(default)]
    #[allow(dead_code)]
    pub session: std::collections::BTreeMap<String, SessionConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ControllerSection {
    pub name: String,
    pub node_type: String,
    pub listen_port: u16,
}

#[derive(Clone, Debug, Deserialize)]
pub struct IdentitySection {
    pub key_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TrustSection {
    pub org_root_key_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PolicySection {
    pub file: PathBuf,
}

/// One peer alias.
#[derive(Clone, Debug, Deserialize)]
pub struct PeerConfig {
    /// libp2p TCP port to dial.
    pub port: u16,
}

/// SOL session declaration. Used by M6 once the SOL VM is wired.
#[derive(Clone, Debug, Deserialize)]
pub struct SessionConfig {
    /// Path to the `.sol` source.
    pub source: String,
}

/// What `relix-controller::main` calls. Returns when the runtime exits
/// (transport drops, or fatal error).
pub async fn run(config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(config_path)?;
    let cfg: ControllerConfig = toml::from_str(&text)?;

    tracing::info!(
        node = %cfg.controller.name,
        node_type = %cfg.controller.node_type,
        port = cfg.controller.listen_port,
        "controller starting"
    );

    // Identity: load or generate the node's signing key.
    let node_signer = load_or_generate_key(&cfg.identity.key_path)?;
    let node_id = NodeId::from_pubkey(&node_signer.verifying_key().to_bytes());
    tracing::info!(node_id = %node_id, "node identity loaded");

    // Trust root: load org-root public key (32 raw bytes).
    let trust_root = load_pubkey(&cfg.trust.org_root_key_path)?;

    // Policy.
    let policy = if cfg.policy.file.exists() {
        PolicyEngine::from_path(&cfg.policy.file)?
    } else {
        tracing::warn!(
            policy_file = %cfg.policy.file.display(),
            "policy file missing — using permissive engine (alpha dev only)"
        );
        PolicyEngine::permissive()
    };
    if policy.is_permissive() {
        tracing::warn!("PERMISSIVE policy in effect — default-deny still applies per-method");
    }

    // Audit log (per node). Default `~/.relix/<node>/audit.log`.
    let data_dir = data_dir_for(&cfg.controller.name)?;
    std::fs::create_dir_all(&data_dir)?;
    let audit_path = data_dir.join("audit.log");

    // Dispatch bridge.
    let mut bridge = DispatchBridge::new(policy, trust_root, &audit_path, node_signer.clone())?;
    register_builtins(&mut bridge);

    let bridge = Arc::new(bridge);

    // Transport.
    let (client, mut events, event_loop) =
        rpc::new(node_signer.to_bytes(), cfg.controller.listen_port).await?;
    tokio::spawn(event_loop.run());

    // Dial configured peers.
    for (alias, peer_cfg) in &cfg.peers {
        let addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{}", peer_cfg.port).parse()?;
        match client.dial(addr.clone()).await {
            Ok(()) => tracing::info!(alias = %alias, addr = %addr, "dialed peer"),
            Err(e) => {
                tracing::warn!(alias = %alias, addr = %addr, error = %e, "dial failed (will retry on demand)")
            }
        }
    }
    client.bootstrap_kademlia().await;

    tracing::info!("controller online; awaiting inbound RPCs");

    // Inbound event loop.
    let bridge_for_loop = bridge.clone();
    while let Some(event) = events.recv().await {
        match event {
            TransportEvent::Request {
                envelope,
                from,
                respond,
            } => {
                let bridge = bridge_for_loop.clone();
                tokio::spawn(async move {
                    let resp = bridge.handle_inbound(envelope).await;
                    respond.respond(resp).await;
                    let _ = from; // peer id available for future audit metadata
                });
            }
            TransportEvent::PeerConnected { peer_id, address } => {
                tracing::info!(peer = %peer_id, addr = %address, "peer connected");
            }
        }
    }

    Ok(())
}

/// Register capabilities every node serves by default.
fn register_builtins(bridge: &mut DispatchBridge) {
    bridge.register(
        "node.health",
        Arc::new(FnHandler(|_ctx: InvocationCtx| async move {
            HandlerOutcome::Ok(b"ok".to_vec())
        })),
    );
    // node.manifest, ping, etc. land in M6 once registry is wired.
}

/// Hook for node-specific modules to register their capabilities. Called by
/// node-type entry points if added in a future revision; the current controller
/// binary registers built-ins only.
#[allow(dead_code)]
pub fn extend_with_handler(
    _bridge: &mut DispatchBridge,
    _method: &str,
    _handler: Arc<dyn Handler>,
) {
    // Placeholder for the next milestone — keeps the public surface visible.
}

fn load_or_generate_key(path: &Path) -> Result<SigningKey, Box<dyn std::error::Error>> {
    if path.exists() {
        let bytes = std::fs::read(path)?;
        if bytes.len() != 32 {
            return Err(format!(
                "{}: expected 32-byte secret key, got {}",
                path.display(),
                bytes.len()
            )
            .into());
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(SigningKey::from_bytes(&arr))
    } else {
        let key = SigningKey::generate(&mut rand::rngs::OsRng);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, key.to_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(path)?.permissions();
            p.set_mode(0o600);
            std::fs::set_permissions(path, p)?;
        }
        tracing::info!(path = %path.display(), "generated new node identity key");
        Ok(key)
    }
}

fn load_pubkey(path: &Path) -> Result<VerifyingKey, Box<dyn std::error::Error>> {
    // Accept either a 32-byte pubkey file OR a 32-byte SECRET key (and derive pubkey).
    // Alpha shortcut: org-root files generated by `relix-cli identity init-org` write
    // the SECRET key; the controller derives the pubkey for trust-root use.
    let bytes = std::fs::read(path)?;
    if bytes.len() == 32 {
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        // Try as pubkey first; if that fails, treat as secret key and derive pubkey.
        match VerifyingKey::from_bytes(&arr) {
            Ok(vk) => Ok(vk),
            Err(_) => {
                let sk = SigningKey::from_bytes(&arr);
                Ok(sk.verifying_key())
            }
        }
    } else if bytes.len() == 64 {
        // Allow concatenated secret||public for convenience.
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes[32..]);
        Ok(VerifyingKey::from_bytes(&arr)?)
    } else {
        Err(format!(
            "{}: expected 32 or 64 bytes for pubkey, got {}",
            path.display(),
            bytes.len()
        )
        .into())
    }
}

fn data_dir_for(node_name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let base = std::env::var("RELIX_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".relix")
        });
    Ok(base.join(node_name))
}

// Re-export for the controller binary main.
pub use crate::transport::rpc::Client as TransportClient;

// Channel type needed by some downstream uses; suppress unused-warning otherwise.
#[allow(dead_code)]
type _UnusedReceiver = mpsc::Receiver<TransportEvent>;
