# AI Provider Configuration

Relix's AI node is **provider-agnostic**. The same SOL chat flow runs unchanged
against any of the supported backends. Provider selection is one config line on
the AI node; credentials live only on that node, never in the web bridge or any
presentation peer.

## Supported providers

| `[ai] provider` | Implementation | Wire family | Typical `api_key_env` | Notes |
|---|---|---|---|---|
| `mock` | `MockProvider` | (none) | (none) | Default; deterministic; for local demos + tests |
| `openai` | `OpenAICompatibleProvider` | OpenAI `/v1/chat/completions` | `OPENAI_API_KEY` | Native SSE stream; embeddings; `/models` |
| `openrouter` | `OpenAICompatibleProvider` | OpenRouter (OpenAI shape) | `OPENROUTER_API_KEY` | Multi-vendor routing |
| `xai` | `OpenAICompatibleProvider` | xAI / Grok (OpenAI-compatible) | `XAI_API_KEY` | |
| `local` | `OpenAICompatibleProvider` | local OpenAI-compatible server | (unset or empty) | Ollama, vLLM, llama.cpp server |
| `anthropic` | `AnthropicProvider` | Anthropic `/v1/messages` | `ANTHROPIC_API_KEY` | SSE stream; prompt caching on system block; extended thinking; no embeddings |
| `gemini` | `GeminiProvider` | Google `generativelanguage.googleapis.com` | `GEMINI_API_KEY` | Fully implemented; native SSE stream; multi-turn history mapping; usage metadata |

Adding a new backend = a new file implementing `ChatProvider` +
a `build_provider` arm in `crates/relix-runtime/src/nodes/ai/mod.rs`. The SOL
flow surface (`ai.chat` arg shape) does not change.

## Config shape (per AI node)

The AI node config is structured in several `[ai.*]` sections. A minimal
production example follows; the sections you do not need may be omitted.

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
default_model = "gemini-2.0-flash"
```

Every active provider needs its matching `[ai.providers.<name>]` subsection.
Inactive sections are ignored (so you can leave the whole map populated and
flip `[ai] provider` to switch backends).

### Per-provider defaults

When `default_model` is not set in `[ai.providers.<name>]`, the AI node falls
back to these hardcoded defaults:

| Provider | Hardcoded fallback model |
|---|---|
| `openai` | `gpt-4o-mini` |
| `anthropic` | `claude-3-5-sonnet-latest` |
| `gemini` | `gemini-2.0-flash` |
| `openai` (embed) | `text-embedding-3-small` |
| all others | provider's own default |

### Additional config sections

#### `[ai.agent]` — soul / persona

```toml
[ai.agent]
name      = "my-assistant"   # triggers soul discovery
soul_path = ""               # explicit path wins over name-based discovery
```

| Key | Type | Default | Effect |
|---|---|---|---|
| `name` | String | `""` | Agent slug; triggers SOUL.md discovery at `~/.relix/souls/<name>.md` then `./souls/<name>.md` |
| `soul_path` | PathBuf? | None | Explicit path to SOUL.md; takes precedence over name-based discovery |

Soul files are hot-reloaded on every `ai.chat` call by comparing file mtime —
no restart needed after editing.

#### `[ai.memory_peer]` — memory / RAG

```toml
[ai.memory_peer]
addr             = "/ip4/127.0.0.1/tcp/9300"
alias            = "memory"
deadline_secs    = 5
max_history_turns = 10
rag_enabled      = false
rag_top_k        = 5
rag_min_score    = 0.70
```

| Key | Type | Default | Effect |
|---|---|---|---|
| `addr` | String | required | libp2p multiaddr of memory peer |
| `alias` | String | `"memory"` | MeshClient dial alias |
| `deadline_secs` | i64 | `5` | Per-call timeout for memory reads |
| `max_history_turns` | usize | `10` | Prior turns fetched via `memory.recent_for_session` |
| `rag_enabled` | bool | `false` | Enable RAG: embed prompt + vector search before provider call |
| `rag_top_k` | usize | `5` | Max RAG hits to include |
| `rag_min_score` | f32 | `0.70` | Cosine similarity floor for RAG hits |

#### `[ai.routing]` — tier-based complexity routing

```toml
[ai.routing]
enabled = true

[ai.routing.tiers.simple]
provider = "openai"
model    = "gpt-4o-mini"

