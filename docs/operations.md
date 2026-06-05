# Relix local operations

Practical guidance for running Relix locally without slowly filling disk or
losing recoverability. All of this is **local + operator-controlled** — none
of it is a remote/unauthenticated surface.

## Where local state lives

- **Run workspaces** — one scoped sandbox per run at
  `<workspace-root>/<run_id>`. The root is `<db_parent>/workspaces/runs`
  (next to the coordinator DB) unless `RELIX_RUN_WORKSPACE_ROOT` overrides it.
  These accumulate as you run agents and are the main disk consumer.
- **Run ledger + logs** — the coordinator SQLite holds `brief_runs` (one row
  per run), `run_events` (transcripts), and `run_artifacts` (changed-file
  metadata). Events/artifacts grow with usage.
- **Admin + token** — `~/.relix/dashboard-admin.json` (Argon2id) and
  `~/.relix/bridge-token`. Secrets — never back these up casually.

## Check storage

- **Dashboard:** Settings → *Maintenance & storage* shows workspace
  count/bytes, run/event/artifact counts, review-state breakdown, and any
  warnings. The Command Center also surfaces storage warnings.
- **API:** `GET /v1/maintenance/summary` (session/token auth). Bounded,
  symlink-skipping, never scans the repo, graceful when the root is missing.

## Prune old run workspaces (safe cleanup)

Cleanup removes **old run workspaces** (the per-run sandboxes) and,
optionally, the verbose `run_events` / `run_artifacts` rows of those runs.
It **never** deletes:

- your source repo or the configured project root,
- a run that is still **running** (its workspace is always kept),
- the newest `keep_latest` workspaces,
- anything newer than `older_than_days`,
- the `brief_runs` ledger row itself (the run stays visible in `/v1/runs`).

It refuses a shallow / filesystem-root workspace root and never follows
symlinks.

**From the dashboard:** Settings → *Maintenance & storage* → set
*Older than (days)* + *Keep latest N* → **Preview (dry-run)** to see exactly
what would be deleted → type `DELETE` → **Execute cleanup**.

**From the API:**

```sh
# Dry-run (DEFAULT) — reports what WOULD be deleted, deletes nothing:
curl -s -X POST http://127.0.0.1:19791/v1/maintenance/prune \
  -H 'content-type: application/json' \
  -b "relix_session=<cookie>" \
  -d '{"dry_run":true,"older_than_days":7,"keep_latest":10}'

# Real delete — explicit dry_run:false:
curl -s -X POST http://127.0.0.1:19791/v1/maintenance/prune \
  -H 'content-type: application/json' -b "relix_session=<cookie>" \
  -d '{"dry_run":false,"older_than_days":7,"keep_latest":10,
       "delete_workspaces":true,"delete_events":false,"delete_artifacts":false}'
```

Body options (all optional): `dry_run` (default `true`), `older_than_days`
(default 7), `keep_latest` (default 10), `delete_workspaces` (default true),
`delete_events` (default false), `delete_artifacts` (default false). A prune
writes an operator audit line to the bridge/coordinator log.

## Back up local state

```powershell
# Windows — local-only zip of dev-data (DBs + configs), excludes build
# output, .git, run workspaces, logs, and secrets by default:
.\scripts\relix-local-backup.ps1
.\scripts\relix-local-backup.ps1 -IncludeWorkspaces   # also run sandboxes
.\scripts\relix-local-backup.ps1 -IncludeSecrets      # also tokens/keys (careful)
```

```sh
# macOS / Linux:
./scripts/relix-local-backup.sh [--include-workspaces] [--include-secrets]
```

For a **consistent DB backup**, stop the mesh first
(`.\scripts\relix-mesh-down.ps1`) so the SQLite files aren't mid-write. The
archive never leaves your machine.

## Forgot the dashboard admin password?

```powershell
.\scripts\relix-dashboard-admin-reset.ps1        # generate a new password
```
…then restart the bridge. Local operator recovery only — see the
operator-console section of the README.

## Honest limitations

- The maintenance summary + prune are **operator-global** (a single bridge
  admin), so run counts are not tenant-scoped — disk/log usage is a global
  concern. Prune operates on disk workspaces (not tenant-labeled on disk).
- Log-row pruning currently targets the runs whose **workspace** is eligible
  for pruning; it deletes only `run_events` / `run_artifacts` rows, never the
  `brief_runs` ledger row. A durable maintenance audit table is future work
  (today the audit is a tracing log line).
- The workspace scan is bounded (caps the directory count + files walked);
  for an enormous tree the reported figures are a floor (`truncated:true`).
