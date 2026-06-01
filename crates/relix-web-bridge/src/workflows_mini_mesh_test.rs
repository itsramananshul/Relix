//! End-to-end mini-mesh integration tests for the workflow
//! HTTP bridge endpoints.
//!
//! Boots a tiny fake "coordinator" peer that registers the
//! four `workflow.*` capabilities with canned JSON responses,
//! builds a real `MeshClient` pointing at it via
//! `discover_and_pin`, mounts the four workflow routes on an
//! ephemeral axum listener, and drives reqwest requests
//! through the stack.
//!
//! Coverage targets the three scenarios called out in the
//! foundation spec:
//!
//!   * `GET  /v1/workflows`          → 200 + catalog list.
//!   * `POST /v1/workflows/run`      → 200 + execution record.
//!   * `POST /v1/workflows/validate` → 400 + clear error.

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

/// Boot a libp2p peer on a random localhost port. The
/// transport event loop runs in the background; inbound RPC
/// events are returned via the `Receiver` so the caller can
/// wire them into a [`DispatchBridge`].
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
                eprintln!("workflows-mini-mesh: boot_peer retry ({e})");
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
        &dir.path().join("coord-audit.log"),
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

/// Wire inbound RPC events into the dispatch bridge so
/// registered handlers actually fire.
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
async fn workflows_mini_mesh_all_three_endpoints() {
    // ─── 1. Boot a fake coordinator with canned workflow.* ───
    let (mut coord_dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "ops_run"
        method = "workflow.run"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_list"
        method = "workflow.list"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_validate"
        method = "workflow.validate"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_status"
        method = "workflow.status"
        allow_groups = ["operators"]
        "#,
    );
    let run_response = serde_json::json!({
        "execution_id": "deadbeef00000000deadbeef00000000",
        "workflow_name": "test-workflow",
        "input": "hi",
        "status": "success",
        "result": "MOCK-RESULT",
        "started_at": 1_700_000_000,
        "ended_at": 1_700_000_001,
        "total_latency_ms": 5,
        "steps": [],
    });
    let list_response = serde_json::json!([
        { "name": "alpha", "description": "first", "version": 1, "path": "/tmp/alpha.workflow" },
        { "name": "beta",  "description": "second", "version": 1, "path": "/tmp/beta.workflow" },
    ]);
    let validate_err_response = serde_json::json!({
        "ok": false,
        "error": "validation error: undefined variable `ghost.output`",
    });
    let run_resp_arc = Arc::new(run_response.clone());
    let list_resp_arc = Arc::new(list_response.clone());
    let validate_resp_arc = Arc::new(validate_err_response.clone());
    coord_dispatch.register(
        "workflow.run",
        Arc::new(FnHandler({
            let r = run_resp_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    coord_dispatch.register(
        "workflow.list",
        Arc::new(FnHandler({
            let r = list_resp_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    coord_dispatch.register(
        "workflow.validate",
        Arc::new(FnHandler({
            let r = validate_resp_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    let coord_dispatch = Arc::new(coord_dispatch);

    let (_coord_client, coord_events, coord_addr) = boot_peer(212).await;
    spawn_inbound_loop(coord_events, coord_dispatch.clone());

    // ─── 2. Mint the bridge's identity, write to disk ───
    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes =
        mint_bridge_bundle_bytes(&org_root, "wf-test-bridge", vec!["operators".into()]);
    let bundle_path = tmpdir.path().join("bridge.bundle");
    std::fs::write(&bundle_path, &bundle_bytes).expect("write bundle");
    let client_key_path = tmpdir.path().join("client.key");
    let chat_template_path = tmpdir.path().join("chat.sol");
    std::fs::write(
        &chat_template_path,
        r#"function start() -> str { return remote_call("coordinator", "noop", "{{SESSION}}|{{MESSAGE}}|"); }"#,
    )
    .expect("write chat template");
    let peers_path = tmpdir.path().join("peers.toml");
    std::fs::write(
        &peers_path,
        format!(
            r#"
[peers.coordinator]
addr = "{}"
"#,
            coord_addr
        ),
    )
    .expect("write peers");

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

    // ─── 3. Build a real MeshClient pointed at the fake coord ───
    use relix_runtime::flow_runner::{PeerEntry, PeersFile};
    use relix_runtime::manifest::{DiscoveryOptions, discover_and_pin};
    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        "coordinator".to_string(),
        PeerEntry {
            addr: coord_addr.to_string(),
        },
    );
    let peers_file = PeersFile { peers: peers_map };
    let bridge_identity = base_state.identity_bundle.clone();
    let bridge_key = base_state.client_key.clone();
    let opts = DiscoveryOptions {
        identity_bundle: bridge_identity.clone(),
        client_key: bridge_key,
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

    // ─── 4. Mount the workflow routes on an ephemeral listener ───
    let app = Router::new()
        .route("/v1/workflows", get(crate::workflows::list))
        .route("/v1/workflows/run", post(crate::workflows::run))
        .route("/v1/workflows/validate", post(crate::workflows::validate))
        .route("/v1/workflows/reload", post(crate::workflows::reload))
        .route(
            "/v1/workflows/status/:execution_id",
            get(crate::workflows::status),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 0");
    let bound = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();

    // ─── 5. GET /v1/workflows → 200 + list ───
    let url = format!("http://{}/v1/workflows", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .expect("GET /v1/workflows returned within 15s")
        .expect("GET succeeded");
    assert_eq!(resp.status().as_u16(), 200, "list expected 200");
    let body_bytes = resp.bytes().await.expect("list body");
    let parsed: Value = serde_json::from_slice(&body_bytes).expect("list JSON");
    assert_eq!(parsed, list_response, "list body mismatch");

    // ─── 6. POST /v1/workflows/run → 200 + execution record ───
    let url = format!("http://{}/v1/workflows/run", bound);
    let body = serde_json::json!({
        "name": "test-workflow",
        "input": "hi",
    })
    .to_string();
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .header("Content-Type", "application/json")
            .body(body)
            .send(),
    )
    .await
    .expect("POST /v1/workflows/run returned within 15s")
    .expect("POST succeeded");
    assert_eq!(resp.status().as_u16(), 200, "run expected 200");
    let body_bytes = resp.bytes().await.expect("run body");
    let parsed: Value = serde_json::from_slice(&body_bytes).expect("run JSON");
    assert_eq!(parsed, run_response, "run body mismatch");

    // ─── 7. POST /v1/workflows/validate (failure body) → 400 + clear error ───
    let url = format!("http://{}/v1/workflows/validate", bound);
    let body = serde_json::json!({
        "source": "name: bad\nversion: 1\n",
    })
    .to_string();
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .header("Content-Type", "application/json")
            .body(body)
            .send(),
    )
    .await
    .expect("POST /v1/workflows/validate returned within 15s")
    .expect("POST succeeded");
    assert_eq!(resp.status().as_u16(), 400, "validate (bad) expected 400");
    let body_bytes = resp.bytes().await.expect("validate body");
    let parsed: Value = serde_json::from_slice(&body_bytes).expect("validate JSON");
    assert_eq!(parsed.get("ok").and_then(Value::as_bool), Some(false));
    let err_str = parsed
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    assert!(
        err_str.contains("validation error"),
        "expected `validation error` in body; got: {err_str}"
    );
}