[ai.routing.tiers.medium]
provider = "openrouter"
model    = "openai/gpt-4o"

[ai.routing.tiers.complex]
provider = "anthropic"
model    = "claude-3-5-sonnet-latest"
```

| Key | Type | Default | Effect |
|---|---|---|---|
| `enabled` | bool | `false` | Enable tier-based complexity routing |
| `tiers.simple.provider` / `.model` | String | None | Provider + model for Simple tier (score 0–1) |
| `tiers.medium.provider` / `.model` | String | None | Provider + model for Medium tier (score 2–3) |
| `tiers.complex.provider` / `.model` | String | None | Provider + model for Complex tier (score ≥4) |

When `enabled = false`, the section is still parsed but routing is a no-op.
The `routing.explain` cap is always registered regardless of this flag.
See `docs/reasoning-pipeline.md` for the full tier-routing specification.

#### `[ai.belief_state]` — LLM-driven belief tracking

```toml
[ai.belief_state]
enabled                = false
belief_model_name      = ""        # empty = provider default cheap model
max_beliefs            = 10
min_confidence_to_retain = 0.55
inject_into_prompt     = true
```

| Key | Type | Default | Effect |
|---|---|---|---|
| `enabled` | bool | `false` | Enable LLM-driven belief extraction and injection |
| `belief_model` | String? | None | Provider name override for belief calls |
| `belief_model_name` | String | `""` | Model id; empty = provider default |
| `max_beliefs` | usize | `10` | Max beliefs retained per `(subject, session)` |
| `min_confidence_to_retain` | f32 | `0.55` | Drop beliefs below this score on each update |
| `inject_into_prompt` | bool | `true` | Prepend current beliefs to system prompt before each call |

#### `[ai.judge]` — judge model

```toml
[ai.judge]
enabled              = false
judge_model_name     = ""      # empty = provider default cheap model
judge_threshold      = 0.6
max_judge_latency_ms = 6000
recent_buffer_size   = 256
```

| Key | Type | Default | Effect |
|---|---|---|---|
| `enabled` | bool | `false` | Enable the judge model |
| `judge_model` | String? | None | Provider name override for judge calls |
| `judge_model_name` | String | `""` | Model id; empty = provider default |
| `judge_threshold` | f32 | `0.6` | Confidence ceiling; judge fires when confidence is below this |
| `max_judge_latency_ms` | u64 | `6000` | Timeout; exceeded → synthetic `proceed` verdict |
| `recent_buffer_size` | usize | `256` | Ring buffer depth for `judge.recent_verdicts` |

A `Block` verdict from the judge returns `POLICY_DENIED` to the caller rather
than exposing the text. See `docs/reasoning-pipeline.md` for activation conditions.

#### `[ai.perception_security]` — two-stage perception isolation

```toml
[ai.perception_security]
enabled          = false
extraction_model = ""      # empty = controller default model
max_output_chars = 8192
```

| Key | Type | Default | Effect |
|---|---|---|---|
| `enabled` | bool | `false` | Enable two-stage isolation for `ai.perception_extract` |
| `extraction_model` | String | `""` | Model id for the extraction stage |
| `max_output_chars` | usize | `8192` | Hard cap on extraction output length |

When disabled, `ai.perception_extract` returns `{"extracted":"","model":"","isolated":false}` so
callers can fall through to plain `ai.chat`.

## `ai.chat` call contract (SIMP-016)

The canonical arg format is JSON:

```json
{"session_id": "my-session", "prompt": "Hello", "history": "..."}
```

Pipe-delimited (`session_id|prompt|history`) is a legacy fallback kept for SOL
flows written before SIMP-016. All new callers should use the JSON form.

The `approval_token=<value>` key-value pair may appear **anywhere** in the args
string; its presence flips a `RequiresApproval` plan step to `Approved` without
requiring the string to be in a specific position.

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
$GEMINI_API_KEY      → same
```

Keys are stored in `zeroize::Zeroizing<String>` and wiped from the heap when
the provider is dropped. They are read once at startup from the environment;
the AI node crashes with a clear message if a referenced env var is missing
or empty.

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

### OpenAI / Anthropic / OpenRouter / xAI / Gemini
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

## Anthropic-specific features

### Prompt caching

