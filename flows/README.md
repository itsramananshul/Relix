# Relix Alpha Flows

Hand-written SOL flows for the alpha. Each flow is loaded by a controller via its `configs/<node>.toml` `[session.<name>] source = "flows/<name>.sol"` declaration.

## Files

- `chat.sol` — the canonical chat agent flow (memory + AI).
- `chat_with_tool.sol` — chat with `tool.web_fetch` integration.

## SOL Surface for the Alpha

The alpha SOL adds one cross-node primitive on top of the existing OpenPrem SOL VM:

```
remote_call("<peer-alias>", "<method>", <args>)
```

- `peer-alias` is resolved per the controller config's `[peers]` section.
- `method` is the fully-qualified capability method name (e.g., `memory.search`).
- `args` is a single argument (string today; will be CBOR-typed at Gate 2).

Synchronous (SIMP-001). Returns the response or raises VM error on failure.

## Adding a New Flow

1. Author `flows/<name>.sol`.
2. Add a `[session.<name>] source = "flows/<name>.sol"` entry to the relevant controller's config.
3. Restart the controller. The flow is compiled at boot and made callable.

## Future (Post-Alpha)

- Async yield model and `try/catch` (RELIX-7; SIMP-001 resolution).
- CDDL-typed args and return.
- `parallel { }` blocks.
- Loading flows from SolFlow live mode.

Per `specs/alpha-simplifications.md`, the alpha's synchronous `remote_call` is a deliberate simplification; the bytecode opcode shape is the production target.
