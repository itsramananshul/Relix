//! libp2p streaming substream protocol for the mesh.
//!
//! Companion to [`super::rpc`]. The existing `request_response`
//! transport is unary — a single request CBOR envelope, a
//! single response CBOR envelope. This module adds a real
//! streaming substream on top of the same libp2p swarm so that
//! capabilities marked `stream_out` can write a sequence of
//! frames back to the caller as they're produced (token-by-
//! token from an AI provider, log lines from a long-running
//! tool call, etc.).
//!
//! ## Why a separate module
//!
//! `request_response::cbor::Behaviour` is fundamentally one-
//! request-one-response; the upstream behaviour completes the
//! request when the response future resolves. Streaming
//! requires a substream that stays open while frames are
//! written, which is exactly what
//! [`libp2p_stream::Behaviour`] provides at the swarm layer.
//!
//! ## Protocol
//!
//! Identifier: [`PROTOCOL`] (`/relix/rpc/stream/1`).
//!
//! Wire shape (over the libp2p substream — already
//! authenticated by Noise XK + multiplexed by Yamux):
//!
//! ```text
//! request_envelope_bytes:
//!   [u32 BE length prefix] [CBOR-encoded RequestEnvelope]
//!
//! response_frames:
//!   [u32 BE length prefix] [CBOR-encoded StreamFrame]
//!   [u32 BE length prefix] [CBOR-encoded StreamFrame]
//!   ...
//! ```
//!
//! Each [`StreamFrame`] carries one of three payloads:
//!
//! - `Header { responder, aid, processed_at }` — metadata about
//!   the responder. MUST be the first frame on the wire so
//!   audit correlation matches the unary path's
//!   [`super::envelope::ResponseEnvelope`].
//! - `Chunk(bytes)` — one chunk of the streaming body. Zero or
//!   more. Bytes are opaque to the transport (chat tokens,
//!   log lines, partial tool output).
//! - `End` — graceful terminator. The responder is done.
//! - `Err(ErrorEnvelope)` — terminal error frame. Replaces
//!   `End` when the responder bailed mid-stream.
//!
//! Either side closing the substream cleanly mid-stream is
//! treated as cancellation — see [`StreamReader::next_frame`]
//! for the read-side semantics.
//!
//! ## Backpressure + cancellation
//!
//! Frames are written sequentially; an unread substream
//! applies natural backpressure via Yamux's flow control. If
//! the caller drops the [`StreamReader`], the AsyncRead is
//! dropped, the substream closes, and the next responder write
//! returns `BrokenPipe` — that's the cancellation signal the
//! responder uses to abandon further provider calls. No extra
//! cancellation channel is needed.

use std::io;

use futures::{AsyncReadExt, AsyncWriteExt};
use libp2p::{Stream, StreamProtocol};
use relix_core::types::{NodeId, Timestamp};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

/// Maximum size of any single frame on the wire (envelope or
/// stream frame). Picked generously so a single chunk can carry
/// a full AI provider chunk (the OpenAI SSE format emits chunks
/// up to a few KB) while still bounding adversarial blowups.
pub const MAX_FRAME_BYTES: u32 = 1024 * 1024;

/// libp2p protocol identifier. Versioned `/1` so a future wire
/// change can ship alongside without breaking older peers — the
/// substream negotiation will reject unknown versions at upgrade
/// time, surfacing a precise error to the caller.
pub const PROTOCOL: StreamProtocol = StreamProtocol::new("/relix/rpc/stream/1");

