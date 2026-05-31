# OpenPrem — Technology Deep Dive

Everything about how OpenPrem works under the hood. Written from a full
read of every source file across `open-prem-main`, `open-prem-experiments`,
and `open-prem-reimagined` (v1 + v2).

---

## What OpenPrem Is

OpenPrem is a **distributed workflow orchestration system**. You write a
workflow in SOL (Simple Orchestration Language), submit it to any controller
node in your network, and it executes across multiple machines automatically
— no central server, no message broker, no orchestration service.

The key mental model: **workflows move to the data, not the other way around.**
When a step needs a capability that lives on a different machine, the entire
workflow state (variables, program counter, AST) is serialized and forwarded
to that machine. That machine runs the step, then forwards state again if the
next step lives somewhere else. This is called the **hot-potato** pattern.

---

## The Three Tiers

```
┌────────────────────────────────────────────────────────┐
│  TIER 3 — Apps / Agents                                │
│  Any language (Python, Go, JS, Rust, etc.)             │
│  Expose capabilities via HTTP                          │
│  Declares actions in openprem.toml                     │
└──────────────────────┬─────────────────────────────────┘
                       │ HTTP (local to controller)
┌──────────────────────▼─────────────────────────────────┐
│  TIER 2 — Controllers                                  │
│  Written in Rust                                       │
│  SOL VM embedded                                       │
│  Peer-to-peer networking (HTTP, no libp2p)             │
│  Workflow state machine                                │
│  ValueStore / distributed ledger                       │
└──────────────────────┬─────────────────────────────────┘
                       │ openprem CLI / Python SDK
┌──────────────────────▼─────────────────────────────────┐
│  TIER 1 — CLI / SDK                                    │
│  Submit workflows, poll status, invoke directly        │
│  SDKs: Python, Rust, Go, TypeScript, Java, Kotlin,     │
│  C#, PHP, Ruby, Swift, C, Zig                          │
└────────────────────────────────────────────────────────┘
```

---

## How the Controller Works

Every controller is a **Rust binary** running an HTTP server (Axum). All
controllers are equal — there is no master node. Any controller can:

- Accept a workflow submission and become its coordinator
- Host registered apps/agents
- Advertise its capabilities to peer controllers
- Receive and execute forwarded workflow steps from other controllers
- Act as a **router node** (config-differentiated role, see below)

### Controller State (`Controller` struct)

```
config          — loaded from controller.toml (name, addr, peers, apps)
local_caps      — capability → HTTP endpoint (local apps only)
peer_caps       — capability → peer controller URL (remote capabilities)
known_peers     — list of known peer URLs
workflows       — WorkflowId → ManagedWorkflow (in-flight workflows)
http_client     — reqwest client (5s connect timeout, 30s request timeout)
value_store     — append-only ledger of variable mutations (the distributed state)
controller_id   — human-readable ID used in ledger entries
role            — "controller" or "router"
router_url      — if this is a non-router peer, the URL to send heartbeats to
peers_info      — peer health tracking (for router role)
sessions        — router-only: session tracking by workflow_id
logs            — router-only: bounded log ring from peers
session_ttl     — router-only: how long to keep completed session records
```

### Capability Resolution

Resolution order is strict: **local first, then peer**.

```rust
fn resolve_capability(capability: &str) -> Option<(endpoint_url, is_local)> {
    if local_caps.contains(capability) → return (local_endpoint, true)
    if peer_caps.contains(capability)  → return (peer_url, false)
    return None  // capability not found anywhere → workflow fails
}
```

---

## How Apps Register

Apps are standalone HTTP servers. They register with their local controller
by POSTing to `/register`:

```json
POST /register
{
  "name": "weather-station",
  "actions": [
    { "name": "read_temperature", "params": { "sensor": "str" } },
    { "name": "read_humidity", "params": { "sensor": "str" } }
  ],
  "endpoint": "http://localhost:9101",
  "endpoints": { "http": "http://localhost:9101" }
}
```

