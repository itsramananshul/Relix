# Relix SQLite Lock Ordering

Every persistent store in Relix wraps its `rusqlite::Connection`
inside `Arc<Mutex<Connection>>`. The runtime never opens nested
SQLite transactions across stores, but multiple stores ARE
manipulated within the same call stack (e.g. the cron scheduler
records a task event and then stamps the cron row). To keep that
safe under concurrent load — and to make the convention auditable
for future contributors — Relix enforces a **canonical lock
ordering**.

## Canonical order

When two or more SQLite connections are touched within the same
logical operation, locks MUST be acquired in this order:

1. **`coordinator`** — `crates/relix-runtime/src/nodes/coordinator/mod.rs` (`TaskStore`)
2. **`memory`** — `crates/relix-runtime/src/nodes/memory/mod.rs` (`MemoryStore`, shared with the embeddings store)
3. **`plugin_registry`** — `crates/relix-runtime/src/plugin/registry.rs` (`PluginRegistry`)
4. **`messaging`** — `crates/relix-runtime/src/nodes/coordinator/messaging/store.rs` (`MessageStore`)
5. **`agent_store`** — `crates/relix-runtime/src/nodes/coordinator/agent/store.rs` (`AgentStore`)
6. **`cron_store`** — `crates/relix-runtime/src/nodes/coordinator/cron/store.rs` (`CronStore`)
7. **`session_store`** — `crates/relix-telegram/src/session_store.rs` (`SqliteSessionStore`)

These are the *types*; the order is by `StoreId` rank in
[`relix_runtime::db::lock_order::StoreId`].

A request for the same store twice is allowed but suspect: every
public method on a store acquires the connection mutex, does its
work, and releases it before returning. Two calls in a row do NOT
hold the lock concurrently; if you find yourself nesting one
store call inside another store's lock, you have a deadlock risk
— flatten it.

## Why this exact order

It matches the natural dependency direction in Relix's task
model:

- **Tasks** (coordinator) sit at the top — every persistent
  artefact in the mesh is, at some point, a task event or a
  task row.
- **Memory** (per-session conversation history + embeddings)
  is referenced from tasks via `session_id`, never the other
  way around.
- **Plugin registry** is referenced from tasks when a flow
  spawns a plugin capability; the registry is never queried
  from inside a memory operation.
- **Messaging**, **agent**, **cron**, and **session** stores
  are all leaves — they reference tasks but tasks never
  reference them.

The order therefore captures the natural producer → consumer
flow. Acquiring "down" the stack (top → bottom) means deadlock
is impossible because every thread that reaches a lower-rank
store has already released the higher-rank locks above it.

## Operations that touch multiple stores today

| Operation | Stores touched (order) | File |
|---|---|---|
| Cron firing | `coordinator` → `cron_store` | `nodes/coordinator/cron/scheduler.rs::fire_now` |
| Approval expire loop | `agent_store` → `coordinator` (sequential, not nested) | `controller_runtime.rs::run_approval_expire_loop` |
| Retention scheduler | `coordinator` (only) | `controller_runtime.rs::run_retention_loop` |

Each of these touches stores **sequentially** — the previous
mutex is released before the next is acquired. That's the
preferred shape; nested locks across stores are not used today
and shouldn't be introduced.

## How to audit for violations

1. **Grep for `.lock()` calls in the same function.** Any
   function that calls `.lock()` on two different store mutexes
   must drop the first guard before acquiring the second.

   ```text
   rg --type rust "\.conn\.lock\(\)" crates/relix-runtime/src/
   ```

2. **Check that store methods don't call other store methods
   while holding their own lock.** Public methods on `TaskStore`
   acquire the `TaskStore` connection mutex at the top of the
   function; if they call into `MemoryStore` or `AgentStore`
   inside that scope, the cross-store call could deadlock if
   another thread acquires the locks in the reverse order.

3. **Run the lock-order property test.** The runtime crate's
   `db::lock_order::tests::acquire_in_canonical_order_does_not_deadlock`
   test exercises two threads acquiring locks down the canonical
   order; if it ever hangs, a future change has broken the
   discipline.

## Enforcement helper

`relix_runtime::db::lock_order::StoreId` ranks each store. In
debug builds, `LockOrderRecorder::acquire(id)` records the
currently-held rank for the current thread and `debug_assert!`s
that the new acquisition is ranked higher than any prior one. The
helper is purely advisory — it doesn't change the runtime
behaviour, but if a future refactor introduces a reverse-order
acquisition the debug assert fires in tests and CI.

Release builds skip the rank check; the cost is one
`thread_local!` access in debug mode and nothing in release.

## When to update this doc

- **A new SQLite-backed store ships.** Decide where it sits in
  the canonical order (with the dependency rule above as the
  guide) and add it to the list. Bump every store ranked
  below it.
- **A cross-store transaction lands.** Document the operation
  in the "Operations that touch multiple stores" table with
  the exact acquisition order.
- **Lock ordering changes.** Update both this doc AND the
  `StoreId` enum.