/// One frame on the response side. The first frame on every
/// substream MUST be `Header`; subsequent frames are zero or
/// more `Chunk`s followed by exactly one of `End` / `Err`.
///
/// The variant discriminant lives in CBOR's tagged-union
/// representation (`{ "Header": {...} }`), matching the rest of
/// the codebase's `serde(rename_all = "snake_case")` posture.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamFrame {
    /// First-frame envelope metadata so the caller can stamp
    /// the audit record id + responder peer id alongside the
    /// streamed body. Mirrors the unary
    /// [`super::envelope::ResponseEnvelope`] header fields the
    /// dispatch bridge already consumes.
    Header {
        /// Responder node id (matches the unary envelope).
        responder: NodeId,
        /// Audit record id — same shape as the unary path's
        /// `ResponseEnvelope::aid` so cross-correlation works
        /// uniformly across streaming + non-streaming calls.
        aid: ByteBuf,
        /// Wall-clock timestamp at which the responder began
        /// the streaming body. Useful for latency diffs in the
        /// per-flow event log.
        processed_at: Timestamp,
    },
    /// One chunk of the streaming body. Bytes are opaque to
    /// the transport.
    Chunk(ByteBuf),
    /// Graceful terminator. The responder is done — the caller
    /// should close the read side.
    End,
    /// Terminal error frame. Carries a free-form cause string
    /// so the dispatcher layer can translate it to the
    /// canonical `ErrorEnvelope` vocabulary.
    Err {
        /// `relix_core::types::error_kinds` value.
        kind: u32,
        /// Human-readable cause.
        cause: String,
    },
}

/// Outbound side of a stream — produced by
/// [`crate::transport::rpc::Client::open_stream`]. The
/// underlying libp2p substream is owned here; dropping this
/// value closes the substream from the caller's side.
pub struct StreamReader {
    inner: Stream,
}

impl StreamReader {
    /// Wrap a freshly-opened libp2p stream. The caller is
    /// responsible for having already written the
    /// [`RequestEnvelope`] via [`write_request_envelope`].
    pub fn new(inner: Stream) -> Self {
        Self { inner }
    }

    /// Read the next frame off the wire. Returns `Ok(None)` on
    /// EOF — the responder closed without sending an `End`
    /// frame, treated as graceful termination by the caller.
    /// Decode failures and oversize frames map to
    /// [`io::ErrorKind::InvalidData`].
    pub async fn next_frame(&mut self) -> io::Result<Option<StreamFrame>> {
        match read_length_prefixed(&mut self.inner).await? {
            None => Ok(None),
            Some(bytes) => Ok(Some(decode_frame(&bytes)?)),
        }
    }

    /// Drop the substream cleanly. Equivalent to letting the
    /// [`StreamReader`] go out of scope, but explicit so
    /// cancellation sites read naturally.
    pub async fn close(mut self) -> io::Result<()> {
        self.inner.close().await
    }
}

/// Inbound side of a stream — produced by the libp2p stream
/// behaviour when a peer opens a new `[PROTOCOL]` substream.
/// Owns the underlying libp2p stream; dropping closes it from
/// the responder's side.
pub struct StreamWriter {
    inner: Stream,
}

impl StreamWriter {
    pub fn new(inner: Stream) -> Self {
        Self { inner }
    }

    /// Read the [`super::envelope::RequestEnvelope`] the caller
    /// wrote at the head of the substream. Caller wrote bytes
    /// via [`write_request_envelope`] — this is the matching
    /// read on the responder side.
    pub async fn read_request_envelope(&mut self) -> io::Result<Vec<u8>> {
        match read_length_prefixed(&mut self.inner).await? {
            None => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stream closed before request envelope arrived",
            )),
            Some(bytes) => Ok(bytes),
        }
    }

    /// Write one [`StreamFrame`] to the wire. Returns a
    /// `BrokenPipe` error when the caller has dropped their
    /// [`StreamReader`] — that's the cancellation signal the
    /// responder uses to stop pulling chunks from upstream.
    pub async fn write_frame(&mut self, frame: &StreamFrame) -> io::Result<()> {
        let body = encode_frame(frame)?;
        write_length_prefixed(&mut self.inner, &body).await
    }

    /// Convenience: write a `Chunk` frame.
    pub async fn write_chunk(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_frame(&StreamFrame::Chunk(ByteBuf::from(bytes.to_vec())))
            .await
    }

    /// Convenience: write the terminal `End` frame and flush.
    pub async fn write_end(mut self) -> io::Result<()> {
        self.write_frame(&StreamFrame::End).await?;
        self.inner.flush().await?;
        self.inner.close().await
    }

    /// Convenience: write a terminal `Err` frame.
    pub async fn write_err(mut self, kind: u32, cause: impl Into<String>) -> io::Result<()> {
        self.write_frame(&StreamFrame::Err {
            kind,
            cause: cause.into(),
        })
        .await?;
        self.inner.flush().await?;
        self.inner.close().await
    }
}

