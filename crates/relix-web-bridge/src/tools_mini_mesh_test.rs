//! RELA-24 mini-mesh integration test for the tool-registry surface.
//!
//! Boots a real libp2p peer that serves a signed `node.manifest`
//! with `node_type = "tool"` and the real tool capability set
//! (`relix_runtime::nodes::tool::advertised_capabilities`). The
//! bridge runs the same `discover_and_pin` pass `main::main`
//! runs at startup, builds the tool registry via the production
//! `crate::tools::registry_from_manifest`, mounts the real
//! `/v1/tools`, `/v1/tools/search`, and `/v1/tools/manifest`
//! routes, serves them over a real TCP listener, and asserts the
//! HTTP responses carry the discovered tools.
//!
//! This is the end-to-end proof that the registry is no longer a
//! dead `empty_registry()` default: real boot, real discovery,
//! real HTTP, real tools.

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_core::identity::{IdentityBundle, issue_identity};
use relix_core::policy::PolicyEngine;
use relix_core::types::NodeId;
use relix_runtime::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use relix_runtime::manifest::ManifestProvider;
use relix_runtime::nodes::tool::{ToolConfig, advertised_capabilities};
use relix_runtime::transport::rpc::{self, Event, Multiaddr};
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::config::{
    AppState, BridgeConfig, BridgeSection, FlowSection, IdentitySection, MeshSection, SseSection,
    TransportSection,
};

// ── helpers ──────────────────────────────────────────────

fn key_for(seed: u8) -> [u8; 32] {
    let mut k = [0u8; 32];
    for (i, slot) in k.iter_mut().enumerate() {
        *slot = seed.wrapping_add(i as u8);
    }
    k
}

async fn boot_peer(seed: u8) -> (rpc::Client, mpsc::Receiver<Event>, Multiaddr) {
    for _ in 0..16 {
        let port: u16 = 36_000 + (rand::random::<u16>() % 20_000);
        match rpc::new(key_for(seed), port).await {
            Ok((client, events, event_loop)) => {
                let listen_addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}")
                    .parse()
                    .expect("valid multiaddr");
                tokio::spawn(event_loop.run());
                return (client, events, listen_addr);
            }
            Err(e) => {
                eprintln!("tools-mini-mesh: boot_peer retry ({e})");
                continue;
            }
        }
    }
    panic!("boot_peer: exhausted port retries");
}

/// Real tool-node responder: a `DispatchBridge` whose policy
/// admits `node.manifest`, plus a signed `ManifestProvider`
/// advertising `node_type = "tool"` and the real tool caps.
fn fresh_tool_responder() -> (DispatchBridge, ManifestProvider, SigningKey, TempDir) {
    let dir = TempDir::new().unwrap();
    let org_root = SigningKey::generate(&mut OsRng);
    let responder = SigningKey::generate(&mut OsRng);
    let policy = PolicyEngine::from_toml(
        r#"
        [[rules]]
        name = "allow_node_manifest"
        method = "node.manifest"
        allow_groups = ["agents"]
        "#,
    )
    .expect("policy parses");
    let bridge = DispatchBridge::new(
        policy,
        org_root.verifying_key(),
        &dir.path().join("tools-audit.log"),
        responder,
    )
    .expect("bridge constructs");

    // Signed manifest provider advertising the real tool caps.
    // The signer is an independent key — discovery TOFU-pins
    // whatever key signs the first manifest, so it need not
    // match the transport key.
    let manifest_signer = SigningKey::generate(&mut OsRng);
    let provider = ManifestProvider::new(
        NodeId::from_pubkey(&key_for(91)),
        "tool-node",
        "tool",
        NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
        vec![],
    )
    .with_signer(manifest_signer);
    for cap in advertised_capabilities(&ToolConfig::default()) {
        provider.add_capability(cap);
    }
    provider.add_capability(relix_core::capability::CapabilityDescriptor::unary(
        "node.manifest",
    ));

    (bridge, provider, org_root, dir)
}

