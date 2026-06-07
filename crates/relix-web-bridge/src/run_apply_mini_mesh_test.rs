//! Mini-mesh HTTP regression for the safe-apply run routes —
//! `GET /v1/runs/:id/diff`, `POST /v1/runs/:id/review`,
//! `POST /v1/runs/:id/apply` — through the WHOLE thin hop:
//! bridge route → `call_peer` → mesh → coordinator capability → the REAL
//! `execute_run_diff` / `execute_run_apply` bodies.
//!
//! The deterministic in-process coverage in `relix-runtime`
//! (`run_apply_capability_*`) already proves the capability bodies on a real
//! file-writing run. This test proves the route/serialization hop that sits in
//! front of them was still manual-only: it boots a fake coordinator peer that
//! registers `run.review` / `run.diff` / `run.apply` against a shared
//! `TaskStore` seeded with a REAL `copy_repo` run that modified a real file,
//! mounts the three bridge routes on an ephemeral listener, and drives the
//! review-to-done loop over HTTP:
//!
//! - `GET /v1/runs/:id/diff` BEFORE review → 200, `eligible:false`, the pending
//!   change is previewable (`plan.changes >= 1`).
//! - `POST /v1/runs/:id/apply` BEFORE review → refused (non-200) and the target
//!   file is NOT written.
//! - `POST /v1/runs/:id/review` accept → 200.
//! - `GET /v1/runs/:id/diff` AFTER review → 200, `eligible:true`.
//! - `POST /v1/runs/:id/apply` AFTER review → 200 with `apply_status:applied`,
//!   `brief_status:done`, `applied_files >= 1`, and the real change lands in the
//!   project root (review-to-done: board `done`, the dependent unblocks).
//!
//! Deterministic, fast, no external CLI / network: the run is produced by a
//! `cmd`/`sh` one-liner registered as a test Rig (the same shape the runtime
//! integration test uses), so the file the apply lands is real.

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
use relix_runtime::controller_runtime::{execute_run_apply, execute_run_diff};
use relix_runtime::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use relix_runtime::nodes::coordinator::heartbeat::{
    DEFAULT_WORKSPACE_MAX_BYTES, DEFAULT_WORKSPACE_MAX_FILES, WorkspaceConfig, WorkspaceContext,
    run_brief_now,
};
use relix_runtime::nodes::coordinator::{RetryPolicy, TaskStore};
use relix_runtime::rig::{ProcessRig, RigRegistry};
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
                eprintln!("run-apply-mini-mesh: boot_peer retry ({e})");
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
        &dir.path().join("run-apply-audit.log"),
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

/// A real adapter that OVERWRITES `seed.txt` in its working directory (the
/// scoped workspace) with `content`. Because `seed.txt` is copied in from the
/// project root, this exercises the MODIFIED-file path — the strongest apply
/// semantics — exactly like the runtime integration test.
fn seed_modifying_rig(content: &str) -> RigRegistry {
    let (prog, args) = if cfg!(windows) {
        (
            "cmd".to_string(),
            vec!["/C".to_string(), format!("echo {content}> seed.txt")],
        )
    } else {
        (
            "sh".to_string(),
            vec!["-c".to_string(), format!("printf '{content}' > seed.txt")],
        )
    };
    let mut reg = RigRegistry::new();
    reg.register(std::sync::Arc::new(ProcessRig::new("mk", prog, args)));
    reg.set_default(Some("mk".to_string()));
    reg
}

fn ready_brief(s: &TaskStore, title: &str, assignee: &str) -> String {
    let id = s
        .create(
            title,
            "flows/none.sol",
            "{}",
            "subj",
            RetryPolicy::None,
            0,
            None,
            None,
        )
        .unwrap();
    s.set_brief_field(&id, "assignee", assignee).unwrap();
    s.set_brief_field(&id, "reviewer", "reviewer_1").unwrap();
    s.set_board_status(&id, "todo").unwrap();
    id
}

