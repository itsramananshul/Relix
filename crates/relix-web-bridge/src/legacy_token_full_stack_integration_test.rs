//! NOT-DONE 3 — Full real-stack integration test for the
//! legacy-token migration + background-task fail-pass.
//!
//! Boots ONLY real components:
//!   - real `AgentStore` (`rusqlite` against a temp file)
//!   - real `TaskStore` (`rusqlite` against the same file)
//!   - real `DispatchBridge` + real `register_agent_capabilities`
//!     (the same code path `controller_runtime::run` calls)
//!   - real `run_legacy_token_orphaned_task_fail_pass`
//!     (the production background pass, awaited deterministically)
//!   - real mesh transport (`relix_runtime::transport::rpc::new`
//!     opens a libp2p socket on `127.0.0.1`)
//!   - real bridge `AppState` constructed via `AppState::try_new`
//!     + a real `MeshClient` from `discover_and_pin`
//!   - real `axum::serve` on an ephemeral TCP port
//!   - real `reqwest` HTTP client
//!
//! No fakes, no stubs, no canned handler responses. The
//! coordinator-side dispatch routes hit the same SQLite file the
//! migration ran on, so what the HTTP client sees is what
//! production agents would see.
//!
//! Asserts the contract spelled out by NOT-DONE 3:
//!   1. `AgentStore::open` runs the legacy-opaque-token migration
//!      and the row's status is `legacy_token_expired` after.
//!   2. The background task transitions every linked task to
//!      `failed` with `error_cause = "legacy_approval_token_expired"`.
//!   3. `GET /v1/approval/:id` returns the migrated row's full
//!      JSON (status + legacy `decision_note`).
//!   4. `GET /v1/approval/:id` returns the normally-decided row's
//!      JSON (`status = "approved"`, no legacy note).
//!   5. `GET /v1/approval/:id` returns HTTP 404 for an unknown id.
//!   6. The `startup_tasks` ledger records the pass completion +
//!      `rows_processed`.
//!   7. On a second boot against the same database, the pass
//!      short-circuits via `startup_task_is_complete` — no rows
//!      are re-processed and the previously-failed task stays in
//!      `failed`.

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::time::timeout;

use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_core::identity::{IdentityBundle, issue_identity};
use relix_core::policy::PolicyEngine;
use relix_core::types::NodeId;

use relix_runtime::controller_runtime::{
    LEGACY_TOKEN_TASK_FAIL_PASS_NAME, register_agent_capabilities,
    run_legacy_token_orphaned_task_fail_pass,
};
use relix_runtime::dispatch::DispatchBridge;
use relix_runtime::nodes::coordinator::agent::{AgentStore, ApprovalStatus};
use relix_runtime::nodes::coordinator::{CoordinatorConfig, RetryPolicy, TaskStore};
use relix_runtime::transport::rpc::{self, Event, Multiaddr};

use crate::config::{
    AppState, BridgeConfig, BridgeSection, FlowSection, IdentitySection, MeshSection, SseSection,
    TransportSection,
};

const LEGACY_APPROVAL_ID: &str = "apr_legacy_full_stack";
const LEGACY_OPAQUE_TOKEN: &str = "legacy-opaque-token-deadbeef";
const MISSING_APPROVAL_ID: &str = "apr_does_not_exist_zzz";

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
                eprintln!("legacy-token-full-stack: boot_peer retry ({e})");
                continue;
            }
        }
    }
    panic!("boot_peer: exhausted port retries");
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

