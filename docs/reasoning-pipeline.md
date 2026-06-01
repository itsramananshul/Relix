# Reasoning Pipeline

Relix 0.4.1 ships a multi-stage reasoning pipeline (RELIX-7.29) that sits
between the raw LLM call and the response returned to the caller. The pipeline
has five optional components, all disabled by default and independently
configurable:

1. **Tier routing** — classifies request complexity and dispatches to the
   appropriate provider + model.
2. **Self-consistency** — fans out parallel samples, scores coherence, returns
   the best answer.
3. **Belief state tracking** — maintains a per-session belief block that is
   injected into every system prompt.
4. **Judge model** — audits responses on a gated subset of calls; can block or
   annotate.
5. **Perception security** — two-stage isolation for content from external
   sources.

All five are co-ordinated through the same `ai.chat` and `ai.chat.stream`
capabilities. The `reasoning.status` coordinator cap provides a live snapshot
of every component's state.

---

## Tier routing

### What it does

The complexity classifier maps a request to one of three tiers:

| Tier | Score | Meaning |
|---|---|---|
| `simple` | 0–1 | Short, conversational, no code or multi-step instructions |
| `medium` | 2–3 | Moderate length or one elevated signal |
| `complex` | ≥ 4 | Long, multi-step, technical, or deep-context session |

The tier resolver then looks up the configured `(provider, model)` for that
tier. When the configured provider is unhealthy or the tier slot is
unconfigured, the resolver walks up the fallback chain:
Simple → Medium → Complex → controller default.

### Scoring signals

| Signal | Points |
|---|---|
| Message length: 50–200 words | +1 |
| Message length: > 200 words | +2 |
| Contains a fenced code block | +1 |
| Multi-step instruction marker ("Step 1", "first…then", etc.) | +1 |
| Technical keyword present | +1 |
| Explicit complexity marker ("think carefully", "analyze in depth", etc.) | +2 |
| Session has > 5 prior turns | +1 |
| > 3 distinct noun-phrase topics detected | +1 |

### Config

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

When `enabled = false` (the default) or a tier slot is unconfigured, the
handler uses the controller's default provider and model for that tier.

### `routing.explain` cap

Always registered regardless of the `enabled` flag. Takes a JSON request and
returns a dry-run routing decision without executing any provider call:

```json
// request
{"message": "write a Rust TCP server", "session_turns": 0}

// response
{
  "score":           {"tier":"medium","score":2,"signals_triggered":["contains_code_block","technical_keyword:tcp"]},
  "decision":        {"provider":"openrouter","model":"openai/gpt-4o","tier":"medium","fell_back":false,"reasoning":"..."},
  "routing_enabled": true
}
```

---

## Self-consistency

### What it does

When the baseline confidence for a request falls below `min_score_to_enable`,
the AI handler fans out `sample_count` parallel provider calls (same prompt,
same model). Each sample's first ~100 words are extracted and embedded. The
pairwise cosine similarities are averaged; the sample with the highest average
cosine to the others (the most coherent sample) is returned as the final
response.

On `ai.chat.stream`, self-consistency runs N unary calls in parallel, picks
the winner, and re-emits it via the whitespace-token chunker
(`chunk_for_stream`) rather than forwarding provider-native token deltas.

### Cost guards

- **Per-request budget** (`per_request_budget_usd`, default $1.00): if the
  estimated cost of SC + judge + belief for a single request would exceed
  this, SC (and then judge, then belief) are skipped in that order.
- **Trigger-rate guard** (`max_trigger_rate_pct`, default 50%): when the
  rolling 1000-sample trigger rate exceeds this percentage, SC is disabled for
  `disable_duration_secs` (default 300 s) and a cost alert fires.
- **Hourly budget** (`sc_hourly_budget_usd`, default $10/hour): crossing the
  hourly spend cap triggers the same disable + alert.

### Config

```toml
[confidence.self_consistency]
enabled              = true
sample_count         = 3        # default
min_score_to_enable  = 0.70     # default
capability_patterns  = ["ai.chat", "ai.chat.*"]  # empty = all caps
per_request_budget_usd  = 1.00  # default
max_trigger_rate_pct    = 50    # default
disable_duration_secs   = 300   # default
sc_hourly_budget_usd    = 10.0  # default
```

---

## Belief state tracking

### What it does

After every `ai.chat` call returns to the caller, a fire-and-forget
`tokio::spawn` asks the configured belief model to extract a short JSON array
of `{text, confidence}` belief items from the conversation. Items below
`min_confidence_to_retain` are dropped; the list is truncated to `max_beliefs`.

On the *next* call for the same `(subject_id, session_id)`, the current belief
block is prepended to the system prompt as:

```text
[Current beliefs about this conversation]
• <belief 1 text>
• <belief 2 text>
```

The belief update is a non-blocking spawn — caller latency is not affected.

Belief state is NOT updated during `ai.chat.stream`.

### Persistence

When a `LayeredMemoryStore` handle is wired (post-RELIX-7.29 follow-up), every
`set()` also writes a Layer-4 record with id
`blake3("belief_state|<subject>|<session>")` and tags `belief_state` +
`session:<session_id>`. Beliefs survive a controller restart for every pair
that was previously written.

### `belief.get` and `belief.reset` caps

```json
// belief.get — request
{"session_id": "my-sess"}
// or
{"subject_id": "user-abc", "session_id": "my-sess"}

// belief.get — response
[{"text": "user prefers short answers", "confidence": 0.82}, …]

// belief.reset — request
{"session_id": "my-sess"}
// response: empty 200
```

