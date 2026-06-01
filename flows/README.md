# Relix Flows (v0.4.1)

Hand-written flows for Relix. Three formats are supported:

| Extension | Language | Notes |
|---|---|---|
| `.sol` | SOL (Rust-like) | Compiled to bytecode; runs on the SOL stack VM |
| `.sflow` | Sflow (step-based) | AST executor; operator-friendly syntax |
| `.yml` / `.yaml` | YAML flow | Lowers to SOL source; runs on the same VM |

Each flow is loaded by a controller via its `configs/<node>.toml`
`[session.<name>] source = "flows/<name>.<ext>"` declaration.

## Files

- `chat.sol` / `chat_template.sol` — the canonical chat agent flow (memory + AI).
- `chat_with_tool.sol` — chat with `tool.web_fetch` integration.
- `chat_template.yml` / `chat_template_streaming.yml` — YAML chat templates.

## Cross-node primitive

Both SOL and YAML flows use `remote_call` to call peers:

```sol
remote_call("<peer-alias>", "<method>", <args>)
```

For streaming responses (chunked delivery to SSE observers):

```sol
remote_call_stream("<peer-alias>", "<method>", <args>)
```

- `peer-alias` is resolved per the controller config's `[peers]` section.
- `method` is the fully-qualified capability method name (e.g., `memory.search`).
- `args` is a single UTF-8 argument string (CBOR-typed at Gate 2).

Both opcodes are synchronous from the flow author's perspective.

## Error handling

SOL and YAML flows support `try` / `catch` / `rethrow`:

```sol
try {
    remote_call("ai", "ai.chat", "session|hello");
} catch timeout {
    // handle timeout
} catch any {
    // handle everything else
}
```

Sflow flows use the same `try` / `catch` / `rethrow` keywords.

## Adding a New Flow

1. Author `flows/<name>.<ext>` in any supported format.
2. Add a `[session.<name>] source = "flows/<name>.<ext>"` entry to the
   relevant controller's config.
3. Restart the controller. The flow is compiled at boot and made callable.

For YAML flows, validate first:

```bash
relix-cli flow yaml --template chat > my.yml
# edit my.yml
relix-cli flow-run --flow my.yml --identity ... --peers ...
```

## Future (Post-Alpha)

- Durable yield model (RELIX-7; SIMP-001 resolution) — async suspend/resume.
- CDDL-typed args and return.
- `parallel { }` blocks.

Per `specs/alpha-simplifications.md`, the synchronous `remote_call` is a
deliberate simplification; the yield-model opcode shape is the Gate 2 target.
