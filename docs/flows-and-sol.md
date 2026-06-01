# Flows and SOL

SOL is a small interpreted language whose job is to *describe an
orchestration*. A SOL program executes inside the bridge's process
(or in `relix-cli flow-run`); its `remote_call` opcode is the only way
to invoke a capability on another peer. Every multi-step plan in Relix
lives in a `.sol` file. Rust code is the execution substrate — never
the orchestration logic.

This document describes what SOL is in the alpha, what it isn't, and
how to read or write a flow.

## Why orchestration lives in SOL

The architectural invariant is "one source of truth for the order of
operations". Putting orchestration in Rust would mean the same plan
exists in two places — the `.sol` file and the bridge's request
handlers — and reality would drift away from either copy as features
were added.

Concretely, the alpha enforces this by giving the bridge exactly one
job: pick a template, substitute three placeholders, run the
`FlowRunner`. The bridge does **not** loop on tool calls, splice
results, or decide what to call next. If you want a new orchestration,
write a new `.sol` file.

The trade-off is that the alpha does not (yet) ask the LLM "what tool
should I call?" — the flow file picks. Real LLM-driven tool use needs
the durable yield model that lands at Gate 2. The current architecture
is what generalises to it.

## What SOL is in the alpha

SOL is a ported subset of the OpenPrem SOL VM with one Relix-specific
extension: the `remote_call` opcode. The VM is synchronous; the host
bridges to async libp2p via `tokio::task::spawn_blocking +
Handle::current().block_on(...)` (SIMP-014 in
[`specs/alpha-simplifications.md`](../specs/alpha-simplifications.md)).

What you get:

- A handful of types — `str` (heap string) is the one you'll use.
- `let name: str = ...;` bindings.
- `function start() -> str { ... }` (one entry point).
- Conditionals, string concatenation with `+`, `print`, `return`.
- `remote_call(peer_alias, method, args)` returning `str`.

What you don't get (in the alpha):

- Typed flow arguments (the bridge does template-string substitution
  via `{{PLACEHOLDER}}` markers). SIMP-018.
- Provider-side LLM tool-calling integration. SIMP-010.
- Loops over `remote_call` results that branch on response shape.
  You can write loops, but the bodies still return strings; for
  branchy AI-driven control you need durable yield (Gate 2).

For VM internals see [`sol-runtime-analysis.md`](sol-runtime-analysis.md).

## The `remote_call` opcode

Signature in SOL:

```sol
let result: str = remote_call(peer_alias, method, args);
```

Three string arguments:

| Field | Meaning |
|---|---|
| `peer_alias` | Either an entry from the operator's `peers.toml` (e.g. `"memory"`, `"ai"`, `"tool"`), or the form `"capability:<method>"` which asks the dispatcher to resolve via the discovered manifest cache. |
| `method` | The fully-qualified capability name (`"memory.write_turn"`, `"ai.chat"`, `"tool.web_fetch"`, `"node.health"`, `"node.manifest"`). |
| `args` | UTF-8 bytes the responder's handler will parse. The wire format is per-method (see below). |

Returns the responder's success-body bytes decoded as a SOL `str`. On
any responder error (policy denied, handler internal error, transport
failure) the VM halts with `VM_ERROR_SENTINEL` and the host surfaces
the `RemoteCallError` (`peer`, `method`, `kind`, `cause`) to the
caller. Subsequent `remote_call`s in the same flow do not run.

## The `remote_call_stream` opcode (RELIX-2)

```sol
let result: str = remote_call_stream(peer_alias, method, args);
```

Same type signature and error contract as `remote_call`. The difference
is at the host layer: the flow runner opens a `/relix/rpc/stream/1`
substream and fires the VM's chunk observer once per arriving Chunk
frame. From the SOL author's perspective the call is still
synchronous — the opcode returns a single concatenated result string.
The streaming benefit is external: when a chunk observer is wired (the
web bridge uses one for HTTP SSE), each chunk reaches the client before
the VM has finished collecting.

If the dispatcher has no streaming implementation, the default trait
method falls back to a single `remote_call` and reports the whole body
as one chunk.

## Flow file formats

`FlowRunner` dispatches on file extension:

| Extension | Pipeline |
|---|---|
| `.sol` | SOL compile pipeline → stack VM |
| `.sflow` | Sflow AST executor |
| `.yml` / `.yaml` | YAML frontend → lowered SOL → stack VM |

YAML flows execute on the **identical** SOL VM as hand-written `.sol`
files — no separate runtime. See
[`yaml-flow-reference.md`](yaml-flow-reference.md) for the full YAML
format.