The controller stores `weather-station.read_temperature → http://localhost:9101`
in `local_caps` and immediately **broadcasts the new capability to all peers**
(via `POST /peers/capabilities`). From that moment on, any controller in the
network can route `weather-station.read_temperature` calls to this controller.

Apps can also be **statically declared** in `controller.toml`:

```toml
[apps.weather-station]
endpoint = "http://localhost:9101"
capabilities = ["weather-station.read_temperature", "weather-station.read_humidity"]
```

Static apps are registered at startup without a dynamic handshake.

### Action invocation

When the controller needs to call a local app:

```
POST {app_endpoint}
Content-Type: application/json

{
  "capability": "weather-station.read_temperature",
  "sensor": "outdoor"
}
```

The app handler strips the `capability` field; remaining fields become
`params`. The app returns `{ "ok": true, "result": { ... } }` or
`{ "ok": false, "error": "reason" }`.

### Python SDK

The Python SDK (`v2/sdk/python/openprem/`) makes writing apps trivial:

```python
app = Application(
    name="weather-station",
    controller="http://localhost:8081",
    transport="http",
    listen=("0.0.0.0", 9101),
)

@app.capability("read_temperature")
def read_temperature(params=None):
    sensor = (params or {}).get("sensor", "default")
    return {"celsius": 23.4, "sensor": sensor}

app.run()
```

On `app.run()`:
1. A registration thread starts, retries every 3 seconds until the
   controller is up and accepts the registration
2. The HTTP server starts (using stdlib `http.server.HTTPServer`)
3. The transport handler extracts `capability` from POST body, dispatches
   to the registered function, serialises the return value as JSON

Other SDKs exist in Go, TypeScript, Java, Kotlin, C#, PHP, Ruby, Swift,
C, and Zig — all following the same register-then-serve pattern.

---

## The SOL Language

SOL (Simple Orchestration Language) is a statically-typed scripting language
purpose-built for workflow definition. It is parsed and executed inside every
controller. No external runtime, no interpretation overhead — it's embedded
Rust code.

### Type system

```
bool, int (i64), float (f64), char, str
[]T                          — array
{ field: Type, ... }         — inline struct
Name { field: Type; }        — named struct (declared with struct keyword)
enum Name { Variant1; ... }  — enum
```

### Syntax in brief

```sol
import app_name;             // import module (enables app_name.action() calls)

workflow "my-workflow" {
    let x: int = 42;
    let temp = app_b.get_temp({ sensor: "roof" });
    let msg = "Temp: " + to_str(temp.celsius) + "C";

    if (temp.celsius > 30) {
        app_c.alert({ message: msg });
    }

    while (true) {
        let n = counter.increment();
        if (n.value >= 10) { return; }
    }

    for item in [1, 2, 3] {
        printer.print(item);
    }
}
```

### Two call styles

**Import style** (preferred, v2):
```sol
import discord_bot;
discord_bot.send_message({ channel: "#alerts", text: msg });
```
`import discord_bot` tells the compiler that `discord_bot.*` calls are
remote capability calls. The resolver maps `discord_bot.send_message`
to the capability string `"discord_bot.send_message"`.

**Direct call style** (legacy, v1):
```sol
let result = call("discord-bot.send_message", { channel: "#alerts", text: msg });
```

### Built-ins

| Function | What it does |
|----------|-------------|
| `print(x)` | Print to controller stdout (debug) |
| `len(x)` | Length of array or string |
| `to_str(x)` | Convert any value to string |
| `type_name(x)` | Return the type name as a string |

---

## The SOL VM — How It Actually Executes

This is the interesting part. The SOL VM is a **step-based state machine**,
not a standard call stack. It's designed to be serializable mid-execution.

### WorkflowExecutor

```rust
struct WorkflowExecutor {
    workflow: WorkflowDecl,           // parsed AST
    pc: usize,                        // program counter into body.stmts
    bindings: HashMap<String, Value>, // all variable values
    step_count: u64,                  // total steps executed (monotonic)
    interpreter: Interpreter,
    completed: bool,
    source: String,                   // original SOL source text
}
```

