# Plugin system

Third-party code can extend Relix without modifying the core
codebase. A plugin is a separate subprocess that exposes one or
more capabilities to the mesh via the `relix-plugin-v1` wire
protocol. The `plugin_host` node type loads plugins at boot,
registers their capabilities on its dispatch bridge, and acts as
a normal mesh peer for the rest of the system.

```
   ┌──────────────────────────────────────────────────────────┐
   │ plugin_host controller (node_type = "plugin_host")       │
   │                                                          │
   │   ┌──────────────┐     ┌──────────────┐                  │
   │   │ DispatchBridge│◄──►│ plugin.list  │   (management)   │
   │   │              │     │ plugin.status│                  │
   │   │              │     │ plugin.reload│                  │
   │   │              │     │ plugin.disable│                 │
   │   │              │     └──────────────┘                  │
   │   │              │                                       │
   │   │              │   TLS loopback (newline JSON)         │
   │   │              ├─────────────────────► [hello-plugin]  │
   │   │              ├─────────────────────► [web-lookup]    │
   │   │              ├─────────────────────► …               │
   │   └──────────────┘                                       │
   │                                                          │
   └──────────────────────────────────────────────────────────┘
```

## When you'd reach for this

- **You need a capability the built-in node types don't have.**
  e.g. wrap a third-party REST API, a database query surface, a
  local LLM tool runtime, an SSO callback handler.
- **You want to ship something written in a non-Rust language.**
  The Rust SDK is provided as a convenience; the wire protocol is
  the contract. Non-Rust implementations must serve the same TLS
  loopback transport (see Protocol below).
- **You want capability-level isolation.** Plugins live in their
  own subprocesses. A panicking plugin can't take the rest of
  the node down.

## Protocol: `relix-plugin-v1`

Plugins run as subprocesses spawned by a `plugin_host`. The host
reads the plugin's stdout for a port announcement, then
communicates with the plugin over **loopback TLS** — not
plaintext HTTP.

### Transport

The plugin protocol is **newline-delimited JSON over a TLS
stream** on `127.0.0.1:<port>`. There are no HTTP endpoints
(`/health`, `/ready`, `/invoke` do not exist). Each TCP
connection carries exactly one request frame followed by exactly
one response frame.

The TLS layer uses `tokio-rustls` with `aws_lc_rs` as the crypto
provider. The dispatcher **pins the exact DER certificate** the
plugin was given at spawn time; no system CAs are trusted. A
client presenting a different certificate fails the TLS handshake
outright.

### Startup contract

1. The plugin binds a TLS listener on `127.0.0.1:<port>` where
   `<port>` is **kernel-chosen** (bind to `127.0.0.1:0`).
2. On its **first line of stdout**, the plugin writes:
   ```
   RELIX_PLUGIN_PORT=<port>
   ```
3. The host reads that line, then polls a health frame until the
   plugin responds `{"ok":true}`. The polling interval is 200ms.
4. Once healthy, the host registers each capability declared in
   the manifest on its dispatch bridge as a handler that routes
   incoming calls to the plugin over the TLS connection.

### Required environment variables

The host loader sets three environment variables on every spawned
plugin process. The plugin **must** read these at startup; the
SDK handles this automatically. If any is absent, `serve()`
fails closed.

| Variable | Content |
|---|---|
| `RELIX_PLUGIN_BEARER` | 32 random bytes hex-encoded; per-plugin, per-launch bearer token |
| `RELIX_PLUGIN_TLS_CERT_DER_B64` | Base64-encoded DER of the plugin's self-signed TLS certificate (IP SAN `127.0.0.1`) |
| `RELIX_PLUGIN_TLS_KEY_DER_B64` | Base64-encoded DER of the corresponding PKCS#8 private key |

The certificate is minted fresh on every spawn. The dispatcher
pins this exact cert in its `RootCertStore`.

### Wire frames

**Request (one JSON object per line):**

```json
{"op":"health"}
{"op":"invoke","bearer":"<64-hex>","request":{"method":"...","args":"...","trace_id":"...","request_id":"...","caller_subject_id":"...","deadline_unix":0}}
```

**Response (one JSON object per line):**

```json
{"ok":true}
{"ok":true,"body":"result string"}
{"ok":false,"error_kind":11,"error_cause":"human-readable cause"}
```

### Invoke request fields

| Field | Type | Meaning |
|---|---|---|
| `method` | string | Capability method name (e.g. `my_plugin.do_thing`) |
| `args` | string | Pipe-delimited or structured argument string |
| `trace_id` | string | Hex trace correlation id |
| `request_id` | string | Hex per-request id |
| `caller_subject_id` | string | Verified caller identity |
| `deadline_unix` | i64 | Unix timestamp; plugin should bail past this |

### Invoke response

