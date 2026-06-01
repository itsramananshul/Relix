//! End-to-end mini-mesh integration tests for the RELIX-7.15
//! training data pipeline HTTP bridge endpoints.
//!
//! Boots a fake "coordinator" peer that registers the six
//! `training.*` capabilities with canned JSON responses, builds
//! a real `MeshClient` via `discover_and_pin`, mounts every
//! bridge route on an ephemeral axum listener, and drives
//! reqwest requests through the stack.
//!
//! Scenarios covered:
//!
//!   * `GET    /v1/training/stats`                      → 200 + aggregate
//!   * `GET    /v1/training/interactions`               → 200 + summaries
//!   * `GET    /v1/training/interactions/:id`           → 200 + full record
//!   * `GET    /v1/training/interactions/ghost`         → 404 + error body
//!   * `POST   /v1/training/score/:id`                  → 200 + score
//!   * `POST   /v1/training/export` (bad format)        → 400 + error body
//!   * `POST   /v1/training/export` (good format)       → 200 + export result
//!   * `DELETE /v1/training/interactions/:id`           → 200 + { deleted: true }
//!   * `DELETE /v1/training/interactions/ghost`         → 404

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::{delete, get, post};
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
                eprintln!("training-mini-mesh: boot_peer retry ({e})");
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
        &dir.path().join("training-audit.log"),
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
async fn training_mini_mesh_all_endpoints() {
    // ─── 1. Boot a fake coordinator with canned training.* ───
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "ops_list"
        method = "training.list_interactions"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_get"
        method = "training.get_interaction"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_export"
        method = "training.export"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_score"
        method = "training.score_interaction"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_stats"
        method = "training.stats"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_delete"
        method = "training.delete_interaction"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_pii_scan"
        method = "training.pii_scan"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_pii_preview"
        method = "training.anonymize_preview"
        allow_groups = ["operators"]
        "#,
    );

    let stats_response = serde_json::json!({
        "total": 100,
        "exported": 25,
        "average_quality_score": 0.78,
        "score_distribution": {
            "buckets": [1, 2, 3, 4, 10, 15, 20, 18, 17, 10],
            "unscored": 0
        },
        "by_agent": [
            {"label": "alice", "count": 60},
            {"label": "bob", "count": 40}
        ],
        "by_model": [
            {"label": "gpt-4o-mini", "count": 100}
        ]
    });
    let list_response = serde_json::json!([
        {
            "interaction_id": "abcd1234",
            "session_id": "s1",
            "agent": "alice",
            "model": "gpt-4o-mini",
            "provider": "openai",
            "latency_ms": 200,
            "success": true,
            "error_kind": null,
            "token_count": 100,
            "recorded_at": 1_700_000_000_000_i64,
            "quality_score": 0.85,
            "exported": false,
            "export_set": null,
            "user_preview": "what is rust?"
        }
    ]);
    let get_response = serde_json::json!({
        "interaction_id": "abcd1234",
        "session_id": "s1",
        "agent": "alice",
        "model": "gpt-4o-mini",
        "provider": "openai",
        "system_prompt": "you are alice",
        "user_message": "what is rust?",
        "response": "Rust is a systems language.",
        "tool_calls": [],
        "token_count": 100,
        "prompt_tokens": 40,
        "completion_tokens": 60,
        "latency_ms": 200,
        "success": true,
        "error_kind": null,
        "recorded_at": 1_700_000_000_000_i64,
        "quality_score": 0.85,
        "exported": false,
        "export_set": null
    });
    let score_response = serde_json::json!({
        "interaction_id": "abcd1234",
        "quality_score": 0.88
    });
    let export_response = serde_json::json!({
        "matched_count": 7,
        "exported_count": 7,
        "output_path": "/tmp/training_export_set_42.jsonl",
        "total_tokens": 700,
        "format": "openai",
        "export_set": "set-x"
    });
    let delete_response = serde_json::json!({
        "interaction_id": "abcd1234",
        "deleted": true
    });
    // RELIX-7.15 PII canned responses.
    let pii_scan_response = serde_json::json!({
        "spans": [
            { "pii_type": "EMAIL", "start": 14, "end": 31, "matched_text": "alice@example.com" }
        ],
        "count": 1
    });
    let pii_preview_response = serde_json::json!({
        "anonymized": "Contact me at [EMAIL]",
        "spans": [
            { "pii_type": "EMAIL", "start": 14, "end": 31, "matched_text": "alice@example.com" }
        ]
    });

    let stats_arc = Arc::new(stats_response.clone());
    let list_arc = Arc::new(list_response.clone());
    let get_arc = Arc::new(get_response.clone());
    let score_arc = Arc::new(score_response.clone());
    let export_arc = Arc::new(export_response.clone());
    let delete_arc = Arc::new(delete_response.clone());
    let pii_scan_arc = Arc::new(pii_scan_response.clone());
    let pii_preview_arc = Arc::new(pii_preview_response.clone());

    dispatch.register(
        "training.list_interactions",
        Arc::new(FnHandler({
            let r = list_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "training.get_interaction",
        Arc::new(FnHandler({
            let hit = get_arc.clone();
            move |ctx: InvocationCtx| {
                let hit = hit.clone();
                async move {
                    let body: Value =
                        serde_json::from_slice(&ctx.args).unwrap_or(serde_json::json!({}));
                    let id = body
                        .get("interaction_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if id == "abcd1234" {
                        HandlerOutcome::Ok(serde_json::to_vec(&*hit).unwrap_or_default())
                    } else {
                        HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                            kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                            cause: format!("training: no interaction with id {id:?}"),
                            retry_hint: 0,
                            retry_after: None,
                        })
                    }
                }
            }
        })),
    );
    dispatch.register(
        "training.export",
        Arc::new(FnHandler({
            let r = export_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "training.score_interaction",
        Arc::new(FnHandler({
            let r = score_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "training.stats",
        Arc::new(FnHandler({
            let r = stats_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "training.delete_interaction",
        Arc::new(FnHandler({
            let hit = delete_arc.clone();
            move |ctx: InvocationCtx| {
                let hit = hit.clone();
                async move {
                    let body: Value =
                        serde_json::from_slice(&ctx.args).unwrap_or(serde_json::json!({}));
                    let id = body
                        .get("interaction_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if id == "abcd1234" {
                        HandlerOutcome::Ok(serde_json::to_vec(&*hit).unwrap_or_default())
                    } else {
                        HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                            kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                            cause: format!("training: no interaction with id {id:?}"),
                            retry_hint: 0,
                            retry_after: None,
                        })
                    }
                }
            }
        })),
    );
    dispatch.register(
        "training.pii_scan",
        Arc::new(FnHandler({
            let r = pii_scan_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "training.anonymize_preview",
        Arc::new(FnHandler({
            let r = pii_preview_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    let dispatch = Arc::new(dispatch);

    let (_client, events, addr) = boot_peer(77).await;
    spawn_inbound_loop(events, dispatch.clone());

    // ─── 2. Mint bridge identity + plumb a real AppState ───
    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes =
        mint_bridge_bundle_bytes(&org_root, "training-test-bridge", vec!["operators".into()]);
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

    // ─── 3. Mount the routes ───
    let app = Router::new()
        .route(
            "/v1/training/interactions",
            get(crate::training::list_interactions),
        )
        .route(
            "/v1/training/interactions/:id",
            get(crate::training::get_interaction).delete(crate::training::delete_interaction),
        )
        .route("/v1/training/export", post(crate::training::export))
        .route(
            "/v1/training/score/:id",
            post(crate::training::score_interaction),
        )
        .route("/v1/training/stats", get(crate::training::stats))
        .route("/v1/training/pii/scan", post(crate::training::pii_scan))
        .route(
            "/v1/training/pii/preview",
            post(crate::training::pii_preview),
        )
        // Cover the GET DELETE on the same path with explicit
        // delete-only fallback (axum needs the trailing
        // `.delete(...)` in the same `.route` for verb routing;
        // we kept that above).
        .route(
            "/v1/training/_unused",
            delete(crate::training::delete_interaction),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();

    // ─── 4. GET /v1/training/stats → 200 + aggregate ───
    let url = format!("http://{}/v1/training/stats", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, stats_response);

    // ─── 5. GET /v1/training/interactions → 200 + summaries ───
    let url = format!(
        "http://{}/v1/training/interactions?page=1&page_size=10&agent=alice",
        bound
    );
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, list_response);

    // ─── 6. GET /v1/training/interactions/abcd1234 → 200 + full ───
    let url = format!("http://{}/v1/training/interactions/abcd1234", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body.get("interaction_id").and_then(Value::as_str),
        Some("abcd1234")
    );
    assert_eq!(
        body.get("response").and_then(Value::as_str),
        Some("Rust is a systems language.")
    );

    // ─── 7. GET /v1/training/interactions/ghost → 404 ───
    let url = format!("http://{}/v1/training/interactions/ghost", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let body: Value = resp.json().await.unwrap();
    assert!(body.get("error").and_then(Value::as_str).is_some());

    // ─── 8. POST /v1/training/score/abcd1234 → 200 + score ───
    let url = format!("http://{}/v1/training/score/abcd1234", bound);
    let resp = timeout(Duration::from_secs(15), http.post(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!((body["quality_score"].as_f64().unwrap() - 0.88).abs() < 1e-4);

    // ─── 9. POST /v1/training/export bad-format → 400 ───
    let url = format!("http://{}/v1/training/export", bound);
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "format": "",
                "export_set": "set-x"
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body.get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("format")
    );

    // ─── 10. POST /v1/training/export valid → 200 + result ───
    let url = format!("http://{}/v1/training/export", bound);
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "format": "openai",
                "export_set": "set-x",
                "min_quality_score": 0.5,
                "include_tool_calls": true
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, export_response);

    // ─── 11. DELETE /v1/training/interactions/abcd1234 → 200 ───
    let url = format!("http://{}/v1/training/interactions/abcd1234", bound);
    let resp = timeout(Duration::from_secs(15), http.delete(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, delete_response);

    // ─── 12. DELETE /v1/training/interactions/ghost → 404 ───
    let url = format!("http://{}/v1/training/interactions/ghost", bound);
    let resp = timeout(Duration::from_secs(15), http.delete(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let body: Value = resp.json().await.unwrap();
    assert!(body.get("error").and_then(Value::as_str).is_some());

    // ─── 13. POST /v1/training/pii/scan → 200 + spans ───
    let url = format!("http://{}/v1/training/pii/scan", bound);
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
    assert_eq!(body, pii_scan_response);

    // ─── 14. POST /v1/training/pii/scan missing text → 400 ───
    let url = format!("http://{}/v1/training/pii/scan", bound);
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

    // ─── 15. POST /v1/training/pii/preview → 200 + anonymized ───
    let url = format!("http://{}/v1/training/pii/preview", bound);
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
    assert_eq!(body, pii_preview_response);
    // The placeholder must end up in the anonymized field.
    assert_eq!(
        body.get("anonymized").and_then(Value::as_str),
        Some("Contact me at [EMAIL]")
    );
}
