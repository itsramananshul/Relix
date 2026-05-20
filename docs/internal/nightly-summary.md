# Nightly Summary — 2026-05-19

Autonomous session. Continuous execution across the C2 runtime
phase and the parallel-track roadmap. Aggressive incremental push
cadence: 18 commits pushed to `main`, every coherent subsystem its
own commit, no batched mega-commits.

## Test posture

- **Before session start:** 205 workspace tests passing.
- **After session end:** 278 workspace tests passing.
- **Net new:** +73 tests across 9 files.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo fmt --all`: clean.
- One real bug discovered and fixed mid-session (web_extract CDATA
  payload leak — see commit `f21ab4b`).

## Architecture invariants preserved

Verified against every change:

1. SOL still owns orchestration. No new RPC entry into the VM.
2. Coordinator still owns durable metadata only — no scheduling,
   no leasing, no auto-relaunch, no autonomous retry.
3. Bridge stays translation-only. Every `/v1/tasks*` endpoint is a
   thin forwarder to a Coordinator capability.
4. No hidden autonomous loops. Recovery scan runs once at startup
   + on-demand via `task.recover`. Period.
5. Capability-first architecture remains mandatory. Every new
   endpoint calls a registered capability that goes through the
   full admission pipeline (identity → policy → handler → audit).

## What shipped (18 commits, chronological)

### C2 — Execution Continuity Foundations

- `8a10459` C2a — lifecycle states + failure class + chronology
  (recap from earlier session; first commit of the night)
- `aabede4` C2b — bounded task recovery scanning (recap)
- `e25b8a9` C2c — structured execution chronology events (recap)
- `8ba09b8` chronology event vocabulary alignment (recap)
- `2b37dd9` operator-guide expansion for C1 lifecycle (recap)
- `9815592` `--pretty` callouts for awaiting_input / failure class
  (recap)
- `29cbf42` **C2a.1/3/4** — per-attempt execution lineage. New
  `task_attempts` table, `TaskStore::update` drives attempt rows
  transactionally, recovery scan rewritten to key off current
  attempt's `started_at`. 8 new tests.
- `3f2bf1e` **C2a.2 + C2b.1** — bridge drives through `running`
  transition + propagates trace_id. New `task.update` 9th slot
  for trace_id (32-hex). `TaskRecorder::start_running` primitive.
  1 new test.
- `dc9e5c0` **C2a.5** — CLI `task attempts` subcommand + pretty
  integration (attempts block inserted before chronology). 6 new
  tests.
- `317002f` **C2c.1 + C2c.2** — `task.retry` operator primitive.
  Coordinator's `RetryDecision::{Accepted, Exhausted, Rejected}`,
  CLI `task retry --force` safety guard. 8 new tests.
- `a98a87c` **C2d.1** — `--pretty` summary line: status, attempts,
  duration, failure class, retry budget. 4 new tests.
- `11120b4` **C2d.2** — chronology grouped by attempt boundaries
  with `---- attempt #N ----` separators. 3 new tests.
- `dd5e8d1` **C2 docs sweep** — new `docs/attempt-lineage.md`
  end-to-end contract + cross-links from runtime-lifecycle,
  task-recovery, operator-guide, current-limitations.

### Track 2 — Task-first API evolution

- `62dd136` **T2.1** — bridge `/v1/tasks` read API. `GET /v1/tasks`
  (with status filter), `GET /v1/tasks/:id`, `GET
  /v1/tasks/:id/attempts`. 503/400/404/502 contract. 5 new tests.
- `229e2a6` operator-guide HTTP-side inspection examples.
- `ec3b54a` **T2.3** — `GET /v1/tasks/:id/summary` JSON shape of
  the CLI's `--pretty` first line. 4 new tests.
- `8499c1a` **T2 expansion** — `POST /v1/tasks/recover` for
  operator-triggered recovery scans over HTTP. 2 new tests.

### Track 3 — Telegram channel scaffolding (blocked on credentials)

- `142c45a` **T3** — `docs/channel-node-architecture.md` end-to-end
  design (process boundary, identity model, async outbound,
  configuration shape, trust boundary, code organisation).
  Blocker archived at
  `docs/internal/nightly-blockers/20260519-0030-telegram-credentials.md`
  explaining why scaffold-only was rejected as dishonest and what
  options were considered.

### Track 4 — Capability discovery / planner foundations

- `12dcc1a` **T4** — `docs/capability-discovery.md`. How discovery
  works today, what's missing, four-stage planner-foundations
  proposal (P1-P4), explicit hard non-goals (no autonomous
  recursive planner, no hidden orchestration prompts, no swarm
  routing, no self-modifying flows).

### Track 5 — Plugin / packaging foundations

- `0399951` **T5** — `docs/plugin-foundations.md`. Today's static-
  linkage model, 7 mandatory constraints (M1-M7), 4 loading-model
  options (A: static; B: out-of-process — recommended; C: WASM;
  D: dynamic dylib — rejected), forbidden surface area, 4
  realistic next steps that preserve every invariant without
  introducing a loader.

### Track 6 — Hardening + security

