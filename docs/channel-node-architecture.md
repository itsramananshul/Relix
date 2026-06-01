# Channel Node Architecture

A **channel node** is a Relix peer whose job is to translate between
an asynchronous external messaging surface (Telegram, Discord, Slack,
email) and the mesh runtime. Channels are deliberately **task-first**:
every inbound message becomes a Task, every outbound reply is the
closing of a Task attempt (or a checkpoint event), and the operator
surfaces (`/v1/tasks`, `relix-cli task get`) treat channel-originated
work identically to HTTP-originated work.

This document is the **design contract** shared by all four channel
implementations.

## TL;DR

- **Channel = task source.** It does not orchestrate, does not plan,
  does not retry. It creates Tasks, marks attempt boundaries, and
  forwards results.
- **The controller calls `ai.chat` directly.** Each channel calls
  `memory.recent_for_session` (last 10 turns) → optional
  `routing.resolve` → `ai.chat` → two `memory.write_turn` calls.
  The `flow_template` config key exists but is reserved and not
  currently validated or wired.
- **Identity is per-channel.** Users do NOT get Relix
  IdentityBundles. They get a channel-scoped derived identity tied
  to their platform user id, and a policy gate that admits
  whichever Relix capability set the operator chooses.
- **Async by default.** Channels can deliver replies seconds,
  minutes, or hours after the inbound message (long-running work,
  awaiting_input). The channel keeps the chat thread → task_id
  mapping so it can re-find the conversation when a request
  finishes.

## Non-goals (deliberate)

- **Multi-channel session bridging** — a user on Telegram and the
  same user on the web bridge today get two unrelated sessions.
  Unifying them needs a "channel-linked identity" model; out of
  scope.
- **Group chats.** The first slice is 1:1 DM / single-channel only.
- **Streaming responses.** Telegram does not natively support token
  streaming. Reserved for a future iteration using repeated message
  edits.
- **No approval-notifier polling loop for Discord + Slack.**
  `operator_user_id` is reserved. The `approval_send` capability
  and bridge interaction webhooks are fully wired; a background loop
  that proactively polls for `awaiting_input` tasks (like Telegram's
  notifier) has not been built for these channels.

## Architecture

### Where the nodes live

```
   ┌──────────────────────────────────────────────────────────────┐
   │                  External messaging platforms                 │
   │   Telegram API   Discord API   Slack Web API   IMAP / SMTP   │
   └───────┬────────────────┬──────────────┬──────────────┬───────┘
           │                │              │              │
           ▼                ▼              ▼              ▼
   ┌────────────┐  ┌──────────────┐  ┌──────────┐  ┌──────────┐
   │  relix-    │  │  relix-      │  │  relix-  │  │  relix-  │
   │  telegram  │  │  discord     │  │  slack   │  │  email   │
   │  (ch node) │  │  (ch node)   │  │ (ch node)│  │ (ch node)│
   └─────┬──────┘  └──────┬───────┘  └────┬─────┘  └────┬─────┘
         │                │               │              │
         └────────────────┴───────────────┴──────────────┘
                          │ libp2p (Noise XK + Yamux)
                          │ same admission pipeline as every other peer
                          ▼
                ┌──────────────────┐
                │   relix mesh     │
                │  coordinator,    │
                │  ai, memory,     │
                │  tool, ...       │
                └──────────────────┘
```

Each channel crate is a **controller** like every other node — same
identity bundle, same policy file, same audit log. The
platform-specific surface (HTTPS to the platform API, the
channel-side session storage) is entirely on one side; the other
side speaks libp2p like any other Relix peer.

### Process boundary

Each channel controller runs as its own OS process started by the
bringup script (next to `relix-controller`-spawned `memory` / `ai`
/ `tool` peers). They do NOT live inside the bridge. Reasons:

1. **Different failure modes.** A platform API outage should not
   take HTTP chat requests down.
2. **Different identity needs.** The bridge's identity is "operator
   surface"; each channel's is "channel ingestor" — these get
   different policy admissions.
3. **Different lifecycle.** Bridge is request/response; channels
   are long-poll, IMAP IDLE, or webhook.

### What the channel does on each inbound message

```
1. Receive platform update (long-poll, IMAP IDLE, or webhook handler).
2. Record in bounded ring + state (messages_seen, last_message_at).
3. Derive / look up ChannelSubject from (channel_id, user_id).
4. Permit-list check (allowed_users / allowed_senders). Blocked
   callers get a static error reply; the ring entry is still recorded.
5. Parse slash command (if applicable).
6. For chat messages:
       a. Emit typing indicator (Telegram only).
       b. memory.recent_for_session(session_id, 10)
       c. Optional: routing.resolve (coordinator decision)
       d. ai.chat(session_id, history)
       e. memory.write_turn × 2 (user + assistant halves)
       f. Deliver reply to the originating surface.
7. task.create(origin_surface=<channel>)
   task.event("task.<channel>.inbound")
   task.update(status=completed|failed)
```

### Async outbound delivery

