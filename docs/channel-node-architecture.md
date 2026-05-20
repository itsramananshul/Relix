# Channel Node Architecture

A **channel node** is a Relix peer whose job is to translate between
an asynchronous external messaging surface (Telegram chat, SMS,
email, IRC, …) and the Task-native runtime. Channels are
deliberately **task-first**: every inbound message becomes a Task,
every outbound reply is the closing of a Task attempt (or a
checkpoint event), and the operator surfaces (`/v1/tasks`,
`relix-cli task get`) treat channel-originated work identically to
HTTP-originated work.

This document is the **design contract**. Telegram is the first
channel and the rest of this doc names it concretely, but the same
shape applies to future channels.

## TL;DR

- **Channel = task source.** It does not orchestrate, does not plan,
  does not retry. It creates Tasks, marks attempt boundaries, and
  forwards results.
- **SOL still owns orchestration.** The channel hands the
  task_id + raw user input to a SOL flow (`flows/channel_*.sol`)
  via the same FlowRunner path the bridge uses. The flow decides
  what to call.
- **Identity is per-channel.** Telegram users do NOT get Relix
  IdentityBundles. They get a channel-scoped derived identity tied
  to their Telegram user id, and a policy gate that admits
  whichever Relix capability set the operator chooses.
- **Async by default.** Channels can deliver replies seconds,
  minutes, or hours after the inbound message (long-running flows,
  awaiting_input, retries). The channel keeps the chat thread →
  task_id mapping so it can re-find the conversation when a flow
  finishes.

## Non-goals (deliberate)

This phase ships the **architecture and scaffold**, not a working
bot. Specifically NOT in scope tonight:

- Sending a token-authenticated message over the actual Telegram
  Bot API (requires credentials — see
  `docs/internal/nightly-blockers/`).
- Long-poll vs webhook delivery model decision (both are valid;
  the architecture supports either).
- Multi-channel session bridging (one user on Telegram + the same
  user on a web bridge).
- Group chats. The first slice is 1:1 DM only.
- Voice / image / file uploads. Text-only first slice.

## Architecture

### Where the node lives

```
   ┌────────────────┐                   ┌──────────────────┐
   │   Telegram     │  Bot API (HTTPS)  │  relix-telegram  │
   │   (external)   │ ◄──────────────►  │  (channel node)  │
   └────────────────┘                   └────────┬─────────┘
                                                 │ libp2p (Noise XK + Yamux)
                                                 │ same admission pipeline as
                                                 │ every other peer
                                                 ▼
                                       ┌──────────────────┐
                                       │   relix mesh     │
                                       │  coordinator,    │
                                       │  ai, memory,     │
                                       │  tool, ...       │
                                       └──────────────────┘
```

The `relix-telegram` crate is a **controller** like every other
node — same identity bundle, same policy file, same audit log. The
Telegram-specific surface (HTTPS to Telegram, the channel-side
session storage) is entirely on one side of the controller; the
other side speaks libp2p like any other Relix peer.

### Process boundary

`relix-telegram` runs as its own OS process started by the bringup
script (next to `relix-controller`-spawned `memory` / `ai` / `tool`
peers). It does NOT live inside the bridge. Reasons:

1. **Different failure modes.** Telegram Bot API outages should not
   take chat HTTP requests down.
2. **Different identity needs.** The bridge's identity is "operator
   surface"; the channel's is "channel ingestor" — these get
   different policy admissions.
3. **Different lifecycle.** Bridge is request/response; the channel
   is long-poll or webhook.

### What the channel does on each inbound message

```
1. Receive Telegram update (long-poll OR webhook handler).
2. Look up / derive ChannelSubject from chat_id + user_id.
3. Apply per-chat rate limit + simple sanitisation.
4. Call coordinator: task.create(
       title="telegram: <truncated message>",
       flow_template="flows/channel_telegram.sol",
       params_json={chat_id, user_id, message_id, text},
       retry_policy=none,
       max_runtime_secs=<configured channel timeout>)
5. Persist (chat_id, message_id) → task_id mapping in the
   channel's local SQLite (separate from the Coordinator's DB).
6. Run the SOL flow via FlowRunner (same path as the bridge):
       - task.update(status=running, trace_id=<minted>)
       - FlowRunner::run with flow_template above
       - on completion: task.update(status=completed, result=...,
         flow_id=..., flow_log_path=...)
       - on failure: task.update(status=failed, error_*, failure_class=...)
7. Once the flow completes, post the result back to Telegram as
   a reply to the original message (look up the chat from the
   persisted mapping).
```