Both SOL and YAML paths run the VM inside `tokio::task::spawn_blocking`
(SIMP-014).

## Per-flow instruction budget (`#steps`)

Every SOL flow has a fuel budget. The defaults:

| Constant | Value |
|---|---|
| `DEFAULT_MAX_STEPS` | 100,000 instructions |
| `MAX_STEPS_CEILING` | 10,000,000 instructions (hard ceiling) |

A hand-written `.sol` file may override the budget with a `#steps N`
directive at the very top of the source:

```sol
#steps 500_000
function start() -> str { ... }
```

`compile_path_with_directives` reads and honours the directive.
YAML flows always use `DEFAULT_MAX_STEPS`; the `#steps` directive is
not supported in `.yml`/`.yaml` files.

When the budget is exhausted the VM halts with `SolError::FuelExhausted`
rather than running indefinitely. This means runaway loops exhaust fuel
before they exhaust host memory.

## Argument wire formats (SIMP-016)

The alpha keeps `remote_call` arg/return as UTF-8 strings. Pipe-
delimited fields are the per-method convention; the last field may
contain `|` (handlers `splitn(N, '|')`).

| Method | Arg | Returns |
|---|---|---|
| `memory.write_turn` | `session_id\|role\|body` | `"ok\n"` |
| `memory.recent_for_session` | `session_id` *or* `session_id\|N` (default N=10) | `role: text\n` per turn, oldest first |
| `memory.search` | `query` *or* `query\|N` (default N=10) | `session_id\trole\ttext\n` per match, best first |
| `ai.chat` | `session_id\|prompt\|history` (history may be empty) | provider's reply text |
| `tool.web_fetch` | `<url>` *or* `<url>\|<max_bytes>` | response body decoded as UTF-8 (text-like content types only) |
| `node.health` | empty | `name=...\ntype=...\nstatus=ok\nruntime=...\n` |
| `node.manifest` | empty | CBOR-encoded `NodeManifest` (binary) |

Typed flow arguments (CBOR + CDDL schemas) replace the pipe-delim
convention at Gate 2.

## The static peer alias map

Aliases used by `remote_call(peer_alias, ...)` come from a small TOML
the operator supplies to the FlowRunner. The bringup script generates
it as `dev-data/<run>/peers.toml`:

```toml
[peers.memory]
addr = "/ip4/127.0.0.1/tcp/19711"

[peers.ai]
addr = "/ip4/127.0.0.1/tcp/19712"

[peers.tool]
addr = "/ip4/127.0.0.1/tcp/19713"
```

The dispatcher dials each entry once at startup (or once per `flow-run`
invocation in the CLI path) and pins the alias → libp2p `PeerId`
mapping for the duration of the process.

`capability:<method>` is the discovery-aware alternative: the
dispatcher looks up the first peer in the bridge's `ManifestCache`
that advertises the method and routes through whichever local alias
that peer has. Static aliases keep working unchanged.

## The chat flow walk-through

[`flows/chat_template.sol`](../flows/chat_template.sol) is what
`POST /chat` and the OpenAI shim render by default. Annotated:

```sol
function start() -> str {
    // {{MESSAGE}} is the user's text; {{SESSION}} is the session id.
    // The bridge substitutes both at request time.
    let user_msg: str = "{{MESSAGE}}";

    // 1. Persist user turn FIRST so recent-history readback includes
    //    it and a crash mid-flow does not lose the user input.
    remote_call("memory", "memory.write_turn",
                "{{SESSION}}|user|" + user_msg);

    // 2. Read recent history (now includes the just-written user turn).
    let history: str = remote_call("memory", "memory.recent_for_session",
                                   "{{SESSION}}");

    // 3. AI call with prompt + history per SIMP-016.
    let reply: str = remote_call("ai", "ai.chat",
                                 "{{SESSION}}|" + user_msg + "|" + history);

    // 4. Persist assistant turn.
    remote_call("memory", "memory.write_turn",
                "{{SESSION}}|assistant|" + reply);

    return reply;
}
```

Four `remote_call`s, two peers. The bridge does not need to know that
this flow involves the memory node at all — it just substitutes and
runs.

## The chat-with-tool flow walk-through

[`flows/chat_with_tool.sol`](../flows/chat_with_tool.sol) adds one
step:

```sol
function start() -> str {
    let user_msg: str = "{{MESSAGE}}";

    remote_call("memory", "memory.write_turn",
                "{{SESSION}}|user|" + user_msg);

    let history: str = remote_call("memory", "memory.recent_for_session",
                                   "{{SESSION}}");

    // 3. Fetch external URL. capability: routes via the bridge's
    //    discovered manifest cache instead of the static "tool" alias —
    //    proves the dynamic-discovery path while leaving other calls
    //    on the static aliases. "|16384" asks the tool node to cap the
    //    body at 16 KiB so the prompt stays small.
    let fetched: str = remote_call("capability:tool.web_fetch",
                                   "tool.web_fetch",
                                   "{{TOOL_URL}}|16384");

    // 4. Build a prompt that carries the URL and fetched body verbatim.
    let prompt: str = "user asked: " + user_msg
        + "  ---  fetched_from {{TOOL_URL}}: "
        + fetched;

    // 5. AI call.
    let reply: str = remote_call("ai", "ai.chat",
                                 "{{SESSION}}|" + prompt + "|" + history);

    // 6. Persist assistant turn.
    remote_call("memory", "memory.write_turn",
                "{{SESSION}}|assistant|" + reply);

    return reply;
}
```

If the tool node rejects the URL (SSRF guard, scheme check, redirect
re-check), step 3 fails with `kind = POLICY_DENIED`, the VM halts,
steps 4–6 never run, and the bridge returns 502 with the cause string.

## Writing your own flow

Three options:

### 1. New bridge template

Set in the bridge config TOML:

```toml
[flow]
template_path        = "flows/chat_template.sol"
tool_template_path   = "flows/chat_with_tool.sol"
```

Only two templates are currently wired from the bridge:
`template_path` (chat flow) and `tool_template_path` (tool flow).
Adding a third would mean a new HTTP route + handler; not hard, but
out of scope for the alpha.

### 2. CLI `flow-run`

For ad-hoc flows that don't go through the HTTP bridge:

```bash
cargo run -p relix-cli -- flow-run \
    --flow flows/your-flow.sol \
    --identity dev-keys/local-bridge.aic \
    --client-key dev-keys/local-bridge.key \
    --peers dev-data/local/peers.toml
```

The CLI compiles and runs the flow against the configured peers and
prints the final result + flow log path. Useful for testing a flow
against the live mesh without going through the bridge.

### 3. Just modify an existing flow

The bridge re-renders the template on every request, so editing a
`.sol` file and re-sending a request picks up the change without a
controller restart.

## Compiling SOL

The compile pipeline lives in `crates/relix-runtime/src/sol/`:

```
Lexer -> Parser -> Analyzer -> Codegen -> Vec<Inst>
```

`FlowRunner::run` runs this on every flow execution. Compile errors
surface as `FlowRunnerError::Vm`. The VM is a small stack machine; the
bytecode is in-memory only (no `.solc` cache today).

For internals see [`sol-runtime-analysis.md`](sol-runtime-analysis.md).

## Inspecting a finished flow

Every `remote_call` writes `RemoteCallIssued` before the network send
and `RemoteCallCompleted` / `RemoteCallFailed` after. Terminal event
is `FlowCompleted` (with the VM exit value) or `FlowFailed` (with the
last `RemoteCallError`).

```bash
cargo run -p relix-flow-inspect -- --flow dev-data/flow-runner/flows/<flow_id>.log
```

Each `RemoteCallIssued`/`Completed` shares the `request_id` with the
responder's audit record, so:

```bash
cargo run -p relix-flow-inspect -- --audit dev-data/local-memory/audit.log
```

…matches them up cross-node.

## Future direction (Gate 2)

These are the planned changes to the SOL surface. None of them are in
the alpha:

- **Durable yield.** A flow can pause waiting for an external event
  (LLM tool-use callback, human approval) and resume later. Removes
  the "VM halts on remote_call error" hard stop and enables real
  multi-turn tool use.
- **Typed CDDL arguments.** Replaces the pipe-delim string convention
  with named, schema-validated fields per method.
- **`Inst::FlowArg`.** First-class flow inputs instead of template
  substitution.
- **Provider-native streaming.** Stream the AI response token-by-token
  through the flow (`remote_call` returns a stream handle that the
  flow can iterate).

The alpha demonstrates the architecture without these. Code paths for
each are marked `// TODO(SIMP-NNN):` in the source.

## See also

- [`architecture.md`](architecture.md) — how flows fit into the request lifecycle.
- [`sol-runtime-analysis.md`](sol-runtime-analysis.md) — VM internals and the `block_on` bridge.
- [`specs/RELIX-7-sol.md`](../specs/RELIX-7-sol.md) — the SOL spec.
- [`specs/RELIX-8-flow.md`](../specs/RELIX-8-flow.md) — the flow / event log spec.
- [`specs/alpha-simplifications.md`](../specs/alpha-simplifications.md) — every SIMP this doc references.
