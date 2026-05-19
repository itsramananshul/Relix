# Streaming and the OpenAI-Compatible Shim

This document explains exactly what the web bridge does (and does NOT do) when an OpenAI-compatible client — most notably **Open WebUI** — points at `http://127.0.0.1:19791/v1/` (the bridge's current default; older drafts referenced `9100`, the pre-M8 port).

It is the operational counterpart to:

- `SIMP-019` (bridge-level SSE chunking, not provider-native token streaming)
- `SIMP-020` (OpenAI shim is request/response translation only)

in `specs/alpha-simplifications.md`.

## What the bridge is, restated

The bridge is a normal Relix peer with its own `IdentityBundle`. It happens to expose an HTTP server. Every request it accepts becomes one SOL flow execution; the flow file is the single source of routing truth (see `flows/chat_template.sol`).

The bridge does NOT:

- hold any AI provider key (those live only on the AI node — see `docs/provider-configuration.md`),
- route requests to providers in Rust (the SOL flow does that, through `remote_call("ai", "ai.chat", …)`),
- bypass identity / policy / audit on responders.

## Endpoints

| Method | Path                     | Body / Output                                            | Notes                          |
|--------|--------------------------|----------------------------------------------------------|--------------------------------|
| `GET`  | `/health`                | `200 ok\n`                                               | smoke-test                     |
| `POST` | `/chat`                  | JSON in / JSON out                                       | original native shape          |
| `POST` | `/chat/stream`           | JSON in / `text/event-stream` (Relix-native frames)      | bridge SSE (SIMP-019)          |
| `GET`  | `/v1/models`             | OpenAI-style models list                                 | advertises configured aliases  |
| `POST` | `/v1/chat/completions`   | OpenAI request → JSON or OpenAI-style SSE                | shim (SIMP-020)                |

All five endpoints share the same `execute_chat_flow` helper, so:

- input validation is identical (`"`, `|`, `\n` rejected on the native path; `\n`/`\t` collapsed to spaces on the OpenAI path, `"`/`|` still rejected),
- every request opens its own `FlowRunner`, mints a fresh `flow_id` + `trace_id`, and writes a per-flow event log,
- every cross-node call hits the responder's full admission pipeline (identity → policy → audit) and lands in its audit log.

## Streaming — what's really happening (SIMP-019)

The alpha SOL VM and `RemoteCallDispatcher` are synchronous (SIMP-001 + SIMP-014). The flow has to finish before the bridge has a reply to return. So "streaming" here is **bridge-level chunking** of an already-materialised reply, not true per-token streaming from the provider.

Two SSE shapes:

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

Slice size and inter-chunk delay are controlled by `[sse] chunk_bytes` and `[sse] chunk_delay_ms` in `configs/web-bridge.toml`. The chunker is UTF-8-safe; multi-byte codepoints are never split.

### OpenAI shape (`POST /v1/chat/completions` with `"stream": true`)

```text
data: {"id":"chatcmpl-…","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}], …}

data: {"id":"chatcmpl-…", …, "choices":[{"index":0,"delta":{"content":"<slice>"},"finish_reason":null}], …}

…

data: {"id":"chatcmpl-…", …, "choices":[{"index":0,"delta":{},"finish_reason":"stop"}], "relix":{…provenance…}}

data: [DONE]
```

The final frame carries a non-standard `relix` extension (`flow_id`, `trace_id`, `flow_log`, `session_id`). OpenAI clients ignore unknown fields, so this is safe. The literal `data: [DONE]` sentinel matches what the official `openai` Python/JS clients and Open WebUI look for.

Latency to first chunk = full flow latency. The UI animates a reply that has already been computed. This is honest "stream-shaped UX," not real concurrent generation. The real model arrives with SIMP-001 + RELIX-2.

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

   The bytes hashed are the *first* turn's content, so subsequent turns (where OpenAI clients resend the full history) hash to the same `session_id`. That's how Relix memory persistence works for OpenAI clients: same conversation → same memory bucket on the memory node.

2. **Extracts the prompt** as the *last* `user` message's content. System messages, prior assistant turns, and prior user turns are all dropped here — the SOL chat flow already pulls history via `memory.recent_for_session`, so the canonical history comes from Relix memory, not from what the client resent.

3. **Sanitises** the prompt: `\r\n` / `\n` / `\t` → single space. The SIMP-018 string-literal boundary is preserved; the SOL string substitution does not need to deal with embedded newlines.

4. **Rejects** the prompt if it contains `"` or `|`. Silently rewriting either character would change what the user said, so the shim returns `400` and tells the client.

5. **Ignores everything else**: `temperature`, `top_p`, `max_tokens`, `n`, `presence_penalty`, `tool_choice`, `logprobs`, … Those are provider-side concerns living on the AI node. The shim accepts and discards them so OpenAI clients don't fail validation.

6. **Resolves the model label** for the response: explicit `model` field wins; otherwise `[openai_compat] default_model`; otherwise the first `[[openai_compat.models]]` entry; otherwise the literal string `"relix"`. The bridge does NOT route based on this — provider selection is the AI node's job. The model id is cosmetic.

7. **Runs the SOL flow** via the same `FlowRunner` the native `/chat` uses. Identity, policy, audit — all identical.

8. **Projects the result**:
   - non-streaming → an OpenAI `chat.completion` object with an extra `relix` field carrying `flow_id` / `trace_id` / `flow_log` / `session_id`,
   - streaming → the OpenAI SSE shape above.

## Open WebUI setup

Open WebUI (https://github.com/open-webui/open-webui) speaks the OpenAI chat-completions API natively, so no fork or plugin is required.

1. Start the Relix mesh:

   ```sh
   ./scripts/alpha-bringup-m8-openwebui.sh --keep
   ```

   This brings up a memory controller, an AI controller (mock provider), and the web bridge on `127.0.0.1:19791` for the duration of the demo. `--keep` leaves processes running so you can talk to the bridge from Open WebUI.

2. Run Open WebUI (Docker shown; any deployment works):

   ```sh
   docker run -d -p 3000:8080 \
       -v open-webui:/app/backend/data \
       --name open-webui \
       ghcr.io/open-webui/open-webui:main
   ```

3. In Open WebUI: **Settings → Connections → OpenAI API**

   - **API Base URL**: `http://host.docker.internal:19791/v1` (Docker on macOS/Windows) or `http://127.0.0.1:19791/v1` (native install).
   - **API Key**: any non-empty string. The alpha shim ignores the `Authorization` header; bind to loopback and trust the network boundary. Documented in SIMP-020.
   - Click **Save**.

4. Open the model picker; you should see whatever ids you configured under `[[openai_compat.models]]`. The default demo script ships `relix-mock`.

5. Chat. Each reply round-trips memory → ai → memory through the mesh. Memory persists across browser refreshes because the bridge's `session_id` derivation is stable across history regrowth.

## Limitations the shim does NOT pretend to handle

- multimodal content (image parts, audio parts)
- OpenAI tool / function calling (the structured JSON protocol)
- `system` messages as prompt input — they only contribute to session-id derivation
- per-call sampling controls (those belong on the AI node)
- the `Authorization` header — bind to loopback only

All of these are tracked under SIMP-020 and slated for Gate 2.

## How to validate end-to-end yourself

```sh
# 1. Native JSON.
curl -sS -X POST http://127.0.0.1:19791/chat \
    -H 'content-type: application/json' \
    -d '{"session_id":"demo","message":"hi"}'

# 2. Native SSE.
curl -sS -N -X POST http://127.0.0.1:19791/chat/stream \
    -H 'content-type: application/json' \
    -d '{"session_id":"demo","message":"hi"}'

# 3. OpenAI shim, non-stream.
curl -sS -X POST http://127.0.0.1:19791/v1/chat/completions \
    -H 'content-type: application/json' \
    -d '{"model":"relix-mock","messages":[{"role":"user","content":"hi"}]}'

# 4. OpenAI shim, stream.
curl -sS -N -X POST http://127.0.0.1:19791/v1/chat/completions \
    -H 'content-type: application/json' \
    -d '{"model":"relix-mock","messages":[{"role":"user","content":"hi"}],"stream":true}'

# 5. Models endpoint.
curl -sS http://127.0.0.1:19791/v1/models
```

Then look at the bridge flow log (`dev-data/flow-runner/flows/<flow_id>.log`) and the responder audit logs (`dev-data/<demo>-memory/audit.log`, `dev-data/<demo>-ai/audit.log`) via `relix-flow-inspect`. Every cross-node call appears in both the flow log (caller side) and the audit log (responder side), correlatable by `request_id` / `trace_id`.
