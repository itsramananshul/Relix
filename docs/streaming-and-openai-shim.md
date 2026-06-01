# Streaming and the OpenAI-Compatible Shim

This document explains exactly what the web bridge does (and does NOT do) when
an OpenAI-compatible client — most notably **Open WebUI** — points at
`http://127.0.0.1:19791/v1/` (the bridge's current default; older drafts
referenced `9100`, the pre-M8 port).

It is the operational counterpart to `SIMP-020` (OpenAI shim is
request/response translation) in `specs/alpha-simplifications.md`.

## What the bridge is, restated

The bridge is a normal Relix peer with its own `IdentityBundle`. It happens to
expose an HTTP server. Every request it accepts becomes one SOL flow execution;
the flow file is the single source of routing truth (see
`flows/chat_template.sol`).

The bridge does NOT:

- hold any AI provider key (those live only on the AI node — see
  `docs/provider-configuration.md`),
- route requests to providers in Rust (the SOL flow does that, through
  `remote_call("ai", "ai.chat", …)`),
- bypass identity / policy / audit on responders.

## Auth

All chat endpoints require `Authorization: Bearer <bridge-token>`. The bridge
token is a 256-bit hex secret generated at first boot and stored at
`token_path` (default `~/.relix/bridge-token`). A missing bearer returns 401;
a non-matching bearer returns 401. There is no "any non-empty bearer" bypass —
that pre-SEC-PART-3 behaviour has been removed from all paths including the
OpenAI shim.

## Endpoints

| Method | Path | Body / Output | Notes |
|--------|------|---------------|-------|
| `GET` | `/health` | `200 ok\n` | Public, no auth |
| `POST` | `/chat` | JSON in / JSON out | Native shape |
| `POST` | `/chat/stream` | JSON in / `text/event-stream` | Relix-native SSE frames |
| `GET` | `/ws/chat` | WebSocket | See `docs/websocket.md` |
| `GET` | `/v1/models` | OpenAI-style models list | Advertises configured aliases |
| `POST` | `/v1/chat/completions` | OpenAI request → JSON or OpenAI-style SSE | Shim (SIMP-020) |
| `GET` | `/v1/info` | Relix server info | Non-OpenAI; version, provider, capabilities |
| `GET` | `/v1/schema` | JSON schema doc | Hand-written; not enforced server-side |

All chat endpoints share the same input validation (`"`, `|`, `\n` rejected on
the native path; `\n`/`\t` collapsed to spaces on the OpenAI path, `"`/`|`
still rejected). Every request mints a fresh `flow_id` + `trace_id` and writes
a per-flow event log. Every cross-node call hits the responder's full admission
pipeline (identity → policy → audit).

## Streaming — what's really happening

There are two distinct streaming paths in the bridge.

### True end-to-end streaming (RELIX-2)

When `[flow] streaming_template_path` is configured AND `stream: true` is
requested on `POST /v1/chat/completions`, the bridge takes the real
end-to-end path:

- The SOL VM's `remote_call_stream(...)` (or YAML `stream:` step) pipes tokens
  from the AI node directly to a tokio mpsc channel.
- The AI node's `ai.chat.stream` capability calls
  `provider.generate_reply_stream()`, which for OpenAI-compatible, Anthropic,
  and Gemini providers produces native SSE token deltas from the upstream API.
- Each `StreamingChunk::Text` frame is forwarded to the SSE response as it
  arrives — this is real concurrent generation, not a simulation.
- A cancellation signal (`Arc<Notify>`) is wired to client disconnect via a
  RAII `CancelGuard` in the SSE stream future; dropping the SSE future aborts
  the in-flight provider call.

### Bridge-level chunking (legacy / no streaming template)

When `streaming_template_path` is NOT configured, or for the native
`POST /chat/stream` endpoint, streaming is bridge-level chunking of an
already-materialised reply. The flow runs to completion before the bridge
begins emitting frames. The UI animates a reply that has already been
computed.

`POST /chat/stream` and the WebSocket endpoint (`GET /ws/chat`) always use
bridge-level chunking regardless of `streaming_template_path`.

### Self-consistency and streaming

