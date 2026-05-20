# Plugin + Packaging Foundations

How Relix capabilities are distributed today (statically linked
into the controller binary), what plugin support might look like,
and the architectural constraints any future plugin system must
satisfy. Track 5 of the autonomous roadmap.

This is a **foundations document**: it documents the current state
honestly and sketches the trust + loading model that a future
plugin system must respect. It is **not** an implementation plan
for a plugin marketplace.

## Today: static linkage, no plugin loading

The capability set on any given controller is determined at
**compile time**:

```
controller binary
├── relix-core           (types, identity, policy, audit)
├── relix-runtime        (libp2p, SOL VM, dispatch bridge, node impls)
└── (the nodes module owns the registered capabilities)
```

When the bringup script spins up `relix-controller` with
`[controller] node_type = "tool"`, the controller calls
`crate::nodes::tool::register(bridge, ...)` which wires the
specific tool capabilities into the dispatch bridge. There is no
runtime plugin loading, no dynamic library dlopen, no WASM
sandbox, no remote code download.

**This is deliberate.** Every alpha capability has been audited as
part of the source tree review. The admission pipeline can prove
that a `policy_denied` is final (the responder did not execute the
handler) precisely because the handler is a static Rust function
inside the controller process.

## The packaging surface that already exists

Three things ARE plugin-like in the alpha and are worth naming:

### 1. `CapabilityDescriptor` as the unit of discovery

A capability is a `(method_name, descriptor, handler)` triple
registered on the dispatch bridge. The descriptor is the part
operators see (via `node.manifest`); the handler is the Rust fn
that runs. See
[`docs/capability-discovery.md`](capability-discovery.md) for the
field-by-field reference.

A "plugin" — in the future — would ship a descriptor + a handler.

### 2. SOL flows as composable units

A SOL flow template (`flows/*.sol`) is a small, version-controlled
program that calls into capabilities. Operators can drop a new
`.sol` file into `flows/` and reference it from any controller
config without rebuilding the runtime. The bridge's
`[flow] template_path` and `tool_template_path` are how this
shows up today.

Flows are NOT plugins; they're orchestration scripts that consume
plugins. The distinction is load-bearing: an attacker who can
write a `.sol` file can do whatever the admission pipeline lets
the bridge's identity do, which is bounded. An attacker who can
write a capability handler can do anything the controller process
can do, which is much more.

### 3. Policy files as deployment-time configuration

`configs/policies/*.toml` is the operator's allowlist. Adding or
removing a capability from a controller's `requires_groups` set,
or scoping the policy rule to a specific group, is a non-code
deployment change. This is the operator-facing analogue of a
plugin install/uninstall.

## What a plugin system would have to satisfy

These are the **mandatory** constraints. A design that doesn't
satisfy all of them is not acceptable.

### M1 — The admission pipeline cannot be bypassed

Every capability call must still flow through
identity → policy → handler → audit. A plugin that registers a
handler must accept exactly the same `(InvocationCtx, args)` shape
the static handlers do, and the dispatch bridge must call it
inside the same pipeline. No plugin-private "trusted" path.

### M2 — Plugins cannot grant themselves trust

The trust root is the org's Ed25519 secret. A plugin cannot mint
identities, modify policy, or write to the audit log out-of-band.
If a plugin needs to perform a privileged action, it does so by
calling another capability — going through the same pipeline.

### M3 — Plugins are auditable from source

Whatever distribution mechanism a plugin uses (statically linked,
dynamic load, WASM, signed manifest pointing at a binary), the
**source** of the handler must be reviewable by the operator
before installation. "Pull from a registry by SHA-256" is OK if
the source is reproducibly built; "fetch and run an unsigned
binary" is not.

### M4 — Plugin sensitivity tags must be honest

The descriptor's `sensitivity_tags` field is what policy authors
use to decide who can call. A plugin that lies about its
sensitivity (e.g. claims `parse:html` but actually writes to the
filesystem) breaks the operator's mental model of what the
policy file admits. There is no automated way to verify this
today; the alpha relies on source review.

### M5 — Capability descriptors are signed

When dynamic plugin loading lands, descriptors must be signed by
a key the operator trusts (probably the org root, or a
delegated capability-signing subkey). Unsigned descriptors are
rejected at load time. This prevents a compromised plugin
distribution channel from silently rewriting `requires_groups`
to admit everybody.

### M6 — Resource bounds enforced by the loader

A plugin handler that allocates 100 GB on every call should fail
**at the loader's enforcement boundary**, not by crashing the
controller. The simplest enforcement is a per-call budget the
dispatch bridge tracks; richer would be `cgroup`-style isolation
when the plugin is a separate process.

### M7 — No network egress without an explicit capability

