//! Swarm-level integration tests for the RELIX-2 streaming
//! substream protocol (`relix-runtime::transport::stream`).
//!
//! These tests boot two real libp2p peers on random localhost
//! ports, dial them together, and exercise:
//!
//! 1. Multi-chunk round trip — caller opens a stream, responder
//!    reads the request envelope and writes a Header + N Chunks
//!    + End. Caller collects every frame in order.
//! 2. Cancellation — caller drops the StreamReader mid-stream.
//!    Responder's next write must fail with a BrokenPipe-class
//!    error, which is the cancellation signal real handlers
//!    use to stop pulling chunks from upstream.
//!
//! The unary `request_response` path is NOT exercised here —
//! these tests pin the streaming-substream contract in
//! isolation. Test-only `#[ignore]`-style gating is not needed
//! because both peers are local; the tests run on every
//! `cargo test --workspace`.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use futures::StreamExt;
use rand::rngs::OsRng;
use relix_core::identity::{IdentityBundle, issue_identity};
use relix_core::policy::PolicyEngine;
use relix_core::types::{ErrorEnvelope, NodeId, error_kinds};
use relix_runtime::dispatch::build_request;
use relix_runtime::dispatch::{DispatchBridge, FnStreamingHandler, HandlerStream, InvocationCtx};
use relix_runtime::transport::rpc::{self, Multiaddr};
use relix_runtime::transport::stream::{
    StreamFrame, StreamReader, StreamWriter, write_request_envelope,
};
use tempfile::TempDir;
use tokio::time::timeout;

/// Build a fresh deterministic-but-unique key for each peer in
/// a test. Using counter-derived bytes so re-running the suite
/// doesn't surface flaky PeerId collisions, while two peers in
/// the SAME test always get distinct keys.
fn key_for(seed: u8) -> [u8; 32] {
    let mut k = [0u8; 32];
    for (i, slot) in k.iter_mut().enumerate() {
        *slot = seed.wrapping_add(i as u8);
    }
    k
}

