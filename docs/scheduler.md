# Scheduler

_Version: 0.4.1_

Lets agents (and operators) schedule their own future work.
"Send me a summary at 9am every Monday." "Re-run this flow in
30 minutes." "Fire once on June 1st." Lives on the
**coordinator** node — there's no new node type — and runs as a
background tokio task next to the existing task ledger.

The wire capabilities are spelled `cron.*` (that's what the mesh
policy admits and what flows call). The dashboard, the
`relix-cli ops cron` subcommand, and the bridge surface
(`/v1/cron/jobs`) all share that prefix. "Scheduler" is the
broader concept; "cron" is the verb on the wire.

## What it does

1. Operators (or agents) create cron jobs via `cron.create` /
   `POST /v1/cron/jobs` / `relix-cli ops cron create`.
2. A background loop on the coordinator ticks every 30 s (default).
3. On every tick, it queries `cron_jobs` for enabled rows with
   `next_run_at <= now()`.
4. For each due job, it:
   - skips if the previous task is still `running` (logged WARN),
   - mints a coordinator task with title `cron:<name>` and
     `origin_surface = "scheduler"`,
   - writes a `cron.job_fired` chronicle event carrying
     `job_id|job_name|run_count`,
   - advances the job's `last_run_at`, `next_run_at` (per the
     schedule format), and `run_count`,
   - spawns the AI dispatch in the background — wrapped in
     `tokio::time::timeout(max_job_secs)` — and writes the AI
     reply to the task chronicle as `cron.job_result` then flips
     the task to `completed` / `failed`.

One-shot jobs (RFC 3339 schedule) are disabled (`enabled = 0`)
after their first fire. Duration + cron jobs keep firing.

## Schedule formats

Three formats, recognised by shape:

### Duration

`<number><unit>` where unit is one of `s` (seconds), `m` (minutes),
`h` (hours), `d` (days), `w` (weeks).

```
30m
2h
1d
7d
```

Re-fires every interval forever (until disabled / deleted).

### Cron (5-field)

Standard `minute hour day-of-month month day-of-week`. Each field
is either `*` (any value) or a single explicit integer. Ranges
(`9-17`), step values (`*/15`), and lists (`1,3,5`) are **not**
supported in the alpha — they're rejected by the parser.

```
0 9 * * 1      # Monday 09:00 UTC
0 0 * * *      # daily at midnight UTC
30 14 1 * *    # 14:30 UTC on the 1st of every month
```

All times are UTC.

### One-shot (RFC 3339 instant)

Any valid RFC 3339 timestamp.

```
2026-06-01T09:00:00Z
2026-12-31T23:59:59+05:30
```

Fires once when reached; the scheduler then sets `enabled = 0`.
If you create a one-shot whose timestamp is already in the past,
it fires on the next tick rather than vanishing.

## Enabling the scheduler

Add a `[coordinator.cron]` section to the coordinator's config:

```toml
[coordinator.cron]
enabled = true
tick_secs = 30
max_concurrent = 3
max_job_secs = 300

[coordinator.cron.ai_peer]
addr = "/ip4/127.0.0.1/tcp/19712"
alias = "ai"
deadline_secs = 60
```

| Field            | Default | What it does |
|------------------|---------|---|
| `enabled`        | `true`  | Master switch. The presence of `[coordinator.cron]` is enough to opt in; set `enabled = false` to register the cron capabilities without spawning the background loop. |
| `tick_secs`      | `30`    | Seconds between scheduler ticks. |
| `max_concurrent` | `3`     | Hardening: maximum jobs the scheduler will fire in flight. Excess due jobs wait for the next tick. |
| `max_job_secs`   | `300`   | Hardening: per-job hard timeout. The AI dispatch is wrapped in `tokio::time::timeout(max_job_secs)`. Exceeded jobs flip the task to `failed` with cause `"ai dispatch exceeded max_job_secs"`. |
| `ai_peer`        | none    | Optional outbound AI peer config (`addr`, `alias`, `deadline_secs`). When absent the scheduler still fires jobs but the AI dispatch is skipped and tasks flip to `failed` with cause `"ai dispatcher unset"`. |

