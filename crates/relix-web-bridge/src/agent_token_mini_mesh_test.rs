//! REL-20 mini-mesh integration tests for the identity REST API.
//!
//! Boots a fake coordinator that registers both `agent.*` and
//! `identity.issue_token` caps with canned responses, wires a
//! real `MeshClient` against it, and asserts the full HTTP
//! surface specified by REL-20:
//!
//! - `POST /v1/agents`              → 200 with `agent_id` + `token`.
//! - `POST /v1/agents/:id/tokens`   → 200 with `agent_id` + `token`.
//! - `POST /v1/agents/:id/tokens`   → 404 when agent unknown.
//! - Token field absent from `POST /v1/agents` when coordinator
//!   has no `identity.issue_token` cap (graceful degradation).

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
                eprintln!("agent-token-mini-mesh: boot_peer retry ({e})");
                continue;
            }
        }
    }
    panic!("boot_peer: exhausted port retries");
}

fn fresh_responder_bridge() -> (DispatchBridge, SigningKey, TempDir) {
    let dir = TempDir::new().unwrap();
    let org_root = SigningKey::generate(&mut OsRng);
    let responder = SigningKey::generate(&mut OsRng);
    let policy = PolicyEngine::from_toml(
        r#"
        [[rules]]
        name = "allow_agent_create"
        method = "agent.create"
        allow_groups = ["agents"]

        [[rules]]
        name = "allow_agent_get"
        method = "agent.get"
        allow_groups = ["agents"]

        [[rules]]
        name = "allow_agent_list"
        method = "agent.list"
        allow_groups = ["agents"]

        [[rules]]
        name = "allow_agent_update"
        method = "agent.update"
        allow_groups = ["agents"]

        [[rules]]
        name = "allow_agent_delete"
        method = "agent.delete"
        allow_groups = ["agents"]

        [[rules]]
        name = "allow_identity_issue_token"
        method = "identity.issue_token"
        allow_groups = ["agents"]

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
        &dir.path().join("agent-token-audit.log"),
        responder,
    )
    .expect("bridge constructs");
    (bridge, org_root, dir)
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

const FAKE_AGENT_ID: &str = "agt_test_abc123";
const FAKE_AGENT_NAME: &str = "TestAgent";
const FAKE_WIRE_TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.fake_wire_token";

/// Register canned `agent.create`, `agent.get`, `agent.delete`,
/// `agent.list`, `agent.update`, and `identity.issue_token` caps.
fn register_caps(dispatch: &mut DispatchBridge) {
    // agent.create — returns the fixed fake agent id
    dispatch.register(
        "agent.create",
        Arc::new(FnHandler(|_ctx: InvocationCtx| async move {
            HandlerOutcome::Ok(format!("{FAKE_AGENT_ID}\n").into_bytes())
        })),
    );

    // agent.get — returns a minimal detail body for the fake id;
    // `not found` for anything else
    dispatch.register(
        "agent.get",
        Arc::new(FnHandler(|ctx: InvocationCtx| async move {
            let id = std::str::from_utf8(&ctx.args)
                .unwrap_or("")
                .trim()
                .to_string();
            if id == FAKE_AGENT_ID {
                let body = format!(
                    "agent_id={}|name={}|role=research|title=Junior|department=rd|team=ops\
                     |created_by=alice|status=active|subject_id=subj-1|risk_ceiling=medium\
                     |approval_timeout_secs=86400|created_at=0|updated_at=0\
                     |surface_allowlist=|allow_categories=|deny_categories=\
                     |allow_sensitivity_tags=|deny_sensitivity_tags=\
                     |approval_required_categories=\n",
                    FAKE_AGENT_ID, FAKE_AGENT_NAME
                );
                HandlerOutcome::Ok(body.into_bytes())
            } else {
                HandlerOutcome::Err(ErrorEnvelope {
                    kind: error_kinds::INVALID_ARGS,
                    cause: format!("agent.get: not found: {id}"),
                    retry_hint: 0,
                    retry_after: None,
                })
            }
        })),
    );

    // identity.issue_token — returns a canned wire token
    dispatch.register(
        "identity.issue_token",
        Arc::new(FnHandler(|_ctx: InvocationCtx| async move {
            let body = serde_json::json!({
                "wire": FAKE_WIRE_TOKEN,
                "token": {
                    "session_id": FAKE_AGENT_ID,
                    "agent_name": FAKE_AGENT_NAME,
                }
            });
            HandlerOutcome::Ok(serde_json::to_vec(&body).unwrap())
        })),
    );
}

/// Build an `AppState` + `MeshClient` pointed at the fake coordinator.
async fn build_state(org_root: &SigningKey, addr: Multiaddr, tmpdir: &TempDir) -> AppState {
    let bundle_bytes = mint_bridge_bundle_bytes(org_root, "agent-token-test-bridge");
    let bundle_path = tmpdir.path().join("bridge.bundle");
    std::fs::write(&bundle_path, &bundle_bytes).unwrap();
    let client_key_path = tmpdir.path().join("client.key");
    let chat_template_path = tmpdir.path().join("chat.sol");
    // Template must contain {{SESSION}} and {{MESSAGE}} placeholders
    // to pass AppState::try_new validation.
    std::fs::write(
        &chat_template_path,
        r#"function start() -> str { let _s = "{{SESSION}}"; let _m = "{{MESSAGE}}"; return remote_call("coord","noop",""); }"#,
    )
    .unwrap();
    let peers_path = tmpdir.path().join("peers.toml");
    std::fs::write(
        &peers_path,
        format!("[peers.coordinator]\naddr = \"{addr}\"\n"),
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
    AppState {
        mesh_client: Some(Arc::new(mesh)),
        ..base_state
    }
}

fn mount_agent_routes(state: AppState) -> Router {
    Router::new()
        .route(
            "/v1/agents",
            get(crate::agent::list_agents).post(crate::agent::create_agent),
        )
        .route(
            "/v1/agents/:agent_id/tokens",
            post(crate::agent::issue_agent_token),
        )
        .route(
            "/v1/agents/:agent_id",
            get(crate::agent::get_agent)
                .patch(crate::agent::update_agent)
                .delete(crate::agent::delete_agent),
        )
        .with_state(state)
}

// ── tests ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_agents_returns_agent_id_and_token() {
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge();
    register_caps(&mut dispatch);
    let dispatch = Arc::new(dispatch);
    let (_peer_client, events, addr) = boot_peer(171).await;
    spawn_inbound_loop(events, dispatch);

    let tmpdir = TempDir::new().unwrap();
    let state = build_state(&org_root, addr, &tmpdir).await;
    let app = mount_agent_routes(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();
    let url = format!("http://{bound}/v1/agents");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "name": "TestAgent",
                "role": "research",
                "title": "Junior",
                "department": "rd",
                "team": "ops",
                "created_by": "alice",
                "subject_id": "subj-1"
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(resp.status().as_u16(), 200, "create must return 200");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body.get("agent_id").and_then(Value::as_str),
        Some(FAKE_AGENT_ID),
        "agent_id must match coordinator response"
    );
    assert_eq!(
        body.get("token").and_then(Value::as_str),
        Some(FAKE_WIRE_TOKEN),
        "token must be the wire token from identity.issue_token"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_agents_id_tokens_returns_fresh_token() {
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge();
    register_caps(&mut dispatch);
    let dispatch = Arc::new(dispatch);
    let (_peer_client, events, addr) = boot_peer(172).await;
    spawn_inbound_loop(events, dispatch);

    let tmpdir = TempDir::new().unwrap();
    let state = build_state(&org_root, addr, &tmpdir).await;
    let app = mount_agent_routes(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();
    let url = format!("http://{bound}/v1/agents/{FAKE_AGENT_ID}/tokens");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url).json(&serde_json::json!({})).send(),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(resp.status().as_u16(), 200, "issue token must return 200");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body.get("agent_id").and_then(Value::as_str),
        Some(FAKE_AGENT_ID)
    );
    assert_eq!(
        body.get("token").and_then(Value::as_str),
        Some(FAKE_WIRE_TOKEN),
        "token must be the wire token from identity.issue_token"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_agents_id_tokens_returns_404_for_unknown_agent() {
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge();
    register_caps(&mut dispatch);
    let dispatch = Arc::new(dispatch);
    let (_peer_client, events, addr) = boot_peer(173).await;
    spawn_inbound_loop(events, dispatch);

    let tmpdir = TempDir::new().unwrap();
    let state = build_state(&org_root, addr, &tmpdir).await;
    let app = mount_agent_routes(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();
    let url = format!("http://{bound}/v1/agents/agt_does_not_exist/tokens");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url).json(&serde_json::json!({})).send(),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(resp.status().as_u16(), 404, "unknown agent must yield 404");
    let body: Value = resp.json().await.unwrap();
    assert!(
        body.get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("not found"),
        "error body must surface `not found`: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_agents_omits_token_when_identity_cap_unavailable() {
    // Coordinator registers only `agent.*` — no `identity.issue_token`.
    // Bridge must still create the agent and return 200 with
    // `agent_id` present; `token` field must be absent (graceful
    // degradation, not a hard error).
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge();
    dispatch.register(
        "agent.create",
        Arc::new(FnHandler(|_ctx: InvocationCtx| async move {
            HandlerOutcome::Ok(format!("{FAKE_AGENT_ID}\n").into_bytes())
        })),
    );
    // identity.issue_token NOT registered — bridge gets UNKNOWN_METHOD.
    let dispatch = Arc::new(dispatch);
    let (_peer_client, events, addr) = boot_peer(174).await;
    spawn_inbound_loop(events, dispatch);

    let tmpdir = TempDir::new().unwrap();
    let state = build_state(&org_root, addr, &tmpdir).await;
    let app = mount_agent_routes(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();
    let url = format!("http://{bound}/v1/agents");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "name": "NoTokenAgent",
                "role": "research",
                "title": "Intern",
                "department": "rd",
                "team": "ops",
                "created_by": "alice",
                "subject_id": "subj-2"
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        200,
        "create must succeed even when identity cap absent"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body.get("agent_id").and_then(Value::as_str),
        Some(FAKE_AGENT_ID)
    );
    assert!(
        body.get("token").is_none(),
        "token must be absent when identity.issue_token cap is not registered"
    );
}
