//! RELIX-7.24 — end-to-end mini-mesh integration test for the
//! planning bridge surface.
//!
//! Boots a fake coordinator peer with canned `planning.*`
//! responders, dials it via `discover_and_pin`, mounts every
//! `/v1/planning/*` route on an ephemeral axum listener, and
//! drives reqwest requests through six scenarios:
//!
//! 1. GET `/v1/planning/agents` → 200 + agent list
//! 2. POST `/v1/planning/agents/search` → 200 + scored matches
//! 3. POST `/v1/planning/agents/search` (empty task) → 400
//! 4. POST `/v1/planning/validate` → 200 + parsed PlanSpec
//! 5. POST `/v1/planning/plan` (dry_run) → 200 + workflow_yaml
//! 6. POST `/v1/planning/plan` (empty spec) → 400

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
                eprintln!("planning-mini-mesh: boot_peer retry ({e})");
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
        &dir.path().join("planning-audit.log"),
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
async fn planning_mini_mesh_all_endpoints() {
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "ops_list_agents"
        method = "planning.list_agents"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_find_agents"
        method = "planning.find_agents"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_validate_spec"
        method = "planning.validate_spec"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_create_plan"
        method = "planning.create_plan"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_orchestrator_status"
        method = "planning.orchestrator_status"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_verification_log"
        method = "planning.verification_log"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_export_spec"
        method = "planning.export_spec"
        allow_groups = ["operators"]
        "#,
    );

    let agents_response = serde_json::json!({
        "agents": [
            {
                "name": "research-agent",
                "description": "Specialised in web research",
                "peer": "research-peer",
                "capabilities": [
                    {
                        "method": "ai.chat",
                        "description": "research queries",
                        "tags": ["research", "web"]
                    }
                ]
            }
        ]
    });
    let find_response = serde_json::json!({
        "matches": [
            { "agent": "research-agent", "score": 5, "matched_capabilities": ["ai.chat"] }
        ]
    });
    let validate_response = serde_json::json!({
        "goal": "Research the web",
        "constraints": [],
        "success_criteria": [],
        "preferred_agents": ["research-agent"],
        "forbidden_agents": [],
        "max_steps": null,
        "budget_hint": null,
        "original_spec": "Research the web. Use research-agent."
    });
    let plan_response = serde_json::json!({
        "plan_spec": validate_response.clone(),
        "topology": "single",
        "workflow_name": "planning__research_the_web",
        "workflow_yaml": "name: planning__research_the_web\nversion: 1\nagents: {}\nflow: {}\n",
        "agents_selected": [],
        "execution": null,
        "orchestrator_activated": false,
        "specialist_count": 0,
        "critic_rounds": 0,
        "critic_approved": true,
        "orchestrator": {
            "activated": false,
            "complexity_score": 0.0,
            "complexity_threshold": 0.6,
            "sub_goals": [],
            "specialist_assignments": [],
            "decomposed_by_heuristic": false,
        },
        "critic": {
            "enabled": true,
            "rounds": 0,
            "approved": true,
            "history": [],
        },
    });
    // RELIX-7.24 Stage-1: a separate canned plan_response that
    // tells operators "yes, the orchestrator fired" with a
    // conflict resolution report attached. Returned when the
    // test scenario sends a complex spec — the canned
    // coordinator switches on `complex` substring match.
    let complex_plan_response = serde_json::json!({
        "plan_spec": validate_response.clone(),
        "topology": "parallel",
        "workflow_name": "planning_orch__research_and_design",
        "workflow_yaml": "name: planning_orch__research_and_design\nversion: 1\nagents: {}\nflow: {}\n",
        "agents_selected": [],
        "execution": null,
        "orchestrator_activated": true,
        "specialist_count": 2,
        "critic_rounds": 0,
        "critic_approved": true,
        "orchestrator": {
            "activated": true,
            "complexity_score": 0.9,
            "complexity_threshold": 0.6,
            "sub_goals": ["Research async runtimes", "Design a benchmark"],
            "specialist_assignments": [
                {
                    "sub_goal": "Research async runtimes",
                    "specialist_agent": "research-agent",
                    "specialist_peer": "research-peer",
                    "match_score": 7,
                }
            ],
            "decomposed_by_heuristic": false,
        },
        "critic": {
            "enabled": true,
            "rounds": 0,
            "approved": true,
            "history": [],
        },
        "conflict_resolution_report": {
            "conflicts_detected": 1,
            "conflicts_resolved": 1,
            "strategy_used": "rename",
            "details": [],
        },
    });
    let status_response = serde_json::json!({
        "orchestrator": {
            "enabled": true,
            "agent": "coordinator",
            "peer": "coordinator",
            "complexity_threshold": 0.6,
            "max_parallel_specialists": 4,
        },
        "critic": {
            "enabled": true,
            "agent": "coordinator",
            "peer": "coordinator",
            "max_rounds": 3,
        },
        "dispatcher_live": true,
    });

    let agents_arc = Arc::new(agents_response.clone());
    let find_arc = Arc::new(find_response.clone());
    let validate_arc = Arc::new(validate_response.clone());
    let plan_arc = Arc::new(plan_response.clone());
    let complex_plan_arc = Arc::new(complex_plan_response.clone());
    let status_arc = Arc::new(status_response.clone());

    dispatch.register(
        "planning.list_agents",
        Arc::new(FnHandler({
            let r = agents_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "planning.find_agents",
        Arc::new(FnHandler({
            let r = find_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "planning.validate_spec",
        Arc::new(FnHandler({
            let r = validate_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    // create_plan returns the simple plan_response by default
    // and the complex_plan_response when the request body's
    // `spec` field contains the word "complex" — that's how the
    // single canned coordinator covers both
    // orchestrator-skipped and orchestrator-active scenarios.
    dispatch.register(
        "planning.create_plan",
        Arc::new(FnHandler({
            let r = plan_arc.clone();
            let r_complex = complex_plan_arc.clone();
            move |ctx: InvocationCtx| {
                let r = r.clone();
                let r_complex = r_complex.clone();
                async move {
                    let arg_text = std::str::from_utf8(&ctx.args).unwrap_or("");
                    let body: &serde_json::Value = if arg_text.contains("complex") {
                        &r_complex
                    } else {
                        &r
                    };
                    HandlerOutcome::Ok(serde_json::to_vec(body).unwrap_or_default())
                }
            }
        })),
    );
    dispatch.register(
        "planning.orchestrator_status",
        Arc::new(FnHandler({
            let r = status_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    // planning.export_spec: canned coordinator returns one
    // export shape for either format.
    dispatch.register(
        "planning.export_spec",
        Arc::new(FnHandler(move |ctx: InvocationCtx| async move {
            let args_str = std::str::from_utf8(&ctx.args).unwrap_or("");
            let v: serde_json::Value =
                serde_json::from_str(args_str).unwrap_or(serde_json::Value::Null);
            let format = v
                .get("format")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("json")
                .to_string();
            let plan_id = v
                .get("plan_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?")
                .to_string();
            let content = if format == "markdown" {
                format!("# Relix Plan {plan_id}\n\n## Goal\n\nDo the thing.\n")
            } else {
                serde_json::json!({
                    "schema_version": 1,
                    "plan_id": plan_id,
                    "spec": {"goal": "Do the thing."},
                })
                .to_string()
            };
            let body = serde_json::json!({
                "plan_id": plan_id,
                "format": format,
                "content": content,
            });
            HandlerOutcome::Ok(serde_json::to_vec(&body).unwrap_or_default())
        })),
    );
    // planning.verification_log: returns a growing list. On
    // the first call we return 1 entry; on subsequent calls
    // we return 2. This lets the SSE stream scenario observe
    // a NEW entry appear after the second poll.
    let verification_call_counter = Arc::new(tokio::sync::Mutex::new(0u32));
    dispatch.register(
        "planning.verification_log",
        Arc::new(FnHandler({
            let counter = verification_call_counter.clone();
            move |_ctx: InvocationCtx| {
                let counter = counter.clone();
                async move {
                    let mut g = counter.lock().await;
                    *g += 1;
                    let n = *g;
                    drop(g);
                    let entries: Vec<serde_json::Value> = if n == 1 {
                        vec![serde_json::json!({
                            "plan_id": "stream-test",
                            "step_id": "first",
                            "criterion": "must include foo",
                            "strategy_used": "keyword_presence",
                            "passed": true,
                            "reason": "first entry",
                            "verified_at_ms": 1000,
                        })]
                    } else {
                        vec![
                            serde_json::json!({
                                "plan_id": "stream-test",
                                "step_id": "first",
                                "criterion": "must include foo",
                                "strategy_used": "keyword_presence",
                                "passed": true,
                                "reason": "first entry",
                                "verified_at_ms": 1000,
                            }),
                            serde_json::json!({
                                "plan_id": "stream-test",
                                "step_id": "second",
                                "criterion": "must include bar",
                                "strategy_used": "keyword_presence",
                                "passed": false,
                                "reason": "second entry",
                                "verified_at_ms": 1500,
                            }),
                        ]
                    };
                    let body = serde_json::json!({ "entries": entries });
                    HandlerOutcome::Ok(serde_json::to_vec(&body).unwrap_or_default())
                }
            }
        })),
    );
    let dispatch = Arc::new(dispatch);

    let (_client, events, addr) = boot_peer(187).await;
    spawn_inbound_loop(events, dispatch.clone());

    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes =
        mint_bridge_bundle_bytes(&org_root, "planning-test-bridge", vec!["operators".into()]);
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
        .route("/v1/planning/agents", get(crate::planning::list_agents))
        .route(
            "/v1/planning/agents/search",
            post(crate::planning::search_agents),
        )
        .route(
            "/v1/planning/validate",
            post(crate::planning::validate_spec),
        )
        .route("/v1/planning/plan", post(crate::planning::create_plan))
        .route(
            "/v1/planning/status",
            get(crate::planning::orchestrator_status),
        )
        .route(
            "/v1/planning/verification/:id",
            get(crate::planning::verification_log),
        )
        .route(
            "/v1/planning/verification/:id/stream",
            get(crate::planning::verification_stream),
        )
        .route("/v1/planning/export/:id", get(crate::planning::export_spec))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();

    // 1. GET /v1/planning/agents → 200 + agents
    let url = format!("http://{bound}/v1/planning/agents");
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, agents_response);

    // 2. POST /v1/planning/agents/search → 200 + matches
    let url = format!("http://{bound}/v1/planning/agents/search");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({ "task": "research the web" }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, find_response);

    // 3. POST /v1/planning/agents/search (empty task) → 400
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({ "task": "" }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    // 4. POST /v1/planning/validate → 200 + parsed PlanSpec
    let url = format!("http://{bound}/v1/planning/validate");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "spec": "Research the web. Use research-agent."
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, validate_response);

    // 5. POST /v1/planning/plan (dry_run) → 200 + workflow_yaml
    let url = format!("http://{bound}/v1/planning/plan");
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "spec": "Research the web. Use research-agent.",
                "dry_run": true,
                "max_agents": 1
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["topology"], "single");
    assert!(body["workflow_yaml"].as_str().unwrap().contains("name:"));

    // 6. POST /v1/planning/plan (empty spec) → 400
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({ "spec": "" }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    // 7. POST /v1/planning/plan with a complex spec → 200 +
    // orchestrator_activated = true + conflict_resolution_report
    // populated. (The canned coordinator returns the
    // complex_plan_response when the spec contains the word
    // "complex".)
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({
                "spec": "Build a complex thing.",
                "dry_run": true,
                "max_agents": 4
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["orchestrator_activated"],
        serde_json::Value::Bool(true)
    );
    assert_eq!(body["specialist_count"], 2);
    let conflict = &body["conflict_resolution_report"];
    assert!(!conflict.is_null(), "expected conflict_resolution_report");
    assert_eq!(conflict["conflicts_resolved"], 1);

    // 8. GET /v1/planning/status → 200 + orchestrator + critic
    // config view.
    let url = format!("http://{bound}/v1/planning/status");
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, status_response);

    // 9. GET /v1/planning/verification/stream-test/stream →
    // SSE stream emits entries as they appear in the
    // verification log. Canned coordinator returns 1 entry
    // on poll #1 and 2 entries on poll #2 — the stream
    // should emit two distinct `entry` events.
    let url = format!("http://{bound}/v1/planning/verification/stream-test/stream");
    let mut resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    // Consume the SSE byte stream until we've seen two
    // entry events OR 10s elapse.
    let mut buf = String::new();
    let mut entry_events_seen = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while entry_events_seen < 2 && std::time::Instant::now() < deadline {
        let chunk = timeout(Duration::from_secs(2), resp.chunk()).await;
        let Ok(Ok(Some(bytes))) = chunk else { break };
        buf.push_str(&String::from_utf8_lossy(&bytes));
        // Count newline-separated `event: entry` markers.
        entry_events_seen = buf.matches("event: entry").count();
    }
    assert!(
        entry_events_seen >= 2,
        "expected at least two SSE entry events, got {entry_events_seen}; buf=\n{buf}"
    );
    // Drop the response → the stream task on the bridge side
    // sees the consumer disconnect and stops.
    drop(resp);

    // 10. GET /v1/planning/export/exp-1?format=markdown → 200
    // + content payload.
    let url = format!("http://{bound}/v1/planning/export/exp-1?format=markdown");
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["format"], "markdown");
    assert_eq!(body["plan_id"], "exp-1");
    assert!(
        body["content"]
            .as_str()
            .unwrap()
            .contains("# Relix Plan exp-1")
    );

    // 11. GET /v1/planning/export/exp-1 (default JSON) → 200
    // + JSON content with schema_version.
    let url = format!("http://{bound}/v1/planning/export/exp-1");
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["format"], "json");
    let content_str = body["content"].as_str().unwrap();
    let content_json: Value = serde_json::from_str(content_str).unwrap();
    assert_eq!(content_json["schema_version"], 1);
}
