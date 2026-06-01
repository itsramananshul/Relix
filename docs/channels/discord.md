# Discord channel

A `node_type = "discord"` controller polls a Discord channel for
inbound messages, runs each non-bot message through the canonical
chat flow (memory recent → ai.chat → memory write × 2), and posts
the reply back. Same architecture as the Telegram channel; the
two coexist on the mesh as independent peers.

The bot is read-only on the Discord side beyond the messages it
sends. There is no Gateway/WebSocket handshake — the controller
uses REST polling against
`GET /channels/:channel_id/messages?after=:last_id`. Simpler
operationally; the trade-off is a 2-second cadence (configurable)
between message arrival and visible reply.

## Setup

### 1. Create the Discord application

1. Go to the **Discord Developer Portal**:
   <https://discord.com/developers/applications>.
2. Click **New Application**, give it a name (e.g. "Relix"), accept
   the developer terms.
3. In the left sidebar choose **Bot**. The portal mints a bot user
   for the application automatically.
4. Click **Reset Token** (or **Copy** if it's a fresh app) to get
   the bot token. **Copy it now** — Discord only shows it once.
   Treat it like an API key.
5. **Privileged Gateway Intents**: on the same Bot page enable
   **Message Content Intent**. Without it Discord returns empty
   `content` strings to the REST poll and the controller will see
   nothing usable.

### 2. Find the target channel id

In the Discord client:

1. **User Settings → Advanced → Developer Mode** → On.
2. Right-click the channel you want the bot to listen on →
   **Copy Channel ID**. Discord snowflakes are 17–19 digits today
   and are strings end-to-end (they exceed the JS safe-int range).

### 3. Invite the bot to the channel

In the Developer Portal:

1. **OAuth2 → URL Generator**, tick scope **`bot`**.
2. Under **Bot Permissions** tick **View Channel**, **Send
   Messages**, **Read Message History**.
3. Copy the generated install URL, paste it in a browser logged into
   the target server, choose the server, authorise.

### 4. Configure + boot

Three env vars before booting the mesh:

```
RELIX_DISCORD=1
RELIX_DISCORD_BOT_TOKEN=<bot-token>
RELIX_DISCORD_CHANNEL_ID=<channel-snowflake>
```

Optional:

```
RELIX_DISCORD_OPERATOR_USER_ID=<discord-user-id>
RELIX_DISCORD_ALLOWED_USERS=42,1234       # comma-separated user_ids
```

Boot:

```powershell
relix boot --with-discord
# or, equivalently:
# .\scripts\relix-mesh-up.ps1
```

The mesh boot script (`scripts/relix-mesh-up.ps1`) reads these
and writes the controller config to
`dev-data/<run>/discord.toml`:

```toml
[controller]
name        = "local-discord"
node_type   = "discord"
listen_port = 19716

[discord]
token_env              = "RELIX_DISCORD_BOT_TOKEN"
channel_id             = "12345678901234567"
allowed_users          = []          # empty == allow everyone
operator_user_id       = ""          # reserved for future use
messages_ring_capacity = 200
poll_interval_secs     = 2

# Optional: enables persistent polling cursor (FIX 2).
# state_db_path = "dev-data/discord-state.sqlite"

[discord.memory_peer]
addr = "/ip4/127.0.0.1/tcp/19711"

[discord.ai_peer]
addr = "/ip4/127.0.0.1/tcp/19712"
deadline_secs = 60

[discord.coord_peer]
addr = "/ip4/127.0.0.1/tcp/19714"
```

The raw token never appears in any config file — only the
`token_env` indirection. Without `RELIX_DISCORD_BOT_TOKEN` set,
the controller still boots but `get_me` fails and the bot stays
offline (the dashboard reports `online=false`).

### 5. Verify

```
GET http://127.0.0.1:19791/v1/discord/status
```

returns the controller's view of `get_me`. Send a message in the
channel from a non-bot account; the bot should reply within
`poll_interval_secs`, and `GET /v1/discord/messages/recent` should
list the inbound row.

## Polling cursor and persistence (FIX 2)

On first boot, the controller fetches the most recent message id
(`?limit=1`) to seed the polling watermark — this prevents replaying
channel history on startup. Subsequent polls use
`GET /channels/:id/messages?after=:last_id`.

By default the cursor is in-memory only and is lost on restart. To
persist it across restarts, set:

```toml
[discord]
state_db_path = "dev-data/discord-state.sqlite"
```

This enables the `DiscordWatermarkStore` — a SQLite WAL database with
the schema:

```sql
CREATE TABLE IF NOT EXISTS discord_watermarks (
    channel_id TEXT PRIMARY KEY,
    last_message_id TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
```

With `state_db_path` configured, a restart resumes from the last
persisted watermark rather than seeding from scratch.

## Slash commands

Commands are detected by message content starting with `/`. They
do **not** use Discord's registered slash command system — that
would need a separate global registration step against the API.
This implementation just inspects the message body.

| Command | Behaviour |
|---|---|
| `/help` | List the available commands. |
| `/status` | Mesh health summary (bot identity + counters). |
| `/memory` | Show the caller's persistent agent + user memory blobs. |
| `/forget` | Wipe the caller's memory (both agent + user halves). |
| anything else | Treated as a chat message and routed to `ai.chat`. |

## Identity model

Discord users do not have Relix IdentityBundles. The channel
mints a derived subject per `(channel_id, user_id)` pair by
hashing `"discord:" + user_id + ":" + channel_id` with blake3
and using the 32-byte result as the subject id. This subject
is stamped on every task the controller creates so operators
can query "all tasks for Discord user X" later via the
coordinator's task list.

The hash is namespaced under `discord:` — distinct from
Telegram's `telegram:` namespace — so a Discord user and a
Telegram user with the same numeric id never collide.

## Allowed users

When `allowed_users` is empty, every Discord user in the
configured channel can chat with the bot. When non-empty, only
listed user ids pass; everyone else gets:

    You are not authorized.

Inbound messages from blocked users still land in the bounded
ring (the dashboard's recent-messages widget) so operators can
audit who tried to talk to the bot.

## Bot self-loop protection

After the controller's first successful `get_me`, every poll
filters out messages whose `author.bot` flag is set. The
`author.id == bot_identity.user_id` guard is applied as a
second layer. This two-layer defence means self-loop protection
is structural — no caller code can forget to check the flag.

## Approval interactions

The `discord.approval_send` capability delivers approval-request
messages to the configured Discord channel with **Approve** /
**Deny** buttons built from Discord's Message Component API.

Inbound button clicks arrive via the bridge webhook:

```
POST /v1/channels/discord/interact
```

The bridge verifies the Ed25519 signature Discord signs every
interaction with. Set:

```
RELIX_BRIDGE_DISCORD_PUBLIC_KEY=<hex-encoded-ed25519-public-key>
```

If this env var is unset the `/v1/channels/discord/interact` endpoint
returns 503. The signed bytes are `timestamp_header_bytes ++
body_bytes`; `verify_strict` (canonical encoding, no malleability) is
used. Interaction types handled:

- Type 1 (PING) → responds with `{"type":1}` (PONG).
- Type 3 (MESSAGE_COMPONENT) → parses the custom_id
  (`approve:<id>` / `deny:<id>`), records the decision, updates
  the approval message with an ephemeral ack.

The bridge also accepts:

```
POST /v1/channels/discord/events
```

for future Events API integration (currently fast-ack only).

## Health capability

`discord.health` (FIX 49) returns a `ChannelHealthSnapshot` JSON
document. The health mode reported is `"polling"`.

## Wire shape

The controller exposes read-only mesh capabilities the bridge proxies:

| Capability | Wire body |
|---|---|
| `discord.status` | `online=<bool>\|username=<str>\|user_id=<str>\|channel_id=<str>\|messages_seen=<u64>\|last_message_at=<i64>\n` |
| `discord.messages_recent` | One tab-separated row per inbound (newest-first): `ts\tuser_id\tusername\tchannel_id\tcontent_preview\n` |

`content_preview` is truncated to 100 chars and stripped of
tabs/newlines so each row stays parseable.

Outbound messages are split at **1900 characters**
(`DISCORD_MAX_MESSAGE_LEN`). Discord's API limit is 2000; the 100-char
margin prevents issues with whitespace and encoding edge cases. Only
the first chunk threads as a reply under the original user message;
subsequent chunks are posted standalone.

## HTTP / CLI surfaces

Bridge endpoints:

```
GET  /v1/discord/status
GET  /v1/discord/messages/recent?limit=20
POST /v1/channels/discord/interact
POST /v1/channels/discord/events
```

CLI (one-shot snapshots):

```
relix-cli ops discord status
relix-cli ops discord messages --limit 50
```

Both support `--json` for raw payloads.

## Security notes

- The bot token is never echoed to logs or returned via HTTP.
  It lives only in the env var referenced by `token_env` in the
  TOML (e.g. `RELIX_DISCORD_BOT_TOKEN`).
- The bridge has no Discord-specific HTTP authentication — same
  posture as every other surface, "local/dev only; put a reverse
  proxy with auth in front for production."
- Approval interactions require a valid Ed25519 signature
  (`RELIX_BRIDGE_DISCORD_PUBLIC_KEY`); unsigned or malformed
  requests are rejected with 401.
- The polling controller dials the memory / ai / coordinator
  peers with its own signed identity bundle (minted off the
  org root). Per-call admission (identity → policy → handler →
  audit) runs on every dispatch — Discord traffic enjoys no
  bypass.
- Rate limit handling: 429 responses honour Discord's
  `retry_after` (a float in seconds, ceiling to integer, clamped
  1..30s). 5xx uses exponential backoff (1s, 2s, 4s — max 3
  retries). Other 4xx is never retried (config / permissions
  problem).

## Non-goals (deliberately)

- **No Gateway/WebSocket client.** The spec asks for REST
  polling to keep operations simple — no `READY` handshake, no
  heartbeat thread, no resume.
- **No formal slash command registration.** The bot does not
  call `POST /applications/:app_id/commands`. Content-detection
  works without operator action.
- **No approval-notifier polling loop.** The `discord.approval_send`
  capability and the bridge interaction webhook are fully wired.
  A background loop that polls the coordinator for `awaiting_input`
  tasks and pro-actively posts notifications (like Telegram's
  notifier) has not yet been built; `operator_user_id` is reserved
  for that feature.

See [`../current-limitations.md`](../current-limitations.md) for
the alpha-wide list of deferred features.

## See also

- [index.md](index.md) — overview of all four channels.
- [`../channel-node-architecture.md`](../channel-node-architecture.md) —
  the design contract.
- [`../configuration.md`](../configuration.md) — full env-var
  reference for the mesh boot script.
