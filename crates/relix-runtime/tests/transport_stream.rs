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

use std::time::Duration;

use futures::StreamExt;
use relix_runtime::transport::rpc::{self, Multiaddr};
use relix_runtime::transport::stream::{
    StreamFrame, StreamReader, StreamWriter, write_request_envelope,
};
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
    // Random port to avoid collisions across parallel test
    // runs. The transport's `new` binds to `127.0.0.1:<port>`.
    let port: u16 = 30_000 + (rand::random::<u16>() % 30_000);
    let (client, mut events, event_loop) = rpc::new(key_for(seed), port)
        .await
        .expect("boot transport peer");
    let listen_addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}")
        .parse()
        .expect("valid multiaddr");
    tokio::spawn(event_loop.run());
    // Drain transport events in the background so the channel
    // doesn't back up (`mpsc::Sender::send` would block the
    // swarm event loop if the receiver were full). Tests below
    // don't need to inspect Event values themselves — the
    // streaming Control has its own internal channel.
    tokio::spawn(async move { while events.recv().await.is_some() {} });
    (client, listen_addr)
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