Because the ai.chat call can take tens of seconds (tool calls,
coordinator approval gates, etc.), the channel does NOT block the
inbound update handler. The handler spawns a tokio task that runs
the entire pipeline and delivers the reply when it finishes. The
inbound poll loop (or webhook server) returns immediately.

All RPC calls — memory, ai, coordinator — are best-effort. A
failure at any step logs WARN, the pipeline degrades gracefully
(empty history, static fallback reply), and the task is marked
failed. Send failures do not panic.

## Identity model

Platform users do not have Relix IdentityBundles. Each channel
creates a **derived subject** per (channel, user) pair:

| Channel | Hash input | Key types |
|---|---|---|
| telegram | `"telegram:{user_id}:{chat_id}"` | both i64 |
| discord | `"discord:{user_id}:{channel_id}"` | both &str |
| slack | `"slack:{user_id}:{channel_id}"` | both &str |
| email | derived from RFC 5322 threading headers | n/a |

All three chat channels use blake3 on the namespaced string. Email
derives the session from `email-thread:<References[0]>` →
`email-thread:<In-Reply-To>` → `email-thread:<Message-ID>`.
Namespacing means a Discord user and a Telegram user with the same
numeric id never collide.

The channel acts as a Relix peer that says "I am facilitating a
request for user X." The coordinator audit log records
`caller_subject_id = <channel's bundle subject>` and the channel
sets `owner_subject_id` on `task.create` to the derived per-user
subject. This is the same trust model as the bridge.

## Configuration shape

Every channel TOML shares a `[controller]` header plus a
channel-specific section. Full per-channel config references are in
the individual channel docs. Key fields common to all chat channels:

| Field | Default | Notes |
|---|---|---|
| `token_env` | required | Env var name holding the bot token. |
| `allowed_users` / `allowed_senders` | `[]` | Empty = allow everyone. |
| `messages_ring_capacity` | `200` | Bounded in-memory ring for dashboard. |
| `poll_interval_secs` | `2` (Discord/Slack), `1` (Telegram) | REST poll cadence. |
| `state_db_path` | absent | Optional SQLite for persistent state (FIX 2, FIX 4). |
| `[<channel>.memory_peer]` | required | `addr`, `alias`, `deadline_secs`. |
| `[<channel>.ai_peer]` | required | `deadline_secs` default 60 s. |
| `[<channel>.coord_peer]` | required | `deadline_secs` default 10 s. |

The `[controller]` section is shared with every other controller
TOML; the bringup script generates it the same way it generates
`memory.toml` / `ai.toml` / etc.

## Capabilities the channel node consumes

Every channel consumes:
- `task.create` — mint the per-message task.
- `task.update` — open + close the attempt, set terminal status.
- `task.event` — inbound event tagging.
- `memory.recent_for_session` / `memory.write_turn` — history reads/writes.
- `routing.resolve` — optional routing decision.
- `ai.chat` / `dispatch_chat` — model invocation.

Additional per-channel capabilities (registered by the channel as a
**server**, not a consumer):

| Capability | Direction | Present on |
|---|---|---|
| `<ch>.status` | read-only | all four |
| `<ch>.messages_recent` | read-only | all four |
| `<ch>.send` | mutating | all four |
| `<ch>.approval_send` | mutating | all four (PART 8) |
| `<ch>.health` | read-only | Telegram, Discord, Slack (FIX 49) |
| `telegram.webhook_update` | mutating | Telegram only (FIX 1) |
| `email.send_template` | mutating | Email only |

## Persistent state per channel

| Channel | Store | SQLite table | Opt-in config |
|---|---|---|---|
| Discord | `DiscordWatermarkStore` | `discord_watermarks` | `state_db_path` |
| Slack | `SlackBotStartStore` | `slack_bot_start` | `state_db_path` |
| Telegram | `SqliteSessionStore` | `telegram_sessions` | `session_db_path` |
| Email | none | — | — |

All SQLite stores use WAL mode, `synchronous=NORMAL`,
`busy_timeout=5000 ms`. The Telegram session store runs a TTL sweep
every `DEFAULT_SWEEP_INTERVAL = 3600 s`; sessions idle longer than
`session_ttl_hours` (default 24 h) are removed. Active sessions are
bumped on every lookup to survive the sweep.

## Message formatting

The shared `channels.rs` module provides formatting helpers used by
all three chat channels:

| Function | Purpose |
|---|---|
| `format_for_discord(text)` | Splits at `DISCORD_MAX_MESSAGE_LEN = 1900` chars. Only the first chunk threads as a reply; subsequent chunks are posted standalone. |
| `format_for_telegram_markdown_v2(text)` | Single-pass MarkdownV2 escaping with fenced-code-block awareness. Does **not** preserve inline markdown (`*bold*`, `_italic_`) — those chars are escaped. |
| `format_for_slack_mrkdwn(text)` | Converts `**bold**` → `*bold*`, strips code-fence language hints. |
| `format_for_slack_blocks(text)` | Flat `section` Block Kit array only (no headers, dividers, or action buttons). |
| `split_at_boundary(text, max_chars)` | Paragraph > sentence > line > space > hard cut. |

