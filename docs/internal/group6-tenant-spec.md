# Group 6 Fix Spec — cross-tenant data leakage in storage schemas

The tenant boundary is plumbed through the handlers but several underlying tables have no tenant_id column, so the data layer leaks across tenants even when the handler is correct. Add tenant_id to every table storing per-caller data, populate it on write from the caller's verified tenant, and filter every read by the caller's verified tenant.

## Known-leaking tables (from the audits — confirm and fix each)
- crates/relix-runtime/src/metrics/store.rs ~line 185 — metrics_invocations has no tenant_id. A tenant-A operator querying by session_id gets every tenant's metrics.
- crates/relix-runtime/src/observability/sinks.rs ~line 382 — observability schema (session_timeline etc.) has no tenant_id. Cross-tenant event read by session_id.
- crates/relix-runtime/src/workflow/chronicle.rs ~line 106 — workflow_executions has no tenant_id.
- crates/relix-runtime/src/training/store.rs ~line 668 — training_interactions has no tenant_id.

## Sweep requirement
Do NOT stop at those four. Grep every CREATE TABLE across crates/relix-runtime for any table that stores per-caller or per-session data and lacks a tenant_id column. For each table found, either add tenant_id + filter, or classify it with a one-line reason why it is tenant-neutral (e.g. global config, node-local infra with no caller data). Print the full classified list.

## The fix per table
1. Add a tenant_id column via a migration.
2. On every INSERT/write, populate tenant_id from the caller's verified tenant (the same verified-context source the handlers already use — never from the wire body).
3. On every SELECT/read that returns per-caller data, filter by the caller's verified tenant. A caller must never receive another tenant's rows, even when querying by a known session_id or other shared key.

## Migration safety (critical — Relix has two migration schemes and some tables skip pragmas)
- Use the project's MODERN migration-id scheme, not the legacy integer-version scheme. If a target table is currently tracked by the legacy scheme, do not mix — follow whatever the surrounding module already uses and note it.
- The migration must be idempotent: running it twice does not error or double-apply.
- Existing rows (written before the column existed) must get a sane explicit default tenant, not NULL that later reads choke on. Document what that default is and why it is safe (these are pre-multi-tenant rows; a reserved "legacy" or "default" tenant they can be attributed to).
- No data loss: existing rows survive the migration with their data intact.
- Apply the standard project pragmas to any of these stores that currently skip them (WAL, busy_timeout, FK, synchronous) — the audit noted some metrics/cost stores bypass crate::db::apply_pragmas.

## Done when ALL printed in the transcript
1. Test: two tenants (A and B) write to metrics_invocations; a query as tenant A returns ONLY A's rows, even when A supplies a session_id that exists under B. Passing, shown in cargo test output.
2. The same cross-tenant isolation test for observability (session_timeline), workflow_executions, and training_interactions — each proving tenant A cannot read tenant B's rows. Passing.
3. Test: the migration is idempotent (run it twice, no error) and existing pre-migration rows are attributed to the documented default tenant, not lost. Passing.
4. Grep over crates/relix-runtime printing every CREATE TABLE storing per-caller/per-session data, each now with tenant_id, and every tenant-neutral table classified with a one-line reason.
5. cargo test for relix-runtime exits 0, and all pre-existing tests still pass — shown in the output.
6. One commit. Message: "fix(security): add tenant_id to per-caller storage schemas and filter all reads by verified tenant". Anshul Raman sole author, no co-author trailers, no Claude attribution.

## Constraints
Filter by VERIFIED tenant context, never a wire-supplied tenant_id (same rule as the identity fix). Do not break single-tenant/default deployments — a deployment with one tenant must still read its own rows normally. Do not touch crates outside relix-runtime. No unrelated refactors. Use one migration scheme consistently. Stop conditions must show real cross-tenant denial, not just that the column exists.
