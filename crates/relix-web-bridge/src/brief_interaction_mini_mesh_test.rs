//! Mini-mesh integration test for the Brief thread-interaction routes
//! (relix-execution-and-issue-design §1.9):
//!
//! - `POST /v1/spine/briefs/:id/interactions` → 200 + `{interaction_id}`,
//!   and the coordinator receives the exact wire arg
//!   `task_id|kind|author|choices_json|prompt` (prompt trailing, choices
//!   as a JSON array).
//! - `GET  /v1/spine/briefs/:id/interactions` → 200 + the JSON array the
//!   coordinator returned (passthrough).
//! - `POST /v1/spine/briefs/:id/interactions/:iid/respond` → 200, with the
//!   wire arg `task_id|interaction_id|responder|status|response`; a
//!   duplicate answer (coordinator INVALID_ARGS) maps to a typed 400.

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
                eprintln!("brief-interaction-mini-mesh: boot_peer retry ({e})");
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
        &dir.path().join("brief-interaction-audit.log"),
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
async fn brief_interaction_routes_round_trip_open_list_respond() {
    // ─── 1. Fake coordinator: canned open/list/respond responders ───
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "open"
        method = "brief.interaction_open"
        allow_groups = ["chat-users"]

        [[rules]]
        name = "list"
        method = "brief.interactions"
        allow_groups = ["chat-users"]

        [[rules]]
        name = "respond"
        method = "brief.interaction_respond"
        allow_groups = ["chat-users"]
        "#,
    );

    // Capture the exact wire args the coordinator receives, to assert the
    // bridge's pipe-delimited encoding (prompt trailing, choices as JSON).
    let open_arg: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let respond_args: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    dispatch.register(
        "brief.interaction_open",
        Arc::new(FnHandler({
            let seen = open_arg.clone();
            move |ctx: InvocationCtx| {
                let seen = seen.clone();
                async move {
                    let arg = String::from_utf8_lossy(&ctx.args).to_string();
                    *seen.lock().unwrap() = arg;
                    HandlerOutcome::Ok(b"bix_test".to_vec())
                }
            }
        })),
    );
    dispatch.register(
        "brief.interactions",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| async move {
            let body = serde_json::json!([{
                "interaction_id": "bix_test",
                "task_id": "task_1",
                "kind": "confirm",
                "prompt": "Ship the v2 plan?",
                "choices": ["yes", "no"],
                "author": "operative-1",
                "status": "open",
                "response": null,
                "created_at": 1_700_000_000_i64,
                "resolved_at": null,
                "resolved_by": null,
            }]);
            HandlerOutcome::Ok(serde_json::to_vec(&body).unwrap())
        })),
    );
    dispatch.register(
        "brief.interaction_respond",
        Arc::new(FnHandler({
            let seen = respond_args.clone();
            move |ctx: InvocationCtx| {
                let seen = seen.clone();
                async move {
                    let arg = String::from_utf8_lossy(&ctx.args).to_string();
                    seen.lock().unwrap().push(arg.clone());
                    // The second respond to the same card is "already
                    // answered" — a typed INVALID_ARGS the bridge maps to 400.
                    if arg.contains("|bix_dup|") {
                        HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::INVALID_ARGS,
                            cause: "brief.interaction_respond: interaction already resolved".into(),
                            retry_hint: 0,
                            retry_after: None,
                        })
                    } else {
                        HandlerOutcome::Ok(Vec::new())
                    }
                }
            }
        })),
    );

    let dispatch = Arc::new(dispatch);
    let (_peer_client, events, addr) = boot_peer(151).await;
    spawn_inbound_loop(events, dispatch.clone());

    // ─── 2. Mint bridge identity + wire a real mesh client ───
    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes = mint_bridge_bundle_bytes(
        &org_root,
        "interaction-test-bridge",
        vec!["chat-users".into()],
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

    // ─── 3. Mount the three interaction routes ───
    let app = Router::new()
        .route(
            "/v1/spine/briefs/:id/interactions",
            get(crate::spine::list_interactions).post(crate::spine::open_interaction),
        )
        .route(
            "/v1/spine/briefs/:id/interactions/:iid/respond",
            post(crate::spine::respond_interaction),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let http = reqwest::Client::new();

    // ─── 4. POST open → 200 + interaction_id, correct wire arg ───
    let url = format!("http://{bound}/v1/spine/briefs/task_1/interactions");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "kind": "confirm",
                "prompt": "Ship the v2 plan?",
                "choices": ["yes", "no"],
                "author": "operative-1",
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
        Some("bix_test")
    );
    assert_eq!(
        *open_arg.lock().unwrap(),
        r#"task_1|confirm|operative-1|["yes","no"]|Ship the v2 plan?"#,
        "open wire arg must be task_id|kind|author|choices_json|prompt"
    );

    // ─── 5. GET list → 200 + the JSON array passthrough ───
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    let arr = body.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0].get("status").and_then(Value::as_str), Some("open"));
    assert_eq!(
        arr[0]
            .get("choices")
            .and_then(Value::as_array)
            .map(|a| a.len()),
        Some(2)
    );

    // ─── 6. POST respond → 200, correct wire arg ───
    let url_ok = format!("http://{bound}/v1/spine/briefs/task_1/interactions/bix_test/respond");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url_ok)
            .json(&serde_json::json!({
                "responder": "founder",
                "status": "resolved",
                "response": "yes — ship",
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        respond_args
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default(),
        "task_1|bix_test|founder|resolved|yes — ship",
        "respond wire arg must be task_id|iid|responder|status|response"
    );

    // ─── 7. A duplicate answer (coordinator INVALID_ARGS) → 400 ───
    let url_dup = format!("http://{bound}/v1/spine/briefs/task_1/interactions/bix_dup/respond");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url_dup)
            .json(&serde_json::json!({
                "responder": "founder",
                "status": "resolved",
                "response": "again",
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "a duplicate answer must surface as a typed 400, not a 5xx"
    );
    let body: Value = resp.json().await.unwrap();
    assert!(
        body.get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("already"),
        "duplicate body must explain the refusal: {body}"
    );

    // ─── 8. Bridge-level input validation: bad kind → 400, no peer call ───
    let url = format!("http://{bound}/v1/spine/briefs/task_1/interactions");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "kind": "suggest_tasks",
                "prompt": "p",
                "author": "op",
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "unknown kind is rejected at the bridge"
    );
}

/// §1.9 suggest_tasks round-trip across a real mesh:
/// - `POST /v1/spine/briefs/:id/suggestions` → 200 + `{interaction_id}`,
///   and the coordinator receives a JSON arg
///   `{task_id, author, summary, children:[{title, priority}]}`.
/// - `GET  /v1/spine/briefs/:id/interactions` → the suggest_tasks card with
///   its proposal passes through.
/// - `POST .../suggestions/:iid/respond` (accept) → 200 + `{created:[…]}`,
///   wire arg `task_id|iid|responder|accept`; a duplicate accept maps to 400.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn brief_suggestion_routes_round_trip_open_list_respond() {
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "open"
        method = "brief.suggest_open"
        allow_groups = ["chat-users"]

        [[rules]]
        name = "list"
        method = "brief.interactions"
        allow_groups = ["chat-users"]

        [[rules]]
        name = "respond"
        method = "brief.suggest_respond"
        allow_groups = ["chat-users"]
        "#,
    );

    let open_arg: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let respond_args: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    dispatch.register(
        "brief.suggest_open",
        Arc::new(FnHandler({
            let seen = open_arg.clone();
            move |ctx: InvocationCtx| {
                let seen = seen.clone();
                async move {
                    *seen.lock().unwrap() = String::from_utf8_lossy(&ctx.args).to_string();
                    HandlerOutcome::Ok(b"bix_sug".to_vec())
                }
            }
        })),
    );
    dispatch.register(
        "brief.interactions",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| async move {
            let body = serde_json::json!([{
                "interaction_id": "bix_sug",
                "task_id": "task_1",
                "kind": "suggest_tasks",
                "prompt": "Break the epic down",
                "choices": [],
                "author": "operative-1",
                "status": "open",
                "response": null,
                "created_at": 1_700_000_000_i64,
                "resolved_at": null,
                "resolved_by": null,
                "proposal": {
                    "summary": "Break the epic down",
                    "children": [
                        { "title": "Design the API" },
                        { "title": "Wire the store", "priority": "high" }
                    ]
                }
            }]);
            HandlerOutcome::Ok(serde_json::to_vec(&body).unwrap())
        })),
    );
    dispatch.register(
        "brief.suggest_respond",
        Arc::new(FnHandler({
            let seen = respond_args.clone();
            move |ctx: InvocationCtx| {
                let seen = seen.clone();
                async move {
                    let arg = String::from_utf8_lossy(&ctx.args).to_string();
                    seen.lock().unwrap().push(arg.clone());
                    // A second accept of the same card is "already answered" —
                    // a typed INVALID_ARGS the bridge maps to a 400.
                    if arg.contains("|bix_dup|") {
                        HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::INVALID_ARGS,
                            cause: "brief.suggest_respond: suggestion already resolved".into(),
                            retry_hint: 0,
                            retry_after: None,
                        })
                    } else if arg.contains("|bix_denied|") {
                        // Accepting a card whose child carries an assignee hint
                        // the accepter's assign-Key forbids is a governance
                        // refusal — a typed POLICY_DENIED the bridge must map to
                        // a 403, NOT a 502 (the assignee-hint smoke regression).
                        HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::POLICY_DENIED,
                            cause: "brief.suggest_respond: assignee hint denied — out of assign scope"
                                .into(),
                            retry_hint: 0,
                            retry_after: None,
                        })
                    } else if arg.contains("|bix_approval|") {
                        // The accept materializes a child in a category that
                        // needs an operator approval first — the coordinator
                        // mints an approval_id and refuses with APPROVAL_REQUIRED.
                        // Not a denial: admissible once approved, so the bridge
                        // must surface a 428 (precondition), NOT a 403 or 502.
                        HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::APPROVAL_REQUIRED,
                            cause: "approval_required:apr-bix-9c2".into(),
                            retry_hint: 0,
                            retry_after: None,
                        })
                    } else if arg.contains("|bix_budget|") {
                        // The accepter's cost cap is exhausted with
                        // action_on_exceed = "reject" — a typed RESOURCE_EXHAUSTED
                        // the bridge must surface as a 429 quota error, NOT a 502.
                        HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::RESOURCE_EXHAUSTED,
                            cause: "budget:reject:agent founder over cap 120/100 resets 2026-06-07T00:00:00Z"
                                .into(),
                            retry_hint: 0,
                            retry_after: None,
                        })
                    } else {
                        HandlerOutcome::Ok(
                            serde_json::to_vec(&serde_json::json!({ "created": ["c1", "c2"] }))
                                .unwrap(),
                        )
                    }
                }
            }
        })),
    );

    let dispatch = Arc::new(dispatch);
    let (_peer_client, events, addr) = boot_peer(157).await;
    spawn_inbound_loop(events, dispatch.clone());

    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes = mint_bridge_bundle_bytes(
        &org_root,
        "suggestion-test-bridge",
        vec!["chat-users".into()],
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
            "/v1/spine/briefs/:id/interactions",
            get(crate::spine::list_interactions),
        )
        .route(
            "/v1/spine/briefs/:id/suggestions",
            post(crate::spine::open_suggestion),
        )
        .route(
            "/v1/spine/briefs/:id/suggestions/:iid/respond",
            post(crate::spine::respond_suggestion),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let http = reqwest::Client::new();

    // ─── open → 200 + interaction_id, JSON wire arg ───
    let url = format!("http://{bound}/v1/spine/briefs/task_1/suggestions");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "author": "operative-1",
                "summary": "Break the epic down",
                "children": [
                    { "title": "Design the API" },
                    { "title": "Wire the store", "priority": "high", "after": 0 }
                ]
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
        Some("bix_sug")
    );
    // The coordinator received a JSON arg with the task_id + proposal.
    let seen: Value = serde_json::from_str(&open_arg.lock().unwrap()).expect("open arg is JSON");
    assert_eq!(seen.get("task_id").and_then(Value::as_str), Some("task_1"));
    assert_eq!(
        seen.get("author").and_then(Value::as_str),
        Some("operative-1")
    );
    assert_eq!(
        seen.get("children")
            .and_then(Value::as_array)
            .map(|a| a.len()),
        Some(2)
    );
    // The optional `after` dependency rides through the JSON wire arg.
    assert_eq!(
        seen["children"][1].get("after").and_then(Value::as_u64),
        Some(0),
        "the `after` dependency index must pass through to the coordinator"
    );

    // ─── list → the suggest_tasks card with its proposal passes through ───
    let list_url = format!("http://{bound}/v1/spine/briefs/task_1/interactions");
    let resp = timeout(Duration::from_secs(15), http.get(&list_url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    let arr = body.as_array().expect("array");
    assert_eq!(
        arr[0].get("kind").and_then(Value::as_str),
        Some("suggest_tasks")
    );
    assert_eq!(
        arr[0]["proposal"]["children"].as_array().map(|a| a.len()),
        Some(2)
    );

    // ─── respond accept → 200 + created ids, correct wire arg ───
    let ok_url = format!("http://{bound}/v1/spine/briefs/task_1/suggestions/bix_sug/respond");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&ok_url)
            .json(&serde_json::json!({ "responder": "founder", "accept": true }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["created"].as_array().map(|a| a.len()), Some(2));
    assert_eq!(
        respond_args
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default(),
        "task_1|bix_sug|founder|accept",
        "respond wire arg must be task_id|iid|responder|verdict"
    );

    // ─── a duplicate accept (coordinator INVALID_ARGS) → 400 ───
    let dup_url = format!("http://{bound}/v1/spine/briefs/task_1/suggestions/bix_dup/respond");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&dup_url)
            .json(&serde_json::json!({ "responder": "founder", "accept": true }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "a duplicate accept must surface as a typed 400, not a 5xx"
    );

    // ─── an assign-denied accept (coordinator POLICY_DENIED) → 403 ───
    // The card materializes children carrying an assignee hint the accepter's
    // assign-Key forbids; the coordinator refuses with POLICY_DENIED (kind 6).
    // The bridge must surface that governance refusal as a 403, not a 502.
    let denied_url =
        format!("http://{bound}/v1/spine/briefs/task_1/suggestions/bix_denied/respond");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&denied_url)
            .json(&serde_json::json!({ "responder": "founder", "accept": true }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "an assign-denied refusal must surface as a typed 403, not a 502"
    );
    // The refusal text is preserved for the client (no existence leak).
    let body: Value = resp.json().await.unwrap();
    assert!(
        body.get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("out of assign scope"),
        "the 403 body must preserve the governance refusal reason"
    );

    // ─── an approval-required accept (coordinator APPROVAL_REQUIRED) → 428 ───
    // The child needs an operator approval first; the coordinator mints an
    // approval_id and refuses with APPROVAL_REQUIRED (kind 19). This is NOT a
    // denial — it is admissible once approved — so the bridge must surface a
    // 428 (precondition required), not a 403 and certainly not a 502.
    let approval_url =
        format!("http://{bound}/v1/spine/briefs/task_1/suggestions/bix_approval/respond");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&approval_url)
            .json(&serde_json::json!({ "responder": "founder", "accept": true }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        428,
        "an approval-required gate must surface as a 428, not a 403 or 502"
    );
    // The minted approval_id rides in the preserved cause so the client can
    // decide it and retry.
    let body: Value = resp.json().await.unwrap();
    assert!(
        body.get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("apr-bix-9c2"),
        "the 428 body must preserve the approval_id from the cause"
    );

    // ─── a budget-exhausted accept (coordinator RESOURCE_EXHAUSTED) → 429 ───
    // The accepter's cost cap is exhausted with action_on_exceed = "reject";
    // the coordinator refuses with RESOURCE_EXHAUSTED (kind 22). The bridge
    // must surface a 429 quota error, not a 502 upstream-failure.
    let budget_url =
        format!("http://{bound}/v1/spine/briefs/task_1/suggestions/bix_budget/respond");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&budget_url)
            .json(&serde_json::json!({ "responder": "founder", "accept": true }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        429,
        "a budget cap must surface as a 429 quota error, not a 502"
    );
    // The limit / reset reason rides in the preserved cause.
    let body: Value = resp.json().await.unwrap();
    assert!(
        body.get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("over cap 120/100"),
        "the 429 body must preserve the budget limit reason"
    );

    // ─── bridge-level validation: no children → 400, no peer call ───
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({ "author": "op", "children": [] }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "an empty proposal is rejected at the bridge"
    );
}

/// §1.8 approval-bound plan confirm across a real mesh:
/// - `POST /v1/spine/briefs/:id/plan-confirm` with an explicit author → 200 +
///   `{interaction_id}`, coordinator receives `task_id|author|prompt`.
/// - the same route with NO author defaults to the bridge identity `operator`.
/// - a Brief with no `plan` Dossier → the coordinator's typed INVALID_ARGS
///   refusal surfaces as a 400 (with the reason preserved), not a 5xx.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn brief_plan_confirm_route_opens_and_refuses_without_plan() {
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "plan_confirm"
        method = "brief.plan_confirm_open"
        allow_groups = ["chat-users"]
        "#,
    );

    let open_args: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    dispatch.register(
        "brief.plan_confirm_open",
        Arc::new(FnHandler({
            let seen = open_args.clone();
            move |ctx: InvocationCtx| {
                let seen = seen.clone();
                async move {
                    let arg = String::from_utf8_lossy(&ctx.args).to_string();
                    seen.lock().unwrap().push(arg.clone());
                    // A Brief with no `plan` Dossier can't bind an approval —
                    // the coordinator refuses typed (INVALID_ARGS), which the
                    // bridge must map to a 400 (not a 502 upstream failure).
                    if arg.starts_with("task_noplan|") {
                        HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::INVALID_ARGS,
                            cause: "brief.plan_confirm_open: no plan Dossier to bind".into(),
                            retry_hint: 0,
                            retry_after: None,
                        })
                    } else {
                        HandlerOutcome::Ok(b"bix_plan".to_vec())
                    }
                }
            }
        })),
    );

    let dispatch = Arc::new(dispatch);
    let (_peer_client, events, addr) = boot_peer(163).await;
    spawn_inbound_loop(events, dispatch.clone());

    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes = mint_bridge_bundle_bytes(
        &org_root,
        "plan-confirm-test-bridge",
        vec!["chat-users".into()],
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
            "/v1/spine/briefs/:id/plan-confirm",
            post(crate::spine::open_plan_confirm),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let http = reqwest::Client::new();

    // ─── open with an explicit author → 200 + interaction_id, wire arg ───
    let url = format!("http://{bound}/v1/spine/briefs/task_1/plan-confirm");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({ "author": "founder", "prompt": "Approve the plan?" }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body.get("interaction_id").and_then(Value::as_str),
        Some("bix_plan")
    );
    assert_eq!(
        open_args
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default(),
        "task_1|founder|Approve the plan?",
        "plan-confirm wire arg must be task_id|author|prompt"
    );

    // ─── open with NO author → defaults to the bridge identity `operator` ───
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url).json(&serde_json::json!({})).send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        open_args
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default(),
        "task_1|operator|",
        "an absent author defaults to the local bridge identity"
    );

    // ─── a Brief with no `plan` Dossier → typed 400 (reason preserved) ───
    let noplan_url = format!("http://{bound}/v1/spine/briefs/task_noplan/plan-confirm");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&noplan_url)
            .json(&serde_json::json!({ "author": "founder" }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "no plan Dossier must surface as a typed 400, not a 5xx"
    );
    let body: Value = resp.json().await.unwrap();
    assert!(
        body.get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("no plan Dossier"),
        "the 400 body must preserve the refusal reason: {body}"
    );
}
