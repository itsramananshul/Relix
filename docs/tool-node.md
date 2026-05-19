# Tool Node (`tool.web_fetch`)

The tool node is the only Relix peer that can dial arbitrary external
endpoints. Its single capability today is `tool.web_fetch`: an HTTPS
GET of a single URL, returning the body decoded as UTF-8 text.

This document covers the operator surface and the request lifecycle.
For the full security model — every SSRF check, the DNS pin
guarantee, the per-hop redirect re-validation, the secure client pool —
see [`tool-node-security.md`](tool-node-security.md).

## Why a separate peer

Three reasons the bridge does **not** fetch URLs itself:

1. **Admission pipeline.** Every `tool.web_fetch` runs the same
   identity → policy → handler → audit pipeline as every other
   capability. If the bridge fetched directly, the SSRF guard, the
   policy check, and the audit record would all be skipped for
   bridge-initiated calls.
2. **Single source of orchestration.** The SOL flow describes the
   plan. Letting the bridge make a side fetch would mean the same
   plan exists in two places.
3. **Blast-radius isolation.** The tool node owns its own
   `reqwest::Client` pool, its own DNS resolver overrides, and its
   own SSRF rules. A bug in the bridge cannot escalate into outbound
   network access — the bridge has no `reqwest::Client` at all.

## Configuration

The bringup script generates the tool config at
`dev-data/<run>/tool.toml`:

```toml
[controller]
name = "<run>-tool"
node_type = "tool"
listen_port = 19713

[identity]
key_path = "dev-keys/<run>-tool.key"

[trust]
org_root_key_path = "dev-keys/<run>-org-root.pub"

[policy]
file = "configs/policies/<run>.toml"

[tool]
max_bytes     = 262144        # default 256 KiB per response
timeout_secs  = 15            # total deadline per fetch (incl. connect)
max_redirects = 3             # hard cap; 0 disables redirects entirely
allow_http    = false         # https-only by default
user_agent    = "Relix-tool/0.1.0"

[peers]
```

The tool node also serves the built-ins `node.health` and
`node.manifest`, so the bridge can discover it during startup.

## Lifecycle of one fetch

What happens when a SOL flow does
`remote_call("tool", "tool.web_fetch", "https://example.com/")`.

### On the calling side (bridge / flow runner)

1. Render the flow template with the URL substituted into
   `{{TOOL_URL}}`.
2. SOL VM reaches the `RemoteCall` opcode.
3. Dispatcher resolves the peer alias (`"tool"`) to a libp2p PeerId
   via the bridge's pooled `MeshClient` (M11 connection pool).
4. Writes `RemoteCallIssued` to the per-flow event log
   (log-before-act).
5. Sends the CBOR-encoded `RequestEnvelope` over `/relix/rpc/1`.

### On the tool node (responder)

1. **Admission pipeline.** Decode → deadline → identity → capability
   lookup → policy → audit-on-error-if-rejected.
2. **Handler** (`handle_web_fetch`) parses the arg into a URL string
   and optional max-bytes cap.
3. **SSRF guard** (`security::resolve_safe_url`) runs *before* any
   network I/O:
   - Scheme allowlist (`https` always; `http` only if
     `[tool] allow_http = true`).
   - Literal-IP range check (loopback, RFC 1918, link-local incl.
     `169.254.169.254`, ULA, multicast, broadcast, documentation,
     benchmark, IPv4-mapped IPv6).
   - Hostname denylist (`localhost`, `.local`, `.internal`,
     `.intranet`, `.lan`, `.corp`, `.home`, `.private`,
     `metadata.google.internal`, ...).
   - **DNS resolution** — every IP returned by the OS resolver is
     range-checked. Mixed-result resolution (one safe, one forbidden)
     is rejected whole.
4. **Pool lookup** (`PinnedClientPool::pinned`) keyed by
   `(hostname, sorted_validated_addrs)`. On a hit, reuse the
   existing `reqwest::Client` and its connection pool. On a miss,
   build a new `Client` with
   `ClientBuilder::resolve_to_addrs(hostname, validated_addrs)`,
   cache it under the same key, and log an INFO line.
5. **Send the request.** URL keeps the hostname so `Host` header
   and TLS SNI keep targeting the original origin. The `resolve_to_addrs`
   pin overrides reqwest's resolver — the TCP connect can only land
   on a validated IP.
