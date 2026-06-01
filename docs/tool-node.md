# Tool Node

The tool node is Relix's external-action subsystem. It is a normal
controller peer (`[controller] node_type = "tool"`) that runs the same
identity → policy → handler → audit pipeline as every other peer. Its
primary job is keeping all outbound network I/O, filesystem access,
shell execution, browser automation, MCP dispatch, and document
parsing **off the bridge** and behind that admission pipeline.

This document covers the full operator surface: all 40+ capabilities,
every `[tool.*]` config key, and the per-call request lifecycle.
For the SSRF model, DNS-pin guarantee, and secure client pool see
[`tool-node-security.md`](tool-node-security.md).

## Why a separate peer

Three reasons the bridge does **not** act externally itself:

1. **Admission pipeline.** Every capability invocation runs identity →
   policy → handler → audit. If the bridge acted directly, the SSRF
   guard, the policy check, and the audit record would all be skipped.
2. **Single source of orchestration.** The SOL flow describes the plan.
   Letting the bridge make side-calls means the same plan lives in two
   places.
3. **Blast-radius isolation.** The tool node owns its own `reqwest::Client`
   pool, its own DNS resolver overrides, and its own SSRF rules. A bug
   in the bridge cannot escalate into outbound network access — the
   bridge has no HTTP client at all.

## Capability families

| Family | Capabilities | Enabled by |
|---|---|---|
| Network | `tool.web_fetch`, `tool.web_get`, `tool.web_extract`, `tool.web_search`, `tool.web.post`, `tool.web.robots_check`, `tool.web.blocklist_summary` | Always registered |
| Document | `tool.parse_document`, `tool.web_read` | Always registered (default config) |
| PDF | `tool.pdf` | `[tool.pdf]` section |
| Filesystem | `tool.read_file`, `tool.write_file`, `tool.search_files`, `tool.patch`, `tool.patch_preview`, `tool.append_file`, `tool.list_dir`, `tool.binary_sniff`, `tool.fuzzy_replace`, `tool.fs.tree`, `tool.fs.stat`, `tool.fs.audit_recent` | `[tool.fs]` section |
| Terminal | `tool.terminal.run`, `tool.terminal.spawn`, `tool.terminal.sessions`, `tool.terminal.tail`, `tool.terminal.cancel`, `tool.terminal.audit_recent`, `tool.terminal.shell.open`, `tool.terminal.shell.input`, `tool.terminal.shell.control`, `tool.terminal.shell.close` | `[tool.terminal]` section |
| Browser | `tool.browser.open_session`, `tool.browser.close_session`, `tool.browser.navigate`, `tool.browser.get_text`, `tool.browser.screenshot`, `tool.browser.list_sessions`, `tool.browser.click`, `tool.browser.type_text`, `tool.browser.wait_for_selector`, `tool.browser.capture_read` | `[tool.browser]` section |
| MCP | `tool.mcp.list_servers`, `tool.mcp.list_tools`, `tool.mcp.invoke` | `[tool.mcp]` section |
| Screen | `tool.screen` | Always registered; disabled by default (`enabled = false`) |
| Text | `tool.text.chunk` | Always registered |
| Operator | `tool.ask_human` | Always registered |
| Memory proxy | `memory.session_search` | Always registered |

