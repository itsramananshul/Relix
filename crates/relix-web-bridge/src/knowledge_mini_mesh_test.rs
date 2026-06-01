//! RELIX-7.16 — end-to-end mini-mesh integration test for the
//! agent-to-agent knowledge transfer bridge surface.
//!
//! Boots a fake "memory" peer with canned `knowledge.*`
//! responders, dials it via `discover_and_pin`, mounts every
//! `/v1/knowledge/*` route on an ephemeral axum listener, and
//! drives reqwest requests through six scenarios:
//!
//! 1. POST `/v1/knowledge/share` → 200 + share summary
//! 2. POST `/v1/knowledge/share` (empty `target_agents`) →
//!    400 (bridge-side validation)
//! 3. GET `/v1/knowledge/groups` → 200 + group list
//! 4. GET `/v1/knowledge/shared/:agent` → 200 + received-rows list
//! 5. POST `/v1/knowledge/broadcast` → 200 + per-target result
//! 6. POST `/v1/knowledge/revoke` → 200 + revoke summary

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
                eprintln!("knowledge-mini-mesh: boot_peer retry ({e})");
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
        &dir.path().join("knowledge-audit.log"),
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
async fn knowledge_mini_mesh_all_endpoints() {
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "ops_share"
        method = "knowledge.share"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_list_shared"
        method = "knowledge.list_shared"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_broadcast"
        method = "knowledge.group_broadcast"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_groups"
        method = "knowledge.groups"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_revoke"
        method = "knowledge.revoke"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_recall"
        method = "knowledge.recall"
        allow_groups = ["operators"]
        "#,
    );

    let share_response = serde_json::json!({
        "shared_count": 1,
        "rejection_count": 0,
        "rejections": [],
        "created_ids": ["copy-abc123"],
        "events": [{
            "kind": "shared",
            "source_agent": "alice",
            "target_agent": "bob",
            "group": "research",
            "observation_ids": ["obs-1"],
            "recorded_at": 1_700_000_000_i64
        }]
    });
    let list_shared_response = serde_json::json!([
        {
            "id": "copy-abc123",
            "text": "user prefers Helvetica",
            "shared_by": "alice",
            "received_by": "bob",
            "created_at": 1_700_000_000_i64,
            "observed_at": 1_700_000_000_i64,
            "message": "worth keeping",
            "tags": ["shared_from:alice"],
            "revoked": false
        }
    ]);
    let groups_response = serde_json::json!([
        {
            "name": "research",
            "members": ["alice", "bob", "carol"],
            "auto_share_layers": ["observation"],
            "min_quality_score": 0.7
        }
    ]);
    let broadcast_response = serde_json::json!({
        "group": "research",
        "per_target": [
            ["bob", { "shared_count": 1, "rejection_count": 0, "rejections": [], "created_ids": ["copy-bob"], "events": [] }],
            ["carol", { "shared_count": 1, "rejection_count": 0, "rejections": [], "created_ids": ["copy-carol"], "events": [] }]
        ]
    });
    let revoke_response = serde_json::json!({
        "revoked_count": 1,
        "missing_ids": [],
        "events": [{
            "kind": "revoked",
            "target_agent": "bob",
            "observation_ids": ["copy-abc123"],
            "recorded_at": 1_700_000_000_i64
        }]
    });
    let recall_response = serde_json::json!({
        "source_ids_processed": 1,
        "total_copies_revoked": 2,
        "per_target": [
            { "target_agent": "bob", "copies_revoked": 1, "missing_copy_ids": [] },
            { "target_agent": "carol", "copies_revoked": 1, "missing_copy_ids": [] }
        ],
        "missing_source_ids": [],
        "unauthorised_source_ids": [],
        "events": [
            { "kind": "revoked", "source_agent": "alice", "target_agent": "bob",
              "observation_ids": ["copy-bob"], "recorded_at": 1_700_000_000_i64 },
            { "kind": "revoked", "source_agent": "alice", "target_agent": "carol",
              "observation_ids": ["copy-carol"], "recorded_at": 1_700_000_000_i64 }
        ]
    });

    let share_arc = Arc::new(share_response.clone());
    let list_arc = Arc::new(list_shared_response.clone());
    let groups_arc = Arc::new(groups_response.clone());
    let broadcast_arc = Arc::new(broadcast_response.clone());
    let revoke_arc = Arc::new(revoke_response.clone());
    let recall_arc = Arc::new(recall_response.clone());

    dispatch.register(
        "knowledge.share",
        Arc::new(FnHandler({
            let r = share_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "knowledge.list_shared",
        Arc::new(FnHandler({
            let r = list_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "knowledge.groups",
        Arc::new(FnHandler({
            let r = groups_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "knowledge.group_broadcast",
        Arc::new(FnHandler({
            let r = broadcast_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "knowledge.revoke",
        Arc::new(FnHandler({
            let r = revoke_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "knowledge.recall",
        Arc::new(FnHandler({
            let r = recall_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    let dispatch = Arc::new(dispatch);

    let (_client, events, addr) = boot_peer(101).await;
    spawn_inbound_loop(events, dispatch.clone());

    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes =
        mint_bridge_bundle_bytes(&org_root, "knowledge-test-bridge", vec!["operators".into()]);
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
        .route("/v1/knowledge/share", post(crate::knowledge::share))
        .route(
            "/v1/knowledge/shared/:agent",
            get(crate::knowledge::list_shared),
        )
        .route("/v1/knowledge/broadcast", post(crate::knowledge::broadcast))
        .route("/v1/knowledge/groups", get(crate::knowledge::groups))
        .route("/v1/knowledge/revoke", post(crate::knowledge::revoke))
        .route("/v1/knowledge/recall", post(crate::knowledge::recall))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();

    // 1. POST /v1/knowledge/share → 200 + summary
    let url = format!("http://{}/v1/knowledge/share", bound);
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "source_agent": "alice",
                "target_agents": ["bob"],
                "observation_ids": ["obs-1"],
                "message": "fyi"
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, share_response);

    // 2. POST /v1/knowledge/share with empty target_agents → 400
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "source_agent": "alice",
                "target_agents": [],
                "observation_ids": ["obs-1"]
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    // 3. GET /v1/knowledge/groups → 200 + list
    let url = format!("http://{}/v1/knowledge/groups", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, groups_response);

    // 4. GET /v1/knowledge/shared/bob → 200 + rows
    let url = format!("http://{}/v1/knowledge/shared/bob", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, list_shared_response);

    // 5. POST /v1/knowledge/broadcast → 200 + per_target
    let url = format!("http://{}/v1/knowledge/broadcast", bound);
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "caller_agent": "alice",
                "group": "research",
                "observation_ids": ["obs-1"]
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, broadcast_response);

    // 6. POST /v1/knowledge/revoke → 200 + summary
    let url = format!("http://{}/v1/knowledge/revoke", bound);
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "observation_ids": ["copy-abc123"]
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, revoke_response);

    // 7. POST /v1/knowledge/recall → 200 + per_target breakdown
    let url = format!("http://{}/v1/knowledge/recall", bound);
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "source_agent": "alice",
                "source_observation_ids": ["obs-1"]
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, recall_response);

    // 8. POST /v1/knowledge/recall with empty source_agent → 400
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "source_agent": "",
                "source_observation_ids": ["obs-1"]
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    // 9. POST /v1/knowledge/recall with empty ids → 400
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "source_agent": "alice",
                "source_observation_ids": []
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}