Steps 4-6 are **identical** to what the bridge does on `POST /chat`.
The only differences are step 7 (channel-specific delivery) and the
fact that step 1 is event-driven rather than request/response.

### Async outbound delivery

Because steps 4-7 can take arbitrarily long (the flow might call
`tool.web_fetch` plus `ai.chat` plus another tool call, totalling
tens of seconds), the channel does NOT block the inbound update
handler. The handler:

1. Persists the mapping.
2. Spawns a tokio task that does steps 4-7.
3. Returns to the long-poll loop immediately.

That tokio task can complete in 50ms or 50 minutes. When it
finishes, it uses the chat_id from its closure to post the reply.
**Telegram never sees a "typing forever" indicator** — the response
arrives whenever the flow finishes.

For flows that the SOL author wants to acknowledge mid-execution
(e.g. "looking that up, one moment..."), the flow can append a
`task.event` with a known type and the channel polls those events
to relay them. This is **optional** and not in the first slice;
the protocol seam is reserved.

## Identity model

Telegram users do not have Relix IdentityBundles. The channel
node creates a **derived subject** per Telegram user:

- `subject_id = blake3("telegram:" + user_id + ":" + chat_id)[..32]`
- `groups = ["channel-telegram-users"]` (or whatever the operator
  configures via the channel's TOML).
- `display_name = "telegram:<username>"`

This derived identity is **the channel node's IdentityBundle's
subject_id when forwarding** — the channel acts as a Relix peer
that says "I am facilitating a request for user X." Two
consequences:

1. The Coordinator's audit log records `caller_subject_id =
   <channel's bundle subject>` and the channel sets `owner_subject_id`
   on `task.create` to the derived per-user subject. Operators can
   query "all Tasks owned by telegram:<user_id>" without parsing.
2. Per-user policy is enforced at the **flow's** capability calls,
   not at the channel's `task.create`. So a flow that calls
   `ai.chat` runs under the channel's identity (admitted to
   `chat-users` group); `tool.web_fetch` ditto. If the operator
   wants per-Telegram-user rate limiting they apply it at the
   channel's rate limiter (step 3 above).

