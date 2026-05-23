# Memory

Memory in Relix has three layers, all served by the same
memory node (one SQLite database; multiple tables). Per-session
chat turns are stored with an FTS5 keyword index. Per-subject
vector embeddings give cosine top-K semantic search. Persistent
agent memory holds frozen text snapshots that get injected into
the system prompt and are kept lean by a background curator.

This doc is the index — it names every capability, surface,
and route. The two layer-specific designs are detailed in
[`agent-memory.md`](agent-memory.md) (text + frozen snapshots +
curator) and [`vector-memory.md`](vector-memory.md) (embeddings
+ search).

## Layer 1 — Chat-turn store

Every chat call writes one row per message. The memory node
runs an FTS5 index across the `text` column so substring search
across all turns is cheap. This is what `memory.recent_for_session`
reads when the chat flow needs the last N turns to send to the
LLM.

| Capability                 | Arg                                | Returns |
|----------------------------|------------------------------------|---------|
| `memory.write_turn`        | `session_id\|role\|text`           | `ok\n` |
| `memory.recent_for_session`| `session_id` or `session_id\|N`    | `role: text\n` per turn (oldest first) |
| `memory.search_turns`      | `query` or `query\|N`              | `session_id\trole\ttext\n` per match |

Note: `memory.search` is the **vector** capability. The FTS5
keyword search is `memory.search_turns` — the rename happened
when vectors landed, so don't confuse them.

Storage: a `turns` table on `memory.db`, with `turns_fts` as
the FTS5 virtual companion. No char cap on individual messages;
the database grows monotonically until an operator truncates
it.

### RAG (Retrieval-Augmented Generation)

Multi-turn history is per-session; RAG retrieves across **all
past sessions** by searching the vector layer. Opt-in per
deployment.

When `[ai.memory_peer] rag_enabled = true` is set, every
`ai.chat` call now also:

1. Embeds the user's prompt locally — the AI node owns a
   `ChatProvider`, so this is a synchronous in-process call,
   no libp2p hop. Providers without embedding support (default
   trait impl returns `Permanent("... does not support
   embeddings")`) silently skip RAG; `ai.chat` proceeds.
2. Calls `memory.search` on the memory peer once per target
   (`agent`, then `user`) with the precomputed embedding sent
   as `embedding=<base64-LE-f32>` in the wire arg. The memory
   node detects the precomputed field and skips its own
   outbound embed call — no AI → memory → AI(embed) circular
   hop.
3. Merges all hits, drops any below `rag_min_score`, sorts by
   descending score, takes the top `rag_top_k`, and formats as:

   ```
   --- Relevant context from memory ---
   [score: 0.92] (agent) <chunk text>
   [score: 0.87] (user)  <other chunk>
   ---
   ```

4. Injects the block into `system_prompt` after the existing
   agent/user memory block. **System prompt order:** agent
   memory → RAG block. Conversation history goes into
   `ChatInput.history`, not the system prompt.

If no hits clear `rag_min_score`, the RAG block is omitted
silently. If the memory peer is unreachable, RAG silently
skips. Like every other memory injection step, `ai.chat`
never fails because RAG failed.

Configuration knobs in `[ai.memory_peer]`:

| Key             | Default | Meaning                                       |
|-----------------|---------|-----------------------------------------------|
| `rag_enabled`   | `false` | Whether the AI node performs RAG retrieval    |
| `rag_top_k`     | `5`     | Max hits in the formatted block (after merge) |
| `rag_min_score` | `0.70`  | Cosine-similarity floor; hits below are dropped |

What gets retrieved: chunks written to the persistent memory
layer via `memory.embed` (directly) or
`memory.embed_all` (retrofit from `memory.agent_write`
entries). RAG does **not** index the chat-turn store —
multi-turn history covers that. Use `memory.embed` to
explicitly add a memory you want surfaced across sessions.

### Automatic history injection

When the AI controller is configured with `[ai.memory_peer]`
(see [`configuration.md`](configuration.md)), the **AI node
fetches `memory.recent_for_session` itself** for every `ai.chat`
call — flows no longer need a manual `remote_call` for it.

The flow:

1. The AI node parses the wire arg `session_id|prompt|history`.
2. If `session_id` is non-empty and the memory dispatcher is
   wired, it calls `memory.recent_for_session` with
   `session_id|max_history_turns` (default 10).
3. The returned `role: text\n` block is merged with the
   caller-supplied `history` field — auto-fetched lines first
   (older context), then caller-supplied (newer or extra
   context).
4. The merged block is passed to the provider as
   `ChatInput.history`. OpenAI-compatible providers wrap it
   into the user message; future providers may project each
   line as a separate message object.

Failure modes are **silent skip**: memory peer unreachable,
unparseable response, empty session — all proceed without
history. `ai.chat` never fails because memory is unavailable.

Configure `max_history_turns` in
`[ai.memory_peer] max_history_turns = N` to change the cap. The
memory node enforces its own ceiling on top of this.

## Layer 2 — Vector memory

Each entry written to persistent agent memory can be embedded
through an AI peer (`ai.embed`) and stored alongside the row,
keyed by `(subject_id, target)`. Cosine top-K then surfaces
topically related chunks for any query — no keyword overlap
needed.

