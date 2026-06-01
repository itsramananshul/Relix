//! RELIX-7.28 end-to-end mini-mesh integration tests.
//!
//! Boots a fake "coordinator" peer that registers the observability,
//! budget, and PII coordinator capabilities with canned JSON responses,
//! builds a real `MeshClient` via `discover_and_pin`, mounts the new
//! bridge routes on an ephemeral axum listener, and drives reqwest
//! requests through the whole stack.

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
        let port: u16 = 36_000 + (rand::random::<u16>() % 25_000);
        match rpc::new(key_for(seed), port).await {
            Ok((client, events, event_loop)) => {
                let listen_addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}")
                    .parse()
                    .expect("valid multiaddr");
                tokio::spawn(event_loop.run());
                return (client, events, listen_addr);
            }
            Err(e) => {
                eprintln!("observability-mini-mesh: boot_peer retry ({e})");
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
        &dir.path().join("obs-audit.log"),
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
async fn observability_mini_mesh_covers_alerts_history_health_budget_pii() {
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "ops_active_alerts"
        method = "observability.active_alerts"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_history"
        method = "observability.alert_history"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_health"
        method = "observability.health_summary"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_budget_status"
        method = "budget.status"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_budget_reset"
        method = "budget.reset"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_pii_stats"
        method = "pii.scan_stats"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_pii_events"
        method = "pii.recent_events"
        allow_groups = ["operators"]
        "#,
    );

    let alerts_response = serde_json::json!([
        {
            "agent": "alice",
            "kind": "budget_exceeded",
            "severity": "critical",
            "triggered_at_ms": 1_700_000_000_000_i64,
            "threshold": 1_000_000.0,
            "actual": 2_500_000.0,
            "message": "agent alice tripped its daily cap",
            "method": "budget:agent:daily"
        }
    ]);
    let history_response = serde_json::json!([
        {
            "event_type": "alert.fired",
            "agent": "alice",
            "metric": "budget_exceeded",
            "severity": "critical",
            "actual_value": 2_500_000.0,
            "threshold_value": 1_000_000.0,
            "triggered_at": "2023-11-14T22:13:20.000Z",
            "recovered_at": null,
            "recorded_at_ms": 1_700_000_000_000_i64,
            "method": "budget:agent:daily",
            "message": "agent alice tripped its daily cap"
        }
    ]);
    let health_response = serde_json::json!({
        "agents": [
            {
                "agent": "alice",
                "score": 65,
                "status": "yellow",
                "error_rate_pct": 5.0,
                "p95_latency_ms": 2500,
                "avg_confidence": 0.8,
                "daily_budget_utilization_pct": 80.0,
                "active_alerts": 1
            }
        ],
        "deployment": {
            "total_cost_usd": 4.25,
            "total_invocations": 1200,
            "overall_error_rate_pct": 2.5,
            "active_alert_count": 1,
            "avg_health_score": 65
        },
        "window_hours": 24
    });
    let budget_status_response = serde_json::json!({
        "agents": [
            {
                "agent": "alice",
                "daily_limit_micros": 1_000_000,
                "daily_actual_micros": 800_000,
                "daily_resets_at_ms": 1_700_086_400_000_i64,
                "hourly_limit_micros": null,
                "hourly_actual_micros": 0,
                "hourly_resets_at_ms": 1_700_003_600_000_i64,
                "action": "throttle"
            }
        ]
    });
    let budget_reset_response = serde_json::json!({"ok": true, "reset": "agent alice / daily"});
    let pii_stats_response = serde_json::json!({
        "window_hours": 24,
        "total_events": 7,
        "blocked": 1,
        "redacted": 5,
        "logged": 1,
        "top_methods": [
            {"method": "ai.chat", "count": 6}
        ]
    });
    let pii_events_response = serde_json::json!([
        {
            "request_id": "req-1",
            "agent": "alice",
            "method": "ai.chat",
            "direction": "inbound",
            "action_taken": "redacted",
            "span_count": 2,
            "recorded_at_ms": 1_700_000_000_000_i64,
            "types": "EMAIL"
        }
    ]);

    let alerts_arc = Arc::new(alerts_response.clone());
    let history_arc = Arc::new(history_response.clone());
    let health_arc = Arc::new(health_response.clone());
    let budget_status_arc = Arc::new(budget_status_response.clone());
    let budget_reset_arc = Arc::new(budget_reset_response.clone());
    let pii_stats_arc = Arc::new(pii_stats_response.clone());
    let pii_events_arc = Arc::new(pii_events_response.clone());

    fn register_canned(bridge: &mut DispatchBridge, method: &'static str, body: Arc<Value>) {
        bridge.register(
            method,
            Arc::new(FnHandler({
                move |_ctx: InvocationCtx| {
                    let body = body.clone();
                    async move { HandlerOutcome::Ok(serde_json::to_vec(&*body).unwrap_or_default()) }
                }
            })),
        );
    }

    register_canned(&mut dispatch, "observability.active_alerts", alerts_arc);
    register_canned(&mut dispatch, "observability.alert_history", history_arc);
    register_canned(&mut dispatch, "observability.health_summary", health_arc);
    register_canned(&mut dispatch, "budget.status", budget_status_arc);
    register_canned(&mut dispatch, "budget.reset", budget_reset_arc);
    register_canned(&mut dispatch, "pii.scan_stats", pii_stats_arc);
    register_canned(&mut dispatch, "pii.recent_events", pii_events_arc);

    let dispatch = Arc::new(dispatch);
    let (_client, events, addr) = boot_peer(91).await;
    spawn_inbound_loop(events, dispatch.clone());

    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes =
        mint_bridge_bundle_bytes(&org_root, "obs-test-bridge", vec!["operators".into()]);
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
            "/v1/observability/alerts",
            get(crate::observability::active_alerts),
        )
        .route(
            "/v1/observability/alerts/history",
            get(crate::observability::alert_history),
        )
        .route(
            "/v1/observability/health",
            get(crate::observability::health),
        )
        .route("/v1/budget/status", get(crate::budget::status))
        .route("/v1/budget/reset", post(crate::budget::reset))
        .route("/v1/pii/stats", get(crate::pii::stats))
        .route("/v1/pii/events", get(crate::pii::events))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();

    // 1. /v1/observability/alerts → 200 + array with budget_exceeded.
    let url = format!("http://{}/v1/observability/alerts", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, alerts_response);
    assert_eq!(
        body.as_array()
            .and_then(|a| a.first())
            .and_then(|v| v["kind"].as_str()),
        Some("budget_exceeded")
    );

    // 2. /v1/observability/alerts/history?limit=10&agent=alice → 200 + array.
    let url = format!(
        "http://{}/v1/observability/alerts/history?limit=10&agent=alice",
        bound
    );
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, history_response);

    // 3. /v1/observability/health → 200 with deployment summary.
    let url = format!("http://{}/v1/observability/health", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body.get("deployment")
            .and_then(|d| d.get("total_invocations"))
            .and_then(Value::as_u64),
        Some(1200)
    );
    assert_eq!(
        body.get("agents")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|v| v.get("status"))
            .and_then(Value::as_str),
        Some("yellow")
    );

    // 4. /v1/budget/status → 200.
    let url = format!("http://{}/v1/budget/status", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, budget_status_response);

    // 5. /v1/budget/reset → 200.
    let url = format!("http://{}/v1/budget/reset", bound);
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&url)
            .json(&serde_json::json!({"agent": "alice", "window": "daily"}))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body.get("ok").and_then(Value::as_bool), Some(true));

    // 6. /v1/pii/stats → 200 with totals.
    let url = format!("http://{}/v1/pii/stats?hours=24", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, pii_stats_response);

    // 7. /v1/pii/events?method=ai.chat → 200 with one row.
    let url = format!("http://{}/v1/pii/events?method=ai.chat&limit=10", bound);
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, pii_events_response);
}
