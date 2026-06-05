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
`delete_events` (default false), `delete_artifacts` (default false).

## Cleanup history (audit)

Every prune attempt — dry-run, refusal, failure, or success — records a
durable row in `maintenance_audit` (bounded to the last 500). The dashboard
*Maintenance & storage* panel shows a **Cleanup history** table; the API is:

```sh
curl -s 'http://127.0.0.1:19791/v1/maintenance/audit?limit=50' -b "relix_session=<cookie>"
```

Each row has: `ts`, `action`, `trigger` (manual / scheduled), `dry_run`,
`deleted_workspaces`, `deleted_bytes`, `pruned_events`, `pruned_artifacts`,
`status` (ok / refused / failed), and a short `note`. No secrets are stored.

## Scheduled (autonomous) cleanup

Off by default. When enabled, a coordinator timer runs the same safe prune
periodically and audits every tick. **Even when enabled it defaults to
dry-run** — you must explicitly opt into a real delete.

| env var | default | meaning |
|---|---|---|
| `RELIX_MAINTENANCE_AUTOPRUNE_ENABLED` | `false` | turn the timer on |
| `RELIX_MAINTENANCE_AUTOPRUNE_DRY_RUN` | `true` | preview-only unless set false |
| `RELIX_MAINTENANCE_AUTOPRUNE_INTERVAL_SECS` | `86400` | tick interval (min 60) |
| `RELIX_MAINTENANCE_AUTOPRUNE_OLDER_THAN_DAYS` | `7` | age threshold |
| `RELIX_MAINTENANCE_AUTOPRUNE_KEEP_LATEST` | `10` | newest kept |
| `RELIX_MAINTENANCE_AUTOPRUNE_DELETE_WORKSPACES` | `true` | remove workspace dirs |
| `RELIX_MAINTENANCE_AUTOPRUNE_DELETE_EVENTS` | `false` | also prune transcript rows |
| `RELIX_MAINTENANCE_AUTOPRUNE_DELETE_ARTIFACTS` | `false` | also prune artifact rows |

The same safety rules apply (never a running run, never an unsafe root,
never the repo). The maintenance summary + dashboard show whether scheduled
cleanup is enabled and in dry-run vs real-delete mode, and warn loudly if it
is set to real-delete.

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
archive never leaves your machine. Add `-ListContents` / `--list-contents`
to print what went in.

**Not backed up by default:** run workspaces (regenerable sandboxes), build
output, `.git`, logs, and secrets (`bridge-token`, `dashboard-admin.json`,
`*.key`, `*.aic`, `.env*`, `dev-keys`). Pass `-IncludeWorkspaces` /
`--include-workspaces` or `-IncludeSecrets` / `--include-secrets` to add them
intentionally.

## Restore from a backup

Restore is **manual** (the scripts never auto-overwrite — destructive
restore is yours to run deliberately). Stop the mesh, then expand the
archive back over the data directory:

```powershell
# Windows PowerShell:
.\scripts\relix-mesh-down.ps1
# inspect first (safe):
Expand-Archive .\backups\relix-backup-<stamp>.zip -DestinationPath restore-preview
# then restore in place (overwrites dev-data in the current directory):
Expand-Archive .\backups\relix-backup-<stamp>.zip -DestinationPath . -Force
.\scripts\relix-mesh-up.ps1
```

```sh
# macOS / Linux:
./scripts/relix-mesh-down.sh
tar -tzf backups/relix-backup-<stamp>.tar.gz   # inspect first
tar -xzf backups/relix-backup-<stamp>.tar.gz   # extract in place
./scripts/relix-mesh-up.sh
```

If the backup excluded secrets, your admin/token files won't be restored —
re-run setup or `scripts/relix-dashboard-admin-reset.ps1` afterward.

## Forgot the dashboard admin password?

```powershell
.\scripts\relix-dashboard-admin-reset.ps1        # generate a new password
```
…then restart the bridge. Local operator recovery only — see the
operator-console section of the README.

## Run reliability: recovery, usage, runtime state, live events

The run/Brief execution spine is built to survive a bridge/coordinator
restart and to be observable from the operator console.

**Boot recovery (automatic).** On coordinator startup, any `brief_runs`
row still marked `running` that has no live in-process child (always the
case after a crash/restart — the in-memory cancel registry starts empty)
is reconciled: marked terminal `failed` with the reason *"Recovered after
process restart; no live child process was found."*, given a `recovered`
transcript event + a `brief.run_recovered` Chronicle note, and its Brief
Claim released so the work can be re-dispatched. Genuinely live runs and
already-terminal runs are never touched. No operator action is required;
the recovery is logged via tracing.

**Per-run usage / cost.** When an adapter emits structured output
(Claude stream-json, Codex JSONL), each run's tokens / model / cost /
`session_id` are captured onto the `brief_runs` ledger row and surfaced
on `GET /v1/runs`. Adapters that emit nothing (echo / raw) leave these
**null** — the values are never fabricated.

**Persistent adapter runtime state.** Per `(tenant, agent, rig, brief)`
the coordinator keeps a resumable `session_id`, accumulated token/cost
totals, and the last run's status/error in an `agent_runtime_state`
table. Inspect or reset it (tenant-scoped, auth-gated):

```bash
# read all runtime-state rows for one Operative
curl -s "http://127.0.0.1:19791/v1/runs/runtime-state?agent_id=<agent_id>" \
  -H "Authorization: Bearer <token>"

# forget it (force a fresh adapter session) — whole agent, or one Brief
curl -s -X POST "http://127.0.0.1:19791/v1/runs/runtime-state/reset" \
  -H "Authorization: Bearer <token>" -H "Content-Type: application/json" \
  -d '{"agent_id":"<agent_id>"}'                       # whole agent
  # -d '{"agent_id":"<agent_id>","brief_key":"<brief_id>"}'  # one Brief
```

Note: the `session_id` is **stored, not yet replayed** into the next
adapter spawn — resume wiring is future work.

**Live execution event stream (SSE).** A tenant-scoped feed of execution
transitions (`run_started` / `run_finished` / `run_cancel_requested` /
`brief_moved` / `review_changed` / `apply_changed`), with keep-alive
pings:

```bash
curl -N "http://127.0.0.1:19791/v1/runs/events/stream" \
  -H "Authorization: Bearer <token>"     # add ?since=<event_id> to resume
```

It is a ~750ms poll over the Chronicle (not push); fine-grained
per-transcript events stay on the per-run `GET /v1/runs/:id/events`.

## Honest limitations

- The maintenance summary + prune are **operator-global** (a single bridge
  admin), so run counts are not tenant-scoped — disk/log usage is a global
  concern. Prune operates on disk workspaces (not tenant-labeled on disk).
- Log-row pruning currently targets the runs whose **workspace** is eligible
  for pruning; it deletes only `run_events` / `run_artifacts` rows, never the
  `brief_runs` ledger row.
- The maintenance audit is durable (`maintenance_audit`, last 500 rows) but
  records prune attempts only — `summary` reads aren't audited.
- The workspace scan is bounded (caps the directory count + files walked);
  for an enormous tree the reported figures are a floor (`truncated:true`).
