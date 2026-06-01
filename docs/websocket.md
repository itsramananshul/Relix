# WebSocket streaming

The bridge exposes `GET /ws/chat` for streaming chat. Clients
open a WebSocket, send one JSON message describing the turn,
and receive a sequence of JSON frames carrying the reply as it
arrives, plus a terminal `done` (or `error`) frame.

## Where the stream comes from

The `ChatProvider` trait has a `generate_reply_stream` method.
The mock provider streams word-by-word with a 20 ms gap so dev
flows behave like real streaming without a key. The
OpenAI-compatible, Anthropic, and Gemini providers override it
with native SSE token streaming against their upstream APIs.

End-to-end provider-native streaming through the **mesh** still
goes through the synchronous `ai.chat` capability — the chat
flow runs to completion before the bridge sees the materialised
reply. `/ws/chat` therefore delivers the bridge's word-by-word
splitter on top of the assembled response. The wire and client
code don't change when libp2p stream support lands; the
provider streaming primitive is already in place to consume it.

## Auth

The upgrade request must carry an `Authorization: Bearer <token>`
header. A missing header, a non-`Bearer` scheme, or an empty
token returns 401 before the WebSocket upgrade completes. The
bearer value is compared against the bridge token using a
constant-time comparison — any mismatch returns 401. Full
identity verification also runs **inside** the mesh on every
responder admission.

The per-principal concurrent-WebSocket cap (`[mesh.rate_limits]
ws_max_concurrent`, default 5) is checked before the upgrade.
Exceeding it returns 429 with `{"error":"rate_limit_exceeded",
"retry_after_secs":60,"ws_limit":N}`.

## Wire format

After the upgrade, the client sends ONE JSON message:

```json
{ "session_id": "demo-1", "message": "Hello", "model": "relix-mock" }
```

`model` is optional and currently informational. Provider
routing lives on the AI node.

The server replies with a stream of JSON text frames:

```json
{ "type": "chunk", "text": "Hello" }
{ "type": "chunk", "text": " world" }
{ "type": "done",  "session_id": "demo-1", "text": "Hello world" }
```

A `done` frame is always the last successful frame. On any
flow failure, the server sends one `error` frame in place of
`done`:

```json
{ "type": "error", "message": "mesh transport: dial timeout" }
```

Then closes the socket.

## JavaScript client

```js
const ws = new WebSocket("ws://127.0.0.1:19791/ws/chat", [], {
  // Browsers can't set headers on WebSocket; for a browser
  // client put the token in the URL query and have the server
  // accept it there too (post-alpha). For node / desktop
  // clients use the per-library header API:
  headers: { Authorization: "Bearer <bridge-token>" },
});

ws.onopen = () => {
  ws.send(JSON.stringify({
    session_id: "demo-1",
    message:    "Hello",
  }));
};

let assembled = "";
ws.onmessage = (ev) => {
  const frame = JSON.parse(ev.data);
  switch (frame.type) {
    case "chunk":
      assembled += frame.text;
      process.stdout.write(frame.text);
      break;
    case "done":
      console.log("\n\nFinal:", frame.text);
      ws.close();
      break;
    case "error":
      console.error("\nError:", frame.message);
      ws.close();
      break;
  }
};
```

## Python client

```python
import asyncio, json
import websockets

async def main():
    headers = {"Authorization": "Bearer <bridge-token>"}
    uri = "ws://127.0.0.1:19791/ws/chat"
    async with websockets.connect(uri, extra_headers=headers) as ws:
        await ws.send(json.dumps({
            "session_id": "demo-1",
            "message":    "Hello",
        }))
        assembled = ""
        async for raw in ws:
            frame = json.loads(raw)
            if frame["type"] == "chunk":
                assembled += frame["text"]
                print(frame["text"], end="", flush=True)
            elif frame["type"] == "done":
                print("\nFinal:", frame["text"])
                break
            elif frame["type"] == "error":
                print("\nError:", frame["message"])
                break

asyncio.run(main())
```

## /ws/chat vs POST /chat/stream

| | `POST /chat/stream` (SSE) | `GET /ws/chat` (WebSocket) |
|---|---|---|
| Wire | `text/event-stream` (`event: chunk`, `event: done`) | One JSON frame per message |
| Auth | `Authorization: Bearer <token>` (constant-time compare) | `Authorization: Bearer <token>` (constant-time compare) |
| Direction | Server → client only | Bidirectional (alpha sends one request frame) |
| Chunking | Byte-sized slices of the full reply | Word-sized chunks with a 20 ms gap |
| Final frame | `event: done` with `flow_id` / `trace_id` JSON | `{ "type": "done", "session_id": ..., "text": ... }` |
| When to use | OpenAI-compatible client integration, browsers without WS auth | App-level chat UIs, dev tools, anything that wants a real WS |

Both endpoints run the same chat flow under the hood: write the
user turn → call `ai.chat` through the mesh → write the
assistant turn → return the reply.

## Error handling

The error frame is the last frame before the socket closes. The
`message` field carries the same human-readable cause string the
HTTP endpoints return (`mesh transport: ...`, `invalid input:
...`, or an internal error message). Client code should treat
any error frame as terminal — there are no partial reply
recoveries within a single `/ws/chat` session.

If the WebSocket closes without a `done` or `error` frame, the
server abort path was hit before it could send a clean message
(network loss, bridge restart). Treat it the same as `error`
and reconnect.