### The `step(budget)` loop

On every iteration, the executor calls `step(budget)`:

```
if completed → return Completed(Unit)
if pending_call exists → return RemoteCall { capability, params }

// Execute up to `budget` statements
loop {
    exec body.stmts[pc]:
        Ok(_)       → pc++, step_count++, continue
        Err("__workflow_call__")  → step_count++, return RemoteCall
        Err(other)  → return Failed(other)

    if pc >= body.stmts.len():
        completed = true
        return Completed(Unit)
}
return Yielded(steps_ran)
```

The sentinel error `"__workflow_call__"` is the mechanism. When the
interpreter hits a `call()` or `module.action()` expression, it does NOT
block waiting for the result. Instead, it stores the call details in
`pending_call` and immediately returns the sentinel. The executor catches
this, stops, and tells the controller: "I need you to invoke this capability."

### Feeding results back: `resolve_remote_call`

The controller invokes the capability (locally or via peer forwarding),
gets a result, then calls `resolve_remote_call(capability, result)`:

```
clear pending_call
set pending_call_result = Some(result)

re-execute body.stmts[pc]:
    Ok(v) → pc++, return Ok(v)
    Err("__workflow_call__") → capture call_site_index into resume_body_index,
                               return Err(sentinel)
    Err(other) → return Err(other)
```

On re-execution, when the interpreter hits the same `call()` expression
again, it checks `pending_call_result` — finds it set — and returns that
value instead of triggering another call. The expression resolves, the
`let` binding gets the value, and `pc` advances.

### Loops with multiple remote calls

When a `while` body has two remote calls:
1. First call: sentinel fires, result is resolved, `pending_call_result`
   consumed, `pc` stays at the while statement, interpreter continues
   into the body
2. Second call: sentinel fires again, `call_site_index` is captured into
   `resume_body_index`
3. Next `resolve_remote_call`: the while handler starts from
   `resume_body_index`, skipping already-executed statements in the body
4. After resolution, execution continues naturally through the rest of the loop

### Full serialization: `WorkflowState`

The executor can be fully serialized at any point:

```rust
struct WorkflowState {
    workflow_id: String,
    source: String,                           // SOL source text
    workflow_name: String,
    pc: usize,                                // current position
    bindings: HashMap<String, Value>,         // all variables
    step_count: u64,
    completed: bool,
    pending_call: Option<(String, Value)>,    // inflight call if any
    pending_call_result: Option<Value>,
    pending_call_result_cap: Option<String>,
    resume_body_index: Option<usize>,
}
```

To restore: re-parse `source` to get the AST, then override all fields
from the saved state. The re-parse is deterministic, so no AST delta
needs to be serialized. This is how hot-potato forwarding works — the
receiving controller doesn't need the sending controller's VM state; it
just has the source + serialized state.

---

## The Hot-Potato Pattern

This is OpenPrem's defining architectural mechanism.

### What happens when a capability is remote

1. Coordinator executes `step()` → gets `RemoteCall { cap: "discord-bot.send_message", params }`
2. `resolve_capability("discord-bot.send_message")` → `(peer_url, false)` — it's remote
3. Coordinator calls `forward_to_peer(peer_url, state, capability, params)`

### `forward_to_peer` — the projection step

Before forwarding, the coordinator **projects bindings**:
- For each variable binding whose value is large or sensitive, it replaces
  the concrete value with a `RemoteRef { id: generated_uuid, owner: self_url }`
- The real value is stored locally in the `ValueStore`
- The `RemoteRef` is lightweight — just two strings

Then it POSTs to `{peer_url}/workflow/execute`:

```json
{
  "state": { "workflow_id": "...", "source": "...", "pc": 4, "bindings": {...}, ... },
  "call": { "capability": "discord-bot.send_message", "params": {...} },
  "budget": 100,
  "ledger_delta": [{ "seq": 3, "controller_id": "ctrl-east", "variable": "temp", "value": 23.4 }]
}
```

