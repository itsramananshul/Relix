//! End-to-end mini-mesh integration test for the Prime Shift-Room status
//! surface (Shift Room Hardening Pack, PART D).
//!
//! Boots a fake coordinator peer with a canned `prime.status` responder that
//! returns a status snapshot for a KNOWN proposal id and a "not found" error
//! for any other id (mirroring the real tenant-gated/cross-Guild not-found —
//! no existence leak). Dials it via `discover_and_pin`, mounts the two real
//! Prime status routes on an ephemeral axum listener, and drives reqwest:
//!
//! 1. GET `…/proposals/prop_known/status`         → 200 + the snapshot JSON
//! 2. GET `…/proposals/prop_unknown/status`       → 404 (not-found, no leak)
//! 3. GET `…/proposals/prop_known/status/stream`  → 200 + initial `event: status`
//! 4. GET `…/proposals/prop_unknown/status/stream`→ 200 + terminal `event: not_found`

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_core::identity::{IdentityBundle, issue_identity};
use relix_core::policy::PolicyEngine;
use relix_core::types::{ErrorEnvelope, NodeId};
use relix_runtime::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use relix_runtime::transport::rpc::{self, Event, Multiaddr};
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
                eprintln!("prime-status-mini-mesh: boot_peer retry ({e})");
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
        &dir.path().join("prime-status-audit.log"),
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

/// The canned status snapshot the fake coordinator returns for `prop_known`.
/// Shape mirrors the real `prime.status` body the dashboard consumes.
fn known_status_json() -> serde_json::Value {
    serde_json::json!({
        "proposal_id": "prop_known",
        "status": "approved",
        "message": "Build a web dashboard",
        "mandate_id": "mdt_1",
        "mandate_title": "Web dashboard",
        "briefs": [
            {
                "brief_id": "brf_1",
                "title": "Engineer track",
                "board_status": "todo",
                "start_readiness": "ready",
                "blockers": [],
                "needs_review": false,
                "latest_run": null,
                "next_action": "start this Brief",
                "exists": true,
            }
        ],
        "counts": {
            "total_briefs": 1, "running": 0, "done": 0, "blocked": 0,
            "needs_review": 0, "refused": 0, "failed": 0, "ready": 1,
            "unassigned": 0, "not_ready": 0, "missing": 0,
        },
        "recommended_next_actions": ["Start 1 ready Brief(s) — they will run as Shifts."],
        "updated_at": 1700,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prime_status_mini_mesh_json_and_stream() {
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "ops_prime_status"
        method = "prime.status"
        allow_groups = ["operators"]
        "#,
    );

    let known = Arc::new(known_status_json());
    // `prime.status` arg is the bare proposal id. Known id → snapshot; any
    // other id → a "not found" error (the SAME response a cross-Guild proposal
    // produces — no existence leak).
    dispatch.register(
        "prime.status",
        Arc::new(FnHandler({
            let known = known.clone();
            move |ctx: InvocationCtx| {
                let known = known.clone();
                async move {
                    let id = std::str::from_utf8(&ctx.args).unwrap_or("").trim();
                    if id == "prop_known" {
                        HandlerOutcome::Ok(serde_json::to_vec(&*known).unwrap_or_default())
                    } else {
                        HandlerOutcome::Err(ErrorEnvelope {
                            kind: relix_core::types::error_kinds::INVALID_ARGS,
                            cause: format!("proposal not found: {id}"),
                            retry_hint: 2,
                            retry_after: None,
                        })
                    }
                }
            }
        })),
    );
    let dispatch = Arc::new(dispatch);

    let (_client, events, addr) = boot_peer(191).await;
    spawn_inbound_loop(events, dispatch.clone());

    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes = mint_bridge_bundle_bytes(
        &org_root,
        "prime-status-test-bridge",
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
        .route(
            "/v1/spine/prime/proposals/:id/status",
            get(crate::spine::prime_status),
        )
        .route(
            "/v1/spine/prime/proposals/:id/status/stream",
            get(crate::spine::prime_status_stream),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();

    // 1. GET …/prop_known/status → 200 + the snapshot JSON.
    let url = format!("http://{bound}/v1/spine/prime/proposals/prop_known/status");
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body, known_status_json());

    // 2. GET …/prop_unknown/status → 404 (not-found, no existence leak).
    let url = format!("http://{bound}/v1/spine/prime/proposals/prop_unknown/status");
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "an unknown / cross-Guild proposal must read as not-found"
    );

    // 3. GET …/prop_known/status/stream → 200 + initial `event: status` snapshot.
    let url = format!("http://{bound}/v1/spine/prime/proposals/prop_known/status/stream");
    let mut resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .contains("text/event-stream"),
        "the status stream is an SSE response"
    );
    let mut buf = String::new();
    let mut saw_status = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !saw_status && std::time::Instant::now() < deadline {
        let chunk = timeout(Duration::from_secs(2), resp.chunk()).await;
        let Ok(Ok(Some(bytes))) = chunk else { break };
        buf.push_str(&String::from_utf8_lossy(&bytes));
        // The initial frame is `event: status` carrying the snapshot JSON.
        saw_status = buf.contains("event: status") && buf.contains("prop_known");
    }
    assert!(
        saw_status,
        "expected an initial `event: status` snapshot; buf=\n{buf}"
    );
    drop(resp);

    // 4. GET …/prop_unknown/status/stream → terminal `event: not_found`.
    let url = format!("http://{bound}/v1/spine/prime/proposals/prop_unknown/status/stream");
    let mut resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let mut buf = String::new();
    let mut saw_not_found = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !saw_not_found && std::time::Instant::now() < deadline {
        let chunk = timeout(Duration::from_secs(2), resp.chunk()).await;
        let Ok(Ok(Some(bytes))) = chunk else { break };
        buf.push_str(&String::from_utf8_lossy(&bytes));
        saw_not_found = buf.contains("event: not_found");
    }
    assert!(
        saw_not_found,
        "an unknown proposal stream must emit a terminal `event: not_found`; buf=\n{buf}"
    );
    drop(resp);
}
