# Blocker: Telegram channel needs bot credentials before implementation

## Subsystem

Track 3 (Telegram task-native channel) — `crates/relix-telegram/`
not yet created; design lives in
[`docs/channel-node-architecture.md`](../../channel-node-architecture.md).

## What I was trying to do

Stand up a `relix-telegram` controller crate that translates inbound
Telegram messages into Tasks on the Coordinator (per the C2 task
runtime), runs a SOL flow per message via FlowRunner, and posts the
reply back to Telegram.

## Why I stopped this item

Two reasons, both load-bearing for a production-correct first slice:

1. **No bot token available.** The Telegram Bot API requires a
   token created via `@BotFather`. The autonomous session has no
   credentials store; minting a bot is a manual operator step that
   needs a human at a Telegram client.
2. **No way to validate the implementation against the real API.**
   Even if I scaffolded with a placeholder token, none of the
   long-poll / message-send code paths would be exercisable — the
   first time anything actually talks to Telegram, it would fail
   with `401 Unauthorized`, and any nuances of the API (rate
   limits, parse_mode escaping, chat_id types, button payloads)
   would be unverified.

A scaffold-only commit was tempting but would be dishonest — it'd
imply "the crate exists and would work" when in fact `cargo run`
on it would do nothing useful.

## Options considered

1. **Scaffold a `relix-telegram` crate with stubs + `unimplemented!()`
   bodies, no actual network code.** Rejected: not the "safe first
   slice" the directive permits. A crate that doesn't compile a
   useful long-poll loop is a doc, not a scaffold. The design doc
   already serves that purpose with more clarity.
2. **Use a mock Telegram server for tests.** Rejected for tonight:
   the goal is task-native correctness, not "we can stub HTTP". The
   tests we'd write would be tautological without a real API
   contract to validate against.
3. **Ship the design doc (`docs/channel-node-architecture.md`) +
   archive this blocker.** Chosen. The design is the contract the
   implementation must satisfy; once a token is available, the
   implementation is straightforward (long-poll loop +
   `reqwest::Client` + the existing `TaskRecorder` pattern).
4. **Pull in a stub for `webhook` mode that runs on a local axum
   route, with no Telegram dependency.** Rejected: the value of
   webhook mode is interoperability with Telegram; an
   axum-only-webhook is just a worse `/chat` endpoint.

## Recommended path

When this is unblocked, the implementation should:

1. Operator mints a bot via `@BotFather`, captures the token, sets
   `$env:RELIX_TELEGRAM_BOT_TOKEN` (or the configured env var).
2. Create the crate (`crates/relix-telegram/`) per the
   "Code organisation" section of
   [`channel-node-architecture.md`](../../channel-node-architecture.md).
3. Ship long-poll mode first; webhook mode is a follow-up.
4. The first SOL flow (`flows/channel_telegram.sol`) mirrors
   `flows/chat_template.sol` — memory.write, memory.read,
   ai.chat. No tool calls yet (rate-limit + abuse model needs to
   stabilise first).
5. Add policy entries to `configs/policies/local.toml` admitting
   the `channel-telegram` group to the bridge's existing capability
   set.
6. Add the channel to `scripts/relix-mesh-up.ps1` / `.sh` as an
   opt-in controller (default disabled until the token is configured).
7. The first tests are integration tests that spawn a real
   coordinator + telegram controller and verify task lifecycle
   without actually hitting the Telegram API (mock the API client
   trait surface).

## What I did instead

- Shipped the **design contract** at
  [`docs/channel-node-architecture.md`](../../channel-node-architecture.md)
  covering: where the node lives, identity model, configuration,
  failure semantics, the SOL flow shape, trust boundaries, and an
  explicit non-goals section.
- Cross-linked from the operator-relevant docs once they get a
  "channels" section.
- Continued autonomous execution on Track 2 (`/v1/tasks` API),
  Track 6 (hardening tests), and Track 8 (docs).

## Follow-up prompt needed

> Mint a Telegram bot via `@BotFather`, capture the token, and
> share whichever path the bringup script should read it from
> (env var, file, secret manager binary). Then ask me to implement
> the `relix-telegram` channel per
> `docs/channel-node-architecture.md`.
