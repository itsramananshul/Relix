# Production deployment checklist (M78)

This is the **operator-runnable** checklist for getting a Relix
deployment past the laptop-demo posture and into something that
can safely face other humans or untrusted traffic.

Companion to [`deployment.md`](deployment.md) (architecture +
mode descriptions) and [`security.md`](security.md) (what the
admission pipeline enforces). This doc is intentionally
checkbox-style — each item is a concrete verification or
configuration operators perform before traffic lands.

## Required env vars (hard requirements, not optional)

These env vars are **fail-closed** — absent or invalid values cause the
relevant subsystem to deny every call. Set them before starting the
coordinator.

- [ ] **`RELIX_APPROVAL_SIGNING_KEY` set on every coordinator that
      issues approval tokens.** 64 hex chars = 32 raw bytes (Ed25519
      seed). Generate with `openssl rand -hex 32`. An empty or absent
      key causes `approval_token_missing_key` on every token-bearing
      call — the entire approval fast-path is inoperative.
- [ ] **`RELIX_CREDENTIAL_KEY` set when `[credentials] enabled = true`.**
      Arbitrary secret string used as the implicit `v1` vault key.
      Absent key → `CredentialError::NoActiveKeyVersion`; vault refuses
      to open.
- [ ] **Bridge bearer token readable by the coordinator and any
      operator tooling.** Auto-generated at first bridge boot to
      `~/.relix/bridge-token` (mode 0600). The `relix doctor` command
      checks this file's permissions. Pass the token to HTTP clients via
      `Authorization: Bearer <token>`; never put it in a URL query param
      (the bridge rejects those with 400).

## Pre-flight: identity + transport

- [ ] **Org root mint is offline.** `relix-cli identity init-org`
      run on an isolated machine; the resulting `.key` lives in
      an HSM / secret manager. The org root is the trust anchor
      for every identity bundle.
- [ ] **Org root pubkey distributed via secure channel.**
      `<run>-org-root.pub` is checked into per-host config OR
      delivered via your secret-distribution path. Treat as
      integrity-critical, not secret.
- [ ] **Each node has a unique signed identity bundle.**
      `relix-cli identity mint` per node. Bundles expire (default
      24h); cron / systemd-timer / k8s job re-mints before
      expiry. The runtime reads from disk on each connect —
      atomic-replace the file and the new bundle is picked up at
      the next mesh dial.
- [ ] **libp2p keys generated locally, never shared.** Each
      node's `<node>.key` MUST stay on that node. Auto-generated
      on first boot; back up if you want stable peer IDs across
      reinstalls.
- [ ] **`peers.toml` minimal.** Only the aliases each node
      actually needs to dial. Aliases not in the file → no dial
      attempt → smaller blast radius if an alias's identity
      bundle is compromised.

## Network posture

- [ ] **Bridge binds to loopback (`127.0.0.1`).** Confirmed via
      `[bridge] listen_addr = "127.0.0.1:19791"`. The bridge enforces
      bearer-token auth + CSRF guard on all non-public routes, but
      for any internet-facing exposure a reverse proxy with TLS + external
      auth is still mandatory. At startup the bridge emits a WARN when
      `listen_addr` is not loopback — that warning is the operator's last
      reminder before traffic flows. See [`deployment.md`](deployment.md)
      for the full auth posture.
- [ ] **Reverse proxy with TLS + auth.** nginx / Caddy / cloud
      LB / cloudflare-tunnel — whichever fits. mTLS for
      machine-to-machine, OAuth or session for human-to-machine.
      Document the chosen auth surface in your runbook.
- [ ] **Outbound egress filter for tool node.** Host-level
      iptables / nftables / Windows firewall rules block RFC
      1918 + link-local from the tool node's UID. Defense in
      depth on top of the SSRF guards.
- [ ] **Coordinator backup network reachable.** If the
      coordinator's SQLite DB lives on a network volume, that
      volume is on the same network the coord controller can
      reach. The coordinator is fail-soft: chat keeps working
      when coord is unreachable, but the ledger silently stops
      accepting writes.

