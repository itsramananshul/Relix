//! RELIX-7.11 GAP 1 — end-to-end mini-mesh test for the
//! `MultiChannelAlertSink` → `telegram.send` / `discord.send` /
//! `slack.send` / `email.send` fan-out.
//!
//! Boots a single fake "channel" peer that registers all four
//! `*.send` capabilities, builds a real `MeshClient` pointing
//! at it via `discover_and_pin`, wires the resulting
//! `AlertMeshContext` into a `MultiChannelAlertSink`, and
//! delivers a fired alert with four targets (one per channel).
//!
//! The responder captures the JSON args for every received
//! call so the test can assert:
//!
//! - Telegram target → `telegram.send` arrives with the
//!   configured `chat_id` and a formatted alert body
//! - Discord  target → `discord.send` arrives with `channel_id`
//! - Slack    target → `slack.send` arrives with the configured
//!   channel
//! - Email    target → `email.send` arrives with the `to`
//!   recipient + a subject
//! - A peer that returns RESPONDER_INTERNAL on one target does
//!   NOT block delivery to the other three.

#![cfg(test)]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use relix_core::bundle::Bundle;
use relix_core::identity::{IdentityBundle, issue_identity};
use relix_core::policy::PolicyEngine;
use relix_core::types::{ErrorEnvelope, NodeId, error_kinds};
use relix_runtime::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use relix_runtime::metrics::alert::{
    ActiveAlert, AlertDeliver, AlertEvent, AlertKind, AlertSeverity,
};
use relix_runtime::metrics::{AlertMeshCell, AlertMeshContext, AlertTarget, MultiChannelAlertSink};
use relix_runtime::transport::rpc::{self, Event, Multiaddr};
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::mpsc;

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
                eprintln!("alert-fanout-mini-mesh: boot_peer retry ({e})");
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
        &dir.path().join("alert-fanout-audit.log"),
        responder,
    )
    .expect("bridge constructs");
    (bridge, org_root, dir)
}

