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

Remaining honest gap: **per-hop redirect re-validation**. If the
configured `max_redirects > 0` and the first hop returns `302 Location:
http://attacker-rebind/`, reqwest's own redirect policy follows it
without re-running `security::resolve_safe_url`. The follow inherits
*this* request's pinning, so it still can't reach an address we have
already validated as forbidden via the same hostname — but a redirect
to a *different* hostname is currently re-resolved by reqwest with no
pin. Tracked for a future milestone; the safest interim posture is
`max_redirects = 0` for high-blast-radius deployments.

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
- **Per-hop redirect re-validation: still open.** Redirects within the
  same hostname inherit this request's pin and therefore can only land
  on validated IPs. *Cross-hostname* redirects (e.g. `Location:
  http://attacker.example/`) are re-resolved by reqwest without a pin
  and not re-checked by `resolve_safe_url`. Operators worried about
  this should set `[tool] max_redirects = 0` until a `Policy::custom`
  is wired in a future milestone.
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