- `f21ab4b` **T6 fs + web_extract** — 7 fs hardening tests
  (traversal-on-write, absolute-on-write, oversize-write,
  patch-on-ghost, no-match search, deep nesting, empty-pattern
  guard, read-cap-rejects-not-truncates) + 6 web_extract
  hardening tests (deep nesting, CDATA skip, script-in-CDATA
  no-leak, surrounding text intact, malformed meta, long
  attribute, void-tag boundaries, double-decode guard). **Real
  bug fix:** the parser was leaking `<script>` payload from
  inside `<![CDATA[ ... ]]>` blocks; added explicit CDATA skip
  alongside the existing comment skip.
- `f40ebf2` **T6 pdf + coordinator** — 4 PDF hardening tests
  (empty body, truncated header, garbage after header, no-
  separator arg) + 4 coordinator hardening tests (500-event
  chronicle rendering, special-char payload roundtrip, 10-cycle
  retry stress, 1000-ID collision-resistance).
- `ea142ec` **T6 SSRF** — 9 SSRF hardening tests (bracketed IPv6
  loopback / link-local / mapped-v4-loopback, hostname denylist
  case-insensitivity for both `localhost` and `*.internal`,
  userinfo smuggling, explicit-port-no-bypass, no-host URL,
  documentation range rejection).

### Track 8 — Documentation

- `7572a8b` **T8** — README refresh: Coordinator row + CLI row
  updated to reflect C1/C2 surface. New "Task lifecycle reference
  (C1 + C2)" subsection in docs index pointing at the 5 reference
  docs in reading order.

## Blockers archived

1. `docs/internal/nightly-blockers/20260519-0030-telegram-credentials.md`
   — Telegram Bot API token required before `relix-telegram`
   crate can be implemented. Design contract shipped instead.
   Follow-up prompt suggested.

## What I deliberately did NOT do

Per the directive, avoided any of:

- Distributed scheduler / leasing system.
- Autonomous retry daemon. (operator-only `task.retry` shipped.)
- Resumable VM.
- Recursive planners or planner-in-SOL.
- Marketplace / plugin store.
- Browser automation / execute_code / shell execution.
- Multi-channel session bridging.
- Kubernetes-style orchestration.

The capability descriptors gained no new "magic" fields; the
admission pipeline was not modified; SOL was not modified.

## Files added this session

```
docs/attempt-lineage.md                                 # C2 reference
docs/capability-discovery.md                            # T4 planner foundations
docs/channel-node-architecture.md                       # T3 channel design
docs/plugin-foundations.md                              # T5 packaging foundations
docs/internal/nightly-blockers/                         # blocker archive dir
docs/internal/nightly-blockers/20260519-0030-telegram-credentials.md
docs/internal/nightly-summary.md                        # this file
crates/relix-web-bridge/src/tasks.rs                    # /v1/tasks endpoints
```

## Files modified (substantive)

```
crates/relix-runtime/src/nodes/coordinator/mod.rs       # +673 lines (C2a)
crates/relix-runtime/src/nodes/tool/web_extract.rs      # CDATA fix + tests
crates/relix-runtime/src/nodes/tool/fs.rs               # hardening tests
crates/relix-runtime/src/nodes/tool/pdf.rs              # hardening tests
crates/relix-runtime/src/nodes/tool/security.rs         # SSRF hardening
crates/relix-runtime/src/flow_runner.rs                 # trace_id plumbing
crates/relix-runtime/src/controller_runtime.rs          # capability manifest
crates/relix-web-bridge/src/flow.rs                     # bridge running transition
crates/relix-web-bridge/src/task_recorder.rs            # read passthroughs + start_running
crates/relix-web-bridge/src/main.rs                     # routes
crates/relix-cli/src/task.rs                            # attempts/retry/pretty additions
crates/relix-cli/src/flow_run.rs                        # trace_id option
docs/operator-guide.md                                  # 3 section additions
docs/task-runtime.md                                    # C2 + retry + attempts updates
docs/runtime-lifecycle.md                               # attempt-lineage cross-links
docs/task-recovery.md                                   # task.retry recipe
docs/retry-model.md                                     # rewrite TL;DR for task.retry
docs/current-limitations.md                             # C2 reframing
docs/coordinator.md                                     # event vocabulary refresh
README.md                                               # task lifecycle docs
```

## Suggested next steps (morning review)

1. **Greenlight P1 of capability discovery foundations** —
   tiny extension to `CapabilityDescriptor` with three optional
   fields. Single commit. Preserves every invariant.
2. **Decide on Telegram channel implementation** — confirm token
   handling pattern (env var vs secret manager) and unblock
   `crates/relix-telegram/` work.
3. **Operator review of `docs/plugin-foundations.md`** — the
   forbidden-surface section and the 7 mandatory constraints
   need explicit operator sign-off before any plugin work starts.
4. **Glance at recent commits in the GitHub repository view** —
   the 18 commits are small, single-purpose, and individually
   reviewable; no mega-commit needs un-bundling.

End of nightly session. No regressions. All pushes through
`origin/main`.