## Failure semantics

All RPC calls (memory, ai, coordinator) are best-effort. Failures
log WARN; the pipeline degrades gracefully:

| Failure | Outcome |
|---|---|
| Platform API outage on inbound | Channel logs WARN; polling resumes when API recovers. Telegram queues undelivered updates for 24 h. |
| Permit-list rejection | Static "You are not authorized" reply sent; inbound still recorded in ring. |
| `memory.recent_for_session` fails | History is empty; ai.chat proceeds with no prior context. |
| `ai.chat` fails or returns empty | User receives: "I'm having trouble reaching my brain right now. Please try again in a moment." |
| Send failure (Discord) | Handler returns; ring entry still recorded; task marked failed. |
| Outbound Telegram failure | 3 retries with exponential backoff; gives up after transient exhaustion. |

## What this design protects against

- **No autonomous retries of mutations.** The channel does not
  loop on failures. Operator-initiated retry only.
- **No bypass of the admission pipeline.** Channel calls
  capabilities exactly as the bridge does — every call goes
  through identity → policy → handler → audit.
- **No token leakage.** Bot tokens and SMTP/IMAP passwords live
  in env vars or a secret manager, never in a config file. The
  channel logs the env var name but never the value.
- **No self-loop.** Discord and Slack drop bot-authored messages
  at the parse layer (structural defence). Telegram drops updates
  with neither `text` nor `voice_file_id`.

## Open questions (deferred)

- **Multi-channel session unification** — a user on both Telegram
  and the web bridge today gets two unrelated sessions. A
  "channel-linked identity" model is out of scope.
- **Streaming responses** — Telegram does not natively support
  streaming; a streaming mode would need repeated message edits.
  Reserved for a future iteration.
- **Group chats** — multi-user group session management and
  `@mention` routing are out of scope for the first slice.
- **Discord + Slack approval-notifier polling loop** — the proactive
  "post when a task enters awaiting_input" pattern (already
  implemented for Telegram) is reserved for Discord and Slack.

## Trust boundary summary

| Trust dimension | Web bridge | Channel nodes |
|---|---|---|
| Inbound auth | None (operator's reverse proxy) | Platform TLS + bot token / credentials |
| Identity mapped to subject | bridge bundle | derived per-user (namespaced blake3) |
| Inbound signature verification | n/a | Discord: Ed25519; Slack: HMAC-SHA256; Telegram webhook: CIDR guard; Email: optional Mailgun HMAC |
| Per-user rate limit | No (proxy-level) | Yes (channel config) |
| Admission pipeline | Yes | Yes |
| Auto-retry | No | No |
| Orchestration | No (direct ai.chat) | No (direct ai.chat) |
| Persistence ownership | Coordinator | Coordinator (+ optional channel-local SQLite) |

## Code organisation

Each channel client crate (`relix-telegram`, `relix-discord`,
`relix-slack`) contains:

```
src/
  lib.rs           # pub trait (BotApi / DiscordApi / SlackApi)
  config.rs        # crate-level config struct (bot_token_env, channel_id, …)
  live.rs          # reqwest-backed live implementation
  mock.rs          # in-memory mock for tests
  messages.rs      # IncomingMessage, OutgoingMessage, ParseMode, …
  identity.rs      # derive_channel_subject (blake3 namespaced hash)
  approval.rs      # SingleChannelDispatch impl + signature verification
  session_store.rs # (telegram only) InMemorySessionStore + SqliteSessionStore
```

The runtime controller lives in
`crates/relix-runtime/src/nodes/<channel>/` with modules:

```
config.rs      # runtime-layer TOML config (token_env, peers, rings, …)
state.rs       # ChannelState + ChannelHealth (FIX 49)
ring.rs        # MessageRing (bounded inbound ring)
controller.rs  # polling/webhook loop, per-message handler
client.rs      # outbound mesh RPC (memory, ai, coordinator calls)
commands.rs    # slash-command parser + static reply strings
mod.rs         # capability registration + handler dispatch
```

Email runtime lives in
`crates/relix-runtime/src/nodes/email/` with `config.rs`,
`state.rs`, `ring.rs`, `controller.rs`, `client.rs`, `commands.rs`,
`smtp.rs`, `imap.rs`, `dkim.rs`, and `mod.rs`.

## See also

- [`docs/coordination.md`](coordination.md) — the Task ledger this
  channel writes into.
- [`docs/task-runtime.md`](task-runtime.md) — wire format for the
  `task.*` capabilities the channel consumes.
- [`docs/runtime-lifecycle.md`](runtime-lifecycle.md) — what
  status transitions the channel drives.
- [`docs/attempt-lineage.md`](attempt-lineage.md) — per-attempt
  rows; channels participate in the same lineage as bridge
  requests.
- [`docs/security.md`](security.md) — the admission pipeline the
  channel goes through on every call.