## Secret storage

- [ ] **`bridge-secrets.toml` mode 0600.** Bridge enforces
      this on POSIX; verify after first write. The file holds
      operator-supplied provider keys + Telegram bot token,
      plus the M58 last-test cache + M69 quarantine state + M77
      routing-trace counters.
- [ ] **`bridge-secrets.toml` excluded from backups OR backed up
      under encryption.** It's a secret. Standard tooling (restic,
      borgbackup, rsync with age) handles either model.
- [ ] **No provider keys in TOML configs.** The bridge stores
      keys in `bridge-secrets.toml`; the AI controller reads
      from env vars referenced by `bot_token_env`. Neither
      should appear in the controller TOML or git history.
- [ ] **Audit log accessible to operators only.** Per-node
      `audit.log` files contain identity-bound RPC records.
      Same secret posture as bridge-secrets.toml.

## Policy + RBAC

The alpha ships a permissive default policy. **Production must
tighten it** before going live.

- [ ] **Default group `chat-users` split.** Production groups
      typically include:
      - `bridge` — what the HTTP bridge can call (chat-side
        capabilities).
      - `telegram` — what the channel controller can call
        (read-only task surfaces).
      - `operator` — operator interventions
        (`task.recover` / `task.retry` / `task.cancel` /
        `task.pause` / `task.resume` / `task.freeze` /
        `task.unfreeze` / `task.note` /
        `task.mark_investigation` / `task.record_*` /
        `task.observe_interruption`). Restrict to operator
        identities only.
      - `runtime` — workers that attest cross-task edges and
        cooperative-interruption observations. Restrict to
        runtime-process identities only.
- [ ] **Policy decisions audited.** Per-node `audit.log`
      records every allow / deny decision with the caller's
      verified subject_id. Confirm log rotation is in place
      (`logrotate` / Windows Event Log policies); the runtime
      never truncates.
- [ ] **Auth at the HTTP boundary verified.** The bridge
      enforces bearer-token auth on `/v1/config/*`, `/v1/tasks/*`,
      and all other non-public endpoints. Confirm that the bridge
      bearer token from `~/.relix/bridge-token` is rotated into
      your reverse proxy / automation credentials and is NOT
      committed to any repository. The proxy's external auth identity
      becomes the bridge's "operator" surrogate; operator intervention
      audit ring (M57) ties every mutation to a correlation_id (M68)
      that operators grep across the audit log.

**Honest scope:** there is no built-in RBAC API yet. The
operator-intervention surfaces (M57/M60/M62/M65/M71/M72/M76)
all record the verified caller's subject_id in the chronicle
event's `payload_json.author` field. A future RBAC layer would
gate the API endpoints by group membership; today the deny
posture is enforced by your reverse proxy + the coordinator's
policy engine.

## Operational hygiene

- [ ] **Chronicle retention plan.** `task_events` grows
      unbounded. The retention design lives in
      [`chronicle-retention.md`](chronicle-retention.md); the
      destructive deletion path has not shipped. Plan disk
      capacity accordingly until it lands. The bridge's
      `compact_events` capability returns a dry-run count so
      operators can size the eventual deletion.
- [ ] **Per-task `max_runtime_secs` set.** Without it, the
      coordinator's recovery scan won't promote runaway tasks
      to `interrupted` and dashboards show forever-running
      flows. Set per-task at `task.create` time or fall back to
      a sensible default (15 min for chat flows is common).
- [ ] **Provider quarantine + cooldown documented in runbook.**
      M69's `PUT /v1/config/providers/:name/quarantine`
      endpoint is operator-visible at the bridge.
      **Restart-required** for the AI controller to honor the
      flag — the bridge enforces it at the test-provider
      endpoint immediately, but the AI controller still reads
      provider config at startup. Your runbook should call out
      the restart.