/// Register the production-shaped `node.manifest` handler that
/// returns the signed snapshot, mirroring `controller_runtime`.
fn register_manifest(dispatch: &mut DispatchBridge, provider: ManifestProvider) {
    dispatch.register(
        "node.manifest",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| {
            let provider = provider.clone();
            async move {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
                    .unwrap_or(0);
                match provider.signed_snapshot(now_ms) {
                    Ok(signed) => match codec::encode(&signed) {
                        Ok(bytes) => HandlerOutcome::Ok(bytes),
                        Err(e) => HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                            kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                            cause: format!("node.manifest encode: {e}"),
                            retry_hint: 1,
                            retry_after: None,
                        }),
                    },
                    Err(e) => HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                        cause: format!("node.manifest sign: {e}"),
                        retry_hint: 1,
                        retry_after: None,
                    }),
                }
            }
        })),
    );
}

fn mint_bridge_bundle_bytes(org_root: &SigningKey, name: &str) -> Vec<u8> {
    let caller_key = SigningKey::generate(&mut OsRng);
    let id = IdentityBundle {
        subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
        name: name.into(),
        org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
        groups: vec!["agents".into()],
        role: "agent".into(),
        clearance: "internal".into(),
        supervisors: vec![],
    };
    let bundle: Bundle = issue_identity(id, org_root, 3600).expect("identity issued");
    codec::encode(&bundle).expect("encode bundle")
}

fn spawn_inbound_loop(mut events: mpsc::Receiver<Event>, bridge: Arc<DispatchBridge>) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if let Event::Request {
                envelope, respond, ..
            } = event
            {
                let bridge = bridge.clone();
                tokio::spawn(async move {
                    let reply = bridge.handle_inbound(envelope).await;
                    respond.respond(reply).await;
                });
            }
        }
    });
}

/// Build an `AppState` whose tool registry was populated by a
/// real discovery pass against the tool peer — the same path
/// `main::main` walks (`discover_and_pin` →
/// `registry_from_manifest`).
async fn build_state(org_root: &SigningKey, addr: Multiaddr, tmpdir: &TempDir) -> AppState {
    let bundle_bytes = mint_bridge_bundle_bytes(org_root, "tools-test-bridge");
    let bundle_path = tmpdir.path().join("bridge.bundle");
    std::fs::write(&bundle_path, &bundle_bytes).unwrap();
    let client_key_path = tmpdir.path().join("client.key");
    let chat_template_path = tmpdir.path().join("chat.sol");
    std::fs::write(
        &chat_template_path,
        r#"function start() -> str { let _s = "{{SESSION}}"; let _m = "{{MESSAGE}}"; return remote_call("tool","noop",""); }"#,
    )
    .unwrap();
    let peers_path = tmpdir.path().join("peers.toml");
    std::fs::write(&peers_path, format!("[peers.tool]\naddr = \"{addr}\"\n")).unwrap();

    let cfg = BridgeConfig {
        bridge: BridgeSection {
            listen_addr: "127.0.0.1:9998".into(),
            secrets_path: Some(tmpdir.path().join("secrets.toml")),
            token_path: Some(tmpdir.path().join("bridge-token")),
            memory_db_path: None,
        },
        identity: IdentitySection {
            bundle_path,
            client_key_path,
        },
        transport: TransportSection {
            peers_path,
            deadline_secs: 30,
            data_dir: Some(tmpdir.path().to_path_buf()),
        },
        flow: FlowSection {
            template_path: chat_template_path,
            tool_template_path: None,
            streaming_template_path: None,
        },
        openai_compat: None,
        sse: SseSection::default(),
        coordinator: None,
        mesh: MeshSection::default(),
        observability: None,
        auth: crate::config::AuthSection::default(),
        logging: crate::config::LoggingSection::default(),
    };
    let base_state = AppState::try_new(cfg).expect("AppState::try_new");

    use relix_runtime::flow_runner::{PeerEntry, PeersFile};
    use relix_runtime::manifest::{DiscoveryOptions, discover_and_pin};
    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        "tool".to_string(),
        PeerEntry {
            addr: addr.to_string(),
        },
    );
    let peers_file = PeersFile { peers: peers_map };
    let opts = DiscoveryOptions {
        identity_bundle: base_state.identity_bundle.clone(),
        client_key: base_state.client_key.clone(),
        peers: peers_file,
        deadline_secs: 30,
        overall_timeout: Duration::from_secs(8),
        local_port: None,
        source_key_registry: None,
    };
    let (cache, mesh) = discover_and_pin(opts).await.expect("discover_and_pin");
    let cache_arc = Arc::new(cache);

    // The exact production wiring from `main::main`.
    let tool_registry = crate::tools::registry_from_manifest(&cache_arc);

    AppState {
        manifest_cache: cache_arc,
        tool_registry,
        mesh_client: Some(Arc::new(mesh)),
        ..base_state
    }
}