### Peer-side execution (`execute_forwarded`)

The receiving controller:
1. Applies the `ledger_delta` to its local ValueStore (syncs variable state)
2. Resolves any `RemoteRef`s in the bindings — first checks local ValueStore,
   falls back to `GET {owner}/values/{id}` HTTP fetch if not found
3. Invokes the local app for the pending `capability`
4. Calls `resolve_remote_call` with the result
5. Continues stepping within its budget:
   - Next call is local → invoke and continue
   - Next call is NOT local → return `forward_needed` (state + next capability)
   - Completed → return `completed` with final result
   - Failed → return `failed`
   - Budget exhausted → return `yielded` (state snapshot)

The peer's response carries a `ledger_delta` of any new variable mutations.
The coordinator applies this delta to its own ValueStore and updates the
workflow's in-memory state.

### Result flows back to submitter

When any controller in the chain completes the workflow, it sends the
final result back to the coordinator's callback URL
(`{coordinator}/workflow/result`). The coordinator updates the workflow
status to `Completed(value)`. The original submitter can poll
`GET /workflow/{id}` for the result.

---

## The ValueStore and Distributed Ledger

The `ValueStore` is an **append-only log** of all variable mutations
across the distributed execution.

```rust
struct ValueStore {
    entries: Vec<LedgerEntry>,            // append-only log
    snapshots: HashMap<String, Value>,    // materialized current view
    next_seq: u64,
}

struct LedgerEntry {
    seq: u64,
    controller_id: String,
    variable: String,
    value: Value,
}
```

Every time a variable gets a new value anywhere in the network:
1. A `LedgerEntry` is appended to the `entries` log
2. The `snapshots` map is updated (latest value for lookup)

When a forwarding delta is received from a peer:
- `apply_delta(delta)` batch-applies all entries
- Sequence numbers prevent duplicate application

The ledger enables `RemoteRef` resolution without the original controller
being required to stay online for the duration of execution — as long as
the ledger delta travels with the forwarded state, the receiving controller
can reconstruct all variable values locally.

---

## Peer Networking

OpenPrem's transport is **plain HTTP** (not libp2p). Peers are statically
configured — there is **no peer discovery, no DHT, no gossip**. Every
peer-to-peer relationship must be explicitly declared in `controller.toml`.

### Startup handshake

On boot, each controller POSTs to every configured peer:

```json
POST {peer}/peers/capabilities
{
  "url": "http://self:8081",
  "capabilities": ["weather-station.read_temperature", ...]
}
```

The receiving peer stores the capabilities and responds with its own
capability list (symmetric handshake). After the handshake both controllers
know each other's full capability surface.

### When new apps register

On dynamic registration, the controller broadcasts the new capability to
all known peers immediately (same `POST /peers/capabilities` format).

### No transitive discovery

If A knows B and B knows C, A does NOT automatically know C. There is no
gossip protocol. Peer-of-peer capabilities are invisible to A. This is
a deliberate design constraint.

### Peer health (v2 router node)

v2 adds a **router role**. Non-router controllers send a heartbeat every
60 seconds to the configured `router_url`:

```json
POST {router}/heartbeat
{
  "url": "http://self:8081",
  "name": "ctrl-alpha",
  "capabilities": ["weather-station.read_temperature", ...],
  "timestamp": 1716300000
}
```

The router tracks `last_heartbeat` per peer and marks a peer `healthy: false`
if no heartbeat arrives within 90 seconds (1.5× the send interval).

The router also exposes:
- `GET /summary` — full mesh overview (peers, sessions, uptime)
- `GET /sessions` — paginated in-flight workflow session list
- `POST /log` — structured log line aggregation (last 10k lines in memory)
- `POST /heartbeat` — liveness registration from peers

The router is the **same binary** as a regular controller — role is
selected by `[controller] role = "router"` in config.

---

## The Full API Surface

