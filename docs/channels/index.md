# Channels

A **channel** in Relix is a peer process that bridges an external
messaging platform (Telegram, Discord, Slack, or email) to the mesh.
Each channel is its own controller (`node_type = "telegram"` /
`"discord"` / `"slack"` / `"email"`) with its own identity bundle,
listens on its own libp2p port, and dials the memory / ai / coordinator
peers exactly like every other mesh participant. The design contract
these channels share — task-first, async by default, derived per-user
subject ids, admission lists, polling instead of websockets in the
alpha — is laid out in
[`../channel-node-architecture.md`](../channel-node-architecture.md).

## The four channels

| Channel | Status | Required env vars | Default port | Doc |
|---|---|---|---|---|
| Telegram | alpha | `RELIX_TELEGRAM=1`, `RELIX_TELEGRAM_BOT_TOKEN` | tcp/19715 | [telegram.md](telegram.md) |
| Discord | alpha | `RELIX_DISCORD=1`, `RELIX_DISCORD_BOT_TOKEN`, `RELIX_DISCORD_CHANNEL_ID` | tcp/19716 | [discord.md](discord.md) |
| Slack | alpha | `RELIX_SLACK=1`, `RELIX_SLACK_BOT_TOKEN`, `RELIX_SLACK_CHANNEL_ID` | tcp/19717 | [slack.md](slack.md) |
| Email | alpha | `RELIX_EMAIL=1`, SMTP + IMAP credentials via env | tcp/19718 | [email.md](email.md) |

The env var names shown above are conventions used by the mesh boot
script. The actual names are whatever the operator writes in the
`token_env` / `smtp_password_env` / etc. fields of the channel's TOML
— the code only ever reads those named env vars.

Bridge endpoints (`/v1/<channel>/status`, `/v1/<channel>/messages/recent`)
all proxy the channel's read capabilities, so a single HTTP client can
talk to every channel uniformly.

## What every channel does

The pipeline is intentionally identical across all four:

1. **Receive inbound content** — Telegram and Discord poll the
   platform's REST endpoint; Slack polls `conversations.history`; the
   email channel uses IMAP IDLE (with poll fallback). Webhook mode is
   also available for Telegram (`mode = "webhook"` + `webhook_url`
   set) and for inbound email replies via the bridge route
   `/v1/channels/email/reply`.
2. **Derive a stable `subject_id`** by hashing a namespaced string
   with blake3. Chat channels hash
   `"<platform>:" + user_id + ":" + chat_id`; the email channel
   derives the session from RFC 5322 threading headers
   (`email-thread:<References[0]>` → `email-thread:<In-Reply-To>` →
   `email-thread:<Message-ID>`).
3. **Admit through policy** — if the channel's admission list
   (`allowed_users` / `allowed_senders`) is non-empty, callers not on
   the list get a static "You are not authorized" reply. The message
   is still recorded in the ring so the operator can audit attempts.
4. **Forward to `ai.chat` directly.** The controller calls
   `memory.recent_for_session` (last 10 turns) → `routing.resolve`
   (optional coordinator decision) → `ai.chat` → two
   `memory.write_turn` calls. There is no intermediate SOL flow for
   the chat path; the `flow_template` config key exists but is
   reserved and not currently validated or wired.
5. **Send the reply** back to the originating surface — Telegram
   reply, Discord post, Slack `chat.postMessage`, SMTP send.

Every turn also creates a coordinator task with
`origin_surface = "<channel>"` so the audit trail in `/v1/tasks` and
`relix-cli task get` lists channel-driven work alongside HTTP-driven
work.

## Operator knobs every channel exposes

- **Admission list** — `allowed_users` (chat channels) or
  `allowed_senders` (email). Empty list means "allow everyone in the
  configured chat / mailbox."
- **`operator_*` ids** — `operator_chat_id` for Telegram's
  approval-notifier loop; `operator_user_id` is reserved for
  Discord + Slack; `operator_address` is reserved for email.
- **`<channel>.health`** — read-only health capability (FIX 49)
  exposed by Telegram, Discord, and Slack. Telegram reports mode
  `"long_poll"`; Discord and Slack report `"polling"`.
- **`<channel>.approval_send`** — mutating capability on all four
  channels that accepts `ApprovalSendArgs` JSON and delegates to the
  platform-specific dispatch type.
- **Bounded message ring** — every channel keeps the most recent 200
  inbound messages in-process (capacity is the
  `messages_ring_capacity` field on the `[<channel>]` config block).
  Exposed via `<channel>.messages_recent` and surfaced by the bridge
  at `GET /v1/<channel>/messages/recent?limit=…`.

## Scheduled summary reports (`[reports]`)

All three chat channels (Telegram, Discord, Slack) can be wired to a
scheduled summary reporter. Add a `[reports]` block to the runtime
config:

```toml
[reports]
enabled  = true
schedule = "0 9 * * *"   # 5-field cron, or a duration shorthand like "30m"
channels = ["telegram", "discord", "slack"]
```

The reporter wakes every minute and checks whether any channel is
due. When the schedule fires:

- It walks the task store (up to 5,000 tasks per assembly) to build a
  `SummaryReport` — tasks completed/failed, average duration,
  most-active agent, memory items added, alerts.
- It renders the report in each channel's native format
  (Telegram MarkdownV2, Discord markdown, Slack mrkdwn) and dispatches
  concurrently. A failure on one channel does not block the others.
- Missed ticks are **not** replayed.

Known limitation: `cost_cents` and `memory_items_added` are always
`0` (billing table and memory hop not yet wired). An alert is included
in the report if the task walk hits the 5,000-task budget.

## See also

- [telegram.md](telegram.md) — BotFather setup, slash commands,
  approval-notifier loop, webhook mode, voice transcription.
- [discord.md](discord.md) — Developer Portal walkthrough, Message
  Content intent, REST polling, persistent watermark store, Ed25519
  interaction verification.
- [slack.md](slack.md) — OAuth scopes, `xoxb-` bot token, `ok=false`
  error model, historical-message filter, HMAC request signing.
- [email.md](email.md) — SMTP/IMAP setup, DKIM, templates,
  `email.send` / `email.send_template`.
- [`../channel-node-architecture.md`](../channel-node-architecture.md) —
  the design contract every channel implements: identity model,
  failure semantics, trust boundaries.
- [`../configuration.md`](../configuration.md) — full env-var
  reference for the mesh boot script.
- [`../current-limitations.md`](../current-limitations.md) — what the
  alpha deliberately does not support yet.