fn mount_tool_routes(state: AppState) -> Router {
    Router::new()
        .route("/v1/tools", get(crate::tools::list))
        .route("/v1/tools/search", post(crate::tools::search))
        .route("/v1/tools/manifest", get(crate::tools::manifest))
        .with_state(state)
}

// ── test ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tools_endpoints_return_discovered_tools_end_to_end() {
    let (mut dispatch, provider, org_root, _audit_dir) = fresh_tool_responder();
    register_manifest(&mut dispatch, provider);
    let dispatch = Arc::new(dispatch);
    let (_peer_client, events, addr) = boot_peer(181).await;
    spawn_inbound_loop(events, dispatch);

    let tmpdir = TempDir::new().unwrap();
    let state = build_state(&org_root, addr, &tmpdir).await;

    // Sanity: discovery genuinely populated the registry.
    assert!(
        !state.tool_registry.is_empty(),
        "discovery must have populated the tool registry from the tool peer"
    );

    let app = mount_tool_routes(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let http = reqwest::Client::new();

    // 1. GET /v1/tools — full list.
    let list: Value = timeout(
        Duration::from_secs(15),
        http.get(format!("http://{bound}/v1/tools")).send(),
    )
    .await
    .unwrap()
    .unwrap()
    .json()
    .await
    .unwrap();
    eprintln!("GET /v1/tools => {list}");
    let count = list["count"].as_u64().unwrap();
    assert!(count > 0, "list must report a non-zero tool count");
    let names: Vec<String> = list["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "tool.web_fetch"),
        "list must contain the real tool.web_fetch capability; got {names:?}"
    );

    // 2. POST /v1/tools/search — keyword hit.
    let search: Value = timeout(
        Duration::from_secs(15),
        http.post(format!("http://{bound}/v1/tools/search"))
            .json(&serde_json::json!({ "query": "fetch a webpage", "limit": 5 }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap()
    .json()
    .await
    .unwrap();
    eprintln!("POST /v1/tools/search => {search}");
    assert!(
        search["count"].as_u64().unwrap() >= 1,
        "search must return at least one hit for a real query"
    );

    // 3. GET /v1/tools/manifest — signed manifest carries tools.
    let manifest: Value = timeout(
        Duration::from_secs(15),
        http.get(format!("http://{bound}/v1/tools/manifest")).send(),
    )
    .await
    .unwrap()
    .unwrap()
    .json()
    .await
    .unwrap();
    eprintln!(
        "GET /v1/tools/manifest => signer={} warning={} tool_count={}",
        manifest["signed"]["manifest"]["signer"],
        manifest["warning"],
        manifest["signed"]["manifest"]["tools"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0)
    );
    let manifest_tools = manifest["signed"]["manifest"]["tools"].as_array().unwrap();
    assert_eq!(
        manifest_tools.len() as u64,
        count,
        "manifest must carry the same tools the list endpoint returned"
    );
}
