# Dashboard redesign

> **Historical design contract (superseded 2026-06-02).** This is the
> design doc written before the console was built. The shipped result
> diverged from it: the dashboard
> (`crates/relix-web-bridge/src/dashboard.html`) is a twenty-two-panel
> single-page app selected from a sidebar, with no `#/...` hash
> routing and no `#/tasks`, `#/overview`, or `#/topology` routes. A
> Tasks panel exists, but the `#/...` routes, the
> activity rail, and the peer drawer described below were not shipped
> in that form. For the panels that actually exist, see
> [operator-guide.md](operator-guide.md) and the `SECTIONS` array in
> `dashboard.html`. Kept for design history only.

Design contract for the Relix operator console.

The current `/dashboard` is a single page that lists tasks plus
two collapsible widgets (chronicle retention dry-run, mesh
topology). It's functional but visually thin and offers no place
for operators to configure providers, Telegram, or other settings
without editing config files in a shell. This doc covers the
redesigned dashboard before any code lands.

## Why now

Relix has shipped enough mesh-side surfaces (tasks, topology,
health, chronicle retention) that operators want a single
console to drive them — not a different curl invocation per
need. Settings + provider keys + Telegram setup are the
load-bearing missing pieces; operators currently hardcode keys
in `.toml` files via the terminal, which is a poor first-run
experience and a foot-gun for accidental commits.

## What was inspected

`reference/openclaw-main/` — OpenClaw's web operator UI. Notable
findings:

- **Layout**: classic sidebar + topbar + content grid
  (`/ui/src/styles/layout.css`). 258px sidebar, 52px topbar.
  Tab groups (Control / Agent / Settings) rather than a flat
  nav.
- **Visual rhythm**: dark theme by default
  (`--bg: #0e1015`, `--card: #161920`), whisper-thin borders,
  subtle shadows. Typography scale 11px / 12px / 14px / 16px
  via tokenised CSS variables.
- **Secret handling**
  (`/ui/src/ui/views/config-form.node.ts`,
  `/ui/src/ui/views/config-form.shared.ts`): all secrets render
  via a `REDACTED_PLACEHOLDER`. A reveal toggle flips visibility
  for the current session only. Inline edit on a masked value
  requires reveal first. The wire model is a `SecretRef`
  `{ source, id, provider? }` — the UI never holds the literal
  value, only metadata.
- **Provider config**
  (`/extensions/anthropic/provider-contract-api.ts` and
  friends): each provider declares its `id`, `label`, accepted
  `envVars`, and multiple `auth` methods (`api_key`, `oauth`,
  `cli`). The settings UI builds a card per provider with the
  available auth method selector.
- **API protocol**: JSON-RPC over HTTP POST
  (`/extensions/admin-http-rpc/src/handler.ts`). Single endpoint
  with `method` field discriminator. Status mapping
  `200 → ok / 400/404/503/504 → typed error`.
- **Routing**: path-based with manual `pushState`, no router
  framework. Tab enum → URL.
- **Tech stack**: Lit web components + Vite + custom CSS
  variables. No Tailwind, no shadcn/ui, no React. State is
  reactive `@state` + manual events.

## What Relix adopts from OpenClaw

| Pattern | Adoption | Why |
|---|---|---|
| Sidebar + topbar + content grid | Yes | Operator-console convention; works for the routes we need. |
| Dark theme tokens (`--bg` / `--card` / `--border` / `--accent`) | Yes | Adapts cleanly; we already have a smaller palette. |
| Typography scale (11/12/14/16) | Yes | Matches the dense data we render. |
| Secret-handling UX: `REDACTED_PLACEHOLDER` + reveal toggle | Yes | Battle-tested, no novel cryptography. |
| Status badges with semantic colors | Yes — already present | Extend the existing freshness-badge pattern. |
| Card components with whisper-thin borders | Yes | Replaces the current "everything on a flat list" look. |
| Tab groups in sidebar navigation | Yes | Operator workflows naturally group (overview / tasks / topology vs. providers / telegram / config). |
| Provider-card auth selector | Yes — adapted | Each AI provider in Relix gets its own card with key + model selection. |
| Empty / loading / error inline states | Yes | Currently inconsistent across the dashboard; standardise. |

## What Relix does NOT adopt