Success:
```json
{ "ok": true, "body": "result string" }
```

Failure:
```json
{
  "ok":          false,
  "error_kind":  11,
  "error_cause": "human-readable cause"
}
```

`error_kind` mirrors `relix_core::types::error_kinds`. The common
ones a plugin returns:

| Kind | Constant | Meaning |
|---|---|---|
| 4   | `UNKNOWN_METHOD`      | Plugin has no handler for `method` |
| 5   | `INVALID_ARGS`        | Caller passed malformed / missing args |
| 11  | `RESPONDER_INTERNAL`  | Plugin's own error — panic-recovered, bad downstream, etc. |
| 12  | `RESPONDER_OVERLOADED`| Plugin's upstream is rate-limited; caller may retry |
| 401 | `UNAUTHORIZED`        | Bearer token missing or wrong |

Wrong-bearer responses look like:
```json
{"ok":false,"error_kind":401,"error_cause":"unauthorized: bearer mismatch"}
```

## `plugin.toml` reference

```toml
[plugin]
name        = "my-plugin"      # lowercase + hyphens + digits, 3..=64 chars
version     = "0.1.0"
description = "What this plugin does"
author      = "Author Name"     # optional
homepage    = ""                # optional
license     = "Apache-2.0"      # optional

# Supply-chain security (optional but recommended for distributed plugins).
# 64-hex Ed25519 public key. When set, the loader requires a sibling
# `plugin.toml.sig` file (128-hex raw Ed25519 signature over the manifest
# TOML bytes). Missing or invalid signature refuses load.
publisher_key = "<64-hex Ed25519 pubkey>"

# At least one provides entry is required.
[[plugin.capabilities.provides]]
method            = "my_plugin.do_thing"     # dotted [a-z][a-z0-9_]*
description       = "Does a thing"
categories        = ["tool", "external"]     # optional
sensitivity_tags  = ["external:api"]         # optional
risk_level        = "low"                    # low | medium | high

[plugin.runtime]
kind                 = "subprocess"          # only "subprocess" today
binary               = "./my-plugin-binary"  # see Binary resolution below
args                 = ["--serve"]           # optional
protocol             = "relix-plugin-v1"     # only "relix-plugin-v1"
invoke_timeout_secs  = 30                    # 1..=300; default 30

# SHA-256 of the binary (lowercase hex, 64 chars). Optional but recommended:
# the loader hashes the binary at spawn time and refuses a mismatch.
binary_sha256 = "<64-hex SHA-256 of binary>"
```

### Binary resolution

- **Bare name** (`binary = "python"`) — **REFUSED**. The loader
  rejects bare command names with no path separator
  (`ManifestError::Invalid`). This prevents a hostile entry on
  the host's `PATH` from shadowing the intended binary.
- **Absolute path** (`binary = "/opt/my-plugin/bin/serve"`) —
  used as-is. Required to exist and canonicalize.
- **Relative path** (`binary = "./my-plugin-binary"`) — resolved
  against the manifest directory, then canonicalized to an
  absolute path.

### Publisher key signing workflow

To ship a signed manifest:

1. Generate an Ed25519 keypair.
2. Set `publisher_key` to the 64-hex encoding of the public key.
3. Sign the manifest's exact TOML bytes (do not normalize) with
   the private key. The signature is 64 raw bytes.
4. Write the signature as 128 lowercase hex characters into
   `plugin.toml.sig` alongside the manifest.

The loader reads the `.sig` file, decodes the hex, and verifies
with `ed25519_dalek`. Missing or invalid signature refuses load.

### Binary SHA-256 pinning

Compute with any standard tool, e.g.:

```
sha256sum my-plugin-binary   # Linux/macOS
```

Record the 64-char lowercase hex output as `binary_sha256` in
the manifest. The loader hashes the binary at spawn time and
refuses if it does not match. Omitting the field skips this check.

## Writing a plugin in Rust (the SDK)

Add the SDK as a dependency:

