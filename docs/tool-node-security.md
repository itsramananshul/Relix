# Tool Node Security Model (M9)

The tool node introduces Relix's first **external-action capability**:
`tool.web_fetch`. It performs an HTTPS GET of a single URL on the node's
behalf and returns the body text to the SOL flow that called it.

It is also the first capability that can reach arbitrary endpoints on the
host network, so it ships with a deliberately conservative safety model.
**Fail closed** is the default everywhere.

## Architecture

The tool node is a normal peer. It runs the same controller binary, the
same admission pipeline (identity → policy → handler → audit), and the
same wire format as memory and AI nodes. There is no central registry,
no special HTTP bypass, no bridge-side execution.

```
bridge --remote_call--> tool node
                       ├─ admission pipeline (identity / policy / audit)
                       └─ ToolBackend.fetch(url, cap)
                          ├─ security::resolve_safe_url   (SSRF guard)
                          └─ reqwest::Client.get(...)
```

The HTTP client (`reqwest`) and the response cap live **on the tool node**.
The bridge cannot dial the outside world; it can only ask a peer that has
the capability registered.

## SSRF Defence (`security::resolve_safe_url`)

Every call goes through these checks **before** any HTTP I/O:

1. **Scheme allowlist** — `https` always; `http` only when
   `[tool] allow_http = true` (default `false`). `file://`, `ftp://`,
   `gopher://`, custom schemes, and missing schemes are all denied.
2. **Literal-IP check** — if the URL host parses as an IP, it is matched
   against the forbidden ranges (no DNS needed).
3. **Hostname denylist** — exact match for `localhost`,
   `metadata.google.internal`, and similar; suffix match for
   `.local`, `.internal`, `.intranet`, `.lan`, `.corp`, `.home`,
   `.private`.
4. **DNS resolution** — the host is resolved via the OS resolver and
   **every** returned address must be safe. A mixed-result resolution
   (one safe IP + one private IP) is rejected as DNS-rebind bait.
5. **Body cap + content-type filter** — non-text/non-json/non-html
   responses are rejected; bodies that exceed the per-request cap (the
   smaller of `[tool] max_bytes` and any `|N` suffix in the SOL arg)
   are aborted mid-stream.

### Forbidden IP ranges

IPv4 — `0.0.0.0`, `127/8`, RFC 1918 (`10/8`, `172.16/12`, `192.168/16`),
`169.254/16` link-local (AWS/GCP metadata), `100.64/10` CGN,
`198.18/15` benchmark, `224.0.0.0/4` multicast, broadcast,
RFC 5737 documentation (`192.0.2/24`, `198.51.100/24`, `203.0.113/24`),
`240/4` reserved.

IPv6 — `::`, `::1`, `fe80::/10` link-local, `fc00::/7` ULA,
`fec0::/10` deprecated site-local, `2001:db8::/32` documentation,
multicast, plus IPv4-mapped (`::ffff:0:0/96`) and IPv4-compatible
embeddings of any of the IPv4 forbidden ranges.

### Honest limitations

- **DNS rebinding** is mitigated, not eliminated. Today the tool node
  resolves the hostname, validates every returned address, then hands
  the original URL to `reqwest`, which re-resolves and connects. A
  rebind between the safety check and the connect would not be caught.
  At Gate 2 we plan to pin the connection to the inspected SocketAddr
  via a custom hyper resolver.
- **Redirect targets** are bounded by `reqwest`'s redirect policy
  (`[tool] max_redirects = 3` default) but each follow is **not**
  re-screened by `security::resolve_safe_url`. A malicious origin
  could redirect to e.g. `https://example-thatresolvesto-10.0.0.1/`.
  A future milestone replaces the default redirect policy with a
  `Policy::custom` that re-runs the SSRF guard on every hop.
- **OS-level egress filtering** is not configured by the tool node.
  On a shared host, operators should add an iptables / Windows-Firewall
  outbound deny for RFC 1918 networks to the tool node's user account.

## Capability descriptor

`tool.web_fetch` is registered with:

- `kind = Unary`
- `idempotency = AtMostOnce` — same URL may return different bodies; do
  not retry on `responder_internal`.
- `cost_class = ExternalPaid` — touches the outside world.
- `sensitivity_tags = ["external:network", "egress:http"]`.
- `requires_groups = ["chat-users"]`.

Policy can attach to `tool.web_fetch` directly. The alpha policy gives
the same `chat-users` group access as `ai.chat` and `memory.*`; tighten
this on real deployments by issuing a separate `tool-users` group.

## Wire format (alpha)

```
arg:     "<url>"            // GET <url>, cap at [tool] max_bytes
         "<url>|<n>"        // GET <url>, cap at min(n, [tool] max_bytes)
return:  body bytes (UTF-8 only — non-UTF-8 responses are an error)
```

Errors map to `ErrorEnvelope`:

| Cause | `kind` |
|---|---|
| SSRF reject, scheme reject, invalid url | `policy_denied` (6) |
| Body too large, non-text content-type, non-utf8 body | `invalid_args` (5) |
| Non-2xx HTTP | `responder_internal` (11) |
| reqwest transport failure | `transport` (1) |

The bridge maps any of these into a 502/400 with the responder's exact
`cause` string in the response body, so curl / Open WebUI see the rejection
reason instead of an empty 200.

## Audit + flow visibility

- The tool node's audit log records every call (allow + handler outcome).
  Use `relix-flow-inspect --audit dev-data/<run>-tool/audit.log`.
- The flow log on disk records `RemoteCallIssued(tool, ...)` →
  `RemoteCallCompleted | RemoteCallFailed`. Find it at
  `dev-data/flow-runner/flows/<flow_id>.log`; the bridge's HTTP error body
  includes the `flow_id` for cross-correlation.