| Pattern | Rejection | Why |
|---|---|---|
| Lit web components / any JS framework | Reject | The Relix dashboard MUST stay buildless. It's embedded in the bridge binary via `include_str!`. Any framework forces a npm/build step, which adds attack surface, breaks the "no external resources loaded" invariant, and is overkill for an operator console at this scale. We stay with vanilla JS + a single HTML file. |
| Tailwind / shadcn/ui | Reject | Same buildless constraint. Use CSS variables + handwritten classes. |
| React Query / TanStack Query | Reject | Same. The dashboard's data needs (10s of fetches) don't justify the dep. Light handwritten fetch wrappers with timeouts are enough. |
| JSON-RPC over a single endpoint | Reject | We already have REST-ish endpoints under `/v1/*`. Convert to JSON-RPC just for the dashboard's sake breaks CLI parity. Each new dashboard surface stays a normal HTTP endpoint. |
| i18n / 26-language support | Reject | English-only for Phase 1; not a Relix priority. |
| File-based routing | Reject | Hash-based routing on the single HTML page is enough; matches the buildless constraint. |
| Server-side rendering / streaming HTML | Reject | The bridge ships static HTML embedded in the binary; the JS hydrates from the JSON endpoints. |
| WebSocket bus for general state | Reject | We already have SSE for chronicle events; that's enough. Settings + provider config are synchronous request/response. |

The non-adoptions all flow from one invariant: **the Relix
dashboard is one HTML file with no build step.** Operators who
clone the repo and `cargo build` get the dashboard for free.
Adding a JS framework would invert that. The price we pay is a
slightly more verbose vanilla-JS render layer; the price we
avoid is owning a frontend toolchain.

## Information architecture

The dashboard has six routes, grouped into two sections:

**Operate** (read-mostly mesh state):

| Route | Purpose | Backing endpoints |
|---|---|---|
| `#/overview` | At-a-glance: uptime, peer freshness summary, recent task count, reconnect counters. The page operators land on. | `/v1/health` + `/v1/topology` + `/v1/tasks/count` |
| `#/tasks` | Task list with filters + search + cursor pagination + detail panel + live SSE chronology + export + retry/recover actions. Today's `/dashboard` content, redesigned. | `/v1/tasks/cursor`, `/v1/tasks/:id/lineage`, `/v1/tasks/:id/events/stream`, `/v1/tasks/:id/export`, `/v1/tasks/recover` |
| `#/topology` | Full peer table with freshness, capability count, methods, last refresh. Click a row → drill-in (future). | `/v1/topology` |

**Configure** (write-capable, restart-aware):

| Route | Purpose | Backing endpoints (new) |
|---|---|---|
| `#/providers` | AI provider cards (mock / openai / anthropic / openrouter / xai / google). Per-provider: key entry, default model, configured/not-set status. | `GET /v1/config/providers`, `PUT /v1/config/providers/:name` |
| `#/telegram` | Telegram bot config: token entry (masked), mode (polling / webhook), test-connection action (when implemented). | `GET /v1/config/telegram`, `PUT /v1/config/telegram` |
| `#/config` | Read-only redacted view of the bridge's effective config — for "what did I actually configure" troubleshooting. | `GET /v1/config` |

The retention dry-run widget moves under `#/tasks` as a button
that opens a modal — it's task-adjacent, not its own route.

## Config / security model

This section is load-bearing for the secret-handling rules. Read
it carefully before implementing any config endpoint.

### Where secrets live on disk

The bridge writes user-supplied secrets to a single local file:

```
<RELIX_DATA_DIR>/bridge-secrets.toml
```

