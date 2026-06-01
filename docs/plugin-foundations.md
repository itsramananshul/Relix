# Plugin + Packaging Foundations

How Relix capabilities are structured, how the shipped
subprocess plugin system works, and the architectural constraints
the system was designed to satisfy. Track 5 of the autonomous
roadmap.

This document has two parts:

1. **Current state (0.4.1)** — the subprocess plugin system is
   shipped. The `plugin_host` node type, `PluginLoader`,
   `PluginDispatcher`, and `relix-plugin-sdk` crate are all
   implemented and in production. See [`docs/plugins.md`](plugins.md)
   for the operator and plugin-author reference.

2. **Historical background** — the constraint analysis (M1–M7)
   and Loading Model Options (A–D) sections below were written
   before the loader landed. They remain accurate as
   architectural rationale and are preserved for context. The
   "sketched, not chosen" framing in the Options section is
   historical: Option B (out-of-process subprocess) is the
   implemented path.

---

## Current state: subprocess plugin system (shipped in 0.4.1)

The capability set on any given controller is determined at
**compile time** for built-in node types. The `plugin_host` node
type adds **runtime-loaded** subprocess plugins on top of the
static capability set.

```
controller binary
├── relix-core           (types, identity, policy, audit)
├── relix-runtime        (libp2p, SOL VM, dispatch bridge, node impls)
│   └── plugin_host node (PluginLoader + PluginDispatcher + registry)
└── (the nodes module owns the registered built-in capabilities)
```

When the bringup script spins up `relix-controller` with
`[controller] node_type = "plugin_host"`, the controller
scans `plugin_dir` at depth 1 for `plugin.toml` manifests,
validates and spawns each plugin as a subprocess, and registers
the plugin's declared capabilities on the dispatch bridge.

### What the loader provides (0.4.1)

- `PluginLoader::spawn` — validates the manifest, gates on
  `binary_sha256` and `publisher_key` signatures, applies the
  Unix sandbox (`RLIMIT_AS`, `RLIMIT_CPU`, `RLIMIT_NOFILE`,
  `RLIMIT_CORE=0`, `PR_SET_NO_NEW_PRIVS`, seccomp BPF on
  Linux), mints a per-plugin bearer token and TLS cert, spawns
  the subprocess, and returns an `Arc<LoadedPlugin>` once the
  health probe succeeds.
- `PluginDispatcher` — speaks the `relix-plugin-v1` wire
  protocol: newline-delimited JSON over loopback TLS with pinned
  cert. No plaintext HTTP path exists.
- `PluginRegistry` — SQLite-backed store tracking
  `(plugin_id, name, version, status, error_message,
  last_seen_at, capabilities)` across reboots.
- `relix-plugin-sdk` — Rust SDK for plugin authors. Handles TLS
  binding, port announcement, bearer validation, and JSON
  framing. Plugin authors call `PluginServer::new()`,
  `.register(method, handler)`, and `.serve()`.

The M6 constraint ("resource bounds enforced by the loader") is
now satisfied by the rlimit + seccomp sandbox. The M5 constraint
("descriptors signed") is satisfied by the `publisher_key` +
`.sig` verification gate for distributed plugins.

---

## Historical background: design constraints (pre-0.4.1)

The sections below document the constraints that shaped the
plugin system design. The "not yet implemented" framing is
historical; the system is now shipped.

### Before the loader: static linkage only

The capability set was determined entirely at compile time. There
was no runtime plugin loading, no dynamic library dlopen, no WASM
sandbox, no remote code download. This was deliberate: every
alpha capability was audited as part of the source tree review.

### The packaging surface that already existed

Three things were plugin-like in the alpha and are worth naming:

#### 1. `CapabilityDescriptor` as the unit of discovery

A capability is a `(method_name, descriptor, handler)` triple
registered on the dispatch bridge. The descriptor is the part
operators see (via `node.manifest`); the handler is the Rust fn
that runs. See
[`docs/capability-discovery.md`](capability-discovery.md) for the
field-by-field reference.

#### 2. SOL flows as composable units

A SOL flow template (`flows/*.sol`) is a small, version-controlled
program that calls into capabilities. Operators can drop a new
`.sol` file into `flows/` and reference it from any controller
config without rebuilding the runtime.

Flows are NOT plugins; they're orchestration scripts that consume
plugins. The distinction is load-bearing: an attacker who can
write a `.sol` file can do whatever the admission pipeline lets
the bridge's identity do, which is bounded. An attacker who can
write a capability handler can do anything the controller process
can do, which is much more.

#### 3. Policy files as deployment-time configuration

`configs/policies/*.toml` is the operator's allowlist. Adding or
removing a capability from a controller's `requires_groups` set,
or scoping the policy rule to a specific group, is a non-code
deployment change. This is the operator-facing analogue of a
plugin install/uninstall.

---

## What a plugin system had to satisfy (M1–M7)

These were the **mandatory** constraints. The shipped system
satisfies all of them to the degree noted.

### M1 — The admission pipeline cannot be bypassed

Every capability call must still flow through
identity → policy → handler → audit. A plugin that registers a
handler must accept exactly the same `(InvocationCtx, args)` shape
the static handlers do, and the dispatch bridge must call it
inside the same pipeline. No plugin-private "trusted" path.

*Status: satisfied.* Plugin capabilities go through the same
`PolicyEngine` and audit log as built-in capabilities.

### M2 — Plugins cannot grant themselves trust