This is the same trust model as the bridge: the bridge identity
is the one the mesh trusts; the bridge is responsible for
verifying its incoming HTTP request. The channel identity is the
one the mesh trusts; the channel is responsible for verifying its
inbound Telegram updates (signed via the Telegram Bot API's TLS).

## Configuration shape

```toml
# channel-telegram.toml — same shape as any controller TOML.
[controller]
identity_bundle = "dev-keys/local-telegram.aic"
client_key      = "dev-keys/local-telegram.key"
data_dir        = "dev-data/local-telegram"
policy_path     = "configs/policies/local.toml"
peers_path      = "dev-data/local/peers.toml"

# Telegram-specific section.
[telegram]
# Source for the Bot API token. The token MUST NOT live in this
# file; the operator either sets the env var or passes a path to
# a secret-management binary.
bot_token_env   = "RELIX_TELEGRAM_BOT_TOKEN"
# Delivery mode: "long_poll" (default, no public ingress) or
# "webhook" (requires a TLS-terminating proxy).
mode            = "long_poll"
# Per-chat rate limit. Conservative default.
max_inbound_per_chat_per_minute = 6
# SOL flow template that runs for each inbound message.
flow_template   = "flows/channel_telegram.sol"
# Hard ceiling on per-message flow runtime. The Coordinator's
# recovery scan flips overdue rows to interrupted.
max_runtime_secs = 60
# Coordinator alias (must match the [peers] entry).
coordinator_alias = "coordinator"
```

The `[controller]` section is shared with every other controller
TOML; the bringup script generates it the same way it generates
`memory.toml` / `ai.toml` / etc.

## Capabilities the channel node consumes

- `task.create` — mint the per-message task.
- `task.update` — open + close the attempt, set terminal status.
- `task.event` — checkpoint events for SOL-mid-flight progress.
- `ai.chat` / `memory.*` / `tool.*` — whatever the channel's SOL
  flow uses. Same admission pipeline as the bridge's flows.

The channel does NOT need its own capability surface (it doesn't
respond to RPCs from other peers). It is a **client-only** peer
in the alpha.

## Capabilities the channel node does NOT consume

- `task.recover` — operator concern, not channel.
- `task.retry` — operator decides; the channel does not auto-retry
  a Telegram message.
- `task.list` / `task.get` / `task.attempts` — operator concern.

This list is enforced by the policy file (channel's group should
only admit the four creation/update/event capabilities + the
flow's own capability set).

## SOL flow shape

A first-cut `flows/channel_telegram.sol`:

```
# Reads channel-supplied params from heap:
#   {{CHAT_ID}}, {{USER_ID}}, {{MESSAGE_ID}}, {{TEXT}}
# Writes a turn to memory (per-user session keyed on user_id).
# Calls ai.chat with whatever context the flow author wants.
# Returns the reply text.

PUSH_S "telegram:user-{{USER_ID}}"           # session id
PUSH_S "{{TEXT}}"
REMOTE_CALL "memory" "memory.write_turn"
REMOTE_CALL "memory" "memory.read_session"
REMOTE_CALL "capability:ai.chat" "ai.chat"
```

Identical primitives to the bridge's flow templates. SOL stays the
orchestration authority.

## Failure semantics

Inbound failure modes and where they land:

| Failure | Where it shows |
|---|---|
| Telegram API outage on inbound | Channel logs WARN; long-poll resumes when the API is back. Inbound updates DELIVERED LATER (Telegram queues them) become tasks then. |
| Channel rate-limited | Channel returns a static "rate limited, try again in N seconds" reply via Telegram without creating a Task. |
| `task.create` fails | Channel logs ERROR (NOT fail-soft like the bridge — without a Task there is no record of the message). Sends a one-line "I couldn't accept that message right now" reply. Falls back to logging the raw message to the channel's local DB for forensic recovery. |
| Flow fails | Same as bridge: `task.failed` event, `last_failure_class` set. Channel posts the failure cause to Telegram (one-line). |
| Flow times out | Recovery scan flips to `interrupted`. Channel polls and posts "your request timed out after N seconds" reply. |
| Outbound Telegram API failure | Channel retries 3x with exponential backoff (transient), then appends a `channel.delivery_failed` task event and gives up. The Task itself is `completed` — the reply text exists; just couldn't be delivered. |

## What this design protects against

- **No autonomous retries of mutations.** The channel does not
  loop on flow failures. Operator-initiated retry only.
- **No hidden orchestration.** SOL owns the flow; the channel is
  glue.
- **No bypass of the admission pipeline.** Channel calls
  capabilities exactly as the bridge does — every call goes
  through identity → policy → handler → audit.
- **No token leakage.** Bot token lives in env or secret manager,
  never in a config file. The channel logs token reference
  (env var name) but never the value.

## Open questions (deferred)

- **Delivery model** (long_poll vs webhook) — both supported by
  the config; first slice picks long_poll because it requires no
  public ingress. Webhook adds an axum endpoint and TLS
  termination concerns; ship after long_poll proves the model.
- **Multi-channel session unification** — a user on both Telegram
  and the web bridge today gets two unrelated sessions. Unifying
  them needs a "channel-linked identity" model; out of scope for
  the first channel.
- **Streaming responses.** Open WebUI streams reply tokens as they
  arrive; Telegram does not natively support that (it has message
  editing). A streaming mode for Telegram would edit the in-flight
  reply message repeatedly. Out of scope tonight.
- **Group chats / mentions / commands.** Bot commands like
  `/help` and `/status` need a dispatcher; mentions need parsing.
  Reserved for the next channel iteration.

## Trust boundary summary

| Trust dimension | Web bridge | Channel node |
|---|---|---|
| Inbound auth | None (operator's reverse proxy) | Telegram TLS + bot token |
| Identity mapped to subject | bridge bundle | derived per-user (`telegram:user_id:chat_id`) |
| Per-user rate limit | No (proxy-level) | Yes (channel config) |
| Admission pipeline | Yes | Yes |
| Auto-retry | No | No |
| Orchestration | No (SOL flow) | No (SOL flow) |
| Persistence ownership | Coordinator | Coordinator (+ tiny channel-local mapping DB) |

## Code organisation (when implementation lands)

```
crates/
  relix-telegram/
    src/
      main.rs           # controller bootstrap (identity, policy, libp2p, run loop)
      config.rs         # parse [telegram] section
      session_store.rs  # SQLite mapping (chat_id, message_id) -> task_id
      ingest.rs         # long-poll loop OR webhook handler
      flow_runner.rs    # task.create + update + dispatch to FlowRunner
      delivery.rs       # outbound POST to Telegram Bot API (with retry)
      derived_identity.rs # blake3 of channel:user_id:chat_id
```

## See also

- [`docs/coordinator.md`](coordinator.md) — the Task ledger this
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