```toml
[dependencies]
relix-plugin-sdk = "0.4.1"   # or = { path = "../relix-plugin-sdk" }
tokio            = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Tiny example:

```rust
use relix_plugin_sdk::{InvokeRequest, PluginError, PluginServer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut server = PluginServer::new();
    server.register("my_plugin.greet", |req: InvokeRequest| async move {
        if req.args.is_empty() {
            return Err(PluginError::invalid_args("name required"));
        }
        Ok(format!("hello, {}", req.args))
    });
    // Reads RELIX_PLUGIN_BEARER, RELIX_PLUGIN_TLS_CERT_DER_B64,
    // RELIX_PLUGIN_TLS_KEY_DER_B64 from env; fails closed if any absent.
    server.serve().await?;
    Ok(())
}
```

The SDK binds `127.0.0.1:0`, writes `RELIX_PLUGIN_PORT=<n>` to
stdout, serves TLS using the cert and key from env, and enforces
the bearer on every `invoke` frame with a constant-time comparison.

See `crates/relix-plugin-sdk/src/` and the reference binary
`relix-tls-echo-plugin` (the `echo.say` handler) for a complete
worked example.

## Writing a plugin in a non-Rust language

The Rust SDK is a convenience wrapper. A plugin in Python or any
other language must implement the wire protocol directly:

1. Read `RELIX_PLUGIN_BEARER`, `RELIX_PLUGIN_TLS_CERT_DER_B64`,
   and `RELIX_PLUGIN_TLS_KEY_DER_B64` from the environment.
   Fail if any is absent.
2. Decode the cert and key from base64-DER.
3. Bind a TLS listener on `127.0.0.1:0` using those materials.
4. Print `RELIX_PLUGIN_PORT=<port>` as the first stdout line.
5. Accept connections; for each, read a newline-delimited JSON
   frame, dispatch on `op`, write a newline-delimited JSON
   response, close the connection.
6. Validate the `bearer` field on every `invoke` frame against
   the env var (use a constant-time comparison).

Note that without the Rust SDK, the TLS setup is more involved
than a plain HTTP server. There are no HTTP endpoints to implement.

## Host config: `[plugin_host]`

```toml
[controller]
node_type   = "plugin_host"
listen_port = 19718

[plugin_host]
plugin_dir       = "./plugins"                    # directory scanned at boot (required)
max_plugins      = 20                             # safety cap; default 20
registry_db_path = "dev-data/plugin-registry.db"  # SQLite registry; default ./plugin-registry.db
max_memory_mb    = 512                            # RLIMIT_AS cap per plugin process (Unix); default 512; 0 = disable
max_cpu_secs     = 30                             # RLIMIT_CPU cap per plugin process (Unix); default 30; 0 = disable
```

`max_open_fds` (RLIMIT_NOFILE) is hardcoded to `100` in
`SandboxLimits::default()` and is **not** operator-configurable
via TOML.

The host walks `plugin_dir` at depth 1, accepting either:

- `plugin_dir/plugin.toml` (single plugin), or
- `plugin_dir/<name>/plugin.toml` (one subdir per plugin).

## Calling a plugin from a flow

Both SOL and `.sflow` work. The plugin_host bridge registers
each capability under two names — the bare manifest name and a
`plugin_host.<method>` alias — so callers can use whichever
form is natural for the language.

SOL — call by the bare manifest name:

```rust
// flows/hello.sol
function start() -> str {
    let reply: str = remote_call("plugin_host", "hello.greet", "alice");
    return reply;
}
```

`.sflow` — the parser preserves the full dotted target as the
wire method, so the natural `plugin_host.<method>` form admits
correctly against the prefixed alias:

```
step reply: plugin_host.hello.greet "alice"
return step.reply.result
```

The same applies to plugin management capabilities — they're
callable from sflow as `plugin_host.plugin.list` /
`plugin_host.plugin.status` / etc.

## Management capabilities + HTTP / CLI

| Capability | HTTP | CLI |
|---|---|---|
| `plugin.list` | `GET /v1/plugins` | `relix-cli ops plugin list` |
| `plugin.status` | `GET /v1/plugins/:id` | `relix-cli ops plugin status --plugin-id <id>` |
| `plugin.reload` | `POST /v1/plugins/:id/reload` | `relix-cli ops plugin reload --plugin-id <id>` |
| `plugin.disable` | `POST /v1/plugins/:id/disable` | `relix-cli ops plugin disable --plugin-id <id>` |

The dashboard `#/plugins` page shows the same data with a
clickable list + a detail card with Reload / Disable buttons.

## Lifecycle states

| State | What it means | How to reach |
|---|---|---|
| `registered` | Manifest parsed + stored. Subprocess not running. | First scan; failed reload |
| `active`     | Subprocess up; health probe returned `{"ok":true}`. Capabilities live. | Successful spawn |
| `error`      | Spawn or health probe failed. `error_message` describes. | Subprocess failed to start, exited, or health never returned ok within the probe deadline |
| `disabled`   | Operator explicitly stopped it. | `plugin.disable` |

## Security posture

- **Subprocess isolation.** Plugins run in their own OS process.
  A panicking plugin can't take the plugin_host down. Killing
  the plugin_host kills the children (tokio
  `Command::kill_on_drop(true)`).

- **Loopback TLS with pinned certificate.** All plugin
  communication uses TLS on `127.0.0.1`. Each plugin gets a
  fresh self-signed certificate (IP SAN `127.0.0.1`) minted at
  spawn time. The dispatcher pins this exact DER cert in its
  `RootCertStore`; no system CAs are trusted. There is no
  plaintext HTTP path.