### Workflow management
| Endpoint | Description |
|----------|-------------|
| `POST /workflow` | Submit SOL source + workflow name → returns workflow_id |
| `GET /workflow/:id` | Poll status: running/completed/error + progress (pc/step_count) |
| `GET /workflows` | List all workflows on this controller |
| `POST /workflow/:id/cancel` | Remove workflow, terminate background task |
| `POST /workflow/execute` | **Peer-to-peer**: receive forwarded workflow step |

### Capability / app management
| Endpoint | Description |
|----------|-------------|
| `POST /register` | Register an app dynamically |
| `GET /capabilities` | List local + remote capabilities |
| `GET /peers/has/:capability` | Check if capability is reachable + return endpoint |
| `POST /peers/capabilities` | **Peer-to-peer**: receive capability broadcast |
| `GET /apps` | List statically configured apps |

### Direct invocation
| Endpoint | Description |
|----------|-------------|
| `POST /invoke` | Directly invoke a capability by name + params |

### Router-only
| Endpoint | Description |
|----------|-------------|
| `POST /heartbeat` | Receive liveness from peer controllers |
| `GET /summary` | Full mesh state: peers, sessions, uptime |
| `GET /sessions` | Session browser: filter by status, paginate |
| `POST /log` | Accept log lines from peers |

### Value store (v2 distributed execution)
| Endpoint | Description |
|----------|-------------|
| `GET /values/:id` | Resolve a RemoteRef by ID |
| `GET /ledger` | View full ledger entries |

---

## Configuration

### Controller config (`controller.toml`)

```toml
[controller]
name = "ctrl-east"
http_addr = "0.0.0.0:8081"
role = "controller"          # or "router"
router_url = "http://router:8084"   # where to send heartbeats
session_ttl_secs = 1800

[[peers]]
name = "ctrl-west"
endpoint = "http://localhost:8082"

[[peers]]
name = "ctrl-shops"
endpoint = "http://localhost:8083"

[apps.warehouse]
endpoint = "http://localhost:9101"
capabilities = ["warehouse.check_stock", "warehouse.fulfill_order"]
```

### App manifest (`openprem.toml`)

```toml
name = "weather-station"
version = "0.1.0"

[[actions]]
name = "read_temperature"
description = "Read temperature from a sensor"
params = { sensor = "str" }

[[actions]]
name = "read_humidity"
params = { sensor = "str", unit = "str?" }   # str? = optional
```

Parameter types: `str`, `int`, `float`, `bool` + `?` suffix for optional + `[type]` for arrays.

---

## The Execution Background Loop (`run_workflow_auto`)

When a workflow is submitted, the controller spawns a background Tokio task
that runs this loop until the workflow terminates:

```
loop {
    step_result = step_workflow(id)

    Completed(v) → mark completed, break
    Failed(e)    → mark failed, break
    Yielded(_)   → continue (no blocking — back to top immediately)

    RemoteCall { capability, params }:
        (endpoint, is_local) = resolve_capability(capability)

        if not found:
            mark failed("capability '{capability}' not found")
            break

        if is_local:
            result = POST {endpoint} { capability, ...params }
            resolve_call(id, capability, result)
            continue

        else:  // remote
            state = clone current workflow state
            forward_result = POST {endpoint}/workflow/execute {
                state, call: {capability, params},
                budget: 100,
                ledger_delta: value_store.capture_mutations(before, after)
            }

            match forward_result.status:
                "completed"      → update state, mark done, break
                "failed"         → mark failed, break
                "forward_needed" → update state from returned state, continue
                "yielded"        → update state, continue
}
```

The `budget` parameter limits how many SOL statements the peer executes in
a single HTTP round-trip before returning control. This prevents infinite
loops from holding the peer's thread indefinitely.

---

## What OpenPrem Is NOT

- **Not a message broker.** There's no queue, no pub-sub, no consumer groups.
  Workflows are synchronous request-response chains.
- **Not event-driven.** Workflows execute linearly, step by step. There is no
  event loop or reactive stream model.
