# Tool Node Security Model

The tool node exposes Relix's external-action capabilities: web fetch,
filesystem access, terminal execution, browser automation, MCP dispatch,
document parsing, and more. Any capability that touches the outside
world ships with a deliberately conservative safety model.
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
                          ├─ operator blocklist check  (HostBlocklist)
                          ├─ security::resolve_safe_url   (SSRF guard)
                          ├─ url_allowlist check       (UrlAllowlist)
                          └─ reqwest::Client.get(...)
```

The HTTP client (`reqwest`) and the response cap live **on the tool node**.
The bridge cannot dial the outside world; it can only ask a peer that has
the capability registered.

## Secure client pool (`PinnedClientPool`)

The first cut of M9 built a fresh `reqwest::Client` per request so each
fetch could pin its hostname to the IPs the SSRF guard had just
validated. Correct but wasteful: every fetch paid TLS root-store load,
hyper connector construction, TCP handshake, and TLS handshake from
scratch.

Naive reuse would be dangerous. A globally-shared `Client` would either:
- skip `resolve_to_addrs` and rely on the OS resolver at connect time —
  defeating the DNS pin entirely; or
- carry one host's `resolve_to_addrs` pin into requests for *other*
  hosts — invalidating the per-route validation.

The pool instead keys cached `Client`s on the *validated route*:

```text
PoolKey = (hostname, sorted_validated_addrs)
```

Same hostname **and** same DNS-validated address set → reuse the
existing `Client` and its hyper connection pool. Different addrs (legit
DNS change, multi-A round-robin, or a hostile flip) → cache miss → new
`Client` built with `resolve_to_addrs(hostname, new_addrs)`. The stale
entry persists (no LRU eviction in alpha) but can only ever serve
requests pinned to the IPs it was originally validated against.

**Security invariants preserved**:

- A pooled `Client` only serves requests whose validated route matches
  what's pinned inside it. The DNS-guard verdict at lookup time is the
  same one baked into the cached `Client`.
- TLS SNI + `Host` header keep targeting the original hostname (the URL
  is unchanged; only the address the connector dials is pinned).
- The same `Policy::custom` closure is built into every pooled `Client`,
  so per-hop SSRF re-validation runs on every redirect hop regardless of
  which pooled `Client` served the request.
- IP-literal URLs use a single shared unpinned `Client` (no DNS happens
  for those; default behaviour is correct and per-IP pinning would be
  meaningless).
- Tool-node audit log unchanged: `tool.web_fetch` calls go through the
  same admission pipeline (identity → policy → handler → audit). Pool
  state lives in handler memory only.

**Observability**: each cache miss emits a structured INFO line —
`tool.web_fetch: pool miss; built new pinned client hostname=...
pinned_addrs=[...] pool_entries=N pool_hits=H pool_misses=M`. Cache
hits are not logged at INFO (they're the common case); a DEBUG line
is available if needed.

**Live measurement** (mesh up, mock provider, 5 sequential fetches
through `/chat_with_tool` against `https://example.com`):

```
cold first request : 229 ms  (Client build + TLS + DNS + connect)
warm steady state  :  ~90 ms (p50 of requests 2..5; pooled Client + TCP+TLS reuse)
```

~60% reduction in steady-state per-fetch latency over the per-request
build, with the security invariants above intact.

**Honest limitations**:

