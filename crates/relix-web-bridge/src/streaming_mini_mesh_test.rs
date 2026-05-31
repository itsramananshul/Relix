//! End-to-end mini-mesh integration test for the RELIX-2
//! streaming chat-completions surface.
//!
//! What this test exercises that the lower-layer tests don't:
//!
//!   * the **HTTP** edge — a real axum router with the
//!     `POST /v1/chat/completions` route bound to a random
//!     loopback port,
//!   * the **bridge** `AppState` built via `AppState::try_new`
//!     from on-disk config (identity bundle, peers file,
//!     streaming SOL template),
//!   * the **bridge → AI** libp2p substream — every layer
//!     between the SSE writer and the streaming handler is
//!     the real production code path (FlowRunner →
//!     RealDispatcher → `/relix/rpc/stream/1` substream →
//!     DispatchBridge admission → registered streaming
//!     handler),
//!   * the **SSE wire shape** — role marker, ordered content
//!     chunks, `finish_reason: stop`, and the literal
//!     `[DONE]` sentinel.
//!
//! The AI peer is booted with a permissive policy + a custom
//! streaming handler that yields a fixed three-chunk reply —
//! the real `ai.chat.stream` (with planner / memory / guardrail
//! pre-flight) is exercised in `nodes::ai` unit tests; this
//! test isolates the transport + bridge + HTTP path.
//!
//! Coordinator is intentionally NOT booted: `task_recorder`
//! is `None` on this AppState, exercising the documented
//! fail-soft path. The streaming SOL template skips
//! `memory.write_turn` for the same reason (no memory peer).
//! These omissions are the test's honest scope statement —
//! coordinator + memory paths have their own integration
//! coverage elsewhere.

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::post;
use ed25519_dalek::SigningKey;
use futures::StreamExt;
use rand::rngs::OsRng;
use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_core::identity::{IdentityBundle, issue_identity};
use relix_core::policy::PolicyEngine;
use relix_core::types::{ErrorEnvelope, NodeId};
use relix_runtime::dispatch::{DispatchBridge, FnStreamingHandler, HandlerStream, InvocationCtx};
use relix_runtime::transport::rpc::{self, Multiaddr};
use relix_runtime::transport::stream::StreamWriter;
use tempfile::TempDir;
use tokio::time::timeout;

use crate::config::{
    AppState, BridgeConfig, BridgeSection, FlowSection, IdentitySection, MeshSection, SseSection,
    TransportSection,
};

// ────────────────────────────── helpers ──────────────────────────

fn key_for(seed: u8) -> [u8; 32] {
    let mut k = [0u8; 32];
    for (i, slot) in k.iter_mut().enumerate() {
        *slot = seed.wrapping_add(i as u8);
    }
    k
}

/// Boot a peer on a random localhost port and spawn its swarm
/// event loop. Mirrors the helper in
/// `crates/relix-runtime/tests/transport_stream.rs` — kept
/// duplicated rather than re-exported because the runtime
/// test crate is a separate target.
async fn boot_peer(seed: u8) -> (rpc::Client, Multiaddr) {
    for _ in 0..16 {
        let port: u16 = 35_000 + (rand::random::<u16>() % 25_000);
        match rpc::new(key_for(seed), port).await {
            Ok((client, mut events, event_loop)) => {
                let listen_addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}")
                    .parse()
                    .expect("valid multiaddr");
                tokio::spawn(event_loop.run());
                tokio::spawn(async move { while events.recv().await.is_some() {} });
                return (client, listen_addr);
            }
            Err(e) => {
                eprintln!("boot_peer: bind on random port failed ({e}); retrying");
                continue;
            }
        }
    }
    panic!("boot_peer: exhausted port retries; system has no free ephemeral ports");
}

/// Build a DispatchBridge for the AI peer with the supplied
/// policy. Returns the org_root SigningKey so the test can
/// mint a bundle the bridge will accept.
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

/// Mint a fresh identity bundle for the bridge, signed by
/// `org_root`. Returns the encoded bytes ready to write to
/// the bundle file `BridgeConfig` points at.
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

/// Wire the streaming-accept loop the controller_runtime
/// spawns at startup. Mirrors the helper in the runtime
/// crate's transport_stream tests.
fn spawn_streaming_accept_task(client: &rpc::Client, bridge: Arc<DispatchBridge>) {
    let mut incoming = client
        .accept_streams()
        .expect("accept_streams: protocol must not be pre-registered");
    tokio::spawn(async move {
        while let Some((_peer, raw_stream)) = incoming.next().await {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                let mut writer = StreamWriter::new(raw_stream);
                let envelope = match writer.read_request_envelope().await {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        eprintln!("mini-mesh: caller closed before envelope ({e})");
                        return;
                    }
                };
                bridge.handle_inbound_stream(envelope, writer).await;
            });
        }
    });
}