fn mint_bundle(org_root: &SigningKey, name: &str, groups: Vec<String>) -> Bundle {
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
    issue_identity(id, org_root, 3600).expect("identity issued")
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

/// Recording responder — holds a JSON value per method.
#[derive(Default)]
struct CallRecorder {
    by_method: Mutex<std::collections::HashMap<String, Vec<Value>>>,
}

impl CallRecorder {
    fn record(&self, method: &str, args: &[u8]) {
        let v: Value = serde_json::from_slice(args).unwrap_or(Value::Null);
        self.by_method
            .lock()
            .unwrap()
            .entry(method.to_string())
            .or_default()
            .push(v);
    }
    fn calls(&self, method: &str) -> Vec<Value> {
        self.by_method
            .lock()
            .unwrap()
            .get(method)
            .cloned()
            .unwrap_or_default()
    }
}

fn fired_alert() -> AlertEvent {
    AlertEvent::Fired(ActiveAlert {
        agent: "alice".into(),
        kind: AlertKind::ErrorRate,
        severity: AlertSeverity::Critical,
        triggered_at_ms: 1_700_000_000_000,
        threshold: 10.0,
        actual: 12.5,
        message: "alice error_rate test".into(),
        method: None,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_channel_sink_fans_out_to_telegram_discord_slack_email() {
    // ─── 1. Recording responder bridge with all four caps ───
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "ops_tg"
        method = "telegram.send"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_dc"
        method = "discord.send"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_sl"
        method = "slack.send"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_em"
        method = "email.send"
        allow_groups = ["operators"]
        "#,
    );
    let recorder = Arc::new(CallRecorder::default());
    for method in ["telegram.send", "discord.send", "slack.send", "email.send"] {
        let recorder = recorder.clone();
        let m = method;
        dispatch.register(
            method,
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let recorder = recorder.clone();
                async move {
                    recorder.record(m, &ctx.args);
                    HandlerOutcome::Ok(b"{\"ok\":true}".to_vec())
                }
            })),
        );
    }
    let dispatch = Arc::new(dispatch);
    let (_client, events, channel_addr) = boot_peer(91).await;
    spawn_inbound_loop(events, dispatch.clone());

    // ─── 2. Caller identity for the MeshClient ───
    let bundle = mint_bundle(&org_root, "alert-test-caller", vec!["operators".into()]);

    // ─── 3. Build a real MeshClient pointed at the responder ───
    use relix_runtime::flow_runner::{PeerEntry, PeersFile};
    use relix_runtime::manifest::{DiscoveryOptions, discover_and_pin};
    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        "channels".to_string(),
        PeerEntry {
            addr: channel_addr.to_string(),
        },
    );
    let peers_file = PeersFile { peers: peers_map };
    // A separate client key for the caller side.
    let client_key = key_for(120);
    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key),
        peers: peers_file,
        deadline_secs: 30,
        overall_timeout: Duration::from_secs(8),
        local_port: None,
        source_key_registry: None,
    };
    let (_cache, mesh) = discover_and_pin(opts).await.expect("discover_and_pin");

    // ─── 4. Build the MultiChannelAlertSink targeting all four ───
    let cell: AlertMeshCell = Arc::new(tokio::sync::OnceCell::new());
    cell.set(AlertMeshContext {
        mesh,
        identity: bundle.clone(),
    })
    .ok()
    .expect("set cell");
    let sink = MultiChannelAlertSink::new(
        cell,
        vec![
            AlertTarget {
                channel: "telegram".into(),
                peer: "channels".into(),
                chat_id: Some("12345".into()),
                ..AlertTarget::default_for_test()
            },
            AlertTarget {
                channel: "discord".into(),
                peer: "channels".into(),
                channel_id: Some("C777".into()),
                ..AlertTarget::default_for_test()
            },
            AlertTarget {
                channel: "slack".into(),
                peer: "channels".into(),
                slack_channel: Some("#ops-alerts".into()),
                ..AlertTarget::default_for_test()
            },
            AlertTarget {
                channel: "email".into(),
                peer: "channels".into(),
                to: Some("ops@example.com".into()),
                subject: Some("Relix critical".into()),
                ..AlertTarget::default_for_test()
            },
        ],
    );

    // ─── 5. Fire the alert ───
    sink.deliver(&fired_alert());

    // ─── 6. Wait for the spawned tasks to dispatch ───
    // Best-effort: poll with a short timeout. 8s is plenty for
    // a localhost libp2p round-trip × 4 targets, and we early-
    // exit the moment all four arrive.
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        let tg = recorder.calls("telegram.send").len();
        let dc = recorder.calls("discord.send").len();
        let sl = recorder.calls("slack.send").len();
        let em = recorder.calls("email.send").len();
        if tg == 1 && dc == 1 && sl == 1 && em == 1 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "alert fan-out did not reach every channel in time: \
                 telegram={tg} discord={dc} slack={sl} email={em}"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // ─── 7. Assert wire shape per channel ───
    let tg = &recorder.calls("telegram.send")[0];
    assert_eq!(tg["chat_id"], "12345");
    assert!(
        tg["text"].as_str().unwrap_or("").contains("alice"),
        "telegram body missing agent name"
    );
    assert!(
        tg["text"].as_str().unwrap_or("").contains("CRITICAL"),
        "telegram body missing severity"
    );

    let dc = &recorder.calls("discord.send")[0];
    assert_eq!(dc["channel_id"], "C777");
    assert!(dc["text"].as_str().unwrap_or("").contains("alice"));

    let sl = &recorder.calls("slack.send")[0];
    assert_eq!(sl["channel"], "#ops-alerts");
    assert!(sl["text"].as_str().unwrap_or("").contains("alice"));

    let em = &recorder.calls("email.send")[0];
    assert_eq!(em["to"][0], "ops@example.com");
    assert_eq!(em["subject"], "Relix critical");
    assert!(em["body"].as_str().unwrap_or("").contains("alice"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_failing_target_does_not_block_others() {
    // Responder that fails `telegram.send` but succeeds on the
    // other three. After the call completes, the recorder still
    // shows that discord / slack / email arrived — proving each
    // target dispatches independently on its own task.
    let (mut dispatch, org_root, _audit_dir) = fresh_responder_bridge(
        r#"
        [[rules]]
        name = "ops_tg"
        method = "telegram.send"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_dc"
        method = "discord.send"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_sl"
        method = "slack.send"
        allow_groups = ["operators"]

        [[rules]]
        name = "ops_em"
        method = "email.send"
        allow_groups = ["operators"]
        "#,
    );
    let recorder = Arc::new(CallRecorder::default());
    // telegram.send always errors.
    dispatch.register(
        "telegram.send",
        Arc::new(FnHandler(|_ctx: InvocationCtx| async move {
            HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: "simulated send failure".into(),
                retry_hint: 0,
                retry_after: None,
            })
        })),
    );
    for method in ["discord.send", "slack.send", "email.send"] {
        let recorder = recorder.clone();
        let m = method;
        dispatch.register(
            method,
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let recorder = recorder.clone();
                async move {
                    recorder.record(m, &ctx.args);
                    HandlerOutcome::Ok(b"{\"ok\":true}".to_vec())
                }
            })),
        );
    }
    let dispatch = Arc::new(dispatch);
    let (_client, events, channel_addr) = boot_peer(92).await;
    spawn_inbound_loop(events, dispatch.clone());

    let bundle = mint_bundle(&org_root, "alert-test-caller-2", vec!["operators".into()]);
    use relix_runtime::flow_runner::{PeerEntry, PeersFile};
    use relix_runtime::manifest::{DiscoveryOptions, discover_and_pin};
    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        "channels".to_string(),
        PeerEntry {
            addr: channel_addr.to_string(),
        },
    );
    let peers_file = PeersFile { peers: peers_map };
    let client_key = key_for(121);
    let opts = DiscoveryOptions {
        identity_bundle: bundle.clone(),
        client_key: zeroize::Zeroizing::new(client_key),
        peers: peers_file,
        deadline_secs: 30,
        overall_timeout: Duration::from_secs(8),
        local_port: None,
        source_key_registry: None,
    };
    let (_cache, mesh) = discover_and_pin(opts).await.expect("discover_and_pin");

    let cell: AlertMeshCell = Arc::new(tokio::sync::OnceCell::new());
    cell.set(AlertMeshContext {
        mesh,
        identity: bundle.clone(),
    })
    .ok()
    .expect("set cell");
    let sink = MultiChannelAlertSink::new(
        cell,
        vec![
            AlertTarget {
                channel: "telegram".into(),
                peer: "channels".into(),
                chat_id: Some("1".into()),
                ..AlertTarget::default_for_test()
            },
            AlertTarget {
                channel: "discord".into(),
                peer: "channels".into(),
                channel_id: Some("C1".into()),
                ..AlertTarget::default_for_test()
            },
            AlertTarget {
                channel: "slack".into(),
                peer: "channels".into(),
                slack_channel: Some("#x".into()),
                ..AlertTarget::default_for_test()
            },
            AlertTarget {
                channel: "email".into(),
                peer: "channels".into(),
                to: Some("ops@e".into()),
                ..AlertTarget::default_for_test()
            },
        ],
    );
    sink.deliver(&fired_alert());

    // The healthy three must arrive even though telegram.send
    // returned an error envelope.
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        let dc = recorder.calls("discord.send").len();
        let sl = recorder.calls("slack.send").len();
        let em = recorder.calls("email.send").len();
        if dc == 1 && sl == 1 && em == 1 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("healthy channels did not all dispatch: dc={dc} sl={sl} em={em}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ── small helper so test builders don't list every Option field ──

trait AlertTargetTestExt {
    fn default_for_test() -> Self;
}

impl AlertTargetTestExt for AlertTarget {
    fn default_for_test() -> Self {
        AlertTarget {
            channel: String::new(),
            peer: String::new(),
            to: None,
            subject: None,
            chat_id: None,
            channel_id: None,
            slack_channel: None,
        }
    }
}