fn coord_cfg_for(db_path: &std::path::Path) -> CoordinatorConfig {
    CoordinatorConfig {
        db_path: db_path.to_path_buf(),
        max_list: 200,
        recovery_scan: false,
        retention: Default::default(),
        ai_peer: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_token_full_stack_real_controller_real_bridge_real_http() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    // ─── PHASE 1 ── temp dir + DB paths ────────────────────────
    let tmpdir = TempDir::new().expect("tempdir");
    let db_path = tmpdir.path().join("coord.db");
    let coord_cfg = coord_cfg_for(&db_path);

    // ─── PHASE 2 ── seed: open AgentStore + TaskStore on the
    // same file. AgentStore::open creates the approval_requests +
    // startup_tasks tables; TaskStore::open creates the tasks +
    // chronicle tables. Both connections coexist under WAL.
    let agent_store_seed = AgentStore::open(&db_path).expect("agent store boot 0");
    let task_store_seed = TaskStore::open(&coord_cfg).expect("task store boot 0");

    let task_id_legacy = task_store_seed
        .create(
            "legacy approval task",
            "noop.sol",
            "{}",
            "subj-op",
            RetryPolicy::None,
            0,
            None,
            Some("integration-test"),
        )
        .expect("create legacy task");
    let task_id_normal = task_store_seed
        .create(
            "normal approval task",
            "noop.sol",
            "{}",
            "subj-op",
            RetryPolicy::None,
            0,
            None,
            Some("integration-test"),
        )
        .expect("create normal task");

    let normal_approval_id = agent_store_seed
        .create_approval(
            "agt-normal",
            "subj-op",
            "tool.web_read",
            "external_api:read",
            "",
            "fetch normal user data",
            &["operators".into()],
            Some(&task_id_normal),
            9_999_999_999_i64,
            &["subj-op".into()],
            "default",
        )
        .expect("create normal approval");
    let _ = agent_store_seed
        .decide_approval(
            &normal_approval_id,
            ApprovalStatus::Approved,
            "subj-op",
            "normal-approved-ok",
        )
        .expect("decide normal approval");

    // Insert the legacy-shaped row via the AgentStore's
    // `#[doc(hidden)]` test scaffold. The seed AgentStore's
    // initial `migrate_legacy_opaque_tokens` call (run inside
    // `open`) already ran against the empty table, so the row
    // we insert here is left in `pending`. The next
    // `AgentStore::open` rerun (Phase 3) is what flips it.
    agent_store_seed
        .force_insert_legacy_pending_approval_for_test(
            LEGACY_APPROVAL_ID,
            &task_id_legacy,
            LEGACY_OPAQUE_TOKEN,
        )
        .expect("seed legacy pending approval");

    // Drop the seed AgentStore so the next AgentStore::open
    // re-runs `migrate_legacy_opaque_tokens` against the row we
    // just wrote. The drop releases the only handle the test
    // owns so no race with the migration write is possible.
    drop(agent_store_seed);

    // ─── PHASE 3 ── REAL controller-side open: AgentStore::open
    // runs `migrate_legacy_opaque_tokens` as part of its boot
    // contract. After this call the legacy row MUST be flipped to
    // `legacy_token_expired`.
    let agent_store = Arc::new(AgentStore::open(&db_path).expect("agent store boot 1"));
    let task_store = Arc::new(task_store_seed);

    {
        let row = agent_store
            .get_approval(LEGACY_APPROVAL_ID)
            .expect("get legacy approval")
            .expect("legacy row present");
        assert_eq!(
            row.status.as_wire(),
            "legacy_token_expired",
            "migration must flip the legacy row's status"
        );
        let note = row.decision_note.unwrap_or_default();
        assert!(
            note.contains("legacy_token_expired:"),
            "migration must stamp the explanatory decision_note: {note:?}"
        );
    }

    // Sanity: the normally-decided row is `approved` and was
    // untouched by the migration.
    {
        let row = agent_store
            .get_approval(&normal_approval_id)
            .expect("get normal approval")
            .expect("normal row present");
        assert_eq!(row.status.as_wire(), "approved");
    }

    // ─── PHASE 4 ── build a REAL DispatchBridge + register the
    // REAL agent caps. This is the exact wiring sequence
    // `controller_runtime::run` executes.
    let org_root = SigningKey::generate(&mut OsRng);
    let responder = SigningKey::generate(&mut OsRng);
    let policy = PolicyEngine::from_toml(
        r#"
        [[rules]]
        name = "ops_get"
        method = "coord.approval.get"
        allow_groups = ["operators"]
        "#,
    )
    .expect("policy parses");
    let mut bridge = DispatchBridge::new(
        policy,
        org_root.verifying_key(),
        &tmpdir.path().join("audit.log"),
        responder,
    )
    .expect("bridge constructs");
    let clock = bridge.clock();
    // SEC PART 7: the agent gate's `describe` closure reads
    // from a `DescriptorCache` populated at capability-
    // registration time. The test doesn't exercise the
    // category-driven gate path, so an empty cache (built via
    // the manifest provider it is normally shared with) is
    // sufficient and keeps the integration honest about the
    // wire-up shape.
    let descriptor_cache = relix_runtime::manifest::ManifestProvider::new(
        relix_core::types::NodeId([0u8; 32]),
        "test-bridge",
        "coordinator",
        relix_core::types::NodeId([0u8; 32]),
        vec![],
    )
    .descriptor_cache();
    register_agent_capabilities(
        &mut bridge,
        agent_store.clone(),
        task_store.clone(),
        None,
        300,
        clock.clone(),
        descriptor_cache,
        // Metrics disabled for this legacy stack test — no live-spend ledger,
        // so the Action Center budget alerts fall back to allowance-only.
        None,
    );
    let bridge = Arc::new(bridge);

    // ─── PHASE 5 ── run the REAL background fail-pass. The
    // function is normally `tokio::spawn`ed from
    // `controller_runtime::run`; here we `.await` it directly so
    // assertions are deterministic.
    let report = run_legacy_token_orphaned_task_fail_pass(
        agent_store.clone(),
        task_store.clone(),
        clock.clone(),
    )
    .await;
    assert!(
        report.completed,
        "fail pass must complete: report={report:?}"
    );
    assert_eq!(report.considered, 1, "exactly one legacy row to process");
    assert_eq!(report.transitioned, 1, "the linked task must transition");
    assert_eq!(report.errored, 0);
    assert_eq!(report.not_found, 0);
    assert_eq!(report.already_terminal, 0);
    assert_eq!(report.final_cursor, LEGACY_APPROVAL_ID);

    // PHASE 5 verifications against the SQLite source of truth.
    let legacy_task_after = task_store
        .get(&task_id_legacy)
        .expect("task get")
        .expect("legacy task present");
    assert_eq!(
        legacy_task_after.status, "failed",
        "linked task must be failed after the pass"
    );
    assert_eq!(
        legacy_task_after.error_cause.as_deref(),
        Some("legacy_approval_token_expired"),
        "error_cause must be set so operators can grep the chronicle"
    );

    let normal_task_after = task_store
        .get(&task_id_normal)
        .expect("task get")
        .expect("normal task present");
    assert_ne!(
        normal_task_after.status, "failed",
        "the normally-decided approval's task must NOT be affected"
    );

    let ledger = agent_store
        .startup_task_get(LEGACY_TOKEN_TASK_FAIL_PASS_NAME)
        .expect("startup_task_get")
        .expect("ledger row present after pass");
    assert!(
        ledger.completed_at_ms.is_some(),
        "ledger must record completion timestamp"
    );
    assert_eq!(ledger.rows_processed, 1);

    // ─── PHASE 6 ── boot a real mesh peer for the coordinator
    // bridge and wire the inbound-event loop. Every `mesh.call`
    // from the bridge side hits the real `register_agent_capabilities`
    // handlers backed by the real SQLite file above.
    let (_peer_client, events, peer_addr) = boot_peer(177).await;
    spawn_inbound_loop(events, bridge.clone());

    // ─── PHASE 7 ── REAL bridge AppState + REAL discover_and_pin
    // against the real coordinator multiaddr. This is the same
    // path `relix-web-bridge::main::main` builds at startup.
    let bridge_tmp = TempDir::new().expect("bridge tempdir");
    let bundle_bytes = mint_bridge_bundle_bytes(
        &org_root,
        "legacy-token-test-bridge",
        vec!["operators".into()],
    );
    let bundle_path = bridge_tmp.path().join("bridge.bundle");
    std::fs::write(&bundle_path, &bundle_bytes).expect("write bundle");
    // SEC PART 1 (agent-gate default-deny): the bridge's
    // identity-bundle subject_id needs an explicit agent
    // profile so the coordinator's gate doesn't fail-closed
    // on every call. The bridge is a trusted internal peer —
    // give it the `allow-all` profile and audit it as such.
    {
        let bundle_decoded: Bundle = codec::decode(&bundle_bytes).expect("decode bridge bundle");
        let id_payload: IdentityBundle =
            codec::decode(bundle_decoded.payload.as_ref()).expect("decode bridge id payload");
        let bridge_subject = id_payload.subject_id.to_string();
        let bridge_agent_id = agent_store
            .create_agent(
                "legacy-token-test-bridge",
                "bridge",
                "Bridge",
                "internal",
                "ops",
                "integration-test",
                &bridge_subject,
                "critical",
                "default",
            )
            .expect("register bridge agent");
        agent_store
            .update_agent_field(&bridge_agent_id, "profile", "allow-all")
            .expect("set bridge profile = allow-all");
    }
    let client_key_path = bridge_tmp.path().join("client.key");
    let chat_template_path = bridge_tmp.path().join("chat.sol");
    std::fs::write(
        &chat_template_path,
        r#"function start() -> str { return remote_call("coordinator", "noop", "{{SESSION}}|{{MESSAGE}}|"); }"#,
    )
    .expect("write chat template");
    let peers_path = bridge_tmp.path().join("peers.toml");
    std::fs::write(
        &peers_path,
        format!("[peers.coordinator]\naddr = \"{peer_addr}\"\n"),
    )
    .expect("write peers");

    let cfg = BridgeConfig {
        bridge: BridgeSection {
            listen_addr: "127.0.0.1:9999".into(),
            secrets_path: Some(bridge_tmp.path().join("secrets.toml")),
            token_path: Some(bridge_tmp.path().join("bridge-token")),
            memory_db_path: None,
        },
        identity: IdentitySection {
            bundle_path,
            client_key_path,
        },
        transport: TransportSection {
            peers_path,
            deadline_secs: 30,
            data_dir: Some(bridge_tmp.path().to_path_buf()),
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
            addr: peer_addr.to_string(),
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

    // ─── PHASE 8 ── REAL HTTP listener on ephemeral port +
    // REAL reqwest client against the bridge route.
    let app = Router::new()
        .route("/v1/approval/:id", get(crate::approval::get_approval))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let bound = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();

    // (a) GET /v1/approval/<legacy_id> → 200 + status=legacy_token_expired
    let url = format!("http://{bound}/v1/approval/{LEGACY_APPROVAL_ID}");
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .expect("http get legacy not timeout")
        .expect("http get legacy ok");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "legacy approval must return 200"
    );
    let body: Value = resp.json().await.expect("json legacy");
    assert_eq!(
        body.get("approval_id").and_then(Value::as_str),
        Some(LEGACY_APPROVAL_ID)
    );
    assert_eq!(
        body.get("status").and_then(Value::as_str),
        Some("legacy_token_expired"),
        "legacy row surfaces the migration-stamped status"
    );
    let note = body
        .get("decision_note")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        note.contains("legacy_token_expired:"),
        "legacy row's decision_note must surface the migration explanation: {note:?}"
    );

    // (b) GET /v1/approval/<normal_id> → 200 + status=approved
    let url = format!("http://{bound}/v1/approval/{normal_approval_id}");
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .expect("http get normal not timeout")
        .expect("http get normal ok");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "normal approval must return 200"
    );
    let body: Value = resp.json().await.expect("json normal");
    assert_eq!(
        body.get("approval_id").and_then(Value::as_str),
        Some(normal_approval_id.as_str())
    );
    assert_eq!(
        body.get("status").and_then(Value::as_str),
        Some("approved"),
        "normal row must surface its approved status"
    );
    let normal_note = body
        .get("decision_note")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        !normal_note.contains("legacy_token_expired:"),
        "normal row's decision_note must NOT carry the legacy explanation: {normal_note:?}"
    );

    // (c) GET /v1/approval/<missing_id> → 404
    let url = format!("http://{bound}/v1/approval/{MISSING_APPROVAL_ID}");
    let resp = timeout(Duration::from_secs(15), http.get(&url).send())
        .await
        .expect("http get missing not timeout")
        .expect("http get missing ok");
    assert_eq!(
        resp.status().as_u16(),
        404,
        "missing approval id must return HTTP 404"
    );

    // ─── PHASE 9 ── simulate second-boot. Drop every handle that
    // could pin the SQLite file open, then open fresh handles
    // against the SAME db path. The pass MUST short-circuit via
    // `startup_task_is_complete` and leave the linked task
    // unchanged.
    drop(bridge);
    drop(agent_store);
    drop(task_store);

    let agent_store_boot2 = Arc::new(AgentStore::open(&db_path).expect("agent store boot 2"));
    let task_store_boot2 = Arc::new(TaskStore::open(&coord_cfg).expect("task store boot 2"));
    let clock_boot2: Arc<dyn relix_core::clock::Clock> = Arc::new(relix_core::clock::SystemClock);
    let report2 = run_legacy_token_orphaned_task_fail_pass(
        agent_store_boot2.clone(),
        task_store_boot2.clone(),
        clock_boot2,
    )
    .await;
    assert!(report2.completed, "second-boot pass short-circuit succeeds");
    assert_eq!(
        report2.considered, 0,
        "second-boot pass must process zero rows (skipped via ledger)"
    );
    assert_eq!(report2.transitioned, 0);
    assert_eq!(report2.progress_checkpoints, 0);

    let legacy_task_boot2 = task_store_boot2
        .get(&task_id_legacy)
        .expect("task get boot2")
        .expect("legacy task boot2");
    assert_eq!(
        legacy_task_boot2.status, "failed",
        "legacy task stays failed across reboot (no re-processing)"
    );
}
