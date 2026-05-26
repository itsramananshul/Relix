# Agent Memory (Frozen-Snapshot Pattern)

Persistent per-agent memory that survives across chat sessions.
Patterned on Hermes's `MEMORY.md` + `USER.md` pair — see
[`docs/proposals/hermes-full-analysis.md`](proposals/hermes-full-analysis.md)
section 4.1 for the design lineage and rationale.

This is the foundation layer. Vector embeddings, cross-agent
memory sharing, and richer scoping are explicitly out of scope
and land in later waves.

## What it is

Two text stores, keyed by the agent's `subject_id`:

| Target  | Char cap | What it holds |
|---------|---------:|---------------|
| `agent` | 2200     | The agent's notes about its environment, tools, project conventions, learned facts. |
| `user`  | 1375     | What the agent knows about the user it serves — preferences, communication style, workflow habits. |

Char caps are character-based (not byte- or token-based)
because char counts are model-independent. Same caps as Hermes.

Entries inside a target are separated by `§` (U+00A7, section
sign). Entries can be multiline; only the delimiter is
forbidden inside an entry. Operators reading the dashboard see
the delimiter verbatim; the model sees the labeled block at the
top of every chat call.

## How agents write to it

Agents write through the `memory.agent_write` capability on the
memory node, called from inside a chat session via the agent's
tool surface (today: directly through `remote_call` in a SOL
flow; future: a wrapped `memory` tool exposed to the LLM).

Wire format:

```
memory.agent_write
  arg: subject_id|target|action|data
  target = "agent" | "user"
  action = "add" | "replace" | "remove" | "read"
  data = action-specific (see below)
```

Action semantics:

| Action    | `data` shape                | Behavior |
|-----------|-----------------------------|----------|
| `add`     | `<new entry text>`          | Append; delimiter inserted between entries. Entry text must NOT contain `§`. |
| `replace` | `<find>\t<replacement>`     | Find the unique entry containing `<find>` (substring match) and replace its entire text with `<replacement>`. Ambiguous matches → `INVALID_ARGS`. |
| `remove`  | `<find>`                    | Find the unique entry containing `<find>` and drop it (along with its delimiter). Ambiguous matches → `INVALID_ARGS`. |
| `read`    | (ignored)                   | Return the current content of the specified target. Same data as `memory.agent_read`, but for one target only. |

Return shape:

- For `add` / `replace` / `remove`: `ok|chars=<new_total>\n`
- For `read`: raw content bytes of the target (no header)

Caps are enforced on every write. A write that would push the
target past its cap returns `INVALID_ARGS` with:

```
'agent' write would be 2245 chars (cap 2200). Remove old
entries before adding new ones.
```

Agents are expected to manage their own memory budgets — Relix
does not silently truncate.

## How it gets injected (frozen-snapshot)

The AI node's `ai.chat` handler reads memory ONCE at the start
of each chat call and bakes it into the system prompt before
invoking the LLM provider. The exact block:

```
--- AGENT MEMORY ---
<agent memory content verbatim>

--- USER MEMORY ---
<user memory content verbatim>
--------------------
```

When both targets are empty, the block is skipped entirely
(no value in sending blank headers to the model).

The injection routes through `ChatInput.system_prompt`:

- The Anthropic provider honors `system_prompt` natively.
- The OpenAI-compat provider prepends a `{"role": "system", ...}`
  message before the user turn.
- The mock provider ignores it (which is fine — mock tests
  the dispatch path, not the LLM behavior).

### Why "frozen-snapshot"

Mid-session memory writes go to the memory store immediately
(durable on the next read), but the running chat session's
prompt does NOT re-render. The snapshot the model sees stays
stable until the **next** session starts. This matches Hermes's
posture and exists for two reasons:

1. **Prompt-cache friendliness.** Most providers cache the
   system prompt across turns. Re-rendering mid-session would
   invalidate the cache on every memory write.
2. **Determinism.** A multi-turn conversation should reason
   over a stable substrate. If the agent edits its own memory
   mid-conversation, the new state lands on the next session —
   the model isn't watching its own reflection shift.

### Silent skip on failure

If the memory peer is unreachable, the response decodes wrong,
or the bridge has no `[ai.memory_peer]` configured, ai.chat
proceeds **without memory injection**. Memory is additive — a
chat call MUST NEVER fail because the memory store is degraded.

## How operators read it

Two surfaces, both read-only — operators never write memory
through these (writes are agent-driven).

### Dashboard