A plugin that wants to call out to the internet must declare it in
its descriptor (`environment_requirements: ["network:outbound"]`)
AND obtain explicit policy admission. The loader rejects plugins
whose runtime tries to dial without the declaration. (Pragmatic
note: this is hard to enforce in-process; the most robust answer
is "external network capabilities must be separate peers like
`tool.web_fetch` already is".)

## Loading model options (sketched, not chosen)

### Option A — Static-only forever

Plugins distributed as Rust crates, the operator rebuilds the
controller. The current model. Pros: maximum auditability, no
loader complexity, no sandbox attack surface. Cons: ergonomically
heavy for ecosystem growth.

Recommended for: production deployments where the capability set
changes monthly, not weekly. Source-trust model is unambiguous.

### Option B — Out-of-process capability nodes

A new controller binary per plugin. The plugin "installs" by
spinning up a new peer with its own identity bundle and policy
admission, exactly the same way `memory` / `ai` / `tool` are
separate peers today. Pros: clean trust boundary (an OS process),
existing pipeline applies. Cons: process overhead per plugin,
plugin author must understand controller config.

Recommended for: anything that does I/O, holds credentials, or
needs distinct rate limits. This is **the path the alpha already
takes** for memory / ai / tool / coordinator.

### Option C — In-process WASM modules

A static loader that pulls signed WASM modules. The dispatch
bridge gets a `WasmHandler` variant that bounds memory + CPU
per call. Pros: rich plugin ecosystem possible. Cons: substantial
sandbox engineering, WASM-bridge for non-trivial I/O is a tower
of complexity, and a WASM bug becomes a CVE in our binary.

Recommended for: only if Option B's process-per-plugin overhead
becomes a real bottleneck (which it probably won't at alpha
scale).

### Option D — Dynamic Rust dylib

`libloading` + ABI compatibility. Strongly discouraged: Rust ABI
isn't stable, plugin must be rebuilt for every controller version,
and `unsafe` boundary at the load point. Lists here only because
it's an option that always gets proposed; **rejected**.

## Forbidden surface area (do not build)

Any future plugin work must NOT include any of these:

- **Capability marketplace.** A central registry of plugins
  pulled at runtime. This violates M3 (auditability), and the
  central-registry shape contradicts the peer-native architecture
  the rest of Relix is built on.
- **Remote code execution.** A capability that takes a code blob
  and runs it. There is `tool.patch` (apply a unified diff to
  a file) but it does not execute anything; that's intentional.
- **Browser automation.** A plugin that drives a real browser is
  effectively `tool.execute_arbitrary_javascript`. Out of scope
  forever.
- **Self-installing plugins.** A capability that downloads and
  installs another plugin. Composability via SOL flows is the
  pattern; "plugins install plugins" is a recipe for confused-
  deputy attacks.
- **Unsigned distribution.** No "pull from URL, run it" path. M5.

## Suggested next concrete steps (when this work is greenlit)

The realistic first slice is to **document better what's already
plugin-like** and clean up the rough edges, NOT introduce a
loader:

1. **Manifest evolution (small):** add `description`, `categories`,
   `environment_requirements` to `CapabilityDescriptor` with
   serde defaults (P1 from
   [`docs/capability-discovery.md`](capability-discovery.md)).
2. **Capability package convention (doc-only):** specify the
   on-disk layout an Option-B "plugin as separate peer" should
   take: `plugins/<name>/` with `controller.toml`,
   `policy-fragment.toml`, `README.md`. The bringup script can
   then optionally compose plugin fragments.
3. **Plugin trust audit doc:** a short reviewer-facing checklist
   ("before merging a new capability, verify: descriptor matches
   the handler's actual side effects; sensitivity tags are
   complete; the test suite covers the failure paths; the policy
   change is operator-acceptable").
4. **Signed descriptor scaffolding:** in advance of any
   out-of-process plugin loading, define the signature shape over
   `CapabilityDescriptor` so the load-time verification is ready
   when the loader lands.

Each of these is incremental and preserves every invariant. None
implements a plugin loader; that's deliberate.

## See also

- [`docs/architecture.md`](architecture.md) — the peer model that
  is already Relix's primary "plugin" mechanism.
- [`docs/capability-discovery.md`](capability-discovery.md) —
  what plugins would expose for discovery.
- [`docs/security.md`](security.md) — the admission pipeline that
  must continue to apply to plugin-registered capabilities.
- [`crates/relix-core/src/capability.rs`](../crates/relix-core/src/capability.rs)
  — the descriptor type a plugin would emit.
- [`specs/alpha-simplifications.md`](../specs/alpha-simplifications.md)
  — what's been deliberately deferred at Gate 1.
