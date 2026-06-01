//! RELIX-7.19 — end-to-end mini-mesh integration test for the
//! confidence bridge surface.
//!
//! Boots a fake coordinator peer with canned `confidence.*`
//! responders, dials it via `discover_and_pin`, mounts every
//! `/v1/confidence/*` route on an ephemeral axum listener,
//! and drives reqwest requests through five scenarios:
//!
//! 1. GET `/v1/confidence/policies` → 200 + policy list
//! 2. GET `/v1/confidence/history/:agent?method=ai.chat` → 200 + snapshot
//! 3. GET `/v1/confidence/history/:agent` (no method) → 400
//! 4. POST `/v1/confidence/reset` (agent+method) → 200 + summary
//! 5. POST `/v1/confidence/reset` (empty agent) → 400

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
use relix_runtime::transport::rpc::{self, Event, Multiaddr};
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::config::{
    AppState, BridgeConfig, BridgeSection, FlowSection, IdentitySection, MeshSection, SseSection,
    TransportSection,
};

fn key_for(seed: u8) -> [u8; 32] {
    let mut k = [0u8; 32];
    for (i, slot) in k.iter_mut().enumerate() {
        *slot = seed.wrapping_add(i as u8);
    }
    k
}

async fn boot_peer(seed: u8) -> (rpc::Client, mpsc::Receiver<Event>, Multiaddr) {
    for _ in 0..16 {
        let port: u16 = 35_000 + (rand::random::<u16>() % 25_000);
        match rpc::new(key_for(seed), port).await {
            Ok((client, events, event_loop)) => {
                let listen_addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}")
                    .parse()
                    .expect("valid multiaddr");
                tokio::spawn(event_loop.run());
                return (client, events, listen_addr);
            }
            Err(e) => {
                eprintln!("confidence-mini-mesh: boot_peer retry ({e})");
                continue;
            }
        }
    }
    panic!("boot_peer: exhausted port retries");
}

fn fresh_responder_bridge(policy_toml: &str) -> (DispatchBridge, SigningKey, TempDir) {
    let dir = TempDir::new().unwrap();
    let org_root = SigningKey::generate(&mut OsRng);
    let responder = SigningKey::generate(&mut OsRng);
    let policy = PolicyEngine::from_toml(policy_toml).expect("policy parses");
    let bridge = DispatchBridge::new(
        policy,
        org_root.verifying_key(),
        &dir.path().join("confidence-audit.log"),
        responder,
    )
    .expect("bridge constructs");
    (bridge, org_root, dir)
}

fn mint_bridge_bundle_bytes(org_root: &SigningKey, name: &str, groups: Vec<String>) -> Vec<u8> {
    let caller_key = SigningKey::generate(&mut OsRng);
    let id = IdentityBundle {
        subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
        name: name.into(),
        org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
        groups,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confidence_mini_mesh_all_endpoints() {
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "ops_policy_list"
        method = "confidence.policy_list"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_score_history"
        method = "confidence.score_history"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_reset_history"
        method = "confidence.reset_history"
        allow_groups = ["operators"]
        "#,
    );

    let policies_response = serde_json::json!([
        {
            "capability": "ai.chat",
            "low_threshold": 0.5,
            "critical_threshold": 0.3,
            "low_action": { "Retry": { "max_retries": 2, "retry_delay_ms": 500 } },
            "critical_action": { "Escalate": { "escalate_to": "ai.chat.premium" } }
        }
    ]);
    let history_response = serde_json::json!({
        "agent": "alice",
        "method": "ai.chat",
        "call_count": 42,
        "error_count": 3,
        "error_rate": 0.0714,
        "p50_latency_ms": 250,
        "p95_latency_ms": 800,
        "p99_latency_ms": 1100,
        "avg_confidence": 0.83
    });
    let reset_response = serde_json::json!({
        "cleared_pair": true,
        "agent": "alice",
        "method": "ai.chat"
    });

    let policies_arc = Arc::new(policies_response.clone());
    let history_arc = Arc::new(history_response.clone());
    let reset_arc = Arc::new(reset_response.clone());

    dispatch.register(
        "confidence.policy_list",
        Arc::new(FnHandler({
            let r = policies_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "confidence.score_history",
        Arc::new(FnHandler({
            let r = history_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "confidence.reset_history",
        Arc::new(FnHandler({
            let r = reset_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    let dispatch = Arc::new(dispatch);

    let (_client, events, addr) = boot_peer(149).await;
    spawn_inbound_loop(events, dispatch.clone());

    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes = mint_bridge_bundle_bytes(
        &org_root,
        "confidence-test-bridge",
        vec!["operators".into()],
    );
    let bundle_path = tmpdir.path().join("bridge.bundle");
    std::fs::write(&bundle_path, &bundle_bytes).unwrap();
    let client_key_path = tmpdir.path().join("client.key");
    let chat_template_path = tmpdir.path().join("chat.sol");
    std::fs::write(
        &chat_template_path,
        r#"function start() -> str { return remote_call("coordinator", "noop", "{{SESSION}}|{{MESSAGE}}|"); }"#,
    )
    .unwrap();
    let peers_path = tmpdir.path().join("peers.toml");
    std::fs::write(
        &peers_path,
        format!(
            r#"
[peers.coordinator]
addr = "{addr}"
"#
        ),
    )
    .unwrap();

    let cfg = BridgeConfig {
        bridge: BridgeSection {
            listen_addr: "127.0.0.1:9999".into(),
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
        "coordinator".to_string(),
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
    let (_cache, mesh) = discover_and_pin(opts).await.expect("discover_and_pin");
    let state = AppState {
        mesh_client: Some(Arc::new(mesh)),
        ..base_state
    };

    let app = Router::new()
        .route("/v1/confidence/policies", get(crate::confidence::policies))
        .route(
            "/v1/confidence/history/:agent",
            get(crate::confidence::history),
        )
        .route("/v1/confidence/reset", post(crate::confidence::reset))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();

    // 1. GET /v1/confidence/policies → 200 + list
    let url = format!("http://{bound}/v1/confidence/policies");
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, policies_response);

    // 2. GET /v1/confidence/history/:agent?method=ai.chat → 200 + snapshot
    let url = format!("http://{bound}/v1/confidence/history/alice?method=ai.chat");
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, history_response);

    // 3. GET /v1/confidence/history/:agent (no method) → 400
    let url = format!("http://{bound}/v1/confidence/history/alice");
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    // 4. POST /v1/confidence/reset (agent+method) → 200 + summary
    let url = format!("http://{bound}/v1/confidence/reset");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "agent": "alice",
                "method": "ai.chat"
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, reset_response);

    // 5. POST /v1/confidence/reset (empty agent) → 400
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({ "agent": "" }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}
