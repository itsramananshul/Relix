# Slack channel

A `node_type = "slack"` controller polls a Slack channel for
inbound messages, runs each non-bot message through the
canonical chat flow (memory recent → ai.chat → memory write × 2),
and posts the reply back as a threaded reply. Same architecture
as Telegram + Discord; the three channels coexist on the mesh
as independent peers.

The bot uses Slack's Web API over HTTPS. **No Socket Mode.** The
controller calls `conversations.history` on a configurable cadence
and pulls messages newer than its last-seen `ts`. Simpler
operationally; the trade-off is a 2-second baseline cadence between
message arrival and visible reply.

Approval interactions arrive via the bridge webhook (see
[Approval interactions](#approval-interactions)).

## Setup

### 1. Create the Slack app

1. Go to <https://api.slack.com/apps> and click
   **Create New App → From scratch**.
2. Name the app (e.g. "Relix"), pick the target workspace, create.
3. On the app's **OAuth & Permissions** page, under **Bot Token
   Scopes**, add:
   - `channels:history` — read messages in public channels.
   - `chat:write` — post messages as the bot.
   - `im:history` — read direct-message channels (only if you
     target a `D…` channel).
   - `im:write` — open / write to DM channels.
   - `app_mentions:read` — receive `@app` mention events when the
     bot is mentioned in a channel.
4. (Optional, depending on target channel)
   - `groups:history` — read messages in private channels (target
     a `G…` channel).
   - `users:read` — populate `username` on inbound messages
     (Slack does not include display names on every event;
     `users.info` lookup needs this scope).
5. Scroll up, **Install to Workspace**, accept the permission
   prompt.
6. Copy the **Bot User OAuth Token** that Slack returns — it
   starts with `xoxb-…`. Treat it like an API key.

### 2. Find the target channel id

In the Slack client:

1. Right-click the channel → **View channel details**.
2. Bottom of the panel → **Channel ID** (a "Copy" button is
   provided).

Slack channel ids start with `C` (public), `G` (private), or `D`
(IM).

### 3. Invite the bot to the channel

In the target channel: `/invite @<your-bot-name>`. Without that,
`conversations.history` returns `not_in_channel` and the
controller stays offline.

### 4. Configure + boot

Three env vars before booting the mesh:

```
RELIX_SLACK=1
RELIX_SLACK_BOT_TOKEN=xoxb-...
RELIX_SLACK_CHANNEL_ID=C01234567
```

Optional:

```
RELIX_SLACK_OPERATOR_USER_ID=U01234567
RELIX_SLACK_ALLOWED_USERS=U01,U02      # comma-separated user_ids
```

Boot:

```powershell
relix boot --with-slack
# or, equivalently:
# .\scripts\relix-mesh-up.ps1
```

The mesh boot script (`scripts/relix-mesh-up.ps1`) reads these
and writes the controller config to `dev-data/<run>/slack.toml`:

```toml
[controller]
name        = "local-slack"
node_type   = "slack"
listen_port = 19717

[slack]
token_env              = "RELIX_SLACK_BOT_TOKEN"
channel_id             = "C01234567"
allowed_users          = []          # empty == allow everyone
operator_user_id       = ""          # reserved for future use
messages_ring_capacity = 200
poll_interval_secs     = 2

# Optional: enables historical-message filter (FIX 4).
# state_db_path = "dev-data/slack-state.sqlite"

[slack.memory_peer]
addr = "/ip4/127.0.0.1/tcp/19711"

[slack.ai_peer]
addr = "/ip4/127.0.0.1/tcp/19712"
deadline_secs = 60

[slack.coord_peer]
addr = "/ip4/127.0.0.1/tcp/19714"
```

The raw `xoxb-…` token never appears in any config file — only
the `token_env` indirection. Without `RELIX_SLACK_BOT_TOKEN` set,
the controller still boots but `auth.test` fails and the bot
stays offline (the dashboard reports `online=false`).

### 5. Verify

```
GET http://127.0.0.1:19791/v1/slack/status
```

returns the controller's view of `auth.test`. Send a message in
the channel from a non-bot account; the bot should reply within
`poll_interval_secs`, and `GET /v1/slack/messages/recent` should
list the inbound row.

## Polling model — no Socket Mode

The controller pulls messages with
`POST /api/conversations.history` on every tick. The first poll
returns up to 50 messages from the channel tail; each subsequent
poll passes `oldest=<last-seen-ts>` so Slack only returns new
arrivals. The cursor is in-memory only — a restart resumes from
"channel tail" rather than replaying.

### Historical-message filter (FIX 4)

On first boot a `SlackBotStartStore` records the current timestamp
so the controller can ignore messages that pre-date the bot. Set:

```toml
[slack]
state_db_path = "dev-data/slack-state.sqlite"
```

This creates an SQLite WAL database with the `slack_bot_start`
table:

```sql
CREATE TABLE IF NOT EXISTS slack_bot_start (
    channel_id TEXT PRIMARY KEY,
    bot_start_ts TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL
);
```

The first-write wins (`INSERT OR IGNORE`): the timestamp is never
overwritten on subsequent boots, so the filter is stable across
restarts. When `state_db_path` is omitted the filter is disabled
and the controller processes from the channel tail on every cold
start.

**Deliberate non-goals:**
- No Socket Mode WebSocket. The handshake + heartbeat thread is
  not built.
- No formal slash-command registration. The bot does not call
  `POST /api/apps.commands.list` or maintain a manifest. Slash
  commands are detected by content.
- No approval-notifier polling loop. `operator_user_id` is
  reserved for a future feature. The `slack.approval_send`
  capability and the bridge interaction webhook are fully wired.

## Slash commands

Detected by message content starting with `/`. No Slack slash
command registration is required — the bot inspects the message
body.

| Command | Behaviour |
|---|---|
| `/help` | List the available commands. |
| `/status` | Mesh health summary (bot identity + counters). |
| `/memory` | Show the caller's persistent agent + user memory blobs. |
| `/forget` | Wipe the caller's memory (both agent + user halves). |
| anything else | Treated as a chat message and routed to `ai.chat`. |

Slack replies are threaded under the inbound message's `ts`.

## Identity model

Slack users do not have Relix IdentityBundles. The channel mints
a derived subject per `(channel_id, user_id)` pair by hashing
`"slack:" + user_id + ":" + channel_id` with blake3 and using
the 32-byte result as the subject id. This subject is stamped
on every task the controller creates so operators can query
"all tasks for Slack user U01234567" later via the coordinator's
task list.

The hash is namespaced under `slack:` — distinct from `discord:`
and `telegram:` — so a Slack user and a Discord user with the
same numeric or string id never collide.

## Allowed users

When `allowed_users` is empty, every Slack user in the
configured channel can chat with the bot. When non-empty, only
listed user ids pass; everyone else gets:

    You are not authorized.

Inbound messages from blocked users still land in the bounded
ring (dashboard's recent-messages widget) so operators can audit
who tried to talk to the bot.

## Bot self-loop protection (defence in depth)

The SDK parses Slack `conversations.history` responses and
drops any message with `subtype` set (bot_message,
channel_join, …) or `bot_id` set, before the controller sees
it. After the first successful `auth.test`, the polling loop
adds a `user_id == bot.user_id` check as a second layer.

## Approval interactions

The `slack.approval_send` capability delivers approval-request
messages to the configured Slack channel as Block Kit messages with
**Approve** / **Deny** action buttons.

Inbound button clicks arrive via the bridge webhook:

```
POST /v1/channels/slack/interact
```

The bridge verifies Slack's HMAC-SHA256 request signature. Set:

```
RELIX_BRIDGE_SLACK_SIGNING_SECRET=<signing-secret>
```

If this env var is unset, `/v1/channels/slack/interact` and
`/v1/channels/slack/events` return 503. Signature verification:
basestring = `"v0:" + timestamp + ":" + raw-body`; the
`X-Slack-Signature` header must start with `"v0="`. Replay window
is 5 minutes (`MAX_SIGNATURE_AGE_SECS = 300`).

The bridge also accepts:

```
POST /v1/channels/slack/events
```

for URL verification challenges and future Events API event types
(currently fast-ack only, same HMAC gate).

When an approval decision is recorded, the original Block Kit
message is updated in-place via `chat.update` to reflect the
decision.

## Health capability

`slack.health` (FIX 49) returns a `ChannelHealthSnapshot` JSON
document. The health mode reported is `"polling"`.

## Wire shape

The controller exposes read-only mesh capabilities the bridge
proxies for the dashboard:

| Capability | Wire body |
|---|---|
| `slack.status` | `online=<bool>\|username=<str>\|user_id=<str>\|team_id=<str>\|channel_id=<str>\|messages_seen=<u64>\|last_message_at=<i64>\n` |
| `slack.messages_recent` | One tab-separated row per inbound (newest-first): `ts\tuser_id\tusername\tchannel_id\tcontent_preview\n` |

`content_preview` is truncated to 100 chars and stripped of
tabs/newlines.

## HTTP / CLI surfaces

Bridge endpoints:

```
GET  /v1/slack/status
GET  /v1/slack/messages/recent?limit=20
POST /v1/channels/slack/interact
POST /v1/channels/slack/events
```

CLI (one-shot snapshots):

```
relix-cli ops slack status
relix-cli ops slack messages --limit 50
```

Both support `--json` for raw payloads.

## Security notes

- The bot token is never echoed to logs or returned via HTTP.
  It lives only in the env var referenced by `token_env` in the
  TOML (e.g. `RELIX_SLACK_BOT_TOKEN`).
- The bridge has no Slack-specific HTTP authentication — same
  posture as every other surface: "local/dev only; put a
  reverse proxy with auth in front for production."
- Approval interactions require a valid HMAC-SHA256 signature
  (`RELIX_BRIDGE_SLACK_SIGNING_SECRET`); unsigned, malformed,
  or stale (> 5 min) requests are rejected before processing.
- The polling controller dials the memory / ai / coordinator
  peers with its own signed identity bundle (minted off the
  org root). Per-call admission (identity → policy → handler
  → audit) runs on every dispatch.
- Rate limit handling: 429 responses honour Slack's
  `Retry-After` header (integer seconds, clamped 1..30s). 5xx
  uses exponential backoff (1s, 2s, 4s — max 3 retries).
- **`ok=false` is treated as a client error** and is **not
  retried** — Slack returns HTTP 200 even on auth / scope /
  channel errors. The operator must fix the configuration.

## What about Slack's typing indicator?

Slack has no REST API for posting a typing indicator. The
`user_typing` event exists on the Events API / Socket Mode
streams (inbound only). The controller deliberately omits the
typing call rather than inventing one.

See [`../current-limitations.md`](../current-limitations.md) for
the alpha-wide list of deferred features.

## See also

- [index.md](index.md) — overview of all four channels.
- [`../channel-node-architecture.md`](../channel-node-architecture.md) —
  the design contract.
- [`../configuration.md`](../configuration.md) — full env-var
  reference for the mesh boot script.