The Anthropic provider always sends the system block with
`cache_control: {type: "ephemeral"}`. This enables Anthropic's prompt-caching
feature, which can reduce costs by ~90% on repeated calls that share a
long system prompt within the cache window (approximately 5 minutes).

```json
"system": [{"type": "text", "text": "...", "cache_control": {"type": "ephemeral"}}]
```

No config is required. Caching activates automatically whenever the Anthropic
provider is in use.

### Extended thinking

Set `thinking_budget_tokens` in the `ChatInput` to enable Anthropic's extended
thinking mode. The provider sends `"thinking": {"type": "enabled", "budget_tokens": N}`
in the request. Thinking deltas are filtered from the stream so callers only
see the final reply text.

## Failure modes

| Failure | Source | Surface |
|---|---|---|
| `api_key_env` names a missing env var | provider startup | `Permanent("missing provider key: $NAME")` — controller crashes with a clear message |
| `api_key_env` set but empty | provider startup | `Permanent("env var 'NAME' is set but empty")` |
| `[ai.providers.X]` missing for active provider | `build_provider` | "requires `[ai.providers.X]` config section" at startup |
| Provider returns HTTP 429 | runtime | `FailoverReason::RateLimitGenuine` or `RateLimitCredentialRotation` → `Transient(...)` |
| Provider returns 5xx | runtime | `FailoverReason::Server5xx` → `Transient(...)` |
| Provider returns 4xx auth error | runtime | `FailoverReason::AuthRejected` → `Permanent(...)` |
| Context window exceeded | runtime | `FailoverReason::ContextOverflow` → `Permanent(...)` |
| Model not found | runtime | `FailoverReason::ModelNotFound` → `Permanent(...)` |
| Network unreachable | runtime | `FailoverReason::TransportFailure` → `Transient(...)` |

### Structured failover labels

All provider errors carry a structured `FailoverReason` with 11 variants.
Each reason exposes a `.label()` string that appears in error messages
(e.g. `[rate-limit]`, `[context-overflow]`, `[auth-rejected]`) and a
`.category()` that maps to `Transient`, `Permanent`, or `Compress`.

| Variant | Label | Category |
|---|---|---|
| `RateLimitGenuine` | `rate-limit` | Transient |
| `RateLimitCredentialRotation` | `rate-limit-rotation` | Transient |
| `Server5xx` | `server-5xx` | Transient |
| `Timeout` | `timeout` | Transient |
| `AuthRejected` | `auth-rejected` | Permanent |
| `ContextOverflow` | `context-overflow` | Compress |
| `PayloadTooLarge` | `payload-too-large` | Compress |
| `ImageRejected` | `image-rejected` | Compress |
| `ModelNotFound` | `model-not-found` | Permanent |
| `InvalidRequest` | `invalid-request` | Permanent |
| `TransportFailure` | `transport-failure` | Transient |
| `Unknown` | `unknown` | Permanent |

## Tests

`crates/relix-runtime/src/nodes/ai/provider/` ships unit coverage:

- `mock`: deterministic reply with history-size check.
- `openai_compat`: missing-base-url error, provider-name passthrough.
- `anthropic`: missing-api-key-env error, no-api-key-env-at-all error.
- `gemini`: full implementation with end-to-end mock HTTP server tests; stream parsing; usage metadata extraction.
- `provider`: `load_api_key` precedence (unset, empty, missing-var-named).
- `mod`: `build_provider` defaults to mock, requires per-provider section,
  rejects unknown, errors clearly on anthropic without env, accepts local
  without key.

Run `cargo test -p relix-runtime nodes::ai` (12+ assertions) to verify
locally before changing provider plumbing.

## Where the web bridge fits

`relix-web-bridge` accepts both native (`POST /chat`, `POST /chat/stream`) and
OpenAI-compatible (`POST /v1/chat/completions`, `GET /v1/models`) requests on
`127.0.0.1`. None of these endpoints select a provider — they all run the same
SOL chat flow, which delegates to whichever provider the AI node is configured
with. The `model` field in OpenAI requests is cosmetic; the bridge advertises
it back unchanged so OpenAI clients show something useful in their picker.

Switching providers on the AI node (mock → openrouter → anthropic) requires
zero changes to Open WebUI's settings or to any other OpenAI-compatible client
pointed at the bridge. See `docs/streaming-and-openai-shim.md`.