// ────────────────────────────── the test ──────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn end_to_end_streaming_chat_completions_through_mini_mesh() {
    // ─── 1. Boot the AI peer + register `ai.chat.stream` ───
    let (mut ai_dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "operators_can_stream_ai_chat"
        method = "ai.chat.stream"
        allow_groups = ["operators"]
        "#,
    );
    ai_dispatch.register_streaming(
        "ai.chat.stream",
        Arc::new(FnStreamingHandler(|_ctx: InvocationCtx| async move {
            let chunks: Vec<Result<Vec<u8>, ErrorEnvelope>> = vec![
                Ok(b"Hello".to_vec()),
                Ok(b" from".to_vec()),
                Ok(b" mini-mesh".to_vec()),
            ];
            Ok(Box::pin(futures::stream::iter(chunks)) as HandlerStream)
        })),
    );
    let ai_dispatch = Arc::new(ai_dispatch);

    let (ai_client, ai_addr) = boot_peer(101).await;
    spawn_streaming_accept_task(&ai_client, ai_dispatch.clone());

    // ─── 2. Build the bridge's identity + on-disk config ───
    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes = mint_bridge_bundle_bytes(&org_root, "test-bridge", vec!["operators".into()]);
    let bundle_path = tmpdir.path().join("bridge.bundle");
    std::fs::write(&bundle_path, &bundle_bytes).expect("write bundle");
    // `client_key_path` doesn't have to exist — try_new
    // generates a 32-byte key on first read miss.
    let client_key_path = tmpdir.path().join("client.key");

    // Peers TOML: just the one "ai" alias pointing at the
    // peer we just booted. The libp2p noise handshake
    // exchanges the responder's public key during dial, so
    // the addr alone (no /p2p/<peerid> suffix) is enough.
    let peers_path = tmpdir.path().join("peers.toml");
    std::fs::write(
        &peers_path,
        format!(
            r#"
[peers.ai]
addr = "{}"
"#,
            ai_addr
        ),
    )
    .expect("write peers");

    // Unary chat template (required by BridgeConfig even
    // though the test only uses the streaming path).
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

    // Streaming chat template. Simplified vs the production
    // `flows/chat_template_streaming.sol`: no
    // `memory.write_turn` calls because this test doesn't
    // boot a memory peer. AppState::try_new validates that
    // the streaming template invokes `remote_call_stream`
    // and contains both placeholders.
    let streaming_template_path = tmpdir.path().join("stream.sol");
    std::fs::write(
        &streaming_template_path,
        r#"
function start() -> str {
    let reply: str = remote_call_stream("ai", "ai.chat.stream", "{{SESSION}}|" + "{{MESSAGE}}" + "|");
    return reply;
}
"#,
    )
    .expect("write streaming template");

    // ─── 3. Build BridgeConfig + AppState ───
    // `listen_addr` is parsed into `bridge_host` / `bridge_port`
    // for the CSRF middleware. The integration test serves on
    // a separate :0 listener; the bridge's listen_addr field
    // is therefore inert here.
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
            streaming_template_path: Some(streaming_template_path),
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

    // ─── 4. Stand up a thin axum router with just the one
    //         route. No auth middleware: this test pins the
    //         streaming path, not the auth layer (which has
    //         its own unit tests in `crate::auth`). ───
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

    // No pre-dial needed: FlowRunner brings up its own
    // ephemeral libp2p peer when `mesh_client` is None (the
    // default after `AppState::try_new`, since
    // `discover_and_pin` isn't run in this test), and dials
    // the "ai" alias from the peers.toml on every call.

    // ─── 5. POST a streaming chat completion request ───
    let url = format!("http://{}/v1/chat/completions", bound);
    let body = serde_json::json!({
        "model": "relix-mini-mesh",
        "messages": [{"role": "user", "content": "hi mini-mesh"}],
        "stream": true,
    })
    .to_string();
    let http = reqwest::Client::new();
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .body(body)
            .send(),
    )
    .await
    .expect("HTTP POST returned within 15s")
    .expect("HTTP POST succeeded");
    assert_eq!(resp.status().as_u16(), 200, "expected 200 OK from bridge");
    let ct = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default();
    assert!(
        ct.contains("text/event-stream"),
        "content-type should be SSE, got `{ct}`"
    );

    // Collect the SSE body. The flow streams 3 chunks, then
    // emits a finish frame + `[DONE]`. Total wall-clock is
    // well under a second; cap at 15s for CI safety.
    let body_text = timeout(Duration::from_secs(15), resp.text())
        .await
        .expect("SSE body returned within 15s")
        .expect("SSE body collected");

    // ─── 6. Parse and assert on the OpenAI SSE wire shape ───
    let mut content_chunks: Vec<String> = Vec::new();
    let mut got_role = false;
    let mut got_finish = false;
    let mut got_done = false;
    let mut got_relix_metadata = false;
    for line in body_text.lines() {
        let line = line.trim_end();
        if !line.starts_with("data:") {
            continue;
        }
        // SSE wire shape: `data: <payload>`. Some lines may
        // be `data:<payload>` without the space depending on
        // the writer; tolerate both.
        let payload = line
            .strip_prefix("data: ")
            .or_else(|| line.strip_prefix("data:"))
            .unwrap_or("");
        if payload.trim() == "[DONE]" {
            got_done = true;
            continue;
        }
        if payload.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(payload)
            .unwrap_or_else(|e| panic!("parse chunk JSON failed ({e}): {payload}"));
        let choices = v
            .get("choices")
            .and_then(|c| c.as_array())
            .expect("choices array");
        let choice = &choices[0];
        let delta = choice
            .get("delta")
            .and_then(|d| d.as_object())
            .expect("delta object");
        if delta.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            got_role = true;
        }
        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
            content_chunks.push(content.to_string());
        }
        if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str())
            && reason == "stop"
        {
            got_finish = true;
        }
        if v.get("relix").is_some() {
            got_relix_metadata = true;
        }
    }

    assert!(got_role, "missing role marker chunk; body:\n{body_text}");
    let reassembled: String = content_chunks.join("");
    assert_eq!(
        reassembled, "Hello from mini-mesh",
        "content chunks should reassemble to the handler's output"
    );
    assert!(
        got_finish,
        "missing finish_reason=stop chunk; body:\n{body_text}"
    );
    assert!(
        got_relix_metadata,
        "final chunk should carry relix metadata envelope; body:\n{body_text}"
    );
    assert!(got_done, "missing [DONE] sentinel; body:\n{body_text}");
}