- [ ] **Backup script for `bridge-secrets.toml` + coordinator
      SQLite + per-node `audit.log`.** Standard rsync /
      borgbackup / restic / s3 sync works — these are flat
      files. The coord SQLite supports `.backup` via
      `sqlite3` CLI for online consistency.
- [ ] **Restore test rehearsed.** Restore the three above to a
      fresh host, point a bridge at the restored secrets file,
      verify `/v1/health` reports expected providers + the
      task ledger reads back at expected count. Run this once
      per change to backup tooling.

## Capability footprint

Each opt-in capability widens the blast radius. Review and
enable only what your flows actually need.

- [ ] **`[tool.fs]` jail directory reviewed.** Confirm the
      jail base path is what you intend; the four fs
      capabilities (read/write/search/patch) operate inside it.
      Files outside are unreachable by design.
- [ ] **`[tool.web_fetch]` allow_http audit.** Default `false`
      means HTTPS-only. If you set `true`, document why and
      restrict via policy to the smallest group that needs it.
- [ ] **`[tool.pdf]` cost class audit.** Pure parser, no
      network, but pulls in significant dependencies. Disable
      via missing config section if unused.
- [ ] **`[tool.terminal]` allowlist audit (CW1).** Highest
      blast radius — sandboxed shell execution. The allowlist
      MUST list only the binaries your flows actually need
      (bare program names, no paths). Default `inherit_env =
      false` keeps controller secrets out of spawned children;
      flip to `true` only when you've audited what your
      controller's env contains.
- [ ] **Capability descriptor sensitivity tags reviewed.**
      `tool.terminal.run` carries `shell:execute`,
      `host:local`, `destructive:potential`. Policy engines +
      dashboard surfaces can treat these specially; production
      policy should require explicit group membership for any
      `shell:execute`-tagged capability.

## Monitoring + observability

- [ ] **`/v1/health` polled by your monitor.** JSON shape
      includes uptime, peer counts (fresh/stale/expired),
      reconnect telemetry, active SSE stream count. Alert on:
      `coordinator_configured=false`, `peers_expired > 0`
      sustained, `reconnect.attempts - reconnect.successes` non-
      monotonic.
- [ ] **`/v1/intervention/recent` polled or scraped.** M57's
      ring + JSONL persistence at
      `<data_dir>/bridge-intervention.log.jsonl` gives you
      tamper-evident operator-action history. Tail or pull
      periodically into your SIEM.
- [ ] **`/v1/tasks/events/stream` consumed by at least one
      sink.** M73's firehose SSE. Drop frames mean the consumer
      fell behind — operators should see these in your
      observability stack.
- [ ] **Provider routing-trace ratio thresholds.** M77's
      `failed_request_count` / `success_request_count` per
      provider land in `/v1/config/providers`. Alert when any
      provider's reliability ratio drops below your threshold
      (90% is a sensible starting point).
- [ ] **Task list `stuck?` filter checked periodically.** M53's
      filter narrows to running/retrying tasks older than 120s.
      A spike in stuck tasks is a runtime backpressure signal.
- [ ] **Bridge task persistence fails closed.** Production bridge
      config should set `[coordinator] required = true`. Verify bridge
      startup fails if the coordinator alias is not discovered, and verify
      chat requests return 503 instead of dispatching when `task.create`
      fails. Anonymous execution with no task ledger is a launch blocker.

## Pre-traffic smoke test

Run this once after every deployment change:

```bash
TOKEN=$(cat ~/.relix/bridge-token)

# 0. One-command health check (permissions + bridge + all components)
relix doctor

# 1. Bridge health
curl -s http://127.0.0.1:19791/v1/health | jq '{
  uptime_secs, coordinator_configured, peer_count, peers_expired
}'

# 2. Provider sanity (replace 'openai' with your defaults)
curl -s -H "Authorization: Bearer $TOKEN" \
  -X POST http://127.0.0.1:19791/v1/config/providers/openai/test \
  | jq '{ok, elapsed_ms, detail}'

# 3. Task ledger reads back
curl -s -H "Authorization: Bearer $TOKEN" \
  'http://127.0.0.1:19791/v1/tasks/cursor?limit=5' | jq '.items[0:5]'

# 4. Operator intervention audit accessible
curl -s -H "Authorization: Bearer $TOKEN" \
  'http://127.0.0.1:19791/v1/intervention/recent?limit=5' \
  | jq '.items | length'

# 5. Firehose responds (close after 1 event or 2 seconds)
timeout 2 curl -sN -H "Authorization: Bearer $TOKEN" \
  'http://127.0.0.1:19791/v1/tasks/events/stream' \
  | head -n 1

# 6. State machine self-check (dashboard is public, no token required)
curl -s 'http://127.0.0.1:19791/dashboard' > /dev/null && echo "dashboard OK"

# 7. Approval signing key present (if coordinator uses approval tokens)
[ -n "$RELIX_APPROVAL_SIGNING_KEY" ] && echo "RELIX_APPROVAL_SIGNING_KEY set" \
  || echo "WARNING: RELIX_APPROVAL_SIGNING_KEY not set"

# 8. Credential vault key present (if credentials enabled)
[ -n "$RELIX_CREDENTIAL_KEY" ] && echo "RELIX_CREDENTIAL_KEY set" \
  || echo "INFO: RELIX_CREDENTIAL_KEY not set (credentials disabled?)"
```

If any of the above fail, **do not flow live traffic**. Check
the bridge log, then the coordinator log, then the relevant
node log.

## Incident response surfaces

When something goes wrong in production, these are the
operator-facing pull points:

| What | Where | Notes |
|---|---|---|
| Recent operator actions | `/v1/intervention/recent` | M57. Includes correlation IDs (M68) for cross-referencing with chronicle events. |
| Chronicle for one task | `/v1/tasks/:id/events` | Full append-only event history. |
| Cross-task firehose | `/v1/tasks/events/stream` | M73 SSE. Real-time. |
| Lineage subtree | `/v1/tasks/:id/lineage` | M66. Walks task_edges in both directions. |
| Subtree metrics | task.subtree_metrics capability | M75. Aggregate roll-up. |
| Per-node admission audit | `<data_dir>/<node>/audit.log` | Hash-chained Ed25519-signed. Tamper-evident. |
| Operator intervention JSONL | `<data_dir>/bridge-intervention.log.jsonl` | M57. Append-only. |
| Provider routing trace | `/v1/config/providers` | M77 counters per provider. |

## What's NOT in this checklist

Deliberately omitted (not yet supported):

- **Auto-failover between providers.** M69 + M77 give operators
  the visibility; live routing still requires AI controller
  restart.
- **Hard process pause/cancel of an executing flow.** M65 / M71
  / M76 ship the cooperative protocol + the suppression of new
  retry attempts; a currently-executing flow runs to completion
  cooperatively or fails on its own. See
  [`interruption-semantics.md`](interruption-semantics.md).
- **Multi-tenant isolation.** Per-tenant resource caps,
  per-tenant audit segregation, per-tenant policy
  customization — all deferred to a future gate. Today: one
  org root = one trust boundary.
- **Auto-scaling.** Add nodes to `peers.toml`, run another
  controller, point at the same org root. Capacity scales
  linearly; no orchestrator-style auto-scaling.

## See also

- [`deployment.md`](deployment.md) — architecture + modes.
- [`security.md`](security.md) — admission pipeline detail.
- [`bridge-invariants.md`](bridge-invariants.md) — what the
  bridge is + is NOT.
- [`current-limitations.md`](current-limitations.md) — alpha
  scope.
- [`chronicle-retention.md`](chronicle-retention.md) —
  durable-state growth model.
- [`interruption-semantics.md`](interruption-semantics.md) —
  cooperative-interruption model.