/// Write a [`super::envelope::RequestEnvelope`] (already
/// CBOR-encoded by the caller — the dispatch bridge owns the
/// envelope shape; this transport layer only frames the bytes)
/// onto a freshly-opened substream. Used by the outbound
/// caller path immediately after
/// [`crate::transport::rpc::Client::open_stream`] returns.
pub async fn write_request_envelope(stream: &mut Stream, envelope: &[u8]) -> io::Result<()> {
    write_length_prefixed(stream, envelope).await
}

/// Encode a [`StreamFrame`] to CBOR bytes. Wraps `ciborium`
/// (the codebase's existing CBOR backend) so the rest of the
/// module doesn't have to know about the codec choice.
pub fn encode_frame(frame: &StreamFrame) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(64);
    ciborium::ser::into_writer(frame, &mut buf).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("stream frame encode: {e}"),
        )
    })?;
    Ok(buf)
}

/// Decode a CBOR-encoded [`StreamFrame`].
pub fn decode_frame(bytes: &[u8]) -> io::Result<StreamFrame> {
    ciborium::de::from_reader(bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("stream frame decode: {e}"),
        )
    })
}

// ─────────────────────── Length-prefixed framing ────────────

/// Write `bytes` to the substream prefixed with a u32 BE
/// length header. Rejects payloads larger than
/// [`MAX_FRAME_BYTES`] to prevent adversarial peers from
/// pinning unbounded memory.
async fn write_length_prefixed<S>(stream: &mut S, bytes: &[u8]) -> io::Result<()>
where
    S: futures::AsyncWrite + Unpin,
{
    if bytes.len() > MAX_FRAME_BYTES as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "stream frame too large: {} bytes exceeds cap {MAX_FRAME_BYTES}",
                bytes.len()
            ),
        ));
    }
    let len = bytes.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// Read one length-prefixed frame off the substream. Returns