- **Per-plugin bearer token.** A 32-byte random bearer token is
  minted at spawn and delivered to the plugin via
  `RELIX_PLUGIN_BEARER`. The plugin validates this token on every
  `invoke` frame using a constant-time comparison. Wrong or
  missing bearer → `error_kind=401`.

- **Binary SHA-256 supply-chain gate.** When `binary_sha256` is
  set in the manifest, the loader hashes the binary at spawn time
  and refuses a mismatch. Prevents a replaced binary from running
  under a trusted manifest.

- **Publisher key signature.** When `publisher_key` is set, the
  loader verifies the sibling `.sig` file (Ed25519 signature over
  the manifest's exact TOML bytes) before any other load step.
  Missing or invalid signature refuses load.

- **Linux sandbox (rlimits + seccomp + PR_SET_NO_NEW_PRIVS).**
  On Linux, each plugin process is sandboxed via `pre_exec`
  (between fork and execve):
  - `RLIMIT_AS` = `max_memory_mb` MiB (virtual memory cap)
  - `RLIMIT_CPU` = `max_cpu_secs` seconds
  - `RLIMIT_NOFILE` = 100 (hardcoded)
  - `RLIMIT_CORE` = 0 (no core dumps, unconditional)
  - `prctl(PR_SET_NO_NEW_PRIVS, 1)` — privilege escalation disabled
  - Seccomp BPF filter with **default ALLOW** and explicit DENY list

  The following 23 syscalls are denied (KillProcess on attempt):
  `init_module`, `finit_module`, `delete_module`, `kexec_load`,
  `kexec_file_load`, `mount`, `umount2`, `pivot_root`, `chroot`,
  `reboot`, `ptrace`, `perf_event_open`, `capset`, `setuid`,
  `setgid`, `setreuid`, `setregid`, `setresuid`, `setresgid`,
  `bpf`, `swapon`, `swapoff`.

  Seccomp is supported on `x86_64` and `aarch64`; other
  architectures keep rlimits + `PR_SET_NO_NEW_PRIVS` but skip
  the BPF filter.

- **macOS / non-Linux Unix:** rlimits only; no seccomp.

- **Windows / non-Unix: fails closed.** `SandboxLimits::ensure_enforceable()`
  returns `LoadError::SandboxUnenforceable` if any cap is
  non-zero. The previous behavior (warn and continue) has been
  replaced by hard refusal. To run a plugin on Windows, set all
  three limits to `0` in TOML (`max_memory_mb = 0`,
  `max_cpu_secs = 0`; `max_open_fds` is not TOML-configurable).
  With all caps at zero, load proceeds but no sandbox is applied.

- **Capability gating through the policy engine.** Every method
  a plugin registers passes through the same `PolicyEngine`
  admission as built-in capabilities. Operators write rules
  for plugin methods in the same TOML they already use:
  ```toml
  [[rules]]
  name         = "my_plugin_do_thing"
  method       = "my_plugin.do_thing"
  allow_groups = ["chat-users"]
  ```

- **No automatic credential sharing.** A plugin process gets
  its own environment. The host does not inject any of its own
  identity, mesh peer credentials, or provider API keys. If a
  plugin needs an API key, the operator sets it in the plugin's
  own env at startup.

- **No mesh trust escalation.** A plugin returning `ok: true`
  doesn't bypass the dispatch bridge's audit log, sensitivity
  tags, or admission steps. The plugin_host treats plugin
  responses the same way it would treat any other handler's
  outcome.

## Deployment notes

- **Plugins must respect `deadline_unix`.** The host sets it
  from `now + invoke_timeout_secs`; plugins should short-circuit
  past it. Today the SDK doesn't enforce — well-behaved plugins
  check `deadline_unix < now` and bail early.
- **Long-running work belongs in the plugin.** The host's
  per-call deadline (default 30s) is a hard ceiling. For a
  multi-minute background task, the plugin should kick off the
  work asynchronously and expose a separate capability to poll
  for results.
- **Plugins can crash.** The host detects this on the next
  invoke (Transport error) and surfaces a 502 to callers. The
  plugin_host does not automatically restart failed plugins —
  use `plugin.reload` (`/v1/plugins/:id/reload`) or restart the
  plugin_host node.
- **The registry survives restarts.** `plugin-registry.db`
  carries `(plugin_id, status, error_message, last_seen_at)`
  across reboots so the dashboard shows persistent history.
- **Plugin IDs are stable.** The `plugin_id` is
  `blake3("name|version|absolute_manifest_path")[0..16]` — a
  16-char hex prefix. It is recomputed deterministically on every
  scan, so the registry row for a given plugin is stable across
  reboots as long as the (name, version, manifest path) triple
  does not change.