When self-consistency is enabled and fires on `ai.chat.stream`, N unary
provider calls run in parallel, pairwise cosine similarity picks the
highest-coherence sample, and the winner is chunked via the whitespace-token
splitter (`chunk_for_stream`) rather than forwarded as native token deltas.

## SSE frame formats

### Relix-native (`POST /chat/stream`)

```text
event: chunk
data: <first slice of reply>

event: chunk
data: <next slice>

…

event: done
data: {"flow_id":"…","trace_id":"…","flow_log":"…"}
```

Slice size and inter-chunk delay are controlled by `[sse] chunk_bytes`
(default 32) and `[sse] chunk_delay_ms` (default 25) in the bridge config.
The chunker is UTF-8-safe; multi-byte codepoints are never split.

### OpenAI shape (`POST /v1/chat/completions` with `"stream": true`)

```text
data: {"id":"chatcmpl-…","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}, …}

data: {"id":"chatcmpl-…", …, "choices":[{"index":0,"delta":{"content":"<token>"},"finish_reason":null}], …}

…

data: {"id":"chatcmpl-…", …, "choices":[{"index":0,"delta":{},"finish_reason":"stop"}], "relix":{…provenance…}, …}

data: [DONE]
```

The final frame carries a non-standard `relix` extension (`flow_id`,
`trace_id`, `flow_log`, `session_id`). OpenAI clients ignore unknown fields,
so this is safe. The `data: [DONE]` sentinel matches what the official
`openai` Python/JS clients and Open WebUI look for.

### Usage and finish-reason frames

When the AI node streams native SSE from the provider, `StreamingChunk::Usage`
and `StreamingChunk::FinishReason` frames are **intercepted and not forwarded**
to the wire. Instead:

- `Usage` frames are routed to `MetricsSink::attach_ai_usage` for internal
  accounting.
- `FinishReason` frames are routed to `MetricsSink::attach_provider_signals`.

The `usage` object in OpenAI-shape responses always carries `prompt_tokens: 0,
completion_tokens: 0, total_tokens: 0` — usage attribution is tracked
internally but not yet projected into the shim response.

## OpenAI shim — translation rules (SIMP-020)

`POST /v1/chat/completions` accepts an OpenAI request shape:

```json
{
  "model": "relix-mock",
  "messages": [
    {"role": "system",    "content": "you are a helpful assistant"},
    {"role": "user",      "content": "hi"},
    {"role": "assistant", "content": "hello"},
    {"role": "user",      "content": "how are you?"}
  ],
  "stream": false,
  "temperature": 0.7
}
```

The bridge does **only** these things with it:

1. **Derives a stable `session_id`** from the first system + first user message:

   ```text
   session_id = "oa-" || hex(blake3(first_system_content || 0x00 || first_user_content))[..12]
   ```

   The bytes hashed are the *first* turn's content, so subsequent turns (where
   OpenAI clients resend the full history) hash to the same `session_id`.
   That's how Relix memory persistence works for OpenAI clients: same
   conversation → same memory bucket on the memory node.

2. **Collects and prepends system messages** in order. Each system message is
   sanitized and framed as `[SYSTEM N]\n<text>\n\n`. The full prompt becomes:

   ```text
   [SYSTEM 1]
   <first system message>

   [SYSTEM 2]
   <second system message>

   [USER]
   <last user message>
   ```

   If there are no system messages, only the bare last user content is sent.
   Prior assistant turns and prior user turns (other than the last) are dropped
   — history comes from the Relix memory node via `memory.recent_for_session`,
   not from what the client resent.

3. **Sanitises** each message: `\r\n` / `\n` / `\t` → single space. The
   SIMP-018 string-literal boundary is preserved; the SOL string substitution
   does not need to deal with embedded newlines.

4. **Rejects** the prompt if it contains `"` or `|`. Silently rewriting either
   character would change what the user said, so the shim returns `400`.

5. **Rejects** `tools`, `tool_calls`, and `role: "tool"` messages with `400`.
   OpenAI tool-calling is not implemented in the shim. Returning 400 is
   preferable to silently dropping tool definitions, which would produce
   answers that look like they honoured the operator's tool surface when they
   did not.

