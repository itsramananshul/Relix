# RELIX-2 — Streaming / Substream Protocol

**Status:** Frozen target. Alpha implements a simplified variant (SIMP-006).

## 2.1 Responsibilities

Bidirectional, credit-controlled, in-order, typed chunk-delivery between two Relix controllers. Carries any capability whose `kind` is not `unary`: server-sent streams (LLM tokens), client-sent (chunked upload), bidi. Long-deferred single results are NOT streams — use unary RPC with long deadlines.

## 2.2 Invariants

1. Each stream has a `stream_id` unique within a libp2p connection.
2. Chunks are strictly ordered with monotonic `seq`.
3. Identity is fixed at open, applies to all chunks.
4. Backpressure is credit-based; senders MUST NOT exceed granted credits.
5. Connection drop terminates all streams on it.

## 2.3 Transport

Over `/relix/stream/1`. One Relix stream = one Yamux substream. Per-connection cap: 256 concurrent streams.

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

## Alpha Implementation Notes

Alpha ships a minimal variant for AI token streaming only:
- Frames: `open`, `chunk(seq, payload, fin)`, `error(payload)`. No `ready` (initial credit implicit / unbounded). No `credit` (no flow control; small chunks). No `heartbeat`. No `cancel` (close the connection to abort).
- Identity check at open: yes.
- Cross-restart resumption: not supported.
- Per RELIX-3, each received chunk is recorded as a separate `StreamChunkReceived` event in the flow log.

This subset is enough for `flows/chat.sol` to consume Anthropic-streamed tokens through the AI node.