The trust root is the org's Ed25519 secret. A plugin cannot mint
identities, modify policy, or write to the audit log out-of-band.
If a plugin needs to perform a privileged action, it does so by
calling another capability — going through the same pipeline.

*Status: satisfied.* The plugin subprocess has no access to the
host's identity bundle or policy store.

### M3 — Plugins are auditable from source

Whatever distribution mechanism a plugin uses (statically linked,
dynamic load, WASM, signed manifest pointing at a binary), the
**source** of the handler must be reviewable by the operator
before installation. "Pull from a registry by SHA-256" is OK if
the source is reproducibly built; "fetch and run an unsigned
binary" is not.

*Status: satisfied by the `binary_sha256` field and the
`publisher_key` / `.sig` verification gate.* The operator pins
the binary hash in the manifest. The loader refuses binaries that
don't match.

### M4 — Plugin sensitivity tags must be honest

The descriptor's `sensitivity_tags` field is what policy authors
use to decide who can call. A plugin that lies about its
sensitivity breaks the operator's mental model. There is no
automated verification; the system relies on source review for
distributed plugins and on the `publisher_key` gate for signed
distribution.

### M5 — Capability descriptors are signed

When dynamic plugin loading lands, descriptors must be signed by
a key the operator trusts. Unsigned descriptors are rejected at
load time.

*Status: partially satisfied.* The `publisher_key` field signs
the manifest (including capability declarations) at the TOML
level. The signature is over the full manifest bytes, which
includes the `[[plugin.capabilities.provides]]` entries.

### M6 — Resource bounds enforced by the loader

A plugin handler that allocates 100 GB on every call should fail
at the loader's enforcement boundary, not by crashing the
controller.

*Status: satisfied on Linux/Unix.* The loader applies
`RLIMIT_AS`, `RLIMIT_CPU`, `RLIMIT_NOFILE`, and `RLIMIT_CORE=0`
via `pre_exec`. Linux additionally applies `PR_SET_NO_NEW_PRIVS`
and a seccomp BPF filter. On Windows the loader fails closed
(`LoadError::SandboxUnenforceable`) unless all caps are set to 0.

### M7 — No network egress without an explicit capability

A plugin that wants to call out to the internet must declare it
and obtain explicit policy admission.

*Status: partially satisfied by process isolation.* The
subprocess boundary means the plugin's network calls don't go
through the host's capability pipeline. Full enforcement (seccomp
`connect` restriction or network namespace) is not yet wired.

---

## Loading model options (historical analysis)

The options below were analyzed before the loader was built.
Option B is the implemented path.

### Option A — Static-only forever

Plugins distributed as Rust crates; the operator rebuilds the
controller. The pre-0.4.1 model. Pros: maximum auditability, no
loader complexity, no sandbox attack surface. Cons: ergonomically
heavy for ecosystem growth.

Recommended for: production deployments where the capability set
changes monthly, not weekly. Source-trust model is unambiguous.

### Option B — Out-of-process capability nodes (IMPLEMENTED)

A subprocess per plugin. The plugin "installs" by shipping a
binary + `plugin.toml` into `plugin_dir`. The `plugin_host` node
spawns it, sandboxes it, and registers its capabilities on the
dispatch bridge. Pros: clean trust boundary (an OS process),
existing pipeline applies. Cons: process overhead per plugin,
plugin author must implement or use the SDK.

**This is the implemented path in 0.4.1.** See
[`docs/plugins.md`](plugins.md) for the full reference.

### Option C — In-process WASM modules

A static loader that pulls signed WASM modules. Pros: rich plugin
ecosystem possible. Cons: substantial sandbox engineering,
WASM-bridge for non-trivial I/O is complex, and a WASM bug
becomes a CVE in the binary.

Not implemented. Revisit only if Option B's process-per-plugin
overhead becomes a real bottleneck.

### Option D — Dynamic Rust dylib

`libloading` + ABI compatibility. Strongly discouraged: Rust ABI
isn't stable, plugin must be rebuilt for every controller version,
and there's an `unsafe` boundary at the load point. **Rejected.**

---

## Forbidden surface area (do not build)

Any future plugin work must NOT include any of these:

- **Capability marketplace.** A central registry of plugins
  pulled at runtime. This violates M3 (auditability), and the
  central-registry shape contradicts the peer-native architecture
  the rest of Relix is built on.
- **Remote code execution.** A capability that takes a code blob
  and runs it.
- **Browser automation.** A plugin that drives a real browser is
  effectively `tool.execute_arbitrary_javascript`. Out of scope
  forever.
- **Self-installing plugins.** A capability that downloads and
  installs another plugin. Composability via SOL flows is the
  pattern; "plugins install plugins" is a recipe for confused-
  deputy attacks.
- **Unsigned distribution.** No "pull from URL, run it" path. M5.

---

## See also

- [`docs/plugins.md`](plugins.md) — operator and plugin-author
  reference for the shipped subprocess plugin system.
- [`docs/architecture.md`](architecture.md) — the peer model that
  is already Relix's primary "plugin" mechanism.
- [`docs/capability-discovery.md`](capability-discovery.md) —
  what plugins expose for discovery.
- [`docs/security.md`](security.md) — the admission pipeline that
  continues to apply to plugin-registered capabilities.
- [`crates/relix-core/src/capability.rs`](../crates/relix-core/src/capability.rs)
  — the descriptor type a plugin emits.
- [`crates/relix-plugin-sdk/`](../crates/relix-plugin-sdk/) —
  the Rust SDK for plugin authors.