6. **Ignores** `temperature`, `top_p`, `max_tokens`, `n`, `presence_penalty`,
   `logprobs`, and other provider-side parameters (accepted via a serde
   `_extra` catch-all so OpenAI clients don't fail validation). Provider
   configuration is the AI node's responsibility.

7. **Resolves the model label** for the response: explicit `model` field wins;
   otherwise `[openai_compat] default_model`; otherwise the first
   `[[openai_compat.models]]` entry; otherwise the literal `"relix"`. The
   bridge does NOT route based on this — provider selection is the AI node's
   job. The model id is cosmetic.

8. **Runs the SOL flow** via the same `FlowRunner` the native `/chat` uses.
   Identity, policy, audit — all identical.

9. **Projects the result**:
   - non-streaming → an OpenAI `chat.completion` object with an extra `relix`
     field carrying `flow_id` / `trace_id` / `flow_log` / `session_id`,
   - streaming → the OpenAI SSE shape above.

## Auto-routing to tool flow

When `[flow] tool_template_path` is configured AND the last user message
contains an `https?://` URL that passes the SSRF check, the shim routes to
the tool-fetch flow. URL detection is skipped on the streaming path — the
`ai.chat.stream` capability does not run the planner or tool dispatcher.

## Open WebUI setup

Open WebUI (https://github.com/open-webui/open-webui) speaks the OpenAI
chat-completions API natively.

1. Start the Relix mesh:

   ```sh
   ./scripts/alpha-bringup-m8-openwebui.sh --keep
   ```

2. Run Open WebUI (Docker shown; any deployment works):

   ```sh
   docker run -d -p 3000:8080 \
       -v open-webui:/app/backend/data \
       --name open-webui \
       ghcr.io/open-webui/open-webui:main
   ```

3. In Open WebUI: **Settings → Connections → OpenAI API**

   - **API Base URL**: `http://host.docker.internal:19791/v1` (Docker on
     macOS/Windows) or `http://127.0.0.1:19791/v1` (native install).
   - **API Key**: the bridge token value from `~/.relix/bridge-token`.
   - Click **Save**.

4. Open the model picker; you should see whatever ids you configured under
   `[[openai_compat.models]]`. The default demo script ships `relix-mock`.

5. Chat. Each reply round-trips memory → ai → memory through the mesh.
   Memory persists across browser refreshes because the bridge's `session_id`
   derivation is stable across history regrowth.

## Limitations the shim does NOT handle

- multimodal content (image parts, audio parts)
- OpenAI tool / function calling (rejected with 400)
- per-call sampling controls (those belong on the AI node)

All of these are tracked under SIMP-020.

## How to validate end-to-end yourself

```sh
# 1. Native JSON.
curl -sS -X POST http://127.0.0.1:19791/chat \
    -H 'Authorization: Bearer <token>' \
    -H 'content-type: application/json' \
    -d '{"session_id":"demo","message":"hi"}'

# 2. Native SSE.
curl -sS -N -X POST http://127.0.0.1:19791/chat/stream \
    -H 'Authorization: Bearer <token>' \
    -H 'content-type: application/json' \
    -d '{"session_id":"demo","message":"hi"}'

# 3. OpenAI shim, non-stream.
curl -sS -X POST http://127.0.0.1:19791/v1/chat/completions \
    -H 'Authorization: Bearer <token>' \
    -H 'content-type: application/json' \
    -d '{"model":"relix-mock","messages":[{"role":"user","content":"hi"}]}'

# 4. OpenAI shim, stream.
curl -sS -N -X POST http://127.0.0.1:19791/v1/chat/completions \
    -H 'Authorization: Bearer <token>' \
    -H 'content-type: application/json' \
    -d '{"model":"relix-mock","messages":[{"role":"user","content":"hi"}],"stream":true}'

# 5. Models endpoint.
curl -sS -H 'Authorization: Bearer <token>' http://127.0.0.1:19791/v1/models

# 6. Server info.
curl -sS -H 'Authorization: Bearer <token>' http://127.0.0.1:19791/v1/info
```

Then look at the bridge flow log (`dev-data/flow-runner/flows/<flow_id>.log`)
and the responder audit logs (`dev-data/<demo>-memory/audit.log`,
`dev-data/<demo>-ai/audit.log`) via `relix-flow-inspect`. Every cross-node
call appears in both the flow log (caller side) and the audit log (responder
side), correlatable by `request_id` / `trace_id`.