/// `Ok(None)` on clean EOF (peer closed gracefully before
/// writing a new frame). Rejects oversize frames the same way
/// the writer does — a hostile peer can't blow the reader's
/// memory budget by announcing a 4 GiB length.
async fn read_length_prefixed<S>(stream: &mut S) -> io::Result<Option<Vec<u8>>>
where
    S: futures::AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("incoming frame announced {len} bytes (cap {MAX_FRAME_BYTES})"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frame round-trip through a tokio duplex pipe — no swarm,
    /// no libp2p. Verifies the CBOR + length-prefix shape in
    /// isolation. The swarm-level round trip is exercised in
    /// the rpc.rs integration tests.
    #[tokio::test]
    async fn length_prefixed_frame_round_trips_through_duplex_pipe() {
        let (mut a, mut b) = tokio::io::duplex(8 * 1024);
        let payload = b"hello-streaming";
        let writer = async {
            use tokio_util::compat::TokioAsyncWriteCompatExt;
            let mut compat = (&mut a).compat_write();
            write_length_prefixed(&mut compat, payload).await.unwrap();
            // EOF on the writer side so the reader's next call
            // returns None. Letting `compat` fall out of scope
            // releases the &mut borrow of `a`; `drop(a)` then
            // closes the duplex's writer half.
            let _ = compat;
            drop(a);
        };
        let reader = async {
            use tokio_util::compat::TokioAsyncReadCompatExt;
            let mut compat = (&mut b).compat();
            let first = read_length_prefixed(&mut compat).await.unwrap();
            assert_eq!(first.as_deref(), Some(&payload[..]));
            let second = read_length_prefixed(&mut compat).await.unwrap();
            assert!(second.is_none(), "expected EOF after one frame");
        };
        tokio::join!(writer, reader);
    }

    #[tokio::test]
    async fn oversize_frame_is_rejected_on_read() {
        // Hand-craft a buffer with a length header above the
        // cap. The reader must reject without allocating the
        // huge buffer.
        use tokio_util::compat::TokioAsyncReadCompatExt;
        let mut hostile = Vec::new();
        hostile.extend_from_slice(&(MAX_FRAME_BYTES + 1).to_be_bytes());
        // No body bytes — but we never get there.
        let cursor = std::io::Cursor::new(hostile);
        // tokio Cursor needs the compat shim same as duplex.
        // Use a tokio duplex pipe instead — easier than
        // wrapping the Cursor in a futures AsyncRead.
        let (mut a, mut b) = tokio::io::duplex(64);
        let writer = async {
            use tokio::io::AsyncWriteExt as _;
            let buf = cursor.get_ref().clone();
            a.write_all(&buf).await.unwrap();
            drop(a);
        };
        let reader = async {
            let mut compat = (&mut b).compat();
            let err = read_length_prefixed(&mut compat).await.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert!(err.to_string().contains("cap"));
        };
        tokio::join!(writer, reader);
    }

    #[tokio::test]
    async fn stream_frame_chunk_then_end_round_trips_via_writer_reader() {
        // Glue StreamWriter + StreamReader together via duplex
        // pipes. We can't construct real libp2p::Stream values
        // in a unit test, so this test exercises the framing
        // helpers in isolation. The swarm-level integration
        // lives in rpc.rs's tests.
        use serde_bytes::ByteBuf;
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        // Producer side: write a Header + 3 Chunk + End.
        let producer = async {
            use tokio_util::compat::TokioAsyncWriteCompatExt;
            let mut compat = (&mut a).compat_write();
            let header = StreamFrame::Header {
                responder: NodeId([0xAB; 32]),
                aid: ByteBuf::from(vec![0u8; 16]),
                processed_at: Timestamp(42),
            };
            let h = encode_frame(&header).unwrap();
            write_length_prefixed(&mut compat, &h).await.unwrap();
            for i in 0..3u8 {
                let chunk = StreamFrame::Chunk(ByteBuf::from(vec![i; 8]));
                let body = encode_frame(&chunk).unwrap();
                write_length_prefixed(&mut compat, &body).await.unwrap();
            }
            let end = encode_frame(&StreamFrame::End).unwrap();
            write_length_prefixed(&mut compat, &end).await.unwrap();
            // Letting `compat` fall out of scope releases the
            // &mut borrow on `a`; `drop(a)` then closes the
            // duplex's writer half so the consumer's next
            // read returns None (EOF).
            let _ = compat;
            drop(a);
        };
        // Consumer side: decode each frame back to a
        // StreamFrame and verify the sequence.
        let consumer = async {
            use tokio_util::compat::TokioAsyncReadCompatExt;
            let mut compat = (&mut b).compat();
            let header_bytes = read_length_prefixed(&mut compat).await.unwrap().unwrap();
            let header = decode_frame(&header_bytes).unwrap();
            assert!(matches!(header, StreamFrame::Header { .. }));
            for i in 0..3u8 {
                let body = read_length_prefixed(&mut compat).await.unwrap().unwrap();
                let frame = decode_frame(&body).unwrap();
                match frame {
                    StreamFrame::Chunk(b) => assert_eq!(b.as_ref(), &vec![i; 8][..]),
                    other => panic!("expected Chunk, got {other:?}"),
                }
            }
            let tail = read_length_prefixed(&mut compat).await.unwrap().unwrap();
            let frame = decode_frame(&tail).unwrap();
            assert!(matches!(frame, StreamFrame::End));
            let after_end = read_length_prefixed(&mut compat).await.unwrap();
            assert!(after_end.is_none(), "EOF expected after End frame");
        };
        tokio::join!(producer, consumer);
    }
}
