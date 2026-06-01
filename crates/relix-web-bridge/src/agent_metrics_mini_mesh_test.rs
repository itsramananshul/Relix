//! End-to-end mini-mesh integration tests for the RELIX-7.11
//! agent-metrics HTTP bridge endpoints.
//!
//! Boots a fake "coordinator" peer that registers the six
//! `metrics.*` capabilities with canned JSON responses, builds
//! a real `MeshClient` pointing at it via `discover_and_pin`,
//! mounts the six bridge routes on an ephemeral axum listener,
//! and drives reqwest requests through the stack.
//!
//! Spec scenarios covered:
//!
//!   * `GET  /v1/metrics/agents`                 → 200 + list
//!   * `GET  /v1/metrics/agents/:a/summary`      → 200 + summary
//!   * `GET  /v1/metrics/agents/:a/summary`      → 404 on empty window
//!   * `GET  /v1/metrics/alerts`                 → 200 + array
//!   * `GET  /v1/metrics/cost`                   → 200 + sorted

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
                eprintln!("metrics-mini-mesh: boot_peer retry ({e})");
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
        &dir.path().join("metrics-audit.log"),
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
async fn metrics_mini_mesh_all_endpoints() {
    // ─── 1. Boot a fake coordinator with canned metrics.* ───
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "ops_agents"
        method = "metrics.agents"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_summary"
        method = "metrics.agent_summary"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_methods"
        method = "metrics.method_breakdown"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_ts"
        method = "metrics.timeseries"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_alerts"
        method = "metrics.alerts_active"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_cost"
        method = "metrics.cost_report"
        allow_groups = ["operators"]
        "#,
    );

    let agents_response = serde_json::json!([
        {
            "agent": "alice",
            "invocations": 100,
            "successes": 95,
            "errors": 5,
            "success_rate": 0.95,
            "error_rate": 0.05,
            "p50_latency_ms": 50,
            "p95_latency_ms": 200,
            "p99_latency_ms": 400,
            "total_tokens": 12000,
            "total_cost_micros": 18000,
            "avg_input_bytes": 100,
            "avg_output_bytes": 200,
            "most_common_error_kind": "INTERNAL",
            "window_hours": 24
        }
    ]);
    let alice_summary = agents_response[0].clone();
    let empty_summary = serde_json::json!({
        "agent": "unknown",
        "invocations": 0,
        "successes": 0,
        "errors": 0,
        "success_rate": 0.0,
        "error_rate": 0.0,
        "p50_latency_ms": 0,
        "p95_latency_ms": 0,
        "p99_latency_ms": 0,
        "total_tokens": 0,
        "total_cost_micros": 0,
        "avg_input_bytes": 0,
        "avg_output_bytes": 0,
        "most_common_error_kind": null,
        "window_hours": 24
    });
    let methods_response = serde_json::json!([
        { "method": "ai.chat", "invocations": 100, "successes": 95, "errors": 5,
          "success_rate": 0.95, "error_rate": 0.05,
          "p50_latency_ms": 50, "p95_latency_ms": 200, "p99_latency_ms": 400,
          "total_tokens": 12000, "total_cost_micros": 18000,
          "avg_input_bytes": 100, "avg_output_bytes": 200,
          "most_common_error_kind": "INTERNAL" }
    ]);
    let timeseries_response = serde_json::json!([
        { "bucket_start_ms": 1_700_000_000_000_i64, "invocations": 30, "errors": 1,
          "p95_latency_ms": 200, "total_tokens": 3600, "total_cost_micros": 5400 },
        { "bucket_start_ms": 1_700_000_300_000_i64, "invocations": 35, "errors": 2,
          "p95_latency_ms": 250, "total_tokens": 4200, "total_cost_micros": 6300 }
    ]);
    let alerts_response = serde_json::json!([
        { "agent": "alice", "kind": "error_rate", "severity": "warning",
          "triggered_at_ms": 1_700_000_000_000_i64,
          "threshold": 10.0, "actual": 12.5,
          "message": "alice: error rate 12.50% (threshold 10.00%)" }
    ]);
    let cost_response = serde_json::json!([
        { "agent": "alice", "method": "ai.chat",
          "total_cost_micros": 18000, "total_tokens": 12000, "invocations": 100 }
    ]);

    // Decide whether to return alice or empty summary based on
    // the `agent` field in the request body.
    let agents_arc = Arc::new(agents_response.clone());
    let methods_arc = Arc::new(methods_response.clone());
    let timeseries_arc = Arc::new(timeseries_response.clone());
    let alerts_arc = Arc::new(alerts_response.clone());
    let cost_arc = Arc::new(cost_response.clone());
    let alice_summary_arc = Arc::new(alice_summary.clone());
    let empty_summary_arc = Arc::new(empty_summary.clone());

    dispatch.register(
        "metrics.agents",
        Arc::new(FnHandler({
            let r = agents_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "metrics.agent_summary",
        Arc::new(FnHandler({
            let alice = alice_summary_arc.clone();
            let empty = empty_summary_arc.clone();
            move |ctx: InvocationCtx| {
                let alice = alice.clone();
                let empty = empty.clone();
                async move {
                    let body: Value =
                        serde_json::from_slice(&ctx.args).unwrap_or(serde_json::json!({}));
                    let agent = body
                        .get("agent")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let chosen = if agent == "alice" { alice } else { empty };
                    HandlerOutcome::Ok(serde_json::to_vec(&*chosen).unwrap_or_default())
                }
            }
        })),
    );
    dispatch.register(
        "metrics.method_breakdown",
        Arc::new(FnHandler({
            let r = methods_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "metrics.timeseries",
        Arc::new(FnHandler({
            let r = timeseries_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "metrics.alerts_active",
        Arc::new(FnHandler({
            let r = alerts_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    dispatch.register(
        "metrics.cost_report",
        Arc::new(FnHandler({
            let r = cost_arc.clone();
            move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { HandlerOutcome::Ok(serde_json::to_vec(&*r).unwrap_or_default()) }
            }
        })),
    );
    let dispatch = Arc::new(dispatch);

    let (_client, events, addr) = boot_peer(53).await;
    spawn_inbound_loop(events, dispatch.clone());

    // ─── 2. Mint bridge identity ───
    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes =
        mint_bridge_bundle_bytes(&org_root, "metrics-test-bridge", vec!["operators".into()]);
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
        .route("/v1/metrics/agents", get(crate::agent_metrics::list_agents))
        .route(
            "/v1/metrics/agents/:agent/summary",
            get(crate::agent_metrics::agent_summary),
        )
        .route(
            "/v1/metrics/agents/:agent/methods",
            get(crate::agent_metrics::agent_methods),
        )
        .route(
            "/v1/metrics/agents/:agent/timeseries",
            get(crate::agent_metrics::agent_timeseries),
        )
        .route("/v1/metrics/alerts", get(crate::agent_metrics::alerts))
        .route("/v1/metrics/cost", get(crate::agent_metrics::cost))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();

    // ─── 4. GET /v1/metrics/agents → 200 + list ───
    let url = format!("http://{}/v1/metrics/agents", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, agents_response);

    // ─── 5. GET /v1/metrics/agents/alice/summary → 200 ───
    let url = format!("http://{}/v1/metrics/agents/alice/summary?hours=24", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body.get("agent").and_then(Value::as_str), Some("alice"));
    assert_eq!(body.get("invocations").and_then(Value::as_u64), Some(100));

    // ─── 6. GET /v1/metrics/agents/unknown/summary → 404 ───
    let url = format!("http://{}/v1/metrics/agents/unknown/summary", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let body: Value = resp.json().await.unwrap();
    assert!(body.get("error").and_then(Value::as_str).is_some());

    // ─── 7. GET /v1/metrics/agents/alice/methods → 200 ───
    let url = format!("http://{}/v1/metrics/agents/alice/methods", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, methods_response);

    // ─── 8. GET /v1/metrics/agents/alice/timeseries → 200 ───
    let url = format!(
        "http://{}/v1/metrics/agents/alice/timeseries?bucket_minutes=5&hours=6",
        bound
    );
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, timeseries_response);

    // ─── 9. GET /v1/metrics/alerts → 200 ───
    let url = format!("http://{}/v1/metrics/alerts", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, alerts_response);

    // ─── 10. GET /v1/metrics/cost → 200 ───
    let url = format!("http://{}/v1/metrics/cost", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, cost_response);
}