`belief.reset` removes both the in-memory entry and the persisted record when
a store is wired.

### Config

```toml
[ai.belief_state]
enabled                  = false
belief_model_name        = ""    # empty = provider default
max_beliefs              = 10    # default
min_confidence_to_retain = 0.55  # default
inject_into_prompt       = true  # default
```

`belief_model` (optional string) overrides which provider is used for the
belief call; absent means use the same provider as `ai.chat`.

---

## Judge model

### What it does

The judge is a second LLM call that audits the primary response. It fires only
when ALL FOUR of the following are true:

1. `[ai.judge] enabled = true`
2. The final confidence is below `judge_threshold` (default 0.6)
3. The response carries a tool call or a structured-output marker (JSON/TOML/YAML
   fenced block, or a leading `{` / `[`)
4. The session has at least 2 prior turns

When the gate opens, the handler dispatches `generate_reply` against the
configured judge model, capped at `max_judge_latency_ms` (default 6000 ms).
A timeout produces a synthetic `proceed` verdict with a timeout note.

The judge prompt asks five questions and expects a JSON object:

```json
{
  "answers_question": "yes" | "no" | "partial",
  "action_is_safe":   "yes" | "no" | "needs_review",
  "factual_errors":   ["<error text>", …],
  "overconfident":    true | false,
  "verdict":          "proceed" | "modify" | "block"
}
```

### Verdict handling

| Verdict | Effect |
|---|---|
| `proceed` | Response returned unchanged |
| `modify` | `[Judge: please revise — <factual_errors>]` appended to response |
| `block` | `POLICY_DENIED` error returned to caller; the response text is not exposed |

The judge is skipped during `ai.chat.stream`.

### Observability caps

**`judge.recent_verdicts`** — returns the most recent records from the
in-process ring buffer (default depth 256):

```json
// request
{"limit": 20}   // default 20

// response
{"verdicts": [{
  "agent": "…", "session_id": "…", "timestamp_ms": 1718000000000,
  "final_confidence": 0.42, "timed_out": false,
  "verdict": {"answers_question":"yes","action_is_safe":"yes","factual_errors":[],"overconfident":false,"verdict":"proceed"}
}, …]}
```

**`judge.stats`** — aggregate counters across all verdicts since process start:

```json
{
  "proceed_count": 12, "modify_count": 1, "block_count": 0,
  "timeout_count": 0,  "recent_buffered": 13, "capacity": 256,
  "per_agent": {"my-agent": {"proceed": 12, "modify": 1, "block": 0}}
}
```

### Config

```toml
[ai.judge]
enabled              = false
judge_model_name     = ""      # empty = provider default
judge_threshold      = 0.6    # default
max_judge_latency_ms = 6000   # default
recent_buffer_size   = 256    # default
```

`judge_model` (optional string) overrides which provider is used for the judge
call; absent means use the same provider as `ai.chat`.

---

## Perception security

### What it does

`ai.perception_extract` implements a two-stage isolation scheme for content
ingested by perception tools (documents, web pages, screenshots, transcripts).
The raw content is passed to the *extraction* model; only the extracted
structured output is ever seen by the planning model. A hostile document can
subvert the extraction stage, but the planner only sees the extracted JSON.

When `[ai.perception_security] enabled = false` (the default), the cap returns:

```json
{"extracted": "", "model": "", "isolated": false}
```

Callers that see `isolated: false` fall through to plain `ai.chat`.

### Wire shape

```json
// request
{
  "content":         "<raw document / web text / transcript>",
  "instructions":    "extract the order id and total amount",
  "max_output_chars": 8192
}

// response
{
  "extracted": "<structured output from the extraction model>",
  "model":     "<extraction model id>",
  "isolated":  true
}
```

`max_output_chars` defaults to 8192 and is hard-capped at the configured value
regardless of what the caller requests.

### Config

```toml
[ai.perception_security]
enabled          = false
extraction_model = ""      # empty = controller default model
max_output_chars = 8192    # default
```

---

## `reasoning.status` cap

Always registered. Returns a live JSON snapshot of every component's state,
useful for health dashboards and debugging. No arguments required.

```json
{
  "routing": {
    "enabled": true,
    "config":  { … RoutingConfig … }
  },
  "self_consistency": {
    "enabled": false,
    "config":  { … SelfConsistencyConfig … },
    "stats":   { … }
  },
  "belief_state": {
    "enabled":          true,
    "config":           { … BeliefStateConfig … },
    "tracked_sessions": 7
  },
  "judge": {
    "enabled": false,
    "config":  { … JudgeConfig … },
    "stats":   { "proceed_count": 0, "modify_count": 0, "block_count": 0, … }
  }
}
```

Components that are wired but disabled report `"enabled": false` with zeroed
counters so operators can distinguish "off" from "broken".

---

## Pipeline ordering in `ai.chat`

The stages run in this fixed order on every `ai.chat` call:

1. Input guardrail
2. Memory + RAG fetch
3. Soul persona load (mtime hot-reload)
4. Skill hint injection
5. **Belief injection** (reads current beliefs → prepends to system prompt)
6. **Tier routing** (classifies prompt → may swap provider + model)
7. Primary provider call
8. Metrics recording
9. **Self-consistency** (conditional — fans out additional samples)
10. **Judge** (conditional — audits response; may block)
11. **Belief update** (fire-and-forget spawn)
12. Planner / Executor / Approval (unary path only)
13. Evidence record + training interaction + provenance

`ai.chat.stream` runs steps 1–9 (SC applies to the stream path) but skips
steps 10–12 (judge, belief update, planner).
