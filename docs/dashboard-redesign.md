# Dashboard redesign

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

## Implementation plan

Milestones map to the eight commits in the user directive:

1. **Retire `docs/internal/nightly-blockers/`** — done in the
   commit preceding this doc.
2. **OpenClaw analysis + this doc** — current commit.
3. **Dashboard layout foundation** — replace `dashboard.html`
   with the new sidebar+topbar+content shell, hash-based
   router, page registry. Move existing task list to
   `#/tasks`. No content redesign yet.
4. **Task / topology / health redesign** — restyle the
   existing surfaces with cards, status badges, empty/loading
   states. Move the retention widget into a modal under
   `#/tasks`. The `#/overview` page synthesises health +
   topology + task count.
5. **Settings/config backend** — new `BridgeSecretsFile`
   abstraction + `/v1/config/*` endpoints. Schema validation,
   secret redaction, file persistence at mode 0600. Tests for
   redaction + presence-without-leak.
6. **Provider settings UI** — `#/providers` page wired to the
   config backend. Per-provider card with masked key entry,
   default model dropdown, restart-required banner. Six
   provider types (mock / openai / anthropic / openrouter /
   xai / google).
7. **Telegram setup UI** — `#/telegram` page. Mode selector
   (polling only today; webhook returns 422). Setup
   instructions including `@BotFather` walkthrough. Status
   badges.
8. **Tests + docs polish** — workspace tests, dashboard
   landmark assertions, secret-redaction unit tests, README
   refresh, operator-guide cross-reference.

Each milestone is its own commit + push, per the directive.

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
