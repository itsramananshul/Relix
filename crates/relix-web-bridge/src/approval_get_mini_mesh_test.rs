//! DEFERRED C — mini-mesh integration test for the
//! `GET /v1/approval/:id` bridge route.
//!
//! Boots a fake coordinator peer that registers
//! `coord.approval.get` with two canned responses (one for a
//! valid id + one for "not found"), wires a real
//! `MeshClient` against it, mounts the bridge route on an
//! ephemeral listener, and asserts:
//!
//! - GET /v1/approval/<known> → 200 with the full JSON row.
//! - GET /v1/approval/<unknown> → 404 with an error body.
//! - A `legacy_token_expired` row surfaces its decision_note
//!   explaining the migration.

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
use relix_core::types::{ErrorEnvelope, NodeId, error_kinds};
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
                eprintln!("approval-get-mini-mesh: boot_peer retry ({e})");
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
        &dir.path().join("approval-get-audit.log"),
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
async fn approval_get_mini_mesh_returns_200_404_and_legacy_note() {
    // ─── 1. Boot a fake coordinator with canned coord.approval.get ───
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "ops_get"
        method = "coord.approval.get"
        allow_groups = ["operators"]
        "#,
    );

    let known_id = "apr_known";
    let legacy_id = "apr_legacy";

    let known_response = serde_json::json!({
        "approval_id": known_id,
        "agent_id": "agt-1",
        "subject_id": "subj-1",
        "method": "tool.web_read",
        "capability_category": "external_api:read",
        "reason": "fetch user data",
        "requested_at": 1_700_000_000_i64,
        "expires_at": 1_700_060_000_i64,
        "status": "pending",
        "decided_at": null,
        "decided_by": null,
        "decision_note": null,
        "task_id": null,
        "authorized_approvers": ["subj-op"],
    });
    let legacy_response = serde_json::json!({
        "approval_id": legacy_id,
        "agent_id": "agt-1",
        "subject_id": "subj-1",
        "method": "tool.web_read",
        "capability_category": "external_api:read",
        "reason": "",
        "requested_at": 0_i64,
        "expires_at": 9_999_999_999_i64,
        "status": "legacy_token_expired",
        "decided_at": 1_700_000_500_i64,
        "decided_by": null,
        "decision_note": "legacy_token_expired: opaque approval_token from a pre-SEC-PART-A deployment cannot be verified by the new HMAC-signed token gate. Retry to mint a fresh structured token.",
        "task_id": null,
        "authorized_approvers": [],
    });

    let known_arc = Arc::new(known_response.clone());
    let legacy_arc = Arc::new(legacy_response.clone());

    dispatch.register(
        "coord.approval.get",
        Arc::new(FnHandler({
            let known = known_arc.clone();
            let legacy = legacy_arc.clone();
            move |ctx: InvocationCtx| {
                let known = known.clone();
                let legacy = legacy.clone();
                async move {
                    // Cap wire arg is raw bytes (the approval id).
                    let id = std::str::from_utf8(&ctx.args)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let chosen = match id.as_str() {
                        "apr_known" => Some(known),
                        "apr_legacy" => Some(legacy),
                        _ => None,
                    };
                    match chosen {
                        Some(v) => HandlerOutcome::Ok(serde_json::to_vec(&*v).unwrap_or_default()),
                        None => HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::INVALID_ARGS,
                            cause: format!("coord.approval.get: not found: {id}"),
                            retry_hint: 0,
                            retry_after: None,
                        }),
                    }
                }
            }
        })),
    );
    let dispatch = Arc::new(dispatch);
    let (_peer_client, events, addr) = boot_peer(141).await;
    spawn_inbound_loop(events, dispatch.clone());

    // ─── 2. Mint bridge identity ───
    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes = mint_bridge_bundle_bytes(
        &org_root,
        "approval-get-test-bridge",
        vec!["operators".into()],
    );
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

    // ─── 3. Mount the single route under test ───
    let app = Router::new()
        .route("/v1/approval/:id", get(crate::approval::get_approval))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();

    // ─── 4. GET /v1/approval/apr_known → 200 + full JSON row ───
    let url = format!("http://{}/v1/approval/{}", bound, known_id);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body.get("approval_id").and_then(Value::as_str),
        Some(known_id)
    );
    assert_eq!(body.get("status").and_then(Value::as_str), Some("pending"));
    assert_eq!(
        body.get("method").and_then(Value::as_str),
        Some("tool.web_read")
    );
    assert_eq!(
        body.get("authorized_approvers")
            .and_then(Value::as_array)
            .map(|a| a.len()),
        Some(1)
    );

    // ─── 5. GET /v1/approval/apr_missing → 404 ───
    let url = format!("http://{}/v1/approval/apr_missing", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404, "missing id must yield 404");
    let body: Value = resp.json().await.unwrap();
    assert!(
        body.get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("not found"),
        "body must surface `not found`: {body}"
    );

    // ─── 6. GET /v1/approval/apr_legacy → 200 + decision_note ───
    let url = format!("http://{}/v1/approval/{}", bound, legacy_id);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body.get("status").and_then(Value::as_str),
        Some("legacy_token_expired")
    );
    assert!(
        body.get("decision_note")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("legacy_token_expired:"),
        "legacy row must surface its explanatory decision_note: {body}"
    );
}