/// Boot a peer on a random port and spawn its swarm event
/// loop. Returns the Client + Multiaddr the peer is listening
/// on (caller uses the addr to dial). The event receiver is
/// silently drained — tests don't need to inspect transport
/// events for the streaming protocol since `IncomingStreams`
/// is the responder-side surface.
async fn boot_peer(seed: u8) -> (rpc::Client, Multiaddr) {
    // Pick a random port + retry on bind failure. Windows has
    // surprisingly large reserved port ranges (Hyper-V, NAT,
    // etc.) that can return OS error 10013 when libp2p tries
    // to bind, even at ports nominally in the ephemeral
    // range. We retry up to 16 times before giving up.
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

/// Dial peer B from peer A and wait until the connection is
/// established. The `dial` call returns as soon as the dial
/// is queued; we then sleep briefly to let the swarms exchange
/// the noise + yamux handshake. A more robust test would
/// listen for the `PeerConnected` event, but the event channel
/// is drained in `boot_peer`. Sleep is bounded.
async fn dial_and_wait(client_a: &rpc::Client, addr_b: &Multiaddr) {
    client_a.dial(addr_b.clone()).await.expect("dial succeeded");
    // 250ms is enough for two localhost peers to complete the
    // handshake; the previous OpenPrem-port tests use the same
    // bound.
    tokio::time::sleep(Duration::from_millis(250)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_chunk_round_trip_over_libp2p_stream() {
    let (client_a, addr_a) = boot_peer(1).await;
    let (client_b, addr_b) = boot_peer(2).await;
    let peer_b = client_b.peer_id();
    let _ = addr_a; // peer A's address is unused (B doesn't dial A).
    dial_and_wait(&client_a, &addr_b).await;

    // Responder side: register the protocol and answer one
    // inbound stream with Header + 3 Chunks + End.
    let mut incoming = client_b
        .accept_streams()
        .expect("accept_streams: protocol must not be pre-registered");
    let responder = tokio::spawn(async move {
        let (peer, raw_stream) = timeout(Duration::from_secs(5), incoming.next())
            .await
            .expect("inbound stream within 5s")
            .expect("incoming channel closed");
        let mut writer = StreamWriter::new(raw_stream);
        // Read the request envelope first.
        let envelope = writer
            .read_request_envelope()
            .await
            .expect("read request envelope");
        assert_eq!(envelope, b"test-request-envelope");
        // Header frame.
        writer
            .write_frame(&StreamFrame::Header {
                responder: relix_core::types::NodeId([0xCD; 32]),
                aid: serde_bytes::ByteBuf::from(vec![0xAA; 16]),
                processed_at: relix_core::types::Timestamp(123),
            })
            .await
            .expect("write header");
        for i in 0u8..3 {
            writer
                .write_chunk(format!("chunk-{i}").as_bytes())
                .await
                .expect("write chunk");
        }
        writer.write_end().await.expect("write end");
        peer
    });

    // Caller side: open the stream + write request envelope +
    // drive `next_frame` until End.
    let mut raw_stream = client_a
        .open_stream(peer_b)
        .await
        .expect("open_stream succeeded");
    write_request_envelope(&mut raw_stream, b"test-request-envelope")
        .await
        .expect("write request envelope");
    let mut reader = StreamReader::new(raw_stream);

    let header = reader
        .next_frame()
        .await
        .expect("read header")
        .expect("header present");
    assert!(matches!(header, StreamFrame::Header { .. }));
    let mut chunks: Vec<String> = Vec::new();
    loop {
        let frame = reader
            .next_frame()
            .await
            .expect("frame read")
            .expect("frame present");
        match frame {
            StreamFrame::Chunk(b) => {
                chunks.push(String::from_utf8(b.to_vec()).expect("utf-8 chunk"));
            }
            StreamFrame::End => break,
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    assert_eq!(chunks, vec!["chunk-0", "chunk-1", "chunk-2"]);
    let _ = responder.await.expect("responder task joined");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn caller_dropping_reader_cancels_responder_writes() {
    let (client_a, _addr_a) = boot_peer(3).await;
    let (client_b, addr_b) = boot_peer(4).await;
    let peer_b = client_b.peer_id();
    dial_and_wait(&client_a, &addr_b).await;

    let mut incoming = client_b
        .accept_streams()
        .expect("accept_streams: protocol must not be pre-registered");

    // Channel used to surface the cancellation result back to
    // the test body. The responder writes one chunk
    // successfully, then keeps trying — the caller drops the
    // reader after one chunk, so subsequent writes must fail.
    // `true` = responder observed the cancellation; `false` =
    // it never failed (the test asserts `true`).
    let (tx_cancel, rx_cancel) = tokio::sync::oneshot::channel::<bool>();
    let responder = tokio::spawn(async move {
        let (_peer, raw_stream) = timeout(Duration::from_secs(5), incoming.next())
            .await
            .expect("inbound stream within 5s")
            .expect("incoming channel closed");
        let mut writer = StreamWriter::new(raw_stream);
        let _ = writer.read_request_envelope().await;
        // First chunk succeeds.
        writer
            .write_chunk(b"first")
            .await
            .expect("first write succeeds before reader drops");
        // Give the caller time to drop the reader.
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Subsequent writes must eventually fail. We try
        // several times because libp2p's stream close may not
        // surface on the very first write after the remote
        // close (yamux buffers).
        let mut cancelled = false;
        for _ in 0..20 {
            match writer.write_chunk(b"after-cancel").await {
                Ok(()) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(_) => {
                    cancelled = true;
                    break;
                }
            }
        }
        let _ = tx_cancel.send(cancelled);
    });

    let mut raw_stream = client_a
        .open_stream(peer_b)
        .await
        .expect("open_stream succeeded");
    write_request_envelope(&mut raw_stream, b"cancel-test")
        .await
        .expect("write envelope");
    let mut reader = StreamReader::new(raw_stream);
    let first = reader
        .next_frame()
        .await
        .expect("read first frame")
        .expect("first frame present");
    match first {
        StreamFrame::Chunk(b) => assert_eq!(b.as_ref(), b"first"),
        other => panic!("expected first Chunk, got {other:?}"),
    }
    // Drop the reader to cancel.
    drop(reader);

    let cancelled = timeout(Duration::from_secs(5), rx_cancel)
        .await
        .expect("responder finished within 5s")
        .expect("oneshot delivered");
    assert!(
        cancelled,
        "responder must observe a write failure after reader drop"
    );
    let _ = responder.await;
}

// ────────────────────────────── Step 2 ─────────────────────────
//
// Dispatch-layer integration tests: real DispatchBridge on the
// responder side, real admission pipeline, real streaming
// handler. The caller opens a `/relix/rpc/stream/1` substream
// and writes a CBOR-encoded RequestEnvelope; the responder's
// streaming accept task routes it through
// `bridge.handle_inbound_stream`, which runs the full
// admission flow (decode → deadline → identity → unknown-method
// → agent gate → policy → access broker → dispatch) before
// invoking the handler.

/// Build a DispatchBridge backed by a temp audit log + the
/// caller-supplied policy. The org-root key is returned so the
/// test can mint an identity bundle the bridge will accept.
fn fresh_bridge(policy_toml: &str) -> (DispatchBridge, SigningKey, TempDir) {
    let dir = TempDir::new().unwrap();
    let org_root = SigningKey::generate(&mut OsRng);
    let responder = SigningKey::generate(&mut OsRng);
    let policy = PolicyEngine::from_toml(policy_toml).expect("policy parses");
    let bridge = DispatchBridge::new(
        policy,
        org_root.verifying_key(),
        &dir.path().join("audit.log"),
        responder,
    )
    .expect("bridge constructs");
    (bridge, org_root, dir)
}

/// Build an identity bundle from `org_root` carrying the given
/// groups. Mirrors the helper in dispatch::tests.
fn make_bundle(
    org_root: &SigningKey,
    name: &str,
    groups: Vec<String>,
) -> relix_core::bundle::Bundle {
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

/// Spawn the streaming-accept task that the controller_runtime
/// also spawns at startup. Used by tests that don't want to
/// boot a full controller binary. Mirrors the logic in
/// `crates/relix-runtime/src/controller_runtime.rs` (kept in
/// sync by hand — if either side drifts the integration test
/// catches it).
fn spawn_streaming_accept_task(client: &rpc::Client, bridge: Arc<DispatchBridge>) {
    let mut incoming = client
        .accept_streams()
        .expect("accept_streams: protocol must not be pre-registered");
    tokio::spawn(async move {
        while let Some((peer, raw_stream)) = incoming.next().await {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                let mut writer = StreamWriter::new(raw_stream);
                let envelope = match writer.read_request_envelope().await {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        eprintln!(
                            "streaming: caller closed before envelope arrived ({e}); peer={peer}"
                        );
                        return;
                    }
                };
                bridge.handle_inbound_stream(envelope, writer).await;
            });
        }
    });
}

/// Read every StreamFrame off the wire until End or Err and
/// return the collected payload as a Vec of strings. Header is
/// captured separately so the test can assert on it. Err frames
/// terminate the stream and surface as the second element of
/// the returned tuple. Caller asserts on both shapes.
async fn collect_stream(
    mut reader: StreamReader,
) -> (Option<NodeId>, Vec<String>, Option<(u32, String)>) {
    let mut header_responder: Option<NodeId> = None;
    let mut chunks: Vec<String> = Vec::new();
    let mut err_terminator: Option<(u32, String)> = None;
    loop {
        let frame = match reader.next_frame().await {
            Ok(Some(f)) => f,
            Ok(None) => break, // EOF without terminator — treat as graceful close.
            Err(e) => {
                err_terminator = Some((error_kinds::TRANSPORT, format!("read frame: {e}")));
                break;
            }
        };
        match frame {
            StreamFrame::Header { responder, .. } => {
                header_responder = Some(responder);
            }
            StreamFrame::Chunk(b) => {
                chunks.push(String::from_utf8_lossy(b.as_ref()).into_owned());
            }
            StreamFrame::End => break,
            StreamFrame::Err { kind, cause } => {
                err_terminator = Some((kind, cause));
                break;
            }
        }
    }
    (header_responder, chunks, err_terminator)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_dispatch_admission_passes_and_pipes_handler_chunks() {
    let (mut bridge, org_root, _audit_dir) = fresh_bridge(
        r#"
        [[rules]]
        name = "operators_can_stream"
        method = "test.stream.echo"
        allow_groups = ["operators"]
        "#,
    );
    bridge.register_streaming(
        "test.stream.echo",
        Arc::new(FnStreamingHandler(|ctx: InvocationCtx| async move {
            // The handler echoes the request args back as
            // three chunks: the literal bytes, the bytes
            // reversed, and a fixed "done" marker. The test
            // asserts on all three.
            let args = ctx.args;
            let reversed: Vec<u8> = args.iter().rev().copied().collect();
            let stream = futures::stream::iter(vec![Ok(args), Ok(reversed), Ok(b"done".to_vec())]);
            Ok(Box::pin(stream) as HandlerStream)
        })),
    );
    let bridge = Arc::new(bridge);

    let (client_a, _addr_a) = boot_peer(11).await;
    let (client_b, addr_b) = boot_peer(12).await;
    let peer_b = client_b.peer_id();
    dial_and_wait(&client_a, &addr_b).await;

    spawn_streaming_accept_task(&client_b, bridge.clone());

    let bundle = make_bundle(&org_root, "alice", vec!["operators".into()]);
    let envelope = build_request("test.stream.echo", b"hi".to_vec(), bundle, 30);

    let mut raw_stream = client_a
        .open_stream(peer_b)
        .await
        .expect("open_stream succeeded");
    write_request_envelope(&mut raw_stream, &envelope)
        .await
        .expect("write envelope");
    let reader = StreamReader::new(raw_stream);
    let (header, chunks, err) = timeout(Duration::from_secs(5), collect_stream(reader))
        .await
        .expect("collect within 5s");
    assert!(err.is_none(), "expected graceful End, got {err:?}");
    assert!(header.is_some(), "Header frame must arrive before Chunks");
    assert_eq!(
        chunks,
        vec!["hi".to_string(), "ih".to_string(), "done".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_dispatch_policy_denied_surfaces_terminal_err_frame() {
    let (mut bridge, org_root, _audit_dir) = fresh_bridge(
        r#"
        [[rules]]
        name = "operators_only"
        method = "test.stream.private"
        allow_groups = ["operators"]
        "#,
    );
    bridge.register_streaming(
        "test.stream.private",
        Arc::new(FnStreamingHandler(|_ctx: InvocationCtx| async move {
            // Should never be invoked — policy denies before
            // dispatch.
            let stream = futures::stream::iter(vec![Ok(b"should-not-reach".to_vec())]);
            Ok(Box::pin(stream) as HandlerStream)
        })),
    );
    let bridge = Arc::new(bridge);

    let (client_a, _addr_a) = boot_peer(13).await;
    let (client_b, addr_b) = boot_peer(14).await;
    let peer_b = client_b.peer_id();
    dial_and_wait(&client_a, &addr_b).await;
    spawn_streaming_accept_task(&client_b, bridge.clone());

    // Caller has `chat-users` group, NOT `operators` — denied.
    let bundle = make_bundle(&org_root, "alice", vec!["chat-users".into()]);
    let envelope = build_request("test.stream.private", b"hi".to_vec(), bundle, 30);

    let mut raw_stream = client_a
        .open_stream(peer_b)
        .await
        .expect("open_stream succeeded");
    write_request_envelope(&mut raw_stream, &envelope)
        .await
        .expect("write envelope");
    let reader = StreamReader::new(raw_stream);
    let (_, chunks, err) = timeout(Duration::from_secs(5), collect_stream(reader))
        .await
        .expect("collect within 5s");
    assert!(
        chunks.is_empty(),
        "no Chunk frames must arrive when policy denies"
    );
    let (kind, cause) = err.expect("policy denial must surface a terminal Err frame");
    assert_eq!(kind, error_kinds::POLICY_DENIED);
    assert!(
        cause.contains("deny") || cause.contains("default_deny"),
        "denial cause should reference deny / default_deny: {cause}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_dispatch_unknown_method_surfaces_terminal_err_frame() {
    // Bridge has NO streaming handler registered — every
    // method lookup misses.
    let (bridge, org_root, _audit_dir) = fresh_bridge("");
    let bridge = Arc::new(bridge);

    let (client_a, _addr_a) = boot_peer(15).await;
    let (client_b, addr_b) = boot_peer(16).await;
    let peer_b = client_b.peer_id();
    dial_and_wait(&client_a, &addr_b).await;
    spawn_streaming_accept_task(&client_b, bridge.clone());

    let bundle = make_bundle(&org_root, "alice", vec!["operators".into()]);
    let envelope = build_request("test.stream.absent", b"hi".to_vec(), bundle, 30);

    let mut raw_stream = client_a
        .open_stream(peer_b)
        .await
        .expect("open_stream succeeded");
    write_request_envelope(&mut raw_stream, &envelope)
        .await
        .expect("write envelope");
    let reader = StreamReader::new(raw_stream);
    let (_, chunks, err) = timeout(Duration::from_secs(5), collect_stream(reader))
        .await
        .expect("collect within 5s");
    assert!(chunks.is_empty(), "unknown method must not produce chunks");
    let (kind, cause) = err.expect("unknown method must surface a terminal Err frame");
    assert_eq!(kind, error_kinds::UNKNOWN_METHOD);
    assert!(
        cause.contains("unknown streaming method"),
        "cause should name the missing method: {cause}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_dispatch_handler_returning_error_surfaces_err_frame() {
    let (mut bridge, org_root, _audit_dir) = fresh_bridge(
        r#"
        [[rules]]
        name = "allow_test"
        method = "test.stream.fails"
        allow_groups = ["operators"]
        "#,
    );
    bridge.register_streaming(
        "test.stream.fails",
        Arc::new(FnStreamingHandler(|_ctx: InvocationCtx| async move {
            // Handler returns an Err directly — admission passed,
            // but the handler bails before producing any chunks.
            Err::<HandlerStream, ErrorEnvelope>(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: "simulated handler failure".to_string(),
                retry_hint: 0,
                retry_after: None,
            })
        })),
    );
    let bridge = Arc::new(bridge);

    let (client_a, _addr_a) = boot_peer(17).await;
    let (client_b, addr_b) = boot_peer(18).await;
    let peer_b = client_b.peer_id();
    dial_and_wait(&client_a, &addr_b).await;
    spawn_streaming_accept_task(&client_b, bridge.clone());

    let bundle = make_bundle(&org_root, "alice", vec!["operators".into()]);
    let envelope = build_request("test.stream.fails", b"hi".to_vec(), bundle, 30);
    let mut raw_stream = client_a
        .open_stream(peer_b)
        .await
        .expect("open_stream succeeded");
    write_request_envelope(&mut raw_stream, &envelope)
        .await
        .expect("write envelope");
    let reader = StreamReader::new(raw_stream);
    let (header, chunks, err) = timeout(Duration::from_secs(5), collect_stream(reader))
        .await
        .expect("collect within 5s");
    // Header still arrives — admission passed before the
    // handler bailed.
    assert!(
        header.is_some(),
        "Header frame must precede handler invocation"
    );
    assert!(chunks.is_empty());
    let (kind, cause) = err.expect("handler error must surface a terminal Err frame");
    assert_eq!(kind, error_kinds::RESPONDER_INTERNAL);
    assert_eq!(cause, "simulated handler failure");
}