6. **Per-hop redirect re-validation.** Every redirect target is
   re-screened by the same SSRF guard before reqwest follows it. A
   `Location: http://127.0.0.1/` is rejected pre-connect; a
   cross-host `Location: http://attacker/` is re-resolved and
   range-checked. The redirect cap (`[tool] max_redirects`) is also
   enforced in the same closure.
7. **Content-type filter.** Refuse anything that isn't
   `text/*` / `application/json` / `application/xml` /
   `application/xhtml+xml` / `application/*+json` / empty.
8. **Streamed bounded read.** Stream the body into a buffer; abort
   if the response (or its declared `Content-Length`) exceeds the
   per-request cap.
9. **UTF-8 decode** the buffer. Non-UTF-8 bodies are an error.
10. **Audit record** — `status = ok`, latency, etc.

### Back on the calling side

11. Decode the response envelope.
12. Write `RemoteCallCompleted` to the flow log with the request id,
    latency, and body length.
13. Return the body bytes as a SOL `str` to the flow.

Any failure on the tool node maps to a structured `ErrorEnvelope`
that the calling flow surfaces as `kind = POLICY_DENIED` (kind 6) for
SSRF/scheme/url failures, `INVALID_ARGS` (kind 5) for cap/content-type
failures, `RESPONDER_INTERNAL` (kind 11) for non-2xx HTTP, or
`TRANSPORT` (kind 1) for connection / redirect failures.

## Secure client pool summary

The naive way to keep the DNS pin would be to build a fresh
`reqwest::Client` per request. The earlier M9 cut did exactly that —
correct, but ~140 ms per fetch of TLS + connect setup.

The pool caches `Client`s by `(hostname, sorted_validated_addrs)`:

- Same hostname **and** same validated DNS → cache hit, same
  `Client`, reqwest's connection pool reuses the TCP/TLS socket.
- Different validated addrs (legit DNS change, multi-A round-robin) →
  cache miss, new `Client` with the new pin. The old `Client` lingers
  but can only ever dial its originally-validated IPs.