/// Register the run.review / run.diff / run.apply capabilities the bridge
/// routes call, wired to a shared seeded store. The diff/apply handlers run the
/// REAL `execute_run_*` bodies; the review handler mirrors the coordinator's
/// thin `run_id|decision|note` parse so the POST route's serialization is
/// exercised verbatim.
fn register_run_caps(bridge: &mut DispatchBridge, store: Arc<TaskStore>) {
    {
        let st = store.clone();
        bridge.register(
            "run.review",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let st = st.clone();
                async move {
                    let arg = String::from_utf8_lossy(&ctx.args).to_string();
                    let mut parts = arg.splitn(3, '|');
                    let run_id = parts.next().unwrap_or("").trim().to_string();
                    let decision = parts.next().unwrap_or("").trim().to_string();
                    let note = parts.next().unwrap_or("").to_string();
                    let tenant = ctx.tenant_id_or_default().to_string();
                    let invalid = |c: String| {
                        HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                            kind: relix_core::types::error_kinds::INVALID_ARGS,
                            cause: c,
                            retry_hint: 0,
                            retry_after: None,
                        })
                    };
                    match st.run_belongs_to_tenant(&run_id, &tenant) {
                        Ok(true) => {}
                        Ok(false) => return invalid(format!("run not found: {run_id}")),
                        Err(e) => return invalid(format!("run.review: {e}")),
                    }
                    match st.set_run_review(&run_id, &decision, &note) {
                        Ok(state) => {
                            let body = serde_json::json!({"run_id": run_id, "review": state});
                            match serde_json::to_vec(&body) {
                                Ok(b) => HandlerOutcome::Ok(b),
                                Err(e) => invalid(format!("run.review encode: {e}")),
                            }
                        }
                        Err(e) => invalid(format!("run.review: {e}")),
                    }
                }
            })),
        );
    }
    {
        let st = store.clone();
        bridge.register(
            "run.diff",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let st = st.clone();
                async move {
                    let run_id = String::from_utf8_lossy(&ctx.args).trim().to_string();
                    let tenant = ctx.tenant_id_or_default().to_string();
                    match execute_run_diff(&st, &run_id, &tenant) {
                        Ok(body) => match serde_json::to_vec(&body) {
                            Ok(b) => HandlerOutcome::Ok(b),
                            Err(e) => HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                                kind: relix_core::types::error_kinds::INVALID_ARGS,
                                cause: format!("run.diff encode: {e}"),
                                retry_hint: 0,
                                retry_after: None,
                            }),
                        },
                        Err(c) => HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                            kind: relix_core::types::error_kinds::INVALID_ARGS,
                            cause: c,
                            retry_hint: 0,
                            retry_after: None,
                        }),
                    }
                }
            })),
        );
    }
    {
        let st = store;
        bridge.register(
            "run.apply",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let st = st.clone();
                async move {
                    let run_id = String::from_utf8_lossy(&ctx.args).trim().to_string();
                    let tenant = ctx.tenant_id_or_default().to_string();
                    match execute_run_apply(&st, &run_id, &tenant) {
                        Ok(body) => match serde_json::to_vec(&body) {
                            Ok(b) => HandlerOutcome::Ok(b),
                            Err(e) => HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                                kind: relix_core::types::error_kinds::INVALID_ARGS,
                                cause: format!("run.apply encode: {e}"),
                                retry_hint: 0,
                                retry_after: None,
                            }),
                        },
                        Err(c) => HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                            kind: relix_core::types::error_kinds::INVALID_ARGS,
                            cause: c,
                            retry_hint: 0,
                            retry_after: None,
                        }),
                    }
                }
            })),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_diff_review_apply_mini_mesh_proves_real_file_review_to_done_over_http() {
    // ─── 1. Seed a REAL copy_repo run that modified a real file ───
    let ws_tmp = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::write(proj.path().join("seed.txt"), "v1").unwrap();

    let mut store = TaskStore::in_memory().unwrap();
    store.set_run_workspace_root(ws_tmp.path().join("runs"));
    store.set_run_workspace_config(WorkspaceConfig {
        context: WorkspaceContext::CopyRepo,
        project_root: proj.path().to_path_buf(),
        max_bytes: DEFAULT_WORKSPACE_MAX_BYTES,
        max_files: DEFAULT_WORKSPACE_MAX_FILES,
    });

    // A track Brief plus a dependent that blocks until the track is done.
    let track = ready_brief(&store, "edit the seed file", "agt_eng");
    let integrate = ready_brief(&store, "integrate", "agt_eng");
    store.add_snag(&integrate, &track).unwrap();

    let report = run_brief_now(
        &store,
        &seed_modifying_rig("v2"),
        None,
        300,
        &track,
        None,
        "go".into(),
    )
    .unwrap();
    let run_id = report.run_id.expect("a committed run has an id");
    // The successful Shift parked the Brief in review; the dependent blocks.
    assert_eq!(
        store.board_status(&track).unwrap().as_deref(),
        Some("in_review")
    );
    assert!(store.is_blocked(&integrate).unwrap());

    let store = Arc::new(store);

    // ─── 2. Boot a fake coordinator peer with the real run.* bodies ───
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "ops_review"
        method = "run.review"
        allow_groups = ["operators"]
        [[rules]]
        name = "ops_diff"
        method = "run.diff"
        allow_groups = ["operators"]
        [[rules]]
        name = "ops_apply"
        method = "run.apply"
        allow_groups = ["operators"]
        "#,
    );
    register_run_caps(&mut dispatch, store.clone());
    let dispatch = Arc::new(dispatch);
    let (_peer_client, events, addr) = boot_peer(173).await;
    spawn_inbound_loop(events, dispatch.clone());

    // ─── 3. Mint bridge identity + wire a real MeshClient at the peer ───
    let tmpdir = TempDir::new().unwrap();
    let bundle_bytes =
        mint_bridge_bundle_bytes(&org_root, "run-apply-test-bridge", vec!["operators".into()]);
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

    // ─── 4. Mount the three safe-apply routes under test ───
    let app = Router::new()
        .route("/v1/runs/:run_id/diff", get(crate::spine::run_diff))
        .route("/v1/runs/:run_id/review", post(crate::spine::run_review))
        .route("/v1/runs/:run_id/apply", post(crate::spine::run_apply))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();
    let diff_url = format!("http://{bound}/v1/runs/{run_id}/diff");
    let review_url = format!("http://{bound}/v1/runs/{run_id}/review");
    let apply_url = format!("http://{bound}/v1/runs/{run_id}/apply");

    // ─── 5. GET /diff BEFORE review → 200, ineligible, change previewable ───
    let resp = timeout(Duration::from_secs(15), http.get(&diff_url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body.get("eligible"),
        Some(&Value::Bool(false)),
        "diff is ineligible before acceptance: {body}"
    );
    assert!(
        body.pointer("/plan/changes")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            >= 1,
        "the pending file change is previewable before acceptance: {body}"
    );

    // ─── 6. POST /apply BEFORE review → refused (non-200), file unchanged ───
    let resp = timeout(Duration::from_secs(15), http.post(&apply_url).send())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        resp.status().as_u16(),
        200,
        "apply must be refused until the run is accepted"
    );
    assert_eq!(
        std::fs::read_to_string(proj.path().join("seed.txt")).unwrap(),
        "v1",
        "a refused apply over HTTP writes nothing"
    );
    assert_eq!(
        store.board_status(&track).unwrap().as_deref(),
        Some("in_review")
    );

    // ─── 7. POST /review accept → 200 ───
    let resp = timeout(
        Duration::from_secs(15),
        http.post(&review_url)
            .json(&serde_json::json!({"decision": "accepted", "note": "lgtm"}))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "accept must succeed");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body.get("review").and_then(Value::as_str), Some("accepted"));

    // ─── 8. GET /diff AFTER review → 200, eligible ───
    let resp = timeout(Duration::from_secs(15), http.get(&diff_url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body.get("eligible"),
        Some(&Value::Bool(true)),
        "acceptance flips the diff eligible: {body}"
    );

    // ─── 9. POST /apply AFTER review → 200, real file lands, review-to-done ─
    let resp = timeout(Duration::from_secs(15), http.post(&apply_url).send())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "an accepted apply succeeds");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body.get("apply_status").and_then(Value::as_str),
        Some("applied"),
        "the apply response field serializes back through the route: {body}"
    );
    assert!(
        body.get("applied_files")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            >= 1
    );
    assert_eq!(
        body.get("brief_status").and_then(Value::as_str),
        Some("done"),
        "a clean apply IS the operator's review-to-done: {body}"
    );
    let landed = std::fs::read_to_string(proj.path().join("seed.txt")).unwrap();
    assert!(
        landed.starts_with("v2"),
        "the run's real change must land in the project root over HTTP: {landed:?}"
    );
    // Review-to-done closed the loop through the capability the route serialized
    // to: the Brief is board `done` and the dependent unblocked.
    assert_eq!(store.board_status(&track).unwrap().as_deref(), Some("done"));
    assert!(
        !store.is_blocked(&integrate).unwrap(),
        "with the track done, the dependent integrate Brief unblocks"
    );
}