Default location: alongside the existing bridge data dir (per
the bringup scripts, that's `dev-data/<RUN>/local-bridge/`).
The path is operator-configurable via `[bridge] secrets_path`
in the bridge config.

The file is:

- **Mode 0600 on POSIX** — owner read/write only.
- **Gitignored** — added to `.gitignore` by name (`bridge-secrets.toml`).
- **Local to one bridge instance** — distinct from controller-side
  configs that already exist. The bridge is the only writer.

Shape (TOML):

```toml
[providers.openai]
api_key = "sk-..."          # written; never read by the dashboard
default_model = "gpt-4o"

[providers.anthropic]
api_key = "sk-ant-..."
default_model = "claude-sonnet-4-6"

[telegram]
bot_token = "1234567:..."
mode      = "polling"        # or "webhook"
```

### What the dashboard sees

The dashboard NEVER receives a raw secret. The
`GET /v1/config/providers` response shape is:

```json
{
  "providers": [
    {
      "name": "openai",
      "configured": true,
      "default_model": "gpt-4o",
      "key_preview": "sk-...4f2c",   // last 4 chars only
      "key_set_at": 1700000000        // wall-clock unix seconds
    },
    {
      "name": "anthropic",
      "configured": false,
      "default_model": null,
      "key_preview": null,
      "key_set_at": null
    }
  ]
}
```

The `key_preview` field is the **only** thing of the original
secret that ever leaves the bridge process. It's the last 4
characters, never the first 4 (avoid revealing provider-prefix
fingerprints). Empty secrets return `null`, not an empty
string.

### Writing secrets

`PUT /v1/config/providers/:name` accepts:

```json
{
  "api_key": "sk-...",
  "default_model": "gpt-4o"   // optional
}
```

- The bridge writes (or updates) the file.
- Returns the same redacted status shape (without the
  just-submitted key).
- Writes a single tracing event at INFO level:
  `config: providers.<name> updated (key_preview=...XXXX)`.
  The full secret is NEVER logged. The redacted preview is
  emitted at INFO so operators can confirm the action.

The endpoint is **idempotent** — re-submitting the same key
overwrites in place; the file timestamp updates.

### Deleting secrets

`DELETE /v1/config/providers/:name` removes the provider's
block from the file. Returns the redacted status (now
`configured: false`).

### Restart-required UX

Provider keys are read at AI controller startup, not at every
chat. So submitting a key via the dashboard does NOT take
effect until the corresponding AI controller is restarted.

The dashboard MUST surface this. Two affordances:

1. After a successful PUT, the response includes
   `restart_required: true` in the response envelope.
2. The provider card shows a yellow "restart required" badge
   until the controller is restarted (detected by comparing
   `key_set_at` to the AI peer's `last_refreshed_at` from
   `/v1/topology`).

The bridge MAY restart its own process on demand (and refresh
its `started_at`); restarting the AI controller is out of
scope for the bridge — the dashboard shows a copy-paste
command instead.

### Telegram token handling

Same model as providers. The Telegram block on disk:

```toml
[telegram]
bot_token = "..."
mode      = "polling"
```

`GET /v1/config/telegram` returns:

```json
{
  "configured": true,
  "token_preview": "...4f2c",
  "mode": "polling",
  "token_set_at": 1700000000
}
```

`PUT /v1/config/telegram` accepts:

```json
{
  "bot_token": "...",
  "mode": "polling"     // optional, default "polling"
}
```

Webhook mode is in the schema but the live HTTPS client is not
yet wired (see "Out of scope below"). Submitting `mode:
webhook` returns a 422 with body
`{"error":"webhook mode not yet implemented; use polling"}`
until the live client lands.

### Auth on these endpoints

**None at the HTTP layer.** Same as every other `/v1/*`
endpoint today. The dashboard config surfaces are governed by
the bridge's listen address — the bridge binds to
`127.0.0.1:19791` by default. Production operators MUST put
a reverse proxy with auth in front before exposing the bridge
beyond loopback. The dashboard config endpoints are clearly
marked as **local/dev only** in their endpoint docs and a
banner appears on the dashboard config pages.

If we ship in production-mode (a future flag, not Phase 1), the
endpoints would refuse to serve from non-loopback addresses
unless an explicit `--allow-remote-config` flag is set.

### What's NOT in scope for the config endpoints

- **Provider key rotation** with overlap (old + new active
  simultaneously). Today, set = overwrite.
- **Encryption-at-rest** of the secrets file. Operators
  responsible for disk security (the file is mode 0600;
  filesystem encryption is the operator's concern).
- **Remote KMS integration** (HashiCorp Vault, AWS Secrets
  Manager). Out of scope.
- **Multi-operator review workflow** for changes. Single
  operator, single write.
- **History / rollback** of changes. The file is
  last-write-wins; operators wanting rollback use their own
  config management.

These all stay deliberately out of Phase 1 — the goal is
"operators can set a key without editing TOML in a shell,"
not "production-grade secret management."

## Implementation status

Milestones M1-M8 landed the foundational redesign. M9-M16
ship the productization layer that makes the dashboard
feel like a real operator console rather than a debug
page.

| # | Status | What |
|---|---|---|
| M1 | ✅ | Retire `docs/internal/nightly-blockers/`. |
| M2 | ✅ | OpenClaw analysis + this design doc. |
| M3 | ✅ | Sidebar + topbar + hash-router shell. Six routes wired. |
| M4 | ✅ | Card-based redesign for task / topology / overview (folded into M3). |
| M5 | ✅ | `BridgeSecrets` + `/v1/config/*` endpoints. Atomic file write, mode 0600, redaction-tested. |
| M6 | ✅ | `#/providers` page: per-provider cards, masked input + reveal toggle, save / delete / restart-required UX. |
| M7 | ✅ | `#/telegram` page: token + mode form, BotFather walkthrough. |
| M8 | ✅ | `#/config` page + redaction endpoint tests + docs polish. |
| M9 | ✅ | Topology graph (SVG) + peer detail drawer. Click-to-inspect peer. |
| M10 | ✅ | Live activity feed on overview. Polls + diffs every 5s. |
| M11 | ✅ | Provider connection test endpoint + UI button. Probes upstream `/models`. |
| M12 | ✅ | Task search, quick-filter chips, toast notifications. |
| M13 | ✅ | Always-on background sync so activity rail stays alive across route switches. |
| M14 | ✅ | Filter + selected task persisted in URL hash for shareable / bookmarkable views. |
| M15 | ✅ | Telegram connection test + URL-token scrubber (load-bearing leak guard). |
| M16 | ✅ | Activity items clickable: navigate to task or open peer drawer. |
| M17 | ✅ | Default provider marker — `PUT /v1/config/providers/default` + Make-default UI badge. |
| M18 | ✅ | Operator-initiated retry endpoint + UI with refused-class confirm flow. |
| M19 | ✅ | Task cancel: ledger-only with honest `flow_still_running` warning (no runtime-side cancellation yet). |
| M20 | ✅ | Execution timeline view in task detail — vertical track, per-event marker colors, attempt grouping, Timeline/Raw toggle. |
| M21 | ✅ | SSE stream metrics — RAII `StreamGuard`, active + opened_total in `/v1/health`. |
| M22 | ✅ | Density pass — tighter paddings, smaller topbar, KPIs grouped Mesh / Runtime / Process. |
| M23 | ✅ | Server-side lifecycle event log — `/v1/topology/events` with joins / freshness / drops ring (cap 500). |
| M24 | ✅ | Topology page surfaces the lifecycle log as a Recent transitions card. |
| M25 | ✅ | Per-stream tracking — `ActiveStream { id, task_id, opened_at }`, `/v1/streams` endpoint, live-streams KPI shows watching ids. |
| M26 | ✅ | Retry chain visualization — horizontal pills + inter-attempt gap markers + outcome summary. |
| M27 | ✅ | Cross-reference panel — trace_id + flow_id + flow_log_path + copy-to-clipboard + CLI command templates. |
| M28 | ✅ | Topology correlation for interrupted/failed tasks — ±30s lifecycle events near the task's failure window. Labeled "Correlation, not causation". |
| M29 | ✅ | Failure breakdown panel — class badge + cause + class-specific operator suggestion (sourced from `docs/retry-model.md`). |
| M30 | ✅ | Runtime anomaly banner on overview — peer flips + task failures + expired peers in last 5 min, elevated / high level escalation. |
| M31 | ✅ | Topology graph activity overlay — ripple + dashed ring per peer with a transition in the last 30s. Distinct from the continuous expired pulse. |
| M32 | ✅ | Latency time-budget bar — stacked horizontal bar under the retry chain, segment width proportional to duration share. |
| M33 | ✅ | `/v1/routing` endpoint + Execution path panel — pairs each invoked method with its current routing target. Honest "Routing as of now" framing because per-call history isn't recorded yet. |
| M34 | ✅ | Minimal instrumentation: `chat_with_tool` flow's `capability.invoked` payload now carries `peer=tool` — the resolved alias. Dashboard renders these rows with a green "recorded" badge instead of the routing-snapshot fallback. |
| M35 | ✅ | `chat` flow gets its own `capability.invoked` emit (`method=ai.chat peer=ai`) — every chat task now has an Execution path. `ai.chat` rows render an explicit "model: not recorded yet" label. |
| M36 | ✅ | Per-attempt timeline filter — click a chain pill → timeline collapses to just that attempt's events. `× clear` chip restores. |
| M37 | ✅ | Clickable peer references in timeline + Execution path — `peer=X` becomes a link into the topology peer drawer. Closes the dashboard's causality navigation loop. |

Each milestone is its own commit + push, per the directive.

## Phase-1C causality stack (per task detail)

The task detail panel reads top-down as a causality story:

1. **Summary row** — current status, attempt count, duration.
2. **Retry chain pills** (M26) — sequence of attempts with
   inter-attempt waits.
3. **Latency time-budget bar** (M32) — where the wall-clock
   actually went.
4. **Failure breakdown panel** (M29) — when failed/interrupted/
   cancelled: class + cause + suggested next step.
5. **Topology correlation panel** (M28) — when failed/interrupted:
   mesh events that happened in the ±30s window around the failure.
6. **Attempts table** — raw per-attempt rows for forensics.
7. **Execution timeline** (M20) — full chronicle as vertical
   marker track, grouped by attempt.
8. **Cross-references panel** (M27) — task_id / trace_id /
   flow_id with copy buttons + CLI command templates for
   per-flow event log + per-node audit drill-in.

Every surface derives from real runtime state. No invented
causality, no fake AI reasoning. The "why" answers come
from class taxonomies (M29 mapping to retry-model.md), from
correlation with lifecycle events (M28), from the actual
attempt + gap timing (M26 + M32) — never from synthesized
narratives.

## Phase-1D: Execution path visibility (M33-M37)

Phase-1D extends the causality stack with explicit
routing visibility — answering "which peer handled
each capability call." Critical honesty rule: where
the runtime doesn't record a fact today, the surface
shows "not recorded yet" rather than inventing
plausible values.

New endpoint: `GET /v1/routing` (M33) — for each
capability method in the bridge's manifest cache,
returns the peer the bridge would route to right now
under first-match-in-cache semantics. The response
includes a self-describing `policy` string so dashboards
never have to invent a rationale, and a
`multiple_candidates` flag per method so operators see
when the choice is non-trivial.

New dashboard surface: Execution path panel in task
detail. Pairs each `capability.invoked` event in the
chronicle with its current routing target. Two states
per row:

- **Recorded** (green badge) — when the
  `capability.invoked` payload includes `peer=ALIAS`
  (shipped via M34 for `chat_with_tool`, M35 for `chat`).
  Ground truth, not inference.
- **Routing snapshot** (freshness-colored badge) — when
  only the method is recorded. The panel explicitly
  labels the snapshot-vs-history gap: "Per-call routing
  not recorded yet — peers shown are the bridge's
  current resolution, which may differ from what handled
  the task at execution time."

Per-method extras: `ai.chat` rows always show "model: not
recorded yet · see the AI controller's [ai] provider
config" — the bridge knows the responding peer but not
its configured model.

Causality navigation (M37): every peer alias on the
dashboard — in the timeline's `capability.invoked`
payloads + in the Execution path panel — is a clickable
link that opens the topology peer drawer. Operators
traverse "this happened" → "here's the entity that
handled it" in one click.

Per-attempt timeline filter (M36): chain pills are now
click-targets. Selecting one filters the timeline to
just that attempt's events (uses `attempt_id` to match
the typed v1 envelope field). `× clear` chip restores
the full view. Operators isolate "what happened during
retry 2" without scrolling past unrelated events.

### What stays "not recorded yet"

These are the honest gaps where the runtime doesn't
capture the data and Phase-1D doesn't fake it:

- **Model name** — the AI peer knows its configured
  model from its TOML; the chronicle doesn't. Surfaced
  as "not recorded yet" with a pointer to the config
  block.
- **Per-step latency** — chronicle has attempt-level
  durations (already shown via M26/M32). Within an
  attempt, individual capability call timing isn't
  in the chronicle. The per-flow event log on disk
  has it; the dashboard's Cross-references panel
  (M27) shows the CLI command to drill in.
- **Provider failover reason** — Relix doesn't
  implement failover today (each AI peer has one
  configured provider). The "selection reason" is
  always "first peer in cache that advertises ai.chat";
  the routing snapshot surfaces this verbatim.
- **Stream-level latency** — `/v1/streams` (M25)
  tracks active stream count + age, not per-event
  latency.

## URL conventions

The dashboard is a single-page app under
`/dashboard`. Routing is hash-based and shareable:

| Hash | What it shows |
|---|---|
| `#/overview` | KPI grid + recent peers + live activity rail. |
| `#/tasks` | Task list + detail panel + chronicle. |
| `#/tasks?status=failed` | Pre-filtered to failed tasks. |
| `#/tasks?status=running&q=demo` | Status filter + free-text search. |
| `#/tasks?task=abc12345…` | Auto-opens that task's detail panel. |
| `#/topology` | Mesh graph + peer table. |
| `#/providers` | AI provider settings (cards). |
| `#/telegram` | Telegram bot setup + test. |
| `#/config` | Effective bridge config snapshot. |

Operators can bookmark any of these. Selecting a task or
changing a filter updates the hash via `history.replaceState`
so the URL stays in sync without inflating browser history.

## New endpoints (Phase-1B)

The Runtime Operations layer (M19-M25) added these
HTTP surfaces. All are read-only or operator-action POSTs
(no orchestration, no autonomous loops, same admission
posture as the rest of `/v1/*`).

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/v1/tasks/:id/retry?force=<bool>` | Operator-initiated retry (M18). Bridge guards non-retryable failure classes; `force=true` overrides. |
| `POST` | `/v1/tasks/:id/cancel` | Mark task cancelled in the Coordinator ledger (M19). Returns `flow_still_running: true` when prior status was running/retrying — honest signal that runtime-side cancellation is not implemented. |
| `GET` | `/v1/streams` | List currently-open SSE consumers — `{ id, task_id, opened_at, age_secs }` per stream (M25). |
| `GET` | `/v1/topology/events?since=<ts>&limit=N` | Server-side lifecycle ring (M23) — joins / freshness changes / drops. Resets on bridge restart. |

Plus health-endpoint additions (M21):

```json
{
  "streams": { "active": 3, "opened_total": 47 }
}
```

## Verification

The redesigned dashboard must satisfy:

- `cargo fmt --all` clean.
- `cargo clippy --workspace --all-targets -- -D warnings`
  clean.
- `cargo test --workspace` passes.
- `GET /dashboard` returns 200 with the new HTML.
- The dashboard loads with no external resource fetches
  (CSP-enforced; the existing `default-src 'none'` policy
  stays).
- All five existing dashboard surfaces (`/v1/tasks*`,
  `/v1/topology`, `/v1/health`, `/v1/capabilities`,
  `/v1/tasks/compact_events`) keep working — the redesign
  is presentation-layer; the wire contracts are unchanged.
- The new `/v1/config/*` endpoints redact secrets in every
  response (unit-tested) and never write raw values to logs
  (review-enforced; pattern documented above).
- `bridge-secrets.toml` is in `.gitignore`.

## Out of scope (deliberately, for this redesign)

These are explicit non-goals:

- **Mobile responsive layout.** The dashboard targets desktop
  operator workflow. A second pass for mobile is fine; not
  this slice.
- **Theming options.** Dark only.
- **User accounts / multi-operator UX.** Single operator,
  single bridge — auth lives at the reverse-proxy layer.
- **Plugin marketplace UI.** Plugins ship out-of-process per
  `plugin-foundations.md`; no in-dashboard installer.
- **Real-time provider-key validation against the upstream
  API.** A button could ping `https://api.openai.com/models`
  to verify a key, but that adds outbound HTTP from the
  bridge — not Phase 1. The dashboard shows
  `configured: true` and trusts the operator until first
  use.
- **Audit log of who-changed-what.** Single-operator model;
  the file's mtime is the audit trail.

## See also

- [`bridge-invariants.md`](bridge-invariants.md) — what the
  bridge may/must-not do. The new config endpoints stay
  translation-only; the secrets file is the only new
  bridge-owned state and it's local-bridge configuration,
  not cross-peer metadata.
- [`deployment.md`](deployment.md) — production hardening.
  The "put a reverse proxy in front" requirement now applies
  to the config endpoints too.
- [`failure-modes.md`](failure-modes.md) — what happens when
  the bridge is down. The config file persists; on restart
  the secrets are read from disk before the HTTP listener
  binds.
- [`restart-safety.md`](restart-safety.md) — exactly what
  survives a bridge restart. The new
  `bridge-secrets.toml` joins the persistent set.
