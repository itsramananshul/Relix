# AI Provider Configuration

Relix's AI node is **provider-agnostic**. The same SOL chat flow runs unchanged
against any of the supported backends. Provider selection is one config line on
the AI node; credentials live only on that node, never in the web bridge or any
presentation peer.

## Supported providers

| `[ai] provider` | Implementation | Wire family | Typical `api_key_env` | Notes |
|---|---|---|---|---|
| `mock` | `MockProvider` | (none) | (none) | Default; deterministic; for local demos + tests |
| `openai` | `OpenAICompatibleProvider` | OpenAI `/v1/chat/completions` | `OPENAI_API_KEY` | |
| `openrouter` | `OpenAICompatibleProvider` | OpenRouter (OpenAI shape) | `OPENROUTER_API_KEY` | Multi-vendor routing |
| `xai` | `OpenAICompatibleProvider` | xAI / Grok (OpenAI-compatible) | `XAI_API_KEY` | |
| `local` | `OpenAICompatibleProvider` | local OpenAI-compatible server | (unset or empty) | Ollama, vLLM, llama.cpp server |
| `anthropic` | `AnthropicProvider` | Anthropic `/v1/messages` | `ANTHROPIC_API_KEY` | |
| `gemini` | `GeminiProvider` | (placeholder) | `GEMINI_API_KEY` | Returns `not_yet_implemented`; full impl is M9+ |

Adding a new backend = a new file implementing `ChatProvider` +
a `build_provider` arm in `crates/relix-runtime/src/nodes/ai/mod.rs`. The SOL
flow surface (`ai.chat` arg shape) does not change.

## Config shape (per AI node)

```toml
[ai]
provider = "openrouter"
model    = ""    # optional caller default; empty = per-provider default_model

[ai.providers.openai]
base_url      = "https://api.openai.com/v1"
api_key_env   = "OPENAI_API_KEY"
default_model = "gpt-4o-mini"
timeout_secs  = 60

[ai.providers.openrouter]
base_url      = "https://openrouter.ai/api/v1"
api_key_env   = "OPENROUTER_API_KEY"
default_model = "openai/gpt-4o-mini"

[ai.providers.xai]
base_url      = "https://api.x.ai/v1"
api_key_env   = "XAI_API_KEY"
default_model = "grok-2-latest"

[ai.providers.local]
base_url      = "http://localhost:11434/v1"
# api_key_env intentionally unset / empty for local servers.
default_model = "llama3:8b"

[ai.providers.anthropic]
api_key_env   = "ANTHROPIC_API_KEY"
default_model = "claude-3-5-sonnet-latest"

[ai.providers.gemini]
api_key_env   = "GEMINI_API_KEY"
default_model = "gemini-1.5-flash"
```

Every active provider needs its matching `[ai.providers.<name>]` subsection.
Inactive sections are ignored (so you can leave the whole map populated and
flip `[ai] provider` to switch backends).

## Credential ownership — non-negotiable

Provider keys live **only on the AI node**:

- `relix-web-bridge` does NOT read `OPENAI_API_KEY` (or any other key). It
  only calls `remote_call("ai", "ai.chat", ...)`.
- Open WebUI / Relix Web does NOT hold provider keys. It calls the bridge
  via HTTP; the bridge calls the mesh; the mesh's AI node owns the secret.
- There is no central credential hub. Each AI node has its own
  `api_key_env` mapping to its own environment.

Concretely:

```text
$OPENAI_API_KEY      → on the AI-node host shell only
$OPENROUTER_API_KEY  → same
$ANTHROPIC_API_KEY   → same
$XAI_API_KEY         → same
```

If a teammate accidentally adds a key to `configs/web-bridge.toml`, the
demo script's secret-containment check (`scripts/alpha-bringup-m8-web-bridge.sh`,
step 8) fails loudly.

## Operational patterns

### Local dev (no costs)
```toml
[ai]
provider = "mock"
```
No env vars needed. Deterministic reply. The MockProvider is what
`scripts/alpha-bringup-m7-chat.sh` and `scripts/alpha-bringup-m8-web-bridge.sh`
ship by default.

### Local Ollama (no costs, real model)
```sh
ollama serve   # exposes /v1/chat/completions on :11434
```
```toml
[ai]
provider = "local"

[ai.providers.local]
base_url      = "http://localhost:11434/v1"
default_model = "llama3:8b"
```

### OpenAI / Anthropic / OpenRouter / xAI
Set the corresponding env var in the AI-node's shell:
```sh
export OPENROUTER_API_KEY=sk-or-...
```
```toml
[ai]
provider = "openrouter"
```
Restart the AI controller. Other nodes are untouched.

### Switching providers mid-run
1. Edit `[ai] provider` on the AI node's config.
2. Restart only the AI controller (`SIGINT`, re-launch).
3. Memory + bridge + flow runners do not need restart.

## Failure modes

| Failure | Source | Surface |
|---|---|---|
| `api_key_env` names a missing env var | provider startup | `Permanent("missing provider key: $NAME")` — controller crashes with the clear message |
| `api_key_env` set but empty | provider startup | `Permanent("env var 'NAME' is set but empty")` |
| `[ai.providers.X]` missing for active provider | `build_provider` | clear "requires `[ai.providers.X]` config section" at startup |
| Provider returns HTTP 429 / 5xx | runtime | `Transient(...)` → `responder_overloaded` → SOL flow sees retryable error |
| Provider returns 4xx (auth / bad request) | runtime | `Permanent(...)` → `responder_internal` → SOL flow's `remote_call` returns `Err` |
| Network unreachable | runtime | `Transient("http: ...")` |

Gemini specifically returns `Permanent("gemini provider not yet implemented")`
on every call until M9+.

## Tests

`crates/relix-runtime/src/nodes/ai/provider/` ships unit coverage:

- `mock`: deterministic reply with history-size check.
- `openai_compat`: missing-base-url error, provider-name passthrough.
- `anthropic`: missing-api-key-env error, no-api-key-env-at-all error.
- `gemini`: stub returns `not_yet_implemented`.
- `provider`: `load_api_key` precedence (unset, empty, missing-var-named).
- `mod`: `build_provider` defaults to mock, requires per-provider section,
  rejects unknown, errors clearly on anthropic without env, accepts local
  without key.

Run `cargo test -p relix-runtime nodes::ai` (12+ assertions) to verify
locally before changing provider plumbing.

## SOL flow contract — unchanged

```sol
let reply: str = remote_call("ai", "ai.chat", session + "|" + prompt + "|" + history);
```

That string contract (SIMP-016) is the same across all providers. The AI node
hands `ChatInput { session_id, prompt, history, model, system_prompt?, temperature?, max_tokens? }`
to the active `ChatProvider`. The reply text comes back as UTF-8.

Typed CBOR `ChatInput` over the wire lands at Gate 2 with the CDDL stdlib.
Until then, string `|` delimiting is the alpha contract.
