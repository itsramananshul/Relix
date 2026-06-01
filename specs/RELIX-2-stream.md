# RELIX-2 — Streaming / Substream Protocol

**Version:** 0.4.1 | **Status:** Frozen target. Alpha implements a simplified variant (SIMP-006).

## 2.1 Responsibilities

Bidirectional, credit-controlled, in-order, typed chunk-delivery between two Relix controllers. Carries any capability whose `kind` is not `unary`: server-sent streams (LLM tokens), client-sent (chunked upload), bidi. Long-deferred single results are NOT streams — use unary RPC with long deadlines.

## 2.2 Invariants

1. Each stream has a `stream_id` unique within a libp2p connection.
2. Chunks are strictly ordered with monotonic `seq`.
3. Identity is fixed at open, applies to all chunks.
4. Backpressure is credit-based; senders MUST NOT exceed granted credits.
5. Connection drop terminates all streams on it.

## 2.3 Transport

Over `/relix/rpc/stream/1`. One Relix stream = one Yamux substream. Per-connection cap: 256 concurrent streams.

## 2.4 Frames (CBOR with `t` discriminator)

- `open`: `{sid, rid, tid, m, mv, dir, args, ib, dl, n, resume_from?}`
- `ready`: `{sid, credit, max_chunk_bytes, heartbeat_interval, aid}`
- `chunk`: `{sid, seq, payload?, fin, err?}`
- `credit`: `{sid, additional}`
- `cancel`: `{sid, reason}`
- `heartbeat`: `{sid}`

## 2.8 Backpressure

Credit-based. Receiver issues credit at open + tops up via `credit` frames. Sender's outstanding chunk count ≤ current credit. Default initial credit: 64.

## 2.9 Cancellation

Either party sends `cancel`. Both sides release resources within 1s. SOL flows observe `Err(Cancelled)` on next read.

## 2.10 Reconnection

Streams are NOT reconnectable by default. Connection drop ⇒ stream cancelled. Capabilities declared `stream_resumable: true` MAY honor `resume_from` on reopen.

## 2.13 Authentication

Identity supplied once at open, applies to all chunks. Per-chunk identity forbidden.

## 2.16 Approval Is Not a Stream

Approval flows are unary RPCs with long deadlines. Streams are for sequences of values, not for "one delayed value."

---

## Alpha Implementation Notes (v0.4.1)

Alpha ships a minimal variant for AI token streaming only, running over the libp2p protocol identifier `/relix/rpc/stream/1` (not the frozen-target identifier `/relix/stream/1` — see §2.3 above for the target; the alpha wire ID is the production one).

**Alpha frame variants** (CBOR, length-prefixed with 4-byte big-endian length; max frame 1 MiB):

| Variant | Fields | Direction | Notes |
|---------|--------|-----------|-------|
| `Header` | `responder: NodeId`, `aid: ByteBuf`, `processed_at: Timestamp` | responder → caller | First frame; carries audit id (server-minted UUIDv4) and responder identity. |
| `Chunk` | `ByteBuf` (payload) | responder → caller | Unbounded sequence of data chunks. No `seq` field; no `fin` flag — stream end signalled by `End` frame. |
| `End` | (none) | responder → caller | Clean stream termination. |
| `Err` | `kind: u32`, `cause: String` | responder → caller | Error termination. `kind` maps to `error_kinds` constants. |

No `open`, `ready`, `credit`, `cancel`, or `heartbeat` frames in alpha. Flow control is implicit (small chunks; no backpressure). To abort a stream, close the connection.

- Caller sends the `RequestEnvelope` (same CBOR shape as unary) as a length-prefixed frame before reading responses.
- Identity is verified on the request envelope using the same admission pipeline as unary; per-chunk identity is not re-checked.
- Cross-restart resumption: not supported.
- Per RELIX-3, each received chunk is recorded as a separate `StreamChunkReceived` event in the flow log.

This subset is enough for `flows/chat.sol` to consume streamed tokens through the AI node.