- Different hostname → different `Client` (the pin is per-host inside
  reqwest's resolver override; cross-contamination is impossible).
- IP-literal URLs → one shared unpinned `Client` (no DNS for those;
  default reqwest behaviour is correct).

The **security invariant**: a pooled `Client` only serves requests
whose validated route matches what's pinned inside it. The cache key
**is** the validated route. There is no scenario where reuse widens
the permitted connect set.

Measurement on the local mesh (mock provider, 5 sequential
`POST /chat_with_tool` against `https://example.com`):

```
cold first  : 229 ms  (Client build + TLS + DNS + connect)
warm steady :  ~90 ms (pooled Client + TCP/TLS reuse)
```

~60% reduction in steady-state per-fetch latency with every SSRF
invariant intact. The tool node's log shows exactly one
`pool miss; built new pinned client` line per unique safe route.

Full key choice + invariants + measured cost recovery + honest
limitations: [`tool-node-security.md`](tool-node-security.md) §
"Secure client pool".

## How to invoke `tool.web_fetch`

From the bridge — two operator paths:

```bash
# Native endpoint: explicit URL.
curl -X POST http://127.0.0.1:19791/chat_with_tool \
  -H 'content-type: application/json' \
  -d '{"session_id":"demo","message":"summarize","url":"https://example.com/"}'

# OpenAI shim auto-route: any http(s) URL in the user message.
curl -X POST http://127.0.0.1:19791/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"relix-mock","messages":[{"role":"user","content":"please fetch https://example.com/ and summarize"}]}'
```

From a SOL flow:

```sol
let body: str = remote_call("tool", "tool.web_fetch", "https://example.com/|16384");
```

The optional `|<N>` suffix caps the body at N bytes for that call
(clamped to `[tool] max_bytes`).

From the CLI directly (bypassing the bridge entirely):

```bash
cargo run -p relix-cli -- flow-run \
    --flow flows/your-flow.sol \
    --identity dev-keys/local-bridge.aic \
    --client-key dev-keys/local-bridge.key \
    --peers dev-data/local/peers.toml
```

## Observability

Every cache miss emits a structured INFO line on the tool node:

```
INFO relix_runtime::nodes::tool: tool.web_fetch: pool miss; built
     new pinned client hostname=example.com pinned_addrs=[...]
     pool_entries=N pool_hits=H pool_misses=M
```

Cache hits are not logged at INFO. A DEBUG line is available.

Every redirect rejection emits a distinct WARN with the SSRF reason:

```
WARN relix_runtime::nodes::tool: tool.web_fetch: redirect
     ssrf-rejected; refusing follow target_url=http://127.0.0.1/
     origin_url=http://example.com/... hops=1
     reason=ip 127.0.0.1 is in forbidden range 'ipv4 loopback (127/8)'
```

`grep ssrf-rejected dev-data/<run>/tool.log` finds every rebind-style
attempt.

The tool node's audit log (`dev-data/<run>-tool/audit.log`) records
every call (allow + handler outcome) with the standard
`request_id` / `trace_id` correlation. Read it via:

```bash
cargo run -p relix-flow-inspect -- --audit dev-data/local-tool/audit.log
```

## Configuration knobs in detail

| Field | Default | Notes |
|---|---|---|
| `max_bytes` | `262144` | Hard cap on body size. Per-call `\|N` cannot exceed this. |
| `timeout_secs` | `15` | Total deadline per fetch. `connect_timeout` is `min(timeout_secs, 10)`. |
| `max_redirects` | `3` | Set to `0` for zero-redirect posture. |
| `allow_http` | `false` | Opt-in `http://` (still SSRF-guarded the same way). |
| `user_agent` | `"Relix-tool/<crate-version>"` | Sent as `User-Agent` header. |

The bringup script also accepts `-ToolAllowHttp` (PowerShell) /
`--tool-allow-http` (sh, **not implemented today** — pass the TOML
override manually) to flip `allow_http = true`.

## Failure modes

| Symptom | Cause | Fix |
|---|---|---|
| `policy_denied: scheme 'http' not allowed (allow_http=false)` | URL is `http://` and node config has default `allow_http = false` | Switch to `https://`, or pass `-ToolAllowHttp` and accept the risk. |
| `policy_denied: ip <ip> is in forbidden range '<range>'` | URL host is a literal IP in a forbidden range | Working as intended. Use a public hostname. |
| `policy_denied: hostname '<host>' is denied (<reason>)` | URL host matched the hostname denylist | Working as intended. Use a public hostname. |
| `policy_denied: dns resolution for '<host>' included forbidden ip <ip>` | DNS for the hostname returned at least one private/loopback IP | Working as intended. The flag is "any" forbidden IP. |
| `invalid_args: tool.web_fetch body too large` | Response body exceeded the cap | Increase `max_bytes`, or use `\|<smaller>` in the SOL arg. |
| `invalid_args: tool.web_fetch content-type not text-like: 'application/pdf' for <url>` | Origin returned a non-textual content type | Working as intended — tool node is text-only. |
| `invalid_args: tool.web_fetch body not utf-8 for <url>` | Body decoded as non-UTF-8 | Working as intended. |
| `responder_internal: tool.web_fetch http 404 for <url>` | Origin returned a non-2xx status | Origin issue. |
| `transport: tool.web_fetch transport: error following redirect for url (...)` | A redirect was rejected by the per-hop SSRF re-check **or** transport failed mid-redirect | Check tool node log for the `redirect ssrf-rejected` WARN — it carries the exact target + reason. |

## Future extensions

The tool node is designed for additional capabilities to land on the
same peer with the same admission posture. The obvious candidates
(none in the alpha):

- `tool.html_extract` — pure-parser extraction of title + visible
  text from caller-supplied HTML. No network surface.
- `tool.json_path` — JSON extraction by path. No network surface.
- `tool.url_parse` — break a URL into components.

Adding any of these means: write the handler, register it on the
controller's dispatch bridge, add it to the manifest provider, add
a policy rule. No new architecture.

## See also

- [`tool-node-security.md`](tool-node-security.md) — full SSRF model,
  DNS pin, redirect re-check, pool security invariants, every
  remaining edge.
- [`security.md`](security.md) — how the tool node fits into the
  whole-mesh security model.
- [`operator-guide.md`](operator-guide.md) — log paths, troubleshooting,
  every operator concern outside the tool node specifically.
- [`current-limitations.md`](current-limitations.md) — what the tool
  node doesn't do (GET-only, text-only, no POST, etc.).
