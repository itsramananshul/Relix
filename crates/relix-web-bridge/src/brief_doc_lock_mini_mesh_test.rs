//! Mini-mesh integration test for the §1.8 Dossier-locking and §1.9
//! interaction cancel / idempotent-create bridge routes:
//!
//! - `POST /v1/spine/briefs/:id/dossiers/lock` → 200 + the lock JSON, with the
//!   wire arg `task_id|kind|subject|reason`; a conflict body (`{conflict:true}`)
//!   maps to a typed **409**.
//! - `POST /v1/spine/briefs/:id/dossiers/unlock` → 200, wire `task_id|kind|subject`.
//! - `GET  /v1/spine/briefs/:id/dossiers/locks` → 200 + the JSON array passthrough.
//! - `POST /v1/spine/briefs/:id/interactions/:iid/cancel` → 200, wire
//!   `task_id|interaction_id|subject`.
//! - `POST /v1/spine/briefs/:id/interactions` with an `idempotency_key` routes
//!   through the JSON `brief.interaction_create` capability and returns the id.

#![cfg(test)]

use std::sync::{Arc, Mutex};
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
                eprintln!("brief-doc-lock-mini-mesh: boot_peer retry ({e})");
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
        &dir.path().join("brief-doc-lock-audit.log"),
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
async fn brief_doc_lock_and_interaction_cancel_routes_round_trip() {
    // ─── 1. Fake coordinator: canned lock/unlock/locks + cancel + create ───
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "lock"
        method = "brief.dossier_lock"
        allow_groups = ["chat-users"]

        [[rules]]
        name = "unlock"
        method = "brief.dossier_unlock"
        allow_groups = ["chat-users"]

        [[rules]]
        name = "locks"
        method = "brief.dossier_locks"
        allow_groups = ["chat-users"]

        [[rules]]
        name = "cancel"
        method = "brief.interaction_cancel"
        allow_groups = ["chat-users"]

        [[rules]]
        name = "create"
        method = "brief.interaction_create"
        allow_groups = ["chat-users"]
        "#,
    );

    let lock_arg: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let cancel_arg: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let create_arg: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    dispatch.register(
        "brief.dossier_lock",
        Arc::new(FnHandler({
            let seen = lock_arg.clone();
            move |ctx: InvocationCtx| {
                let seen = seen.clone();
                async move {
                    let arg = String::from_utf8_lossy(&ctx.args).to_string();
                    *seen.lock().unwrap() = arg.clone();
                    // A lock on the `taken` kind is held by someone else → conflict.
                    if arg.contains("|taken|") {
                        let body = serde_json::json!({
                            "conflict": true, "kind": "taken", "locked_by": "someone-else"
                        });
                        HandlerOutcome::Ok(serde_json::to_vec(&body).unwrap())
                    } else {
                        let body = serde_json::json!({
                            "task_id": "task_1", "kind": "plan", "locked_by": "alice",
                            "locked_at": 1_700_000_000_i64, "reason": "drafting"
                        });
                        HandlerOutcome::Ok(serde_json::to_vec(&body).unwrap())
                    }
                }
            }
        })),
    );
    dispatch.register(
        "brief.dossier_unlock",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| async move {
            let body = serde_json::json!({ "unlocked": true, "kind": "plan",
                "previously_locked_by": "alice" });
            HandlerOutcome::Ok(serde_json::to_vec(&body).unwrap())
        })),
    );
    dispatch.register(
        "brief.dossier_locks",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| async move {
            let body = serde_json::json!([{
                "task_id": "task_1", "kind": "plan", "locked_by": "alice",
                "locked_at": 1_700_000_000_i64
            }]);
            HandlerOutcome::Ok(serde_json::to_vec(&body).unwrap())
        })),
    );
    dispatch.register(
        "brief.interaction_cancel",
        Arc::new(FnHandler({
            let seen = cancel_arg.clone();
            move |ctx: InvocationCtx| {
                let seen = seen.clone();
                async move {
                    *seen.lock().unwrap() = String::from_utf8_lossy(&ctx.args).to_string();
                    HandlerOutcome::Ok(Vec::new())
                }
            }
        })),
    );
    dispatch.register(
        "brief.interaction_create",
        Arc::new(FnHandler({
            let seen = create_arg.clone();
            move |ctx: InvocationCtx| {
                let seen = seen.clone();
                async move {
                    *seen.lock().unwrap() = String::from_utf8_lossy(&ctx.args).to_string();
                    HandlerOutcome::Ok(b"bix_idem".to_vec())
                }
            }
        })),
    );

    let dispatch = Arc::new(dispatch);
    let (_peer_client, events, addr) = boot_peer(167).await;
    spawn_inbound_loop(events, dispatch.clone());

    // ─── 2. Mint bridge identity + wire a real mesh client ───
    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes =
        mint_bridge_bundle_bytes(&org_root, "doc-lock-test-bridge", vec!["chat-users".into()]);
    let bundle_path = tmpdir.path().join("bridge.bundle");
    std::fs::write(&bundle_path, &bundle_bytes).unwrap();
    let client_key_path = tmpdir.path().join("client.key");
    let chat_template_path = tmpdir.path().join("chat.sol");
    std::fs::write(
        &chat_template_path,
        r#"function start() -> str { return remote_call("coord", "noop", "{{SESSION}}|{{MESSAGE}}|"); }"#,
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

    // ─── 3. Mount the new routes ───
    let app = Router::new()
        .route(
            "/v1/spine/briefs/:id/dossiers/lock",
            post(crate::spine::lock_dossier),
        )
        .route(
            "/v1/spine/briefs/:id/dossiers/unlock",
            post(crate::spine::unlock_dossier),
        )
        .route(
            "/v1/spine/briefs/:id/dossiers/locks",
            get(crate::spine::list_dossier_locks),
        )
        .route(
            "/v1/spine/briefs/:id/interactions",
            post(crate::spine::open_interaction),
        )
        .route(
            "/v1/spine/briefs/:id/interactions/:iid/cancel",
            post(crate::spine::cancel_interaction),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let http = reqwest::Client::new();

    // ─── 4. POST lock → 200 + lock JSON, correct wire arg ───
    let lock_url = format!("http://{bound}/v1/spine/briefs/task_1/dossiers/lock");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&lock_url)
            .json(&serde_json::json!({ "kind": "plan", "subject": "alice", "reason": "drafting" }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body.get("locked_by").and_then(Value::as_str), Some("alice"));
    assert_eq!(
        *lock_arg.lock().unwrap(),
        "task_1|plan|alice|drafting",
        "lock wire arg must be task_id|kind|subject|reason"
    );

    // ─── 5. POST lock on a kind held by another → 409 ───
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&lock_url)
            .json(&serde_json::json!({ "kind": "taken", "subject": "alice" }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        409,
        "a lock conflict must surface as a typed 409"
    );

    // ─── 6. POST unlock → 200 ───
    let unlock_url = format!("http://{bound}/v1/spine/briefs/task_1/dossiers/unlock");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&unlock_url)
            .json(&serde_json::json!({ "kind": "plan", "subject": "alice" }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // ─── 7. GET locks → 200 + array passthrough ───
    let locks_url = format!("http://{bound}/v1/spine/briefs/task_1/dossiers/locks");
    let resp = timeout(Duration::from_secs(15), http.get(&locks_url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body.as_array().map(|a| a.len()), Some(1));

    // ─── 8. POST cancel → 200, correct wire arg ───
    let cancel_url = format!("http://{bound}/v1/spine/briefs/task_1/interactions/bix_x/cancel");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&cancel_url)
            .json(&serde_json::json!({ "subject": "founder" }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        *cancel_arg.lock().unwrap(),
        "task_1|bix_x|founder",
        "cancel wire arg must be task_id|interaction_id|subject"
    );

    // ─── 9. POST open with idempotency_key → routes to JSON create ───
    let open_url = format!("http://{bound}/v1/spine/briefs/task_1/interactions");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&open_url)
            .json(&serde_json::json!({
                "kind": "ask",
                "prompt": "Which region?",
                "author": "op",
                "idempotency_key": "k-123",
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body.get("interaction_id").and_then(Value::as_str),
        Some("bix_idem")
    );
    let seen_create = create_arg.lock().unwrap().clone();
    let parsed: Value = serde_json::from_str(&seen_create).expect("create arg is JSON");
    assert_eq!(
        parsed.get("idempotency_key").and_then(Value::as_str),
        Some("k-123"),
        "a keyed open must route through brief.interaction_create with the key"
    );
    assert_eq!(
        parsed.get("task_id").and_then(Value::as_str),
        Some("task_1")
    );
}
