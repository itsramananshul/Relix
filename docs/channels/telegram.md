# Telegram channel

The Telegram channel turns every inbound Telegram message into a
chat-flow run on the Relix mesh and posts the agent's reply back to
the originating chat. It runs as its own controller process
(`node_type = "telegram"`) and dials the memory, AI, and coordinator
peers like any other mesh participant.

This document covers what the channel does, how to set it up, and the
operator-facing knobs (allowed users, approval notifications, slash
commands).

## What it does

For every inbound text message:

1. Derives a stable `subject_id` for the sender by hashing
   `telegram:<user_id>:<chat_id>` with blake3. Same user in the same
   chat is always the same subject across restarts.
2. Records the message in a bounded in-memory ring (default capacity
   200, configurable via `[telegram] messages_ring_capacity`) so the
   dashboard can render the recent-messages widget without touching
   the AI / memory peers.
3. Enforces the `allowed_users` permit list (empty list = allow
   everyone). Unauthorised callers get a static reply and no further
   dispatch happens.
4. Parses the leading slash command (see [Slash commands](#slash-commands)).
   Unrecognised commands are treated as plain chat.
5. For chat: emits a `typing` chat-action, reads the last 10 turns
   from `memory.recent_for_session`, dispatches `ai.chat` with the
   rendered history, persists both halves of the turn via
   `memory.write_turn`, posts the AI's reply, and flips the
   coordinator task to `completed`. If the AI peer is unreachable
   or returns empty the user gets the spec-mandated fallback:
   *"I'm having trouble reaching my brain right now. Please try
   again in a moment."*
6. When an `operator_chat_id` is configured a background notifier
   polls the coordinator every `approval_poll_interval_secs`
   (default 15 s) for tasks in `awaiting_input` and posts a
   one-line "Approval required" message (deduped in-memory across
   polls; dedup resets on restart).
7. Voice messages are routed to `tool.audio.transcribe` when the
   optional `[telegram.audio_peer]` section is configured. Without
   it the bot sends a static "voice transcription not configured"
   reply.

## Setup

### 1. Mint a bot token with @BotFather

Open Telegram and message [@BotFather](https://t.me/BotFather):

1. Send `/newbot`.
2. Pick a **display name** (this is what users see at the top of the
   chat — e.g. "My Relix Agent").
3. Pick a **username** that ends in `bot` and is globally unique on
   Telegram — e.g. `myrelix_dev_bot`.
4. BotFather replies with the bot's HTTP API token. It looks like
   `1234567:ABCDEFghijklmnop`. **Copy it now** — BotFather will not
   show it again without `/revoke`.

Optionally tighten the bot from BotFather:

- `/setprivacy` → **Disable** if you want the bot to read all group
  messages (it defaults to "Enabled", which means it only sees
  messages addressed to it via `@<username>` or replies). For the 1:1
  DM flow Relix ships in alpha you can leave it on.
- `/setjoingroups` → **Disable** if you want to keep the bot a
  pure-DM bot.

### 2. Decide where the token lives

The controller reads the token from an env var named in `[telegram]
token_env`. The default mesh script reads `RELIX_TELEGRAM_BOT_TOKEN`.
You have two options:

- **One-shot**: `export RELIX_TELEGRAM_BOT_TOKEN=…` before invoking
  the mesh script. Token lives only in the controller process; nothing
  is written to disk.
- **Durable**: store it in `bridge-secrets.toml` via the dashboard's
  Telegram settings page. The dashboard writes the token to disk at
  mode 0600 (gitignored). When you boot the mesh from a wrapper
  script that exports the saved token from `bridge-secrets.toml` into
  `RELIX_TELEGRAM_BOT_TOKEN`, the controller picks it up the same
  way.

Either way, the token never appears in a checked-in config.

### 3. Enable the telegram controller

Windows / PowerShell:

```powershell
$env:RELIX_TELEGRAM = "1"
$env:RELIX_TELEGRAM_BOT_TOKEN = "<your-bot-token>"
$env:RELIX_TELEGRAM_ALLOWED_USERS = "<your-numeric-user-id>"
relix boot --with-telegram
# or, equivalently:
# .\scripts\relix-mesh-up.ps1
```

When `RELIX_TELEGRAM = 1` the script:
- mints a `dev-keys/<run>-telegram.{key,bundle}` identity for the
  channel controller,
- writes `dev-data/<run>/telegram.toml` with default peer addresses
  pointing at memory / ai / coordinator,
- appends `[peers.telegram]` to the bridge's `peers.toml`,
- adds `telegram.status` + `telegram.messages_recent` to the shared
  policy file,
- starts the telegram controller alongside the other nodes.

The mesh banner reports the telegram port (default `tcp/19715`) and
whether the token env var is set. If the token is missing the
controller boots but its long-poll loop idles — the dashboard shows
`online=false`.

### 4. Verify

After boot:

```
GET http://127.0.0.1:19791/v1/telegram/status
```

Returns JSON like:

```json
{
  "peer": "telegram",
  "online": true,
  "username": "yourbot",
  "first_name": "Your Bot",
  "user_id": 1234567,
  "messages_seen": 0,
  "last_message_at": null
}
```

Open Telegram and send `/start` to your bot. You should see the
welcome message reply, and `GET /v1/telegram/messages/recent`
should return one row.

## Configuring allowed users

By default the channel accepts messages from everyone. Lock it down
by listing Telegram numeric user_ids in `RELIX_TELEGRAM_ALLOWED_USERS`
(comma-separated). The script writes them into
`[telegram] allowed_users` for you:

```powershell
$env:RELIX_TELEGRAM_ALLOWED_USERS = "42,1234567"
```

Or edit `dev-data/<run>/telegram.toml` directly:

```toml
[telegram]
allowed_users = [42, 1234567]
```

When the list is non-empty, any caller not on the list gets the
static reply:

> You are not authorized to use this bot.

The reply still records the inbound in the ring so the operator can
see who attempted to talk to the bot.

To find your own numeric user_id: message `@userinfobot` (or any
similar id bot) on Telegram, or read it out of
`GET /v1/telegram/messages/recent` after you send a test message.

## Approval notifications

Set `RELIX_TELEGRAM_OPERATOR_CHAT_ID` to a numeric chat_id (your
own DM or a group) to receive an approval-required notification any
time a task enters `awaiting_input`. The notifier polls every 15 s
by default (`approval_poll_interval_secs = 15`); tweak via the
TOML field.

The notification body:

```
⏳ Approval required
Task: <task_id>
Agent: <subject_short>
Action: <method>
Reason: <reason>
Reply /approve <task_id> or /reject <task_id>
```

The notification is delivered with an inline keyboard so the operator
can tap **✅ Approve** or **❌ Deny** directly in the chat. The
keyboard callback data uses `/approve <id>` and `/deny <id>` (Telegram
limits `callback_data` to 64 bytes). The notification text and the
slash-command parser both accept `/reject` as the rejection command.

`/approve` and `/reject` are operator-only — they are rejected with
"Approval commands are operator-only." when the caller's chat_id
doesn't match the configured `operator_chat_id`.

## Slash commands

The commands the controller actually understands (see
`crates/relix-runtime/src/nodes/telegram/commands.rs`):

| Command | What it does |
|---|---|
| `/start` | Welcome message explaining what the bot does. |
| `/help` | List the supported commands. |
| `/status` | Mesh-side summary: bot online state, messages seen, allow-list mode. |
| `/memory` | Show the caller's persistent agent + user memory blobs. |
| `/forget` | Clear the caller's agent + user memory. |
| `/approve <task_id>` | Mark an `awaiting_input` task as approved and append a chronicle event. Operator-only. |
| `/reject <task_id>` | Flip an `awaiting_input` task to rejected and append a chronicle event. Operator-only. |

Telegram appends `@<bot_username>` to slash commands sent in group
chats; the parser strips it transparently. Command heads are
case-insensitive (`/Help`, `/HELP` → `Help`). Unknown slash commands
fall through to the chat-flow path so they're not silently
swallowed.

## Delivery mode

The controller defaults to Telegram's `getUpdates` long-poll. To
use webhook mode instead:

```toml
[telegram]
mode        = "webhook"
webhook_url = "https://bot.example.com/v1/channels/telegram/webhook"
```

Both fields must be set — if `webhook_url` is absent or empty,
the controller falls back to long-poll regardless of `mode`
(`effective_mode()` logic). Webhook mode calls `setWebhook` at
startup. The bridge webhook route:

```
POST /v1/channels/telegram/webhook
```

validates the source IP against Telegram's CIDRs
(`149.154.160.0/20` and `91.108.4.0/22`) before processing the
update body.

## Session persistence

By default the channel stores `(chat_id, message_id) → task_id`
mappings in memory only; they are lost on restart.

To persist sessions across restarts:

```toml
[telegram]
session_db_path = "dev-data/telegram-sessions.sqlite"
```

This enables `SqliteSessionStore` — a SQLite WAL database with the
`telegram_sessions` table. A background TTL sweep runs every hour
(`DEFAULT_SWEEP_INTERVAL = 3600 s`) and removes sessions idle for
more than `session_ttl_hours` (default 24 h). Active sessions are
bumped on every lookup so they survive the sweep. When
`session_db_path` is omitted, `InMemorySessionStore` is used and
sessions are not swept.

## Voice messages

When `[telegram.audio_peer]` is configured, voice messages are
routed to `tool.audio.transcribe`:

```toml
[telegram.audio_peer]
addr         = "/ip4/127.0.0.1/tcp/19720"
alias        = "tool"
deadline_secs = 90
```

Without this section the bot sends a static reply explaining
that voice transcription is not configured.

## Health capability

`telegram.health` (FIX 49) returns a `ChannelHealthSnapshot` JSON
document. The health mode reported is `"long_poll"` (or `"webhook"`
when webhook mode is active).

## Security notes

- The bot token is the channel's single point of authentication. It
  is never logged, never echoed back via HTTP, and never serialised
  into the controller config. It only lives in the env var referenced
  by `token_env` (e.g. `RELIX_TELEGRAM_BOT_TOKEN`) and optionally
  `bridge-secrets.toml` (mode 0600, gitignored).
- `subject_id` derivation is **deterministic** but not authenticated:
  a Telegram account ID is the only thing the channel sees. Apply
  permit lists in adversarial settings; Telegram's accounts are not
  anti-Sybil.
- The channel writes `task.create` for every chat turn with
  `origin_surface = "telegram"` so the audit trail in the
  coordinator and dashboard knows which surface drove each task.
- Telegram updates older than the last processed `update_id` are
  not replayed across restarts (Telegram's offset semantics handle
  this). Updates that arrive during a restart are buffered by
  Telegram (24 h) and picked up on next poll.

## HTTP / CLI surfaces

Bridge endpoints:

```
GET  /v1/telegram/status
GET  /v1/telegram/messages/recent?limit=20
POST /v1/channels/telegram/webhook
```

CLI (one-shot snapshots):

```
relix-cli ops telegram status
relix-cli ops telegram messages --limit 50
```

## Reading the wire

The two read capabilities the bridge proxies:

- `telegram.status` arg `""`; returns
  `online=<bool>|username=<str>|first_name=<str>|user_id=<i64>|messages_seen=<u64>|last_message_at=<i64>\n`
  (`-1` for "no message yet").
- `telegram.messages_recent` arg `<limit>`; returns one tab-separated
  row per message newest-first:
  `<ts>\t<from_user_id>\t<from_username>\t<chat_id>\t<text_preview>\n`
  (preview truncated to 100 chars, tabs/newlines replaced with
  spaces).

Both bodies are stable across releases; the bridge JSON layer is the
recommended consumer (the wire format is documented here for SOL
flows that want to call the capabilities directly).

## See also

- [index.md](index.md) — overview of all four channels.
- [`../channel-node-architecture.md`](../channel-node-architecture.md) —
  the design contract.
- [`../current-limitations.md`](../current-limitations.md) — what
  alpha deliberately omits (group chats, multi-channel session
  unification).
- [`../configuration.md`](../configuration.md) — full env-var
  reference for the mesh boot script.
