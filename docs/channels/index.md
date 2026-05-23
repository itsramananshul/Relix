# Channels

A **channel** in Relix is a peer process that bridges an external chat
platform (Telegram, Discord, Slack) to the same SOL chat flow the HTTP
bridge uses. Each channel is its own controller (`node_type = "telegram"`
/ `"discord"` / `"slack"`) with its own identity bundle, listens on its
own libp2p port, and dials the memory / ai / coordinator peers exactly
like every other mesh participant. The design contract these channels
share — task-first, async by default, derived per-user subject ids,
`allowed_users` admission, polling instead of websockets in the alpha —
is laid out in [`../channel-node-architecture.md`](../channel-node-architecture.md).

## The three channels

| Channel | Status | Required env vars | Default port | Doc |
|---|---|---|---|---|
| Telegram | alpha | `RELIX_TELEGRAM=1`, `RELIX_TELEGRAM_BOT_TOKEN` | tcp/19715 | [telegram.md](telegram.md) |
| Discord | alpha | `RELIX_DISCORD=1`, `RELIX_DISCORD_BOT_TOKEN`, `RELIX_DISCORD_CHANNEL_ID` | tcp/19716 | [discord.md](discord.md) |
| Slack | alpha | `RELIX_SLACK=1`, `RELIX_SLACK_BOT_TOKEN`, `RELIX_SLACK_CHANNEL_ID` | tcp/19717 | [slack.md](slack.md) |

Bridge endpoints (`/v1/<channel>/status`, `/v1/<channel>/messages/recent`)
all proxy the channel's read capabilities, so a single HTTP client can
talk to every channel uniformly.

## What every channel does

The pipeline is intentionally identical across all three:

1. **Poll** the platform's messaging endpoint (Telegram `getUpdates`,
   Discord `GET /channels/:id/messages`, Slack
   `POST /api/conversations.history`). No websockets, no public webhook,
   no Socket Mode in alpha.
2. **Derive a stable `subject_id`** by hashing
   `"<platform>:" + user_id + ":" + chat_id` with blake3. The subject is
   namespaced per platform so Telegram user `42` and Discord user `42`
   never collide.
3. **Admit through policy** — if `[<channel>] allowed_users` is
   non-empty, callers not on the list get a static "You are not
   authorized" reply and the message is dropped after recording it in
   the ring (so the operator can audit attempts).
4. **Forward to `ai.chat`** through the canonical SOL chat flow:
   `memory.recent_for_session` → `ai.chat` → two `memory.write_turn`
   calls (one for the user half, one for the agent half).
5. **Write to memory + reply.** The reply goes back to the originating
   chat — Telegram reply, Discord post, Slack threaded reply under the
   inbound `ts`.

Every turn also creates a coordinator task with
`origin_surface = "<channel>"` so the audit trail in
`/v1/tasks` and `relix-cli task get` lists channel-driven work
alongside HTTP-driven work.

## Operator knobs every channel exposes

- **`allowed_users`** — empty list means "allow everyone in the
  configured chat / channel"; a non-empty list locks the bot down to
  the listed user ids. Blocked users still land in the ring for audit.
- **`operator_*` ids** — `RELIX_TELEGRAM_OPERATOR_CHAT_ID` (Telegram's
  approval notifier currently uses this; Discord + Slack reserve
  `operator_user_id` for the same feature once it ships).
- **Bounded message ring** — every channel keeps the most recent 200
  inbound messages in-process (capacity is the
  `messages_ring_capacity` field on the `[<channel>]` config block).
  The ring is exposed via the mesh capability
  `<channel>.messages_recent` and surfaced by the bridge at
  `GET /v1/<channel>/messages/recent?limit=…` and by
  `relix-cli ops <channel> messages`.

## See also

- [telegram.md](telegram.md) — BotFather setup, slash commands,
  approval-notifier loop.
- [discord.md](discord.md) — Developer Portal walkthrough, Message
  Content intent, REST polling cadence.
- [slack.md](slack.md) — OAuth scopes, `xoxb-` bot token, `ok=false`
  error model.
- [`../channel-node-architecture.md`](../channel-node-architecture.md) —
  the design contract every channel implements: identity model,
  failure semantics, trust boundaries.
- [`../configuration.md`](../configuration.md) — full env-var
  reference for the mesh boot script.
- [`../current-limitations.md`](../current-limitations.md) — what the
  alpha deliberately does not support yet (group chats, voice / image
  uploads, Slack Socket Mode, Discord Gateway, multi-channel session
  unification).
