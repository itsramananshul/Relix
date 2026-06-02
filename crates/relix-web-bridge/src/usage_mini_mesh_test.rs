//! RELA-23: end-to-end mini-mesh integration test for the
//! unary `/v1/chat/completions` usage contract.
//!
//! What this pins that the serialization unit tests don't:
//!
//!   * the **HTTP** edge: a real axum router with the
//!     `POST /v1/chat/completions` route on a loopback port,
//!   * the **bridge** `AppState` built via `AppState::try_new`
//!     from on-disk config (identity bundle, peers file, unary
//!     chat SOL template),
//!   * the **bridge to AI** libp2p path: FlowRunner brings up
//!     its own ephemeral peer, dials the `ai` alias, and the
//!     registered `ai.chat` handler returns a plain-text reply
//!     exactly like the production handler (which routes its
//!     real token counts out-of-band to the metrics sink, so
//!     they never travel this wire).
//!
//! The assertion is the honest-omission contract: a real chat
//! response carries no `usage` object, because the bridge has
//! no real token counts at this layer. The pre-fix code emitted
//! a fabricated `0/0/0`; this test fails if that regresses.
//!
//! Coordinator is intentionally NOT booted: `task_recorder` is
//! `None`, exercising the documented fail-soft path. That is the
//! test's honest scope statement; the coordinator path has its
//! own coverage elsewhere.

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
                eprintln!("usage-mini-mesh: boot_peer retry ({e})");
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
        &dir.path().join("ai-audit.log"),
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
async fn unary_chat_completion_omits_usage_over_mini_mesh() {
    // ─── 1. Boot the AI peer + register a unary `ai.chat` ───
    // The handler returns a plain-text reply body, exactly like
    // the production `ai.chat` (which never puts token counts in
    // the response body).
    let (mut ai_dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "operators_can_chat"
        method = "ai.chat"
        allow_groups = ["operators"]
        "#,
    );
    let reply_text = "Hello from unary mini-mesh";
    ai_dispatch.register(
        "ai.chat",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| async move {
            HandlerOutcome::Ok(reply_text.as_bytes().to_vec())
        })),
    );
    let ai_dispatch = Arc::new(ai_dispatch);

    let (_ai_client, ai_events, ai_addr) = boot_peer(173).await;
    spawn_inbound_loop(ai_events, ai_dispatch.clone());

    // ─── 2. Build the bridge's identity + on-disk config ───
    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes =
        mint_bridge_bundle_bytes(&org_root, "usage-test-bridge", vec!["operators".into()]);
    let bundle_path = tmpdir.path().join("bridge.bundle");
    std::fs::write(&bundle_path, &bundle_bytes).expect("write bundle");
    let client_key_path = tmpdir.path().join("client.key");

    let peers_path = tmpdir.path().join("peers.toml");
    std::fs::write(
        &peers_path,
        format!(
            r#"
[peers.ai]
addr = "{ai_addr}"
"#
        ),
    )
    .expect("write peers");

    // Unary chat template: a single `ai.chat` remote_call whose
    // result becomes the reply. No streaming template, so the
    // request takes the unary JSON path that builds the
    // `ChatCompletionResponse` (the struct carrying `usage`).
    let chat_template_path = tmpdir.path().join("chat.sol");
    std::fs::write(
        &chat_template_path,
        r#"
function start() -> str {
    return remote_call("ai", "ai.chat", "{{SESSION}}|{{MESSAGE}}|");
}
"#,
    )
    .expect("write chat template");

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
    let state = AppState::try_new(cfg).expect("AppState::try_new");

    // ─── 3. Stand up a thin axum router with just the route ───
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(crate::openai::chat_completions),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 0");
    let bound = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // ─── 4. POST a non-streaming chat completion request ───
    let url = format!("http://{bound}/v1/chat/completions");
    let body = serde_json::json!({
        "model": "relix-mini-mesh",
        "messages": [{"role": "user", "content": "hi mini-mesh"}],
    })
    .to_string();
    let http = reqwest::Client::new();
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .header("Content-Type", "application/json")
            .body(body)
            .send(),
    )
    .await
    .expect("HTTP POST returned within 15s")
    .expect("HTTP POST succeeded");
    assert_eq!(resp.status().as_u16(), 200, "expected 200 OK from bridge");

    let v: Value = timeout(Duration::from_secs(15), resp.json())
        .await
        .expect("JSON body within 15s")
        .expect("JSON body parsed");

    // ─── 5. The honest-omission contract ───
    // No real token counts reach the bridge, so the response
    // must NOT carry a `usage` object at all (the pre-fix code
    // emitted a fabricated 0/0/0 here).
    assert!(
        v.get("usage").is_none(),
        "usage must be omitted on a real chat path, got: {v}"
    );
    // Sanity: the reply itself round-tripped through the mesh.
    assert_eq!(
        v["choices"][0]["message"]["content"]
            .as_str()
            .expect("reply content present"),
        reply_text,
        "reply should be the AI peer's output; body: {v}"
    );
}
