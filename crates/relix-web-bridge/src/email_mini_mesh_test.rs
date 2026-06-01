//! End-to-end mini-mesh integration tests for the email
//! HTTP bridge endpoints.
//!
//! Boots a tiny fake "email" peer that registers
//! `email.send`, `email.send_template`, and `email.status`
//! with canned responses, builds a real `MeshClient` pointing
//! at it via `discover_and_pin`, mounts the three email routes
//! on an ephemeral axum listener, and drives reqwest requests
//! through the stack.
//!
//! Coverage:
//!
//!   * `POST /v1/email/send` with a valid body → 200 + Message-ID.
//!   * `POST /v1/email/send` with missing `to` → 400 + clear error.
//!   * `GET  /v1/email/status` → 200 + parsed connection state.

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
                eprintln!("email-mini-mesh: boot_peer retry ({e})");
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
        &dir.path().join("email-audit.log"),
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
async fn email_mini_mesh_all_three_endpoints() {
    // ─── 1. Boot a fake email peer with canned email.* ───
    let (mut email_dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "ops_send"
        method = "email.send"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_send_template"
        method = "email.send_template"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_status"
        method = "email.status"
        allow_groups = ["operators"]
        "#,
    );
    let send_resp = serde_json::json!({ "message_id": "abc123@example.com" });
    let send_template_resp = serde_json::json!({
        "message_id": "tpl456@example.com",
        "template": "welcome",
    });
    let send_resp_arc = Arc::new(send_resp.clone());
    let send_template_resp_arc = Arc::new(send_template_resp.clone());
    email_dispatch.register(
        "email.send",
        Arc::new(FnHandler({
            let r = send_resp_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    email_dispatch.register(
        "email.send_template",
        Arc::new(FnHandler({
            let r = send_template_resp_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    let status_body = b"smtp=connected|imap=connected|from=Relix <bot@example.com>|smtp_host=smtp.e|imap_host=imap.e|imap_folder=INBOX|messages_seen=2|messages_sent=3|last_send_at=1700000000|last_poll_at=1700000010|last_message_at=1700000020|smtp_error=|imap_error=\n".to_vec();
    let status_body_arc = Arc::new(status_body);
    email_dispatch.register(
        "email.status",
        Arc::new(FnHandler({
            let r = status_body_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok((*r).clone()) }
            }
        })),
    );
    let email_dispatch = Arc::new(email_dispatch);

    let (_email_client, email_events, email_addr) = boot_peer(178).await;
    spawn_inbound_loop(email_events, email_dispatch.clone());

    // ─── 2. Mint the bridge's identity, write to disk ───
    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes =
        mint_bridge_bundle_bytes(&org_root, "email-test-bridge", vec!["operators".into()]);
    let bundle_path = tmpdir.path().join("bridge.bundle");
    std::fs::write(&bundle_path, &bundle_bytes).expect("write bundle");
    let client_key_path = tmpdir.path().join("client.key");
    let chat_template_path = tmpdir.path().join("chat.sol");
    // The bridge AppState validates that the chat template
    // contains {{SESSION}} and {{MESSAGE}} placeholders even
    // though we never actually run the chat flow in this test.
    // Drop in a placeholder template that satisfies the
    // validator.
    std::fs::write(
        &chat_template_path,
        r#"function start() -> str { return remote_call("email", "noop", "{{SESSION}}|{{MESSAGE}}|"); }"#,
    )
    .expect("write chat template");
    let peers_path = tmpdir.path().join("peers.toml");
    std::fs::write(
        &peers_path,
        format!(
            r#"
[peers.email]
addr = "{}"
"#,
            email_addr
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

    // ─── 3. Build a real MeshClient pointed at the fake email peer ───
    use relix_runtime::flow_runner::{PeerEntry, PeersFile};
    use relix_runtime::manifest::{DiscoveryOptions, discover_and_pin};
    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        "email".to_string(),
        PeerEntry {
            addr: email_addr.to_string(),
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

    // ─── 4. Mount the email routes on an ephemeral listener ───
    let app = Router::new()
        .route("/v1/email/send", post(crate::email::send))
        .route("/v1/email/send_template", post(crate::email::send_template))
        .route("/v1/email/status", get(crate::email::status))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 0");
    let bound = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();

    // ─── 5. POST /v1/email/send with valid body → 200 + Message-ID ───
    let url = format!("http://{}/v1/email/send", bound);
    let body = serde_json::json!({
        "to": ["alice@example.com"],
        "subject": "hi",
        "body": "hello",
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
    .expect("POST /v1/email/send returned within 15s")
    .expect("POST succeeded");
    assert_eq!(resp.status().as_u16(), 200, "send expected 200");
    let body_bytes = resp.bytes().await.expect("send body");
    let parsed: Value = serde_json::from_slice(&body_bytes).expect("send JSON");
    assert_eq!(
        parsed.get("message_id").and_then(Value::as_str),
        Some("abc123@example.com")
    );

    // ─── 6. POST /v1/email/send with missing `to` → 400 ───
    let url = format!("http://{}/v1/email/send", bound);
    let body = serde_json::json!({
        "subject": "hi",
        "body": "hello",
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
    .expect("POST /v1/email/send (missing to) returned within 15s")
    .expect("POST succeeded");
    assert_eq!(resp.status().as_u16(), 400, "missing to expected 400");
    let body_bytes = resp.bytes().await.expect("missing-to body");
    let parsed: Value = serde_json::from_slice(&body_bytes).expect("missing-to JSON");
    let err_str = parsed
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    assert!(
        err_str.contains("to"),
        "missing-to error should mention `to`, got {err_str:?}",
    );

    // ─── 7. POST /v1/email/send with missing subject → 400 ───
    let url = format!("http://{}/v1/email/send", bound);
    let body = serde_json::json!({
        "to": ["alice@example.com"],
        "body": "hello",
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
    .expect("POST /v1/email/send (missing subject) returned within 15s")
    .expect("POST succeeded");
    assert_eq!(resp.status().as_u16(), 400, "missing subject expected 400");

    // ─── 8. POST /v1/email/send_template → 200 + Message-ID + template name ───
    let url = format!("http://{}/v1/email/send_template", bound);
    let body = serde_json::json!({
        "template_name": "welcome",
        "to": ["alice@example.com"],
        "variables": { "name": "Alice" },
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
    .expect("POST /v1/email/send_template returned within 15s")
    .expect("POST succeeded");
    assert_eq!(resp.status().as_u16(), 200, "send_template expected 200");
    let body_bytes = resp.bytes().await.expect("send_template body");
    let parsed: Value = serde_json::from_slice(&body_bytes).expect("send_template JSON");
    assert_eq!(
        parsed.get("message_id").and_then(Value::as_str),
        Some("tpl456@example.com")
    );
    assert_eq!(
        parsed.get("template").and_then(Value::as_str),
        Some("welcome")
    );

    // ─── 9. GET /v1/email/status → 200 + parsed connection state ───
    let url = format!("http://{}/v1/email/status", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .expect("GET /v1/email/status returned within 15s")
        .expect("GET succeeded");
    assert_eq!(resp.status().as_u16(), 200, "status expected 200");
    let body_bytes = resp.bytes().await.expect("status body");
    let parsed: Value = serde_json::from_slice(&body_bytes).expect("status JSON");
    assert_eq!(
        parsed.get("smtp").and_then(Value::as_str),
        Some("connected")
    );
    assert_eq!(
        parsed.get("imap").and_then(Value::as_str),
        Some("connected")
    );
    assert_eq!(
        parsed.get("from").and_then(Value::as_str),
        Some("Relix <bot@example.com>")
    );
    assert_eq!(parsed.get("messages_seen").and_then(Value::as_u64), Some(2));
    assert_eq!(parsed.get("messages_sent").and_then(Value::as_u64), Some(3));
    assert_eq!(
        parsed.get("last_send_at").and_then(Value::as_i64),
        Some(1_700_000_000)
    );
    assert!(
        parsed
            .get("smtp_error")
            .map(|v| v.is_null())
            .unwrap_or(false)
    );
}