All capabilities share the same admission pipeline. The output of every
capability passes through the **Output Guard** before returning to the
caller: truncated at 50 000 characters and scanned for prompt-injection
patterns (see [Output Guard](#output-guard)).

## Configuration

The bringup script generates the tool config at `dev-data/<run>/tool.toml`.
A fully-annotated example covering all subsystems:

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
# ── Network ──────────────────────────────────────────────────────────────────
max_bytes            = 262144    # 256 KiB hard cap on fetch/post body
timeout_secs         = 15        # total per-request deadline (connect + read)
max_redirects        = 3         # 0 disables all redirects
allow_http           = false     # https-only by default
user_agent           = "Relix-tool/<crate-version>"  # set dynamically at build time
extract_max_input_bytes = 1048576  # 1 MiB cap on tool.web_extract HTML input

# ── SSRF / blocklist ─────────────────────────────────────────────────────────
ssrf_protection = true      # master switch; false logs WARNING and disables
                            # private-IP blocking (development only)
blocked_hosts   = [         # exact-match hostname denylist (case-insensitive)
  "evil.example.com",       # blocks ONLY this hostname, NOT subdomains
]
url_allowlist   = [         # glob host allowlist; empty = no restriction
  "*.openai.com",           # * matches any chars including dots
  "api.anthropic.com",
]
# Cloud-tier clients (LlamaParse/Jina/Firecrawl) are EXEMPT from url_allowlist
# but still subject to private-IP blocking when ssrf_protection = true.

# ── Filesystem jail ───────────────────────────────────────────────────────────
[tool.fs]
root             = "dev-data/local/workspace"   # must exist at startup
max_read_bytes   = 10485760   # 10 MiB per read (hard reject, no truncation)
max_write_bytes  = 10485760   # 10 MiB per write/patch
max_search_results = 200

# ── PDF parser ───────────────────────────────────────────────────────────────
[tool.pdf]
max_input_bytes  = 20971520   # 20 MiB (base64-decoded)
max_pages        = 200
max_output_chars = 200000

# ── Terminal ─────────────────────────────────────────────────────────────────
[tool.terminal]
allowed_commands = ["git", "rg", "jq"]   # bare names only; no paths, no globs
allowed_shells   = []                     # bare shell names for shell.open
max_timeout_secs = 30
inherit_env      = false          # when true, scrubs credential vars from env
working_dir      = "/tmp/work"    # child cwd (optional)
allowed_dirs     = []             # when non-empty, working_dir must be inside
env_allowlist    = []             # credential vars exempted from scrubber
pty              = false          # requires --features terminal-pty

# ── Browser ──────────────────────────────────────────────────────────────────
[tool.browser]
backend                 = "none"             # none | headless_chrome | playwright | webdriver
max_sessions            = 16
call_timeout_secs       = 30
webdriver_url           = "http://127.0.0.1:9515"  # chromedriver/geckodriver URL
screenshot_on_failure_dir = "/tmp/relix-screenshots"  # optional; dir must exist

# ── MCP ───────────────────────────────────────────────────────────────────────
[tool.mcp]
[[tool.mcp.servers]]
id             = "fs-helper"
transport      = "stdio"
endpoint       = "mcp-fs-server"
description    = "Local filesystem MCP server"
declared_tools = ["search", "read", "write"]

[[tool.mcp.servers]]
id       = "npm-everything"
transport = "stdio"
endpoint = "everything"
command  = "npx"
args     = ["-y", "@modelcontextprotocol/server-everything"]

[[tool.mcp.servers]]
id             = "remote-search"
transport      = "http"
endpoint       = "https://mcp.example.com"
declared_tools = []

# ── Document parsing ─────────────────────────────────────────────────────────
[tool.parse_document]
enabled                  = true
prefer_cloud             = true    # false = skip all cloud tiers
llama_cloud_api_key_env  = "LLAMA_CLOUD_API_KEY"
jina_api_key_env         = "JINA_API_KEY"
firecrawl_api_key_env    = "FIRECRAWL_API_KEY"
cloud_timeout_secs       = 60

[tool.web_read]
cloud_timeout_secs = 30   # note: different default from parse_document (60)

# ── Screen capture ───────────────────────────────────────────────────────────
[tool.screen]
enabled      = false   # must opt in; captures the host's live display
timeout_secs = 15
# temp_dir = "/tmp"   # optional; defaults to std::env::temp_dir()

[peers]
```

> **Important:** `ToolConfig` uses `#[serde(deny_unknown_fields)]`. An
> unrecognised key (for example the legacy `max_body_bytes` instead of
> `max_bytes`) is a **hard parse error**, not a silently-dropped key.
> The tool node will refuse to start until the TOML is corrected.

> **`user_agent`:** The default value is built at compile time as
> `format!("Relix-tool/{}", env!("CARGO_PKG_VERSION"))`. Do not
> hardcode a version string in TOML unless you specifically need to
> override it; the dynamic default tracks the binary's version.

## Configuration knobs in detail

### Top-level `[tool]`

| Field | Default | Notes |
|---|---|---|
| `max_bytes` | `262144` (256 KiB) | Hard cap on fetch/post body. Per-call `\|N` cannot exceed this. |
| `timeout_secs` | `15` | Total deadline per request. `connect_timeout = min(timeout_secs, 10)`. |
| `max_redirects` | `3` | Set `0` for zero-redirect posture. |
| `allow_http` | `false` | Opt-in `http://`; still SSRF-guarded. |
| `user_agent` | `"Relix-tool/<crate-version>"` | Dynamic; see note above. |
| `extract_max_input_bytes` | `1048576` (1 MiB) | Cap for `tool.web_extract` HTML input. |
| `ssrf_protection` | `true` | `false` logs WARNING and disables private-IP blocking everywhere (tool caps AND cloud tiers). Production must leave `true`. |
| `blocked_hosts` | `[]` | Exact hostname match; case-insensitive; no subdomain matching. Runs before scheme/DNS. |
| `url_allowlist` | `[]` | Glob host patterns (`*` matches any chars including `.`); empty = no restriction. Cloud tiers exempt. |

### `[tool.fs]`

| Field | Default | Notes |
|---|---|---|
| `root` | required | Jail root directory; must exist at startup. |
| `max_read_bytes` | 10 MiB | Hard reject before read; not truncation. |
| `max_write_bytes` | 10 MiB | Per write/patch cap. |
| `max_search_results` | `200` | Also applies to `tree` and `list_dir`. |

### `[tool.pdf]`

| Field | Default | Notes |
|---|---|---|
| `max_input_bytes` | 20 MiB | Base64-decoded PDF byte cap. |
| `max_pages` | `200` | Page count cap. |
| `max_output_chars` | `200000` | Extracted text truncation cap. |

### `[tool.terminal]`

| Field | Default | Notes |
|---|---|---|
| `allowed_commands` | required | Bare program names for `run`/`spawn`; no path separators. |
| `allowed_shells` | `[]` | Bare names for `shell.open`. |
| `max_timeout_secs` | `30` | Per-run ceiling. |
| `inherit_env` | `false` | When `true`, scrubs credential vars (see [Credential scrubber](#terminal-credential-scrubber)) before passing env to child. |
| `working_dir` | None | Child cwd; defaults to controller's cwd. |
| `allowed_dirs` | `[]` | When non-empty, `working_dir` must canonicalize under one of these. |
| `env_allowlist` | `[]` | Credential env-var names exempted from the scrubber. |
| `pty` | `false` | PTY mode; requires `--features terminal-pty`. Selecting `true` without the feature is a loud startup error. |

### `[tool.browser]`

| Field | Default | Notes |
|---|---|---|
| `backend` | `"none"` | One of `none`, `headless_chrome`, `playwright`, `webdriver`. See [browser-tool.md](browser-tool.md). |
| `max_sessions` | `16` | Session cap; enforced by every backend. |
| `call_timeout_secs` | `30` | Per-call deadline; surfaced in error envelopes. |
| `webdriver_url` | `"http://127.0.0.1:9515"` | Chromedriver / geckodriver URL; used only when `backend = "webdriver"`. |
| `screenshot_on_failure_dir` | None | Directory for failure PNGs; enables `tool.browser.capture_read`. Dir must already exist. |

### `[[tool.mcp.servers]]`

| Field | Notes |
|---|---|
| `id` | Unique per node; non-empty. |
| `transport` | `"stdio"` or `"http"`. |
| `endpoint` | Bare program name (stdio, fallback) or `http(s)://` URL (http). |
| `command` | Optional; wins over `endpoint` when set (e.g. `"npx"`). |
| `args` | Program args; default `[]`. |
| `declared_tools` | Static tool list; used as fallback when live discovery fails. |
| `description` | Human-readable label. |

### `[tool.parse_document]`

| Field | Default | Notes |
|---|---|---|
| `enabled` | `true` | Master switch. |
| `prefer_cloud` | `true` | `false` skips all cloud tiers without requiring env vars to be unset. |
| `llama_cloud_api_key_env` | `"LLAMA_CLOUD_API_KEY"` | Env var name (read at runtime, not parse time). |
| `jina_api_key_env` | `"JINA_API_KEY"` | Env var name. |
| `firecrawl_api_key_env` | `"FIRECRAWL_API_KEY"` | Env var name. |
| `cloud_timeout_secs` | `60` | Per-cloud-call deadline. |

### `[tool.web_read]`

| Field | Default | Notes |
|---|---|---|
| `cloud_timeout_secs` | `30` | Note: 30 s here vs 60 s for `parse_document`. |

### `[tool.screen]`

| Field | Default | Notes |
|---|---|---|
| `enabled` | `false` | Must be `true` to capture; always registered. |
| `timeout_secs` | `15` | Capture subprocess deadline. |
| `temp_dir` | None | Defaults to `std::env::temp_dir()`. |

## Network capabilities

### `tool.web_fetch`

HTTP(S) GET, returning UTF-8 body text.

```
arg:    "<url>"          # cap at [tool] max_bytes
        "<url>|<n>"      # cap at min(n, max_bytes)
return: body bytes (UTF-8)
```

### `tool.web_get`

One-shot fetch + HTML extraction.

```
arg:    "<mode>|<url>"
modes:  text | title | links | meta | all | raw
```

### `tool.web_extract`

Pure-CPU HTML extraction (no network); input is caller-supplied HTML.

```
arg:    "<mode>|<html>"
modes:  text | title | links | meta | markdown | all
```

### `tool.web_search`

DuckDuckGo HTML scrape. No API key required.

```
arg:    "<query>"              # default max_results = 10
        "<query>|<max>"        # clamped to 1–20
return: tab-delimited rows: <rank>\t<url>\t<title>\t<snippet>
        + trailing "result_count=N"
```

### `tool.web.post`

HTTP POST with the same SSRF pipeline as `web_fetch`. Forwards
`Set-Cookie` headers verbatim.

### `tool.web.robots_check`

Fetches `<scheme>://<host>/robots.txt` and checks the target URL
against RFC 9309 / Google-subset rules. Tie-break goes to `Allow`.
Path matching is **literal prefix only** — no wildcard path patterns.

```
arg:    "<target_url>"
        "<target_url>|<user_agent>"
return: key=value block: target, robots_url, user_agent, allowed,
        matched_rule, crawl_delay, source
```

Non-2xx robots.txt → `source=missing` (default allow, per RFC 9309 §2.3).

### `tool.web.blocklist_summary`

Returns the sorted blocked-hosts list. Pure read; no network, no DNS.

## Document capabilities

### `tool.parse_document`

Tiered pipeline: LlamaParse → Jina → Firecrawl → local (lopdf for PDF;
plain-text passthrough for text/markdown/code).

```json
// Input JSON
{"kind": "text"|"markdown"|"code"|"pdf"|"url", "payload": "<base64 or url>", "source": "optional-label"}

// Output JSON
{"text": "...", "chunks_created": 0, "tier_used": "llama_parse"|"jina_reader"|"firecrawl"|"local", "source": "..."}
```

Cloud errors or unconfigured env vars silently fall through to the next
tier. `prefer_cloud = false` skips all cloud tiers. Plain
text/markdown/code always uses the local tier regardless.

### `tool.web_read`

URL-only pipeline: Jina → Firecrawl → local fetch+extract.
No LlamaParse. Same `cloud_timeout_secs` knob with a different default (30 s).

### `tool.pdf`

Pure-Rust lopdf backend. No OCR; no encrypted PDFs; `/Info` dictionary
only for metadata.

```
arg:    "<mode>|<base64_pdf>"
modes:  text | pages | meta | all
```

`all` output: `pages=N\nmeta:<k>=<v>\n...\ntext=<body>` (capped at `max_output_chars`).

## Filesystem capabilities

All filesystem capabilities require `[tool.fs]` to be configured.
Every path is jail-checked: absolute paths, `..` components, symlinks,
and paths that escape the jail root are all hard-rejected with
`POLICY_DENIED`.

| Capability | Arg | Returns |
|---|---|---|
| `tool.read_file` | `<rel_path>` or `<rel_path>\|<max_bytes>` | File body (UTF-8; non-UTF-8 is error) |
| `tool.write_file` | `<rel_path>\|overwrite\|<content>` or `<rel_path>\|create_new\|<content>` | `ok bytes=N path=<rel>\n` |
| `tool.search_files` | `<mode>\|<pattern>\|<max_results>` (modes: `name`, `content`, `glob`) | One match per line; content mode: `path:line:text` |
| `tool.patch` | `<rel_path>\|unified_diff\|<diff>` | `ok bytes=N\n` |
| `tool.patch_preview` | `<rel_path>\|<unified_diff>` | Patched body (no write) |
| `tool.append_file` | `<rel_path>\|<bytes>` | `ok appended=N new_size=M\n` |
| `tool.list_dir` | `<rel_path>` | Tab-delimited `kind\tname\tsize\tmtime` rows (`kind` ∈ `dir/file/symlink/other`) |
| `tool.binary_sniff` | `<rel_path>` | Key=value: `path`, `size`, `sniff_bytes`, `is_binary`, `detected_class` (`utf8/ascii/binary/empty`), `null_byte_count`, `first_bytes_hex` |
| `tool.fuzzy_replace` | `<rel_path>\|<search>\|<replace>` | `ok bytes=N path=<rel>\n` |
| `tool.fs.tree` | `<rel_path>` or `<rel_path>\|<max_depth>` (default 5) | Tab-delimited `depth\tkind\trel_path\tsize` rows |
| `tool.fs.stat` | `<rel_path>` | Key=value: `path`, `kind`, `size`, `mtime`, `is_symlink`, `exists` |
| `tool.fs.audit_recent` | empty, `<N>`, or JSON `{"max":N,"op":"write\|append\|patch\|fuzzy_replace"}` | Tab-delimited audit rows, newest first |

Notes:
- Writes use an atomic tempfile-then-rename pattern. Tempfile name:
  `.relix-tool-write-<pid>-<nanos>.tmp` in the same directory.
- `tool.append_file` refuses to create new files; the file must exist.
- `tool.fuzzy_replace` refuses on zero matches or more than one match
  (per-line whitespace normalization; internal whitespace collapsed).
- The audit ring holds the 256 most recent write/append/patch/fuzzy_replace
  operations. The JSON form of the arg lets you filter by operation type.
- Walk (`tool.fs.tree`, `tool.list_dir`) is breadth-first, never follows
  symlinks, hard cap 50 000 entries.

## Terminal capabilities

Terminal capabilities require `[tool.terminal]` to be configured.
This is the highest-blast-radius family; opt in deliberately.

| Capability | Risk | Notes |
|---|---|---|
| `tool.terminal.run` | High | Sync; waits for completion; returns JSON `RunResponse` |
| `tool.terminal.spawn` | High | Fire-and-forget; returns `{session_id,pid,...}` immediately |
| `tool.terminal.sessions` | Safe | Snapshot of live runs |
| `tool.terminal.tail` | Safe | Polling cursor: JSON `{session_id,stream,offset}` |
| `tool.terminal.cancel` | Low | Triggers `Arc<Notify>`; idempotent |
| `tool.terminal.audit_recent` | Safe | Bounded ring snapshot |
| `tool.terminal.shell.open` | High | Persistent shell from `allowed_shells` |
| `tool.terminal.shell.input` | High | JSON `{session_id,bytes?,bytes_base64?}` |
| `tool.terminal.shell.control` | High | JSON `{session_id,control}` |
| `tool.terminal.shell.close` | Low | Drops stdin writer (EOF); does not kill |

**RunRequest JSON:** `{command: String, args?: Vec<String>, timeout_secs?: u64}`

**RunResponse JSON:** `{exit_code?, stdout, stderr, duration_ms, timed_out, cancelled, truncated_stdout, truncated_stderr, command, timeout_secs}`

No shell interpolation — commands run via `tokio::process::Command` with a
separate args vector. `kill_on_drop(true)` on every spawned child.

Output cap: 1 MiB per stream (stdout and stderr each). Overflow: drainer
stops reading → OS pipe fills → child blocks on write. Flagged
`truncated_stdout`/`truncated_stderr`.

### Terminal credential scrubber

When `inherit_env = true`, the scrubber clears env then re-populates
from the controller's env minus sensitive variables. Always passes `PATH`
(plus `PATHEXT` and `SYSTEMROOT` on Windows).

**`SENSITIVE_ENV_VARS`** (exact match, case-sensitive):
`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`,
`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`,
`GEMINI_API_KEY`, `XAI_API_KEY`, `DATABASE_URL`, `RELIX_BRIDGE_TOKEN`.

**`SENSITIVE_ENV_PATTERNS`** (case-folded suffix match):
`_secret`, `_token`, `_password`, `_key`.

Variables listed in `env_allowlist` are exempted from this scrub.

### `tool.terminal.shell.control` byte mapping

| Name | Value | Notes |
|---|---|---|
| `etx` | 0x03 | Without PTY, this is a plain stdin byte — not SIGINT |
| `eot` | 0x04 | |
| `tab` | 0x09 | |
| `cr` | 0x0D | |
| `lf` | 0x0A | |
| `enter` | CRLF (Windows) / LF (Unix) | |
| `esc` | 0x1B | |
| `backspace` | 0x7F | |
| `bs` | 0x08 | |
| `sub` | 0x1A | |
| `nak` | 0x15 | |

Without PTY, control bytes are plain stdin bytes; no signal delivery.
PTY mode (`pty = true` + `--features terminal-pty`) is required to
deliver signals such as SIGINT via `etx`.

## Browser capabilities

See [`browser-tool.md`](browser-tool.md) for the full browser subsystem
guide. Summary of wire shapes:

| Capability | Arg | Returns |
|---|---|---|
| `tool.browser.open_session` | empty | `<session_id>\n` (16 hex chars) |
| `tool.browser.close_session` | `<session_id>` | `"closed\n"` |
| `tool.browser.navigate` | `<session_id>\|<url>` | `"navigated\n"` |
| `tool.browser.get_text` | `<session_id>` | Text body |
| `tool.browser.screenshot` | `<session_id>` | Raw PNG bytes |
| `tool.browser.list_sessions` | empty | Tab-delimited rows + `count=N` |
| `tool.browser.click` | `<session_id>\|<css_selector>` | `"clicked\n"` |
| `tool.browser.type_text` | `<session_id>\|<selector>\|<text>` | `"typed\n"` |
| `tool.browser.wait_for_selector` | `<session_id>\|<selector>\|<timeout_ms>` | `"found\n"` |
| `tool.browser.capture_read` | `<filename.png>` | Raw PNG bytes |

`javascript:` and `data:` URLs are refused by `handle_navigate`
regardless of backend. `tool.browser.capture_read` requires
`screenshot_on_failure_dir` to be configured; the filename must end with
`.png` and must not contain `/`, `\`, `..`, `\0`, or `:`.

`tool.browser.type_text` logs character count only — not the text
content — so credentials typed into forms are not captured in logs.

## MCP capabilities

See [`mcp-tool.md`](mcp-tool.md) for the full MCP guide.

```
tool.mcp.list_servers              → server registry rows
tool.mcp.list_tools|<server_id>    → tool list rows
tool.mcp.invoke|<server_id>|<tool_name>|<args_json>  → JSON result bytes
```

`stdio` transport: live subprocess spawn on first invoke, MCP
`initialize` handshake (`protocol version 2024-11-05`), then
`tools/call`. `http` transport: returns `RuntimeNotConnected` until the
HTTP client ships. Boot-time HTTP discovery runs as a non-blocking
`tokio::spawn` and does not delay tool node startup.

## Utility capabilities

### `tool.screen`

Captures the host's live display as a PNG. Always registered; disabled
by default (`enabled = false`). Operator must opt in explicitly.

```json
// Input JSON
{"region": {"x": 0, "y": 0, "width": 1920, "height": 1080}}   // region optional

// Output JSON
{"image_base64": "...", "width": 1920, "height": 1080, "format": "png", "backend_used": "..."}
```

Backends: Linux: `scrot` → `import` (fallback). macOS:
`/usr/sbin/screencapture`. Windows: PowerShell + `System.Windows.Forms`.

**Security note:** pixel data comes from the host display. Downstream
LLM consumers should wrap via `UntrustedText` / `ai.perception_extract`.

### `tool.text.chunk`

Pure-CPU text splitter for embedding / retrieval prep. Sizes in
characters (not bytes). Always registered.

```json
// Input JSON
{"text": "...", "chunk_size": 512, "chunk_overlap": 0}   // chunk_overlap optional

// Output JSON
{"chunk_count": 4, "chunks": [{"index": 0, "char_start": 0, "char_end": 512, "text": "..."}]}
```

Splitting priority: paragraph (`\n\n`) → sentence (`. `/`! `/`? `/`.\n`)
→ word whitespace → UTF-8 char boundary. Lookback: at most
`chunk_size / 4`. `chunk_overlap` must be less than `chunk_size`.

### `tool.ask_human`

Posts a question to the configured operator channel and awaits the
reply. Always registered; unwired handle returns `{"timeout": true}`.
Default timeout: 300 seconds.

```json
// Input JSON
{"question": "...", "context": "optional", "timeout_secs": 300}

// Output JSON — one of:
{"answer": "<operator reply>"}
{"timeout": true}
```

### `memory.session_search`

Forwarded from the tool node to the memory peer. Always registered;
unwired handle returns `PEER_UNREACHABLE`.

```
arg:    "<subject_id>|<query>|<limit>"
return: JSON array of session entries
```

## Output Guard

Every tool reply passes through `ToolOutputGuard::inspect` before
returning to the caller:

1. **Truncation** — output exceeding 50 000 characters is truncated;
   `"\n...[truncated]"` is appended. Truncated replies still succeed
   (logged at WARN).
2. **Suspicious JSON keys** — checked first (before phrase scan).
   Keys containing `system_prompt`, `instructions`, or `ignore_previous`
   (case-insensitive, substring) in a JSON payload are flagged.
3. **Injection phrases** — same phrase set as the AI input guardrail.

`injection_detected = true` maps to `HandlerFailed`; `truncated = true`
passes through with a WARN log.

## Lifecycle of one fetch

What happens when a SOL flow calls
`remote_call("tool", "tool.web_fetch", "https://example.com/")`:

### On the calling side (bridge / flow runner)

1. SOL VM reaches the `RemoteCall` opcode.
2. Dispatcher resolves the peer alias `"tool"` via the bridge's `MeshClient`.
3. Writes `RemoteCallIssued` to the per-flow event log (log-before-act).
4. Sends the CBOR-encoded `RequestEnvelope` over `/relix/rpc/1`.

### On the tool node (responder)

1. Decode → deadline → identity → capability lookup → policy → audit-on-reject.
2. Handler parses the arg into URL + optional max-bytes.
3. SSRF guard (`security::resolve_safe_url`): operator blocklist → scheme
   allowlist → literal-IP range check → hostname denylist → DNS resolution
   (all IPs checked) → `url_allowlist` enforcement.
4. Pool lookup (`PinnedClientPool`) keyed on `(hostname, sorted_validated_addrs)`.
5. Send request; URL keeps hostname so `Host` header and TLS SNI stay correct.
6. Per-hop redirect re-validation via `resolve_safe_url_blocking`.
7. Content-type filter (must be text/json/xml or empty).
8. Streamed bounded read; abort if body exceeds cap.
9. UTF-8 decode.
10. Output Guard inspection.
11. Audit record — status, latency, etc.

## How to invoke capabilities

From the bridge:

```bash
# Native endpoint
curl -X POST http://127.0.0.1:19791/chat_with_tool \
  -H 'content-type: application/json' \
  -d '{"session_id":"demo","message":"summarize","url":"https://example.com/"}'
```

From a SOL flow:

```sol
// Fetch with optional per-call byte cap
let body: str = remote_call("tool", "tool.web_fetch", "https://example.com/|16384");

// Run a terminal command
let result: str = remote_call("tool", "tool.terminal.run",
    "{\"command\":\"git\",\"args\":[\"status\"]}");

// Read a file
let content: str = remote_call("tool", "tool.read_file", "src/main.rs");

// Ask the operator
let reply: str = remote_call("tool", "tool.ask_human",
    "{\"question\":\"Which environment should I target?\"}");
```

## Observability

Every pool cache miss (web capabilities) emits a structured INFO line:

```
INFO relix_runtime::nodes::tool: tool.web_fetch: pool miss; built
     new pinned client hostname=example.com pinned_addrs=[...]
     pool_entries=N pool_hits=H pool_misses=M
```

Every redirect SSRF rejection emits a distinct WARN:

```
WARN relix_runtime::nodes::tool: tool.web_fetch: redirect
     ssrf-rejected; refusing follow target_url=http://127.0.0.1/
     origin_url=https://example.com/ hops=1
     reason=ip 127.0.0.1 is in forbidden range 'ipv4 loopback (127/8)'
```

The tool node's audit log records every call (allow + handler outcome)
with `request_id` / `trace_id` correlation. Read it via:

```bash
cargo run -p relix-flow-inspect -- --audit dev-data/local-tool/audit.log
```

## Failure modes

| Symptom | Cause | Error kind |
|---|---|---|
| `policy_denied: scheme 'http' not allowed` | URL is `http://` and `allow_http = false` | `POLICY_DENIED` |
| `policy_denied: ip <ip> is in forbidden range` | Literal IP in forbidden range | `POLICY_DENIED` |
| `policy_denied: hostname '<host>' is denied` | Hostname matched denylist | `POLICY_DENIED` |
| `policy_denied: dns resolution for '<host>' included forbidden ip` | Mixed-result DNS | `POLICY_DENIED` |
| `policy_denied: host blocked by operator blocklist` | Host in `blocked_hosts` | `POLICY_DENIED` |
| `policy_denied: host not in url_allowlist` | Host not matched by any allowlist glob | `POLICY_DENIED` |
| `policy_denied: jail escape / symlink detected` | FS path outside root or symlink found | `POLICY_DENIED` |
| `policy_denied: command not in allowed_commands` | Terminal command not allowlisted | `POLICY_DENIED` |
| `invalid_args: body too large` | Body exceeded cap | `INVALID_ARGS` |
| `invalid_args: content-type not text-like` | Non-text content-type | `INVALID_ARGS` |
| `invalid_args: body not utf-8` | Non-UTF-8 body | `INVALID_ARGS` |
| `invalid_args: screen capture disabled` | `[tool.screen] enabled = false` | `INVALID_ARGS` |
| `responder_internal: http 4xx/5xx` | Non-2xx from origin | `RESPONDER_INTERNAL` |
| `responder_internal: BackendNotConnected` | Browser backend not live | `RESPONDER_INTERNAL` |
| `responder_internal: mcp: bad response` | Malformed MCP response | `RESPONDER_INTERNAL` |
| `transport: redirect ssrf-rejected` | Redirect to forbidden target | `TRANSPORT` |

## Build feature flags

| Flag | Effect |
|---|---|
| `browser-headless-chrome` | Enables `headless_chrome` browser backend |
| `browser-playwright` | Enables `playwright` browser backend |
| `browser-webdriver` | Enables `webdriver` browser backend |
| `terminal-pty` | Enables PTY mode for terminal sessions |

Selecting a backend whose feature is not compiled is a **loud startup error**;
there is no silent fallback.

## See also

- [`tool-node-security.md`](tool-node-security.md) — full SSRF model,
  DNS pin, redirect re-check, pool security invariants.
- [`browser-tool.md`](browser-tool.md) — browser subsystem detail.
- [`mcp-tool.md`](mcp-tool.md) — MCP subsystem detail.
- [`security.md`](security.md) — how the tool node fits into the
  whole-mesh security model.
- [`operator-guide.md`](operator-guide.md) — log paths, troubleshooting.