- **No eviction in alpha.** Entries accumulate over process lifetime,
  one per unique safe `(hostname, addrs)` route. A soft cap of 256
  triggers a WARN; no automatic LRU eviction. Bound is operator-driven
  (set of hosts the operator's flows fetch). LRU lands in a future
  milestone if observed entries grow unbounded in practice.
- **Cross-host redirects** behave exactly as before pooling. The pooled
  `Client` only has a pin for its origin host; cross-host follows are
  handled by the same `Policy::custom` re-validation, with the same
  small window between policy check and connect for the new host that
  exists today.

## DNS pinning between guard and connect (M9 hardening)

The previous alpha cut accepted that `resolve_safe_url` was advisory:
reqwest re-resolved the hostname when it actually dialled, so a hostile
authoritative server could in principle return a safe address during the
guard's lookup and a forbidden one (e.g. `127.0.0.1`) for the connect.

That window is now closed. `ToolBackend::fetch` is now structured as:

1. `security::resolve_safe_url(url)` performs the safety check and
   returns **every** IP the OS resolver gave us — already validated.
2. The handler then builds a **per-request reqwest client** with
   `ClientBuilder::resolve_to_addrs(hostname, &[SocketAddr; n])` pinned
   to those validated addresses.
3. `client.get(url).send()` is called. The URL still contains the
   hostname, so the `Host` header and the TLS SNI keep pointing at the
   original origin. The TCP connect, however, can only target an IP
   we already inspected — reqwest bypasses its built-in resolver when
   a host has a `resolve_to_addrs` entry.

Cost: one reqwest `Client` per request. The pre-pin alpha shared a
single client across all requests; we lose that connection pool. In
exchange we get a property the alpha needs much more than a few ms of
shaved latency — the guard's verdict is the connect's verdict.

For URLs whose host is already an **IP literal** (e.g. `https://1.1.1.1/`)
no pin is set: reqwest doesn't run a resolver in that case, and the
literal IP was already accepted/rejected in step 1.

Live evidence in `pin_forces_connect_to_validated_ip_not_dns` and
`pin_to_one_ip_ignores_other_addresses_in_dns` (run with
`cargo test -p relix-runtime --lib nodes::tool`): a synthetic hostname
in the RFC 2606 `.invalid` TLD is reached over the pin even though it
has no real DNS, and the control test
`unpinned_hostname_fails_dns_proving_pin_is_load_bearing` confirms the
same hostname fails when no pin is set.

Per-hop redirect handling: the original M9 cut left this as a documented
limitation. It's now closed by a `reqwest::redirect::Policy::custom`
closure that (a) enforces `[tool] max_redirects` as a hard cap and
(b) re-runs `resolve_safe_url_blocking` (the sync twin of the async
guard) on every redirect target — same-hostname or cross-hostname.
A `Location:` pointing at `127.0.0.1`, an RFC 1918 literal, or a
hostname that resolves to a forbidden range is rejected before reqwest
follows it. The closure is synchronous (reqwest's API requires it) and
blocks briefly on `std::net::ToSocketAddrs` for the DNS-needing case;
acceptable because redirects are rare. Verified by
`redirect_to_loopback_literal_is_rejected_per_hop`,
`redirect_to_rfc1918_literal_is_rejected_per_hop`, and
`redirect_cap_zero_blocks_any_redirect`.

## SSRF Defence (`security::resolve_safe_url`)

Every outbound web call goes through these checks **before** any HTTP I/O,
in the order listed. All checks also run on every redirect target:

1. **Operator blocklist** (`blocked_hosts`) — case-insensitive **exact**
   hostname match. Runs before scheme/DNS validation so a blocked host
   never reaches the resolver. Configure via `[tool] blocked_hosts = [...]`.
   Matching is exact-only; subdomains are NOT blocked unless listed
   separately (see [Blocklist semantics](#blocklist-semantics)).
2. **Scheme allowlist** — `https` always; `http` only when
   `[tool] allow_http = true` (default `false`). `file://`, `ftp://`,
   `gopher://`, custom schemes, and missing schemes are all denied.
3. **Literal-IP check** — if the URL host parses as an IP, it is matched
   against the forbidden ranges (no DNS needed).
4. **Hostname denylist** — exact match for `localhost`,
   `metadata.google.internal`, and similar; suffix match for
   `.local`, `.internal`, `.intranet`, `.lan`, `.corp`, `.home`,
   `.private`.
5. **DNS resolution** — the host is resolved via the OS resolver and
   **every** returned address must be safe. A mixed-result resolution
   (one safe IP + one private IP) is rejected as DNS-rebind bait.
6. **URL allowlist** (`url_allowlist`) — glob host patterns; empty = no
   restriction. Only fires when the list is non-empty. Cloud-tier clients
   (LlamaParse, Jina, Firecrawl, Tavily, etc.) are **exempt** from this
   check but still subject to the private-IP block. Configure via
   `[tool] url_allowlist = ["*.example.com"]`.
7. **Body cap + content-type filter** — non-text/non-json/non-html
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

### Blocklist semantics

`blocked_hosts` is an **exact hostname match** (case-insensitive). Listing
`evil.example.com` blocks only that exact hostname — NOT `www.evil.example.com`
or `evil.example.com.br`. To block a subtree, list each hostname explicitly.
This matches the per-hostname granularity of feeds like URLhaus and avoids
accidentally blocking unrelated domains.

`url_allowlist` uses glob patterns where `*` matches any run of characters
**including** the `.` separator, so `*.openai.com` matches
`api.openai.com`, `platform.openai.com`, etc. Patterns are stripped of
scheme and path at configuration time: `https://api.example.com/v1/`
normalises to `api.example.com`. When the list is empty (default) no
allowlist filtering occurs.

### `ssrf_protection = false`

Setting `[tool] ssrf_protection = false` **disables the private-IP block
for ALL outbound HTTP** — both tool capability handlers and cloud-tier
clients (LlamaParse, Jina, Firecrawl, etc.). The controller logs a WARNING
at startup. The `url_allowlist` still fires when configured.

This setting is intended for development against a local model server. It
**must not** be set in production.

### Honest limitations

- **DNS rebinding between guard and connect: closed (M9 hardening).**
  See the "DNS pinning between guard and connect" section above. The
  TCP connect now targets a `SocketAddr` validated by the same
  resolution that fed the safety check, via
  `reqwest::ClientBuilder::resolve_to_addrs`. Original hostname is
  preserved in the URL → `Host` header and TLS SNI keep working.
  Verified by the live tests
  `pin_forces_connect_to_validated_ip_not_dns` /
  `pin_to_one_ip_ignores_other_addresses_in_dns` and the control
  `unpinned_hostname_fails_dns_proving_pin_is_load_bearing`.
- **Per-hop redirect re-validation: closed.** A
  `reqwest::redirect::Policy::custom` closure re-runs
  `resolve_safe_url_blocking` on every redirect target before reqwest
  follows it. Cross-hostname `Location: http://attacker.example/`,
  literal-IP `Location: http://127.0.0.1/`, and RFC 1918 targets are
  all rejected. The closure also enforces `[tool] max_redirects` as a
  hard cap. The closure is sync (reqwest's API requires it) and blocks
  the calling Tokio worker briefly on `std::net::ToSocketAddrs`; this
  is acceptable since redirects are rare. Operators who still prefer
  zero redirect-follow ambiguity should set `[tool] max_redirects = 0`.
- **OS-level egress filtering** is not configured by the tool node.
  On a shared host, operators should add an iptables / Windows-Firewall
  outbound deny for RFC 1918 networks to the tool node's user account.

## Capability descriptors

`tool.web_fetch` is registered with:

- `kind = Unary`
- `idempotency = AtMostOnce` — same URL may return different bodies; do
  not retry on `responder_internal`.
- `cost_class = ExternalPaid` — touches the outside world.
- `risk = Medium`.
- `sensitivity_tags = ["external:network", "egress:http"]`.
- `requires_groups = ["chat-users"]`.

`tool.terminal.run` is registered with:

- `idempotency = AtMostOnce`
- `cost_class = ExternalPaid`
- `risk = High`
- `requires_groups = ["operators"]`

Policy can attach to any capability directly. The alpha policy gives
`chat-users` access to web and memory capabilities; terminal and browser
require `operators`. Tighten further by issuing narrower group scopes.

## Output Guard

Every tool reply passes through `ToolOutputGuard::inspect` before returning
to the caller. Two risks are mitigated:

- **Prompt injection via tool output** — a fetched web page containing
  "ignore previous instructions" would otherwise inject those instructions
  into the model's context on the next turn.
- **Pathologically large output** — uncapped tool output could exhaust
  the model's context window.

Processing order (the earlier check sets the verdict):

1. **Suspicious JSON keys** — keys containing `system_prompt`,
   `instructions`, or `ignore_previous` (case-insensitive substring) in
   any JSON payload. Checked first so structured injection reports the
   stronger key-shaped signal.
2. **Injection phrases** — same phrase set as the AI input guardrail.
3. **Truncation** — output exceeding 50 000 characters is truncated;
   `"\n...[truncated]"` is appended. Truncated replies still succeed
   (logged at WARN).

`injection_detected = true` maps to `HandlerFailed`. `truncated = true`
passes through with a WARN log.

## Wire format (`tool.web_fetch`)

```
arg:     "<url>"            // GET <url>, cap at [tool] max_bytes
         "<url>|<n>"        // GET <url>, cap at min(n, [tool] max_bytes)
return:  body bytes (UTF-8 only — non-UTF-8 responses are an error)
```

Error mapping for all web capabilities:

| Cause | `kind` |
|---|---|
| SSRF reject, scheme reject, invalid url, blocklist hit, not-allowlisted | `policy_denied` (6) |
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
