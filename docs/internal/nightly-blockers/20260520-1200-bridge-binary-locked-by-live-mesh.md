# Blocker: bridge binary re-link blocked by live dev mesh

## Subsystem

Bridge binary tests (`cargo test --package relix-web-bridge`) +
any change that touches `src/main.rs` or modules consumed by it
(topology.rs, dashboard.html, tasks.rs handlers).

## What I was trying to do

Run the bridge crate's binary-level unit tests after adding
`/v1/health` + `HealthResponse` to `crates/relix-web-bridge/src/topology.rs`,
to confirm the bridge-side serialization matches the contract
the CLI consumes.

## Why I stopped this item

A live dev mesh from a prior interactive session has 4
`relix-controller` PIDs and 1 `relix-web-bridge` PID running on
this machine:

```
17916 relix-controller
22576 relix-controller
36984 relix-controller
47232 relix-controller
39048 relix-web-bridge
```

`cargo test --package relix-web-bridge` (with or without `--no-run`)
attempts to re-link `target/debug/relix-web-bridge.exe`, fails with
Windows `os error 5: access is denied` because the running bridge
process is holding the file open.

Two reasons not to just `taskkill` these PIDs:

1. **The user's interactive session may still be using them.** The
   processes were not spawned by this autonomous session; killing
   them would silently interrupt whatever the user was doing.
2. **The architecture invariants from the user's directive say
   nothing about killing processes.** Even in autonomous mode,
   "killing the user's running infra" is a step that warrants
   confirmation.

## Options considered

1. **`taskkill /F /PID <pid>` the bridge process** to free the
   binary lock. Rejected without permission: risk of disrupting an
   active user session.
2. **Wait for the user to stop the mesh.** Out of band; the
   directive says "rotate continuously," so blocking on this
   isn't tenable.
3. **Skip bridge binary tests for now; verify the new code via
   CLI consumer tests + library tests.** Chosen. The
   `HealthResponse` JSON shape is contract-tested from the CLI
   consumer side (`crates/relix-cli/src/topology.rs::tests`), and
   the lib-only `freshness_label` tests cover the bucket math.

## What we shipped despite the block

- `feat(health): /v1/health endpoint + relix-cli topology health`
  (commit `8840095`). Code compiles cleanly via
  `cargo clippy --workspace`. CLI consumer tests pass. The bridge
  binary's bind/test cycle is the only thing that hit the lock.
- The non-binary slice of work in this rotation kept moving:
  multi-node-bringup.md doc, failure-modes.md doc, runtime-
  observability.md update, task-api.md update.

## What unblocks this

Either of:

- The user runs `taskkill /F /IM relix-web-bridge.exe ; taskkill /F /IM relix-controller.exe`
  (or the POSIX equivalent if running under WSL), then says go.
- The user explicitly authorizes the autonomous session to stop
  leftover dev processes when they hold a build lock.

Once unblocked: `cargo test --package relix-web-bridge` should
run clean, and the next bridge change in the rotation can verify
end-to-end.

## Architectural note

The block reveals one operational gap worth noting (not fixing
here): `scripts/relix-mesh-up.{sh,ps1}` traps Ctrl-C and stops
its spawned PIDs, but if the operator kills the terminal or the
SSH session dies, the children are orphaned. Cleanup is manual
(`pkill` / `taskkill`). A graceful shutdown hook or PID-file
based reaper is a worthwhile follow-up.