| Capability         | Arg                                          | Returns |
|--------------------|----------------------------------------------|---------|
| `memory.embed`     | `subject_id\|target\|text`                   | `embedding_id=<id>\n` (new) or `ok\|embedding_id=<id>\n` (dedup) |
| `memory.search`    | `subject_id\|target\|query[\|limit]`         | `<id>\t<score>\t<chunk>\n` per hit, then `count=N\n` |
| `memory.embed_all` | `subject_id`                                 | `ok\|chunks_embedded=N\n` |

`target` is `"agent"` or `"user"`. Default `limit` is 5,
max 20. Scores are cosine similarities in `[-1, 1]`; higher
is closer.

Storage: a `memory_embeddings` table with `(subject_id,
target, entry_hash)` uniqueness — re-embedding the same text
under the same subject + target returns the existing row.

Wiring: requires `[memory.embedding_peer]` on the memory
controller config pointing at an AI peer's `ai.embed`. Without
it, the three capabilities still register but return a clear
"embedding dispatcher not configured" error.

Full design: [`vector-memory.md`](vector-memory.md).

## Layer 3 — Persistent agent memory

Two text stores per agent, keyed by `subject_id` and capped at
2200 / 1375 chars. Entries are delimited by `§` (U+00A7).
Frozen at the start of every chat session — the snapshot the
LLM sees stays stable for the whole session, even if the agent
writes new entries mid-flow.

| Capability               | Arg                                | Returns |
|--------------------------|------------------------------------|---------|
| `memory.agent_read`      | `subject_id`                       | `agent_bytes=N\|user_bytes=M\n<N bytes><M bytes>` |
| `memory.agent_write`     | `subject_id\|target\|action\|data` | `ok\|chars=N\n` for writes, raw content for read |
| `memory.agent_curate`    | `subject_id\|ai_peer_alias`        | pipe-delim before/after summary |
| `memory.curator_status`  | (none)                             | pipe-delim `key=value` status |

`target` is `"agent"` or `"user"`. `action` is `add` /
`replace` / `remove` / `read`. Writes that would push the
target past its cap return `INVALID_ARGS` rather than silently
truncating — agents manage their own budgets.

The curator is a background loop on the memory node (opt-in
via `[memory.curator]`) that asks the AI peer to consolidate
redundant entries on a cadence. It NEVER wipes memory, invents
entries, or exceeds the cap — every failure path preserves
existing content.

Full design: [`agent-memory.md`](agent-memory.md).

## Bridge HTTP surface

Operator-facing reads + manual curator / embed triggers. Every
route is a thin proxy onto the memory capabilities above.

| Method | Path                            | Capability proxied |
|--------|---------------------------------|--------------------|
| GET    | `/v1/memory/agent`              | `memory.agent_read` |
| POST   | `/v1/memory/curate`             | `memory.agent_curate` |
| GET    | `/v1/memory/curator/status`     | `memory.curator_status` |
| POST   | `/v1/memory/embed`              | `memory.embed` |
| POST   | `/v1/memory/search`             | `memory.search` |
| POST   | `/v1/memory/embed_all`          | `memory.embed_all` |

Operator-side writes happen only through `memory.agent_curate`
(consolidation) — the dashboard never exposes a raw
`memory.agent_write` form. Agents own their own memory.

## Policy

The boot-script policy admits every memory capability for
`chat-users` by default. The exact `[[rules]]` names emitted
into `configs/policies/<run>.toml`:

```
mem_recent              memory.recent_for_session
mem_write               memory.write_turn
mem_search_turns        memory.search_turns
mem_search              memory.search
mem_embed               memory.embed
mem_embed_all           memory.embed_all
mem_agent_read          memory.agent_read
mem_agent_write         memory.agent_write
mem_agent_curate        memory.agent_curate
mem_curator_status      memory.curator_status
```

The two `memory.search` rules (`mem_search` and
`mem_semantic_search`) are both emitted; they're duplicates
that resolve to the same method.

## Dashboard

`#/memory` surfaces all three layers:

- **Persistent memory** — paste a `subject_id`, `read` to view
  the current agent + user content with char counts vs cap;
  `curate` to trigger consolidation.
- **Vector embeddings — semantic search** — pick target, type
  a query, hit search. Results rank by cosine score.
- **Embed all entries** — retrofit embeddings onto an existing
  flat-text memory (idempotent; skips chunks already embedded).
- **Curator status** — surface what the bridge knows about the
  scheduler (configured peer alias, the `bridge_note` field
  naming the in-process state the bridge can't currently
  reach).

Chat-turn search is not on the dashboard today — flows hit
`memory.search_turns` directly when they need it.

## CLI

```
relix-cli ops agent-memory --subject-id <hex>

relix-cli ops memory embed \
  --subject-id <hex> --target agent --text "..."

relix-cli ops memory search \
  --subject-id <hex> --target agent --query "..." --limit 5

relix-cli ops memory embed-all --subject-id <hex>
```

All three accept `--json` for the raw bridge payload.

## See also

- [`agent-memory.md`](agent-memory.md) — frozen-snapshot
  design, write semantics, curator invariants, exact storage
  schema for the text layer.
- [`vector-memory.md`](vector-memory.md) — embedding flow,
  cosine ranking, OpenAI-compatible providers, the linear-scan
  performance posture.
- [`configuration.md`](configuration.md) — `[memory]`,
  `[memory.embedding_peer]`, `[memory.curator]` TOML blocks.