- **Not fault-tolerant by default.** If a controller crashes mid-execution, the
  in-flight workflows on that controller are lost. There is no durable state
  store — everything is in-memory. Resubmission is the recovery model.
- **Not peer-discovering.** Every peer must be explicitly listed in config.
  No DHT, no multicast, no gossip.
- **Not transport-agnostic.** Everything is HTTP. The libp2p experiments
  (`open-prem-experiments/`) explored TCP + Noise + Yamux (the stack Relix
  uses) but that was never merged into the main OpenPrem codebase.

---

## Evolutionary History (Three Generations)

### Generation 1 — `open-prem-experiments`

The experiments directory shows the R&D phase:

- `p2p-mini` — raw libp2p test: Kademlia DHT + TCP + Noise encryption. Pure
  peer discovery experiment. No SOL, no workflows.
- `libp2ptest` — supply chain simulation over libp2p. First attempt at
  multi-node coordination without HTTP.
- `network_with_sol` — libp2p network combined with an early SOL interpreter.
  RPC messages encoded as CBOR. This was the prototype that proved SOL could
  drive distributed execution.
- `sol_interpreter` — standalone SOL interpreter (no networking). Used to
  harden the language: parser, lexer, bytecode compiler, VM.
- `controller` — earliest controller implementation: SOL + session management
  + handler dispatch. HTTP-based, no libp2p.

### Generation 2 — `open-prem-reimagined` (v1)

Reimagined as an HTTP-first platform with explicit app manifests. Key pieces:
- `crates/openprem-controller` — Rust controller with HTTP transport, registry,
  router for hot-potato forwarding
- `crates/openprem-sol` — dedicated SOL crate with AST, interpreter, value system,
  and workflow runner
- `crates/openprem-sdk` — thin Rust SDK
- `apps/` — polyglot demo apps: Rust (data-provider, weather-station), Python
  (data-consumer, notifier), Go (factory), JavaScript (warehouse), TypeScript (shop)
- `spec/` — the formal spec documents that defined the v2 design

### Generation 3 — `open-prem-reimagined/v2`

The current production codebase. Major additions over v1:
- **RemoteRef + ValueStore + LedgerEntry** — projected bindings for distributed
  state without shipping large values over every hop
- **Router node** — config-differentiated role for mesh observability
  (heartbeats, session tracking, log aggregation)
- **Polyglot SDKs** — Python, Go, TypeScript, Java, Kotlin, C#, PHP, Ruby,
  Swift, C, Zig all following the same Application + `@capability` pattern
- **v2 SOL** — import-based module syntax replaces legacy `call()` string form;
  `resume_body_index` fix for multi-call loop bodies
- **CLI** — `openprem run/status/ps/kill/invoke/capabilities`
- **Six example projects** — simple-demo, three-node, supply-chain,
  diagnostic (most complex — includes Python psutil agent + live web dashboard),
  my-first-network, starter-pack

---

## How OpenPrem Differs from Relix

| Dimension | OpenPrem | Relix |
|-----------|----------|-------|
| Transport | HTTP (plain) | libp2p (TCP + Noise XK + Yamux) |
| Peer discovery | Static config only | Kademlia DHT (automatic) |
| Encryption | None (plain HTTP) | Noise XK (mutual auth, encrypted) |
| Identity | None | Ed25519 IdentityBundle, org root signing |
| Policy | None | Per-node allowlist (Cedar upgrade next) |
| Audit | None | Hash-chained append-only event log |
| State persistence | In-memory only | SQLite WAL, FTS5 |
| App model | HTTP agents (any language) | SOL flows + node types |
| Workflow state | Serialized + forwarded (hot-potato) | SOL VM in controller |
| Failure recovery | Resubmit | Task system: 8-state lifecycle, retry, replay |
| Observability | Minimal (`GET /workflows`) | 65 HTTP endpoints, dashboard, CLI, metrics |

OpenPrem is the **research prototype** that proved the concept. Relix is the
**production version** with hardened transport, identity, policy, audit, and
a full operator surface.
