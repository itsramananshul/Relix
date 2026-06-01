//! End-to-end mini-mesh integration test for the RELIX-7.15
//! memory-layer PII bridge endpoints:
//!
//! - `POST /v1/memory/pii/scan`
//! - `POST /v1/memory/pii/preview`
//!
//! Boots a fake "memory" peer with canned `memory.pii_scan` +
//! `memory.anonymize_preview` responders, builds a real
//! `MeshClient` via `discover_and_pin`, mounts both routes on
//! an ephemeral axum listener, and drives reqwest requests
//! through the stack across four scenarios:
//!
//! 1. POST /v1/memory/pii/scan       → 200 + canned spans.
//! 2. POST /v1/memory/pii/scan empty → 400 (bridge-side).
//! 3. POST /v1/memory/pii/preview    → 200 + canned anonymized.
//! 4. POST /v1/memory/pii/preview empty → 400.

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::post;
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
                eprintln!("memory-pii-mini-mesh: boot_peer retry ({e})");
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
        &dir.path().join("memory-pii-audit.log"),
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
async fn memory_pii_mini_mesh_all_endpoints() {
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "ops_scan"
        method = "memory.pii_scan"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_preview"
        method = "memory.anonymize_preview"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_bulk"
        method = "memory.bulk_anonymize"
        allow_groups = ["operators"]
        "#,
    );

    let scan_response = serde_json::json!({
        "spans": [
            { "pii_type": "EMAIL", "start": 14, "end": 31, "matched_text": "alice@example.com" }
        ],
        "count": 1
    });
    let preview_response = serde_json::json!({
        "anonymized": "Contact me at [EMAIL]",
        "spans": [
            { "pii_type": "EMAIL", "start": 14, "end": 31, "matched_text": "alice@example.com" }
        ]
    });
    let bulk_response = serde_json::json!({
        "turns": { "scanned": 12, "changed": 4 },
        "records": {
            "raw":         { "scanned": 8, "changed": 3 },
            "semantic":    { "scanned": 5, "changed": 2 },
            "observation": { "scanned": 3, "changed": 1 },
            "model":       { "scanned": 1, "changed": 0 },
            "total_scanned": 17,
            "total_changed": 6
        }
    });
    let scan_arc = Arc::new(scan_response.clone());
    let preview_arc = Arc::new(preview_response.clone());
    let bulk_arc = Arc::new(bulk_response.clone());

    dispatch.register(
        "memory.pii_scan",
        Arc::new(FnHandler({
            let r = scan_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "memory.anonymize_preview",
        Arc::new(FnHandler({
            let r = preview_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "memory.bulk_anonymize",
        Arc::new(FnHandler({
            let r = bulk_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    let dispatch = Arc::new(dispatch);

    let (_client, events, addr) = boot_peer(91).await;
    spawn_inbound_loop(events, dispatch.clone());

    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes = mint_bridge_bundle_bytes(
        &org_root,
        "memory-pii-test-bridge",
        vec!["operators".into()],
    );
    let bundle_path = tmpdir.path().join("bridge.bundle");
    std::fs::write(&bundle_path, &bundle_bytes).unwrap();
    let client_key_path = tmpdir.path().join("client.key");
    let chat_template_path = tmpdir.path().join("chat.sol");
    std::fs::write(
        &chat_template_path,
        r#"function start() -> str { return remote_call("memory", "noop", "{{SESSION}}|{{MESSAGE}}|"); }"#,
    )
    .unwrap();
    let peers_path = tmpdir.path().join("peers.toml");
    std::fs::write(
        &peers_path,
        format!(
            r#"
[peers.memory]
addr = "{}"
"#,
            addr
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
        "memory".to_string(),
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
        .route("/v1/memory/pii/scan", post(crate::memory_pii::scan))
        .route("/v1/memory/pii/preview", post(crate::memory_pii::preview))
        .route(
            "/v1/memory/pii/bulk_anonymize",
            post(crate::memory_pii::bulk_anonymize),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();

    // 1. POST /v1/memory/pii/scan → 200 + spans
    let url = format!("http://{}/v1/memory/pii/scan", bound);
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({ "text": "Contact me at alice@example.com" }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, scan_response);

    // 2. POST /v1/memory/pii/scan empty → 400
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({ "text": "" }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    // 3. POST /v1/memory/pii/preview → 200 + anonymized
    let url = format!("http://{}/v1/memory/pii/preview", bound);
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "text": "Contact me at alice@example.com",
                "strategy": "redact"
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, preview_response);
    assert_eq!(
        body.get("anonymized").and_then(Value::as_str),
        Some("Contact me at [EMAIL]")
    );

    // 4. POST /v1/memory/pii/preview empty → 400
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({ "text": "" }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    // 5. POST /v1/memory/pii/bulk_anonymize → 200 + per-layer counts
    let url = format!("http://{}/v1/memory/pii/bulk_anonymize", bound);
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url).json(&serde_json::json!({})).send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, bulk_response);
    // Spot-check the shape: turns + records.{raw,semantic,observation,model,total_*}.
    assert_eq!(
        body.pointer("/turns/changed").and_then(Value::as_u64),
        Some(4)
    );
    assert_eq!(
        body.pointer("/records/total_scanned")
            .and_then(Value::as_u64),
        Some(17)
    );
    assert_eq!(
        body.pointer("/records/raw/changed").and_then(Value::as_u64),
        Some(3)
    );

    // 6. POST /v1/memory/pii/bulk_anonymize with no body → 200
    // (operators may POST with a content-length header of 0).
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .header("content-type", "application/json")
            .body("")
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}
