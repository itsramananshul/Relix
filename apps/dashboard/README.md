# Relix dashboard

The Relix operator console — a Vite + React + TypeScript single-page app
served by the web bridge at **`/dashboard`**.

This replaces the legacy single-file `crates/relix-web-bridge/src/dashboard.html`
debug page. That file is kept only as a fallback for source-only
checkouts that have not run the frontend build.

## How it is served

`npm run build` emits the production bundle straight into
`crates/relix-web-bridge/dashboard-dist/` (configured via `vite.config.ts`
`build.outDir`). At boot the bridge's `dashboard::resolve_spa_dir()`
discovers that directory and serves it as static assets with an SPA
history fallback to `index.html`. The built bundle is committed, so
`cargo build` + `relix` boot serve the real app with no extra step.

- Override the bundle location with `RELIX_DASHBOARD_DIST=/path/to/dist`.
- If no bundle is found, the bridge falls back to the legacy embedded
  HTML page, so the console always works.

The app is built with Vite `base: "/dashboard/"` and
`modulePreload.polyfill = false`, so it has **no inline scripts** and
loads cleanly under the bridge's strict default CSP (`script-src 'self'`).

## Auth

The dashboard never handles a bearer token. It logs in with a
username/password (first-run setup creates the admin; Argon2id hash on
the bridge) and rides an HTTP-only `relix_session` cookie. Every API
call uses `credentials: "include"`, and the bridge auth middleware admits
a valid session cookie. Endpoints: `/v1/auth/{status,setup,login,logout,me}`.

## Develop

```sh
cd apps/dashboard
npm install
npm run dev      # Vite dev server on :5273, proxies /v1 -> 127.0.0.1:19791
```

Run a bridge locally (`relix` / the web bridge on its default port) so the
dev server's proxy reaches the real APIs.

## Build (the whole pipeline)

```sh
cd apps/dashboard
npm install      # first time only
npm run build    # -> crates/relix-web-bridge/dashboard-dist/
```

Then rebuild/boot the bridge as usual. Re-run `npm run build` and commit
the regenerated `dashboard-dist/` whenever the UI changes.

## Mesh policy

The dashboard's board / inbox / crew / runs pages call the coordinator's
product-spine capabilities (`brief.*`, `mandate.*`, `agent.roster_summary`,
`task.recent_events`, `task.stuck`). The mesh runs default-deny, so the
boot scripts (`scripts/relix-mesh-up.ps1` / `.sh`) grant the `web-bridge`
caller (`chat-users` group) allow-rules for those methods when they
generate the run policy. Per-agent Key gates (tenant / manage / assign)
still apply inside each capability — the policy rule only lifts the mesh
default-deny so the operator console can reach the spine. If you run a
custom policy, add the same `[[rules]]` (see the boot script) or the
spine pages will return 502 (`default_deny`).

## Known backend gaps (UI degrades, not faked)

- **List-all mandates**: there is no list endpoint — only
  `mandate.search` (requires a non-empty query). The Company page shows a
  search box and only loads mandates on search.
- **Crew list when empty**: `/v1/spine/roster` is a count summary
  (`{active,total}`); the Operative *list* is `/v1/agents/access`. With no
  hired crew both are empty and the pages show empty states.

## Stack

- React 18 + react-router-dom 6
- Vite 5 + TypeScript 5 (strict)
- No UI framework — a small hand-written B&W design system in
  `src/styles.css`.
