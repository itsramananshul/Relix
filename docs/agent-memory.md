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

No new config — the memory node grows the table automatically
on first start. Existing `memory.write_turn` / `memory.search`
behavior is unchanged.

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

## What's deliberately NOT here

- **Vector embeddings.** Memory is keyword text. Future waves.
- **Cross-agent shared memory.** Each `subject_id` owns its row.
- **Per-session scoping.** Memory is global per-agent across all
  sessions.
- **Per-team / department scoping.** No grouping today.
- **Auto-eviction.** Agents must remove old entries themselves.
- **Search across memory.** No FTS index on the new table —
  `memory.search` only covers chat-turn `turns`.
- **Operator-side editing.** Dashboard + CLI are READ-ONLY.

When these become real needs (and they will) they land as
follow-up tracks on top of the foundation here.