When the section is **missing** the cron capabilities are still
registered (operators can create jobs ahead of time) but the
background loop is not spawned.

## Creating a job

### Dashboard

`#/cron` in the Operate sidebar. Fill in the form: name, schedule,
prompt, subject_id. Hit **Create**.

### CLI

```
relix-cli ops cron create \
  --name daily-summary \
  --schedule "0 9 * * *" \
  --prompt "summarise the last 24h of activity" \
  --subject-id alice
```

`--flow-template` defaults to `flows/chat_template.sol`.

### HTTP

```
POST /v1/cron/jobs
Content-Type: application/json

{
  "name": "daily-summary",
  "schedule": "0 9 * * *",
  "flow_template": "flows/chat_template.sol",
  "prompt": "summarise the last 24h of activity",
  "subject_id": "alice"
}
```

### Direct capability call

```
cron.create  arg: name|schedule|flow_template|prompt|subject_id
```

## Triggering manually

`relix-cli ops cron trigger --job-id <id>` creates the coordinator
task immediately and spawns the AI dispatch. Returns the new
`task_id`. The dashboard's Scheduled Jobs panel offers the same
per-job trigger (and a New Job form), backed by
`POST /v1/cron/jobs/:job_id/trigger`.

Watch the chronicle land with `relix-cli task watch <task_id>` (or
`GET /v1/tasks/<task_id>/events/stream`).

## Hardening

| Concern | What the scheduler does |
|---|---|
| Runaway flows | `max_job_secs` (default 300s) hard timeout via `tokio::time::timeout`. Exceeded jobs flip to `failed`. |
| Concurrent fires | `max_concurrent` (default 3) semaphore. Excess due jobs are deferred to the next tick rather than queued. |
| Pile-ups | If the previous task is still `running`, the next fire is skipped with a WARN log. |
| AI peer unreachable | `tokio::time::timeout` returns `Err` → task flips to `failed` with cause; the cron row's `last_status` records the failure. The scheduler tick stays short. |
| Coordinator crash | The cron loop is just a tokio task — when the coordinator restarts the loop respawns. `next_run_at` is durable in SQLite so missed deadlines fire on the next tick. |

The scheduler **never crashes the coordinator** on a job failure.
Every failure path is `tracing::warn!` or `tracing::error!` and
the loop keeps ticking.

## What happens to the result

Two chronicle events land on the task:

- `cron.job_fired` (written before the AI dispatch) — payload
  `job_id=<id>|job_name=<name>|run_count=<n>`.
- `cron.job_result` (written after the AI dispatch) — payload
  `ok=1|chars=<n>|preview=<first 200 chars>` on success,
  `ok=0|cause=<reason>` on failure.

The task's `latest_result` column holds an 800-char preview of the
full AI reply. Telegram-side delivery is a follow-up; for now,
results land in the task chronicle, readable via
`relix-cli task get <id>` or `GET /v1/tasks/<id>`.

## Capability surface

| Method        | Arg                                                    | Return |
|---------------|--------------------------------------------------------|--|
| `cron.create` | `name\|schedule\|flow_template\|prompt\|subject_id`    | `<job_id>\n` |
| `cron.list`   | `<subject_id>` (empty = all)                           | rows + `count=N\n` |
| `cron.get`    | `<job_id>`                                             | pipe-delim `key=value` body |
| `cron.update` | `<job_id>\|<field>\|<value>` (field ∈ enabled/schedule/prompt) | `ok\n` |
| `cron.delete` | `<job_id>`                                             | `ok\n` |
| `cron.trigger`| `<job_id>`                                             | `<task_id>\n` or `skipped previous_task_id=...\n` |

The bridge proxies all six as JSON at `/v1/cron/jobs[/...]`.