`#/memory` page in the operator dashboard. Paste a subject_id
(64-char hex from an agent's identity bundle) and click `read`.
Shows the current agent + user memory verbatim, with character
counts vs caps.

### CLI

```bash
relix-cli ops agent-memory --subject-id <hex>
```

Pretty output with both targets, char counts, and a reminder of
the delimiter + per-subject scoping. `--json` dumps the raw
bridge response.

Both hit `GET /v1/memory/agent?subject_id=<id>&peer=<alias>`
on the bridge, which proxies `memory.agent_read` to the memory
node.

## Storage

A new SQLite table on the memory node's existing database:

```sql
CREATE TABLE agent_memory (
    subject_id TEXT    NOT NULL,
    target     TEXT    NOT NULL,
    content    TEXT    NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (subject_id, target)
);
```

One row per `(subject_id, target)`. UPSERT on every write.
Subject isolation is by primary key — agent A's row literally
cannot contain agent B's content.

## Configuration

### Memory node

No new config — the memory node grows the `turns` / `agent_memory`
tables automatically on first start. Note: the old FTS5-over-turns
capability is now called `memory.search_turns`; the name
`memory.search` is taken by the new vector-embedding semantic
search (see [`docs/vector-memory.md`](./vector-memory.md)).
Optional `[memory.embedding_peer]` enables the new
`memory.embed` / `memory.search` / `memory.embed_all` surface.

### AI node

To enable memory injection, the AI controller's config grows an
optional `[ai.memory_peer]` section:

```toml
[ai.memory_peer]
addr = "/ip4/127.0.0.1/tcp/19711"
# alias = "memory"      # default; the alias the outbound dial uses
# deadline_secs = 5     # default; ai.chat memory fetch budget
```

When `[ai.memory_peer]` is missing, the AI node boots without
outbound mesh capability and memory injection is silently
skipped. Existing chat behavior is unchanged.

The AI controller uses the same identity bundle and signing key
the heartbeat sender uses (`<identity.key_path>.bundle`).
Missing bundle → warn + skip; chat keeps serving.

## Memory Curator

A background process that periodically asks the AI peer to
consolidate redundant entries, remove stale information, and
keep each agent's memory lean and useful. Patterned on
Hermes's curator subsystem (`agent/curator.py`, 1781 lines) but
scoped to just the two memory targets that already exist —
no skills, no procedural memory, no cross-agent state.

### What it does

For each agent whose total (agent + user) memory is over the
configured threshold:

1. Read both targets via `MemoryStore::agent_read`.
2. For each non-empty target, build a structured prompt with
   explicit rules (deduplicate, consolidate, drop stale,
   preserve `§`, stay under the cap) and send it to `ai.chat`
   on the AI peer.
3. Validate the reply: non-empty AND within the target's cap.
4. Atomically replace the target's content via the internal
   `MemoryStore::agent_set_content` method.

The capability is also exposed directly for on-demand /
operator-triggered curation:

```
memory.agent_curate(arg = "subject_id|ai_peer_alias")
returns pipe-delimited summary with:
  subject_id, agent_entries_before/after,
  agent_chars_before/after, user_entries_before/after,
  user_chars_before/after, chars_saved
```

### How to enable it

The memory controller's config grows a new section:

```toml
[memory.curator]
enabled = true              # master switch
interval_secs = 3600        # tick cadence (1 hour default)
min_chars_to_curate = 100   # skip agents below this threshold

[memory.curator.ai_peer]
addr = "/ip4/127.0.0.1/tcp/19712"
alias = "ai"                # default
deadline_secs = 30          # ai.chat budget (slow; give it room)
```

When `[memory.curator]` is missing entirely, the scheduler is
not spawned AND `memory.agent_curate` returns
`RESPONDER_INTERNAL` with a clear "AI dispatcher not
configured" message. The capability stays advertised so
operators can see it in the manifest; calls just fail loud
rather than silently no-op.

When `enabled = false`, the scheduler is not spawned but the
capability is still wired — operators can curate manually via
the dashboard or `POST /v1/memory/curate`.

### How to trigger manually

**Dashboard** — `#/memory` page has a `curate` button next to
`read`. Paste a subject_id, click curate, wait (slow — runs
a real LLM call), and the panels refresh with the curated
content. A toast announces `chars_saved`.

**Bridge** — `POST /v1/memory/curate` with body
`{ "subject_id": "...", "peer": "memory", "ai_peer": "ai" }`.
Response shape:

```json
{
  "peer": "memory",
  "subject_id": "...",
  "result": {
    "agent_entries_before": 5,
    "agent_entries_after":  3,
    "agent_chars_before":   200,
    "agent_chars_after":    120,
    "user_entries_before":  3,
    "user_entries_after":   2,
    "user_chars_before":    80,
    "user_chars_after":     50,
    "chars_saved":          110
  }
}
```

**Status** — `GET /v1/memory/curator/status?peer=memory`.
Honest scope: the runtime's `CuratorState` lives in-process on
the memory node and isn't yet reachable via a capability. The
endpoint surfaces what the bridge knows (configured peer alias)
plus an explicit `bridge_note` field naming the gap. Manual
curation through `/v1/memory/curate` works end-to-end; the
scheduler-wide stats land when a follow-up
`memory.curator_status` capability ships.

### What the curation prompt does

The prompt is templated and tested verbatim (`tests::
build_curation_prompt_contains_delimiter_cap_and_content`):

```
Curate the following agent memory. Rules:
1. Remove duplicate or near-duplicate entries
2. Consolidate related entries into one clear entry
3. Remove entries that are outdated or no longer useful
4. Keep entries that are specific and actionable
5. Preserve § as the delimiter between entries
6. Stay within <cap> characters total
7. Return ONLY the curated entries separated by §, nothing else

Current entries:
<content>
```

The history field carries a system-context line ("You are a
memory curator…") so providers that respect history-as-context
see the role assignment. The agent + user targets are curated
in **separate** ai.chat calls so a bad reply on one doesn't
poison both.

### Hard invariants

The curator NEVER:

- **Wipes memory.** Empty or whitespace-only replies are
  rejected; existing content is preserved.
- **Invents entries.** The prompt explicitly tells the model
  to return only consolidated / preserved entries — never
  to add facts.
- **Exceeds the cap.** Replies over `AGENT_MEMORY_CAP_CHARS`
  (2200) / `USER_MEMORY_CAP_CHARS` (1375) are rejected;
  existing content is preserved.
- **Re-renders running sessions.** Curator writes hit the
  memory store immediately, but the next ai.chat session
  re-reads the frozen snapshot — same contract as a regular
  agent write.

Every failure path is a `tracing::warn` + the agent's existing
memory is left untouched. The dashboard / CLI just sees the
unchanged contents on the next read.

## Session search across chat-turn history

Operators searching for "what did the agent say about X last
week" hit the **session search** surface. Unlike persistent
agent memory (above), session search is a query over the
coordinator's chat-turn chronicle: every `chat.user_turn` /
`chat.assistant_turn` event landed by the bridge's chat flow
(W5) is searchable.

### Surfaces

Every surface routes to the same coordinator capability
`task.session_search` so the results stay consistent across
clients.

**From a SOL flow** — agents call the tool-node proxy:

```sol
let hits: str = remote_call("tool", "memory.session_search", "alice|kubernetes|10")
```

Wire: `subject_id|query|limit`. Empty `subject_id` searches
every session; non-empty restricts to sessions owned by that
subject (joined through `tasks.owner_subject_id`). Limit
defaults to 20, capped at 100.

Reply is a JSON array; each entry is:

```jsonc
{
  "session_id":     "oa-abcdef",
  "role":           "assistant",
  "content":        "Yes — the kubernetes operator we …",
  "timestamp_unix": 1716000000,
  "snippet":        "…the kubernetes operator we recommended last…",
  "score":          1.0
}
```

`score` is `1.0` for every hit today; reserved for a BM25
score when FTS5 indexing of the chronicle lands.

**From the CLI** — operators search the same chronicle:

```bash
# Find every turn that mentioned "kubernetes"
relix-cli ops session-search --query kubernetes

# Restrict to one agent's sessions
relix-cli ops session-search --query kubernetes --subject-id <hex>

# JSON for piping into jq
relix-cli ops session-search --query kubernetes --json | jq '.results | length'
```

**From the dashboard** — `#/session-search` (under Memory in
the sidebar) renders the same search with a form: query
input, subject_id filter, 10/20/50/100 limit selector, and a
results list where every `session_id` links straight to
`GET /v1/sessions/export?session=...` so the operator can pop
the full transcript open.

**From an HTTP client** — the bridge endpoint:

```
GET /v1/memory/sessions/search?q=<query>&subject_id=<id>&limit=<n>
→ 200 { results: [...], total: N, query: "...", subject_id: "..." }
→ 400 when q is missing or empty
→ 503 when no memory peer is wired
```

### What it indexes

Only chronicle events of type `chat.user_turn` and
`chat.assistant_turn`. Those land on a per-turn basis through
the bridge's chat flow (`flow.rs::record_chat_turn`). The
search ignores task lifecycle events, capability invocations,
and the per-turn payload of persistent agent memory.

### Operator-only "search everything"

When `subject_id` is empty, the gate skips the
`tasks.owner_subject_id` join and returns hits from every
session the coordinator knows about. That's the operator
debugging path — agents calling through the tool node always
have a subject_id on their bundle, so this branch only fires
when the bridge or CLI explicitly leaves the filter blank.

## What's deliberately NOT here

- **Vector embeddings.** Memory is keyword text. Future waves.
- **Cross-agent shared memory.** Each `subject_id` owns its row.
- **Per-session scoping** on persistent memory. Persistent
  memory is global per-agent across all sessions; session
  search is the per-session-scoped surface for chat-turn
  history.
- **Per-team / department scoping.** No grouping today.
- **Auto-eviction.** Agents must remove old entries themselves.
- **BM25 / FTS5 ranking.** Session search uses `LIKE '%q%'`
  today. Score is always 1.0; the FTS5 index is a follow-up
  that doesn't change the wire shape.
- **Operator-side editing.** Dashboard + CLI are READ-ONLY.

When these become real needs (and they will) they land as
follow-up tracks on top of the foundation here.
