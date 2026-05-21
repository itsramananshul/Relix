# Continuation State — Wave 1 Real Tool Backends

This file is the authoritative handoff for the next Claude session.
Treat the **WHEN USER SAYS CONTINUE, START HERE** block at the end
as the resume command.

---

## Repository state at the checkpoint

- **Branch:** `main`
- **HEAD:** `8304d21 test(capability): PH-WEB-POST-RISK-CROSS regression guards on web_post + --risk filter`
- **Status:** clean working tree, branch up to date with `origin/main`
- **Remote:** `origin` → `https://github.com/itsramananshul/Relix.git`
- **Workspace tests:** 1093 passing, 0 failures
  - relix-core: 66
  - relix-policy: 72
  - relix-runtime: 664
  - relix-runtime router_node integration: 6
  - relix-telegram: 23
  - relix-cli: 31 (lib) + 2 (bin)
  - relix-web-bridge: 232
  - bridge invariants: 3
- **Gates:** `cargo fmt --all` clean, `cargo clippy --workspace --all-targets -- -D warnings` clean.

---

## What shipped this session (post-shutdown recovery)

15 milestones since the post-shutdown recovery, all green:

| Commit | Milestone | Concept |
|---|---|---|
| ca08428 | docs(internal) | recovered-execution-state + D-008 |
| 8ed8e05 | PH-FS-PARITY3 | tool.search_files `glob` mode (`*`/`**`/`?`) |
| 47b5db9 | PH-FS-PARITY4 | tool.fs.audit_recent + per-jail mutation ring |
| 1feb8ce | PH-TERM-SESSIONS | tool.terminal.sessions live run registry |
| acac673 | PH-MCP-PROTO | MCP JSON-RPC wire layer (no I/O) + D-009 logged |
| 512e727 | PH-TERM-AUDIT | tool.terminal.audit_recent completion ring |
| 9d4381d | PH-TERM-CANCEL | tool.terminal.cancel + manual drain refactor |
| c8d7fcb | PH-TERM-STREAM1 | tool.terminal.tail polling cursor |
| d5587d2 | PH-TERM-SPAWN | tool.terminal.spawn + validate_and_spawn refactor |
| 57c575d | PH-TERM-SHELL | tool.terminal.shell.{open,input,close} |
| cf2ea48 | PH-TERM-CONTROL | tool.terminal.shell.control + D-010 logged |
| e825be6 | PH-FS-FUZZY + PH-FS-TREE + PH-FS-STAT | filesystem Hermes parity |
| 4199b9b | PH-WEB-MARKDOWN | tool.web_extract `markdown` mode — HTML → Markdown structural conversion |
| 8632314 | PH-PDF-CHUNK | tool.text.chunk — general text chunker (paragraph > sentence > word > char) |
| 31c160e | PH-MCP-CLI | relix-cli mcp {servers,tools} — libp2p dial-and-call for MCP registry inspection |
| f642e52 | PH-TERM-CLI | relix-cli terminal {sessions,audit,cancel} — terminal observability + cooperative cancel |
| a307138 | PH-FS-AUDIT-FILTER | tool.fs.audit_recent JSON arg form with optional op filter |
| a27e4a3 | PH-CAP-RISK stage 1 | RiskLevel enum + field on CapabilityDescriptor + validator + CLI render |
| f570a9b | PH-CAP-RISK stage 2 | sweep every shipped descriptor with explicit risk tier |
| 4de4a53 | PH-CAP-RISK3 | relix-cli capability ls --risk filter (exact + at-or-above) |
| 8ae5ff1 | PH-BRIDGE-MCP | GET /v1/mcp/{servers,tools} bridge HTTP proxy for MCP registry |
| 10bb4d9 | PH-DASH-MCP | dashboard #/mcp page with server table + expandable tools per row |
| 155ca06 | PH-BRIDGE-MCP-INVOKE | POST /v1/mcp/invoke bridge proxy (502 until D-009 unblocks runtime) |
| aa37f66 | PH-WEB-POST | tool.web.post — POST + body + raw cookie header + Set-Cookie response capture |
| 8304d21 | PH-WEB-POST-RISK-CROSS | regression guards: web POST descriptors are Medium-tier + --risk medium+ surfaces tool.web.post |

Plus 6 docs-only commits tallying each milestone into
`docs/internal/recovered-execution-state.md`.

Aggregate code change: ~4000+ LOC across `tool/fs.rs`,
`tool/terminal.rs`, `tool/mcp.rs`, `controller_runtime.rs`,
`docs/capabilities.md`, `docs/internal/hermes-capability-map.md`,
`docs/internal/decisions-pending.md`,
`docs/internal/recovered-execution-state.md`.

Runtime tests: 507 → 615 (+108). Workspace: 887 → 995 (+108).

---

## Wave 1 status by track

### A. Browser backend — BLOCKED on D-008

CW4 (`tool.browser.*`) still ships as honest scaffold:
`NoneBackend` returns `BackendNotConnected` on every non-noop.
D-008 (Playwright sidecar vs headless_chrome vs WebDriver,
recommendation **b: headless_chrome**) is open and gates the live
backend. Pure additive once decided; the existing trait + scaffold
slot in via the existing `build_backend()` path.

### B. MCP runtime — protocol layer shipped, runtime BLOCKED on D-009

`tool.mcp.list_servers / list_tools / invoke` are registry +
discovery only (`RuntimeNotConnected` on invoke). PH-MCP-PROTO
shipped the JSON-RPC wire layer (`mcp::proto` module) with 12
tests so the next session can wire the stdio transport in a
follow-up without bootstrapping protocol code. D-009 asks the
operator to identify a concrete MCP server target before live
wiring proceeds (recommendation: defer).

### C. Filesystem — substantial parity now

Shipped: read / write / append / list_dir / patch / patch_preview /
binary_sniff / audit_recent / search_files (name/content/glob) /
fuzzy_replace / tree / stat.

Gaps vs Hermes:
- `file_state` (per-session undo store) — needs a per-session
  storage layer that doesn't exist.
- batch operations wrapper — small.
- safe `delete` / `mkdir` / `move` / `rename` / `copy` — need
  policy decisions about which to surface; tool node node has no
  delete capability today.

### D. Terminal — substantially complete in the no-PTY model

Shipped:
- `tool.terminal.run` (sync, CW1)
- `tool.terminal.spawn` (background, PH-TERM-SPAWN)
- `tool.terminal.sessions` (live registry, PH-TERM-SESSIONS)
- `tool.terminal.cancel` (cooperative kill, PH-TERM-CANCEL)
- `tool.terminal.tail` (polling stream, PH-TERM-STREAM1)
- `tool.terminal.audit_recent` (completion ring, PH-TERM-AUDIT)
- `tool.terminal.shell.{open,input,close}` (persistent shells,
  PH-TERM-SHELL)
- `tool.terminal.shell.control` (named control chars,
  PH-TERM-CONTROL)

Refactors landed:
- `validate_and_spawn` + `drive_to_completion` shared by run +
  spawn + shell.open (SpawnMode enum).
- `write_to_session_stdin` shared by shell.input + shell.control.
- Manual stdout/stderr drain replaced `wait_with_output`, enabling
  cancel + tail.

Gaps:
- Full PTY backend (D-010 deferred): TUI / isatty-checking
  programs.
- Streaming-with-consumer-drain: tail is read-only; a > 1 MiB
  producer still stalls when the bounded buffer fills.
- Command-boundary tracking inside a shell session (operators
  use their own sentinels today).

### E. Web — markdown extraction shipped

`tool.web_fetch / web_get / web_search / web.robots_check`
already shipped. `tool.web_extract` now also supports
`markdown` mode (PH-WEB-MARKDOWN) — full HTML-to-Markdown
conversion (headings, paragraphs, links, lists, code blocks,
blockquotes, hr, emphasis, images). Hermes gaps remaining:
POST, cookies, URL safety (URLhaus), OSV check, crawl_limited,
explicit OpenGraph extraction (current meta mode handles it
indirectly). All decision-free.

### F. PDF / document — text chunking shipped

`tool.pdf` shipped. `tool.text.chunk` (PH-PDF-CHUNK) is the new
general text chunker — works on any text source (PDF extract,
HTML extract, web_get body, read_file). Paragraph > sentence >
word > char break priority; UTF-8 safe (char-counted).
Hermes gaps remaining: page-limited extraction, document
metadata beyond /Info dict (XMP). Decision-free.

### G. Safety / policy — descriptor surface only

Every shipped capability has sensitivity tags + requires_groups
+ idempotency + cost_class set honestly. The dispatch-side
sanitizer (`sanitize.rs`) covers tool-call repair. No
**`risk_level`** field on `CapabilityDescriptor` yet; adding it
would be a broad change touching every descriptor.

### H. Capability registry — current

Every new capability this session was added to the manifest in
`controller_runtime.rs`. `docs/capabilities.md` was kept in sync.
Dashboard explorer (PH-DASH3) was untouched but reads from the
manifest, so new capabilities appear automatically.

### I. Observability / dashboard — no movement this session

Existing dashboard surfaces (capability explorer, firehose,
provider health) still work. Per-tool-family inspectors (browser
sessions, MCP server explorer, terminal sessions panel, fs audit
panel) remain pending.

### J. Tests + docs — kept current per milestone

Every milestone shipped tests + updated `docs/capabilities.md` +
tallied into `docs/internal/recovered-execution-state.md`. The
Hermes capability map was refreshed (`docs/internal/hermes-capability-map.md`).

---

## Pending decisions (unblock real backends)

From `docs/internal/decisions-pending.md`:

| ID | Topic | Recommendation | Blocks |
|---|---|---|---|
| D-001 | task.memory adoption | (b) defer | memory_tool parity |
| D-002 | MCP trust tiers | (b) single-tier operator-opt-in | None (proceed) |
| D-003 | Chronicle compaction threshold | (c) on-demand only | H2 backfill |
| D-004 | `origin_surface` column | (a) ship — additive | None |
| D-006 | iteration-budget grace-call | (c) — **shipped** | — |
| D-007 | computer_use_tool backend | (c) defer | computer_use_tool |
| D-008 | Browser backend (PW vs HC vs WD) | (b) headless_chrome | **CW4 real backend** |
| D-009 | MCP server target | (c) defer | **MCP stdio runtime** |
| D-010 | PTY backend (`portable-pty`) | (b) defer | TUI / isatty-true |

D-008 and D-009 are the two material blockers. D-010 has a
recommendation; the next session can proceed without operator
intervention if it picks (b).

---

## Capability counts (per `docs/capabilities.md`)

### Filesystem (`tool/fs.rs`)
- tool.read_file
- tool.write_file
- tool.append_file (PH-FS-PARITY1)
- tool.list_dir (CW2)
- tool.search_files (name/content/glob; PH-FS-PARITY3)
- tool.patch
- tool.patch_preview (PH-FS-PARITY1)
- tool.binary_sniff (PH-FS-PARITY2)
- tool.fs.audit_recent (PH-FS-PARITY4)
- tool.fuzzy_replace (PH-FS-FUZZY)
- tool.fs.tree (PH-FS-TREE)
- tool.fs.stat (PH-FS-STAT)

### Terminal (`tool/terminal.rs`)
- tool.terminal.run (CW1)
- tool.terminal.spawn (PH-TERM-SPAWN)
- tool.terminal.cancel (PH-TERM-CANCEL)
- tool.terminal.sessions (PH-TERM-SESSIONS)
- tool.terminal.tail (PH-TERM-STREAM1)
- tool.terminal.audit_recent (PH-TERM-AUDIT)
- tool.terminal.shell.open (PH-TERM-SHELL)
- tool.terminal.shell.input (PH-TERM-SHELL)
- tool.terminal.shell.close (PH-TERM-SHELL)
- tool.terminal.shell.control (PH-TERM-CONTROL)

### Browser (`tool/browser.rs`) — scaffold
- tool.browser.{open_session,close_session,navigate,get_text,
  screenshot,list_sessions} — all `BackendNotConnected` (D-008)

### MCP (`tool/mcp.rs`) — scaffold + protocol layer
- tool.mcp.{list_servers,list_tools,invoke}
- `mcp::proto` module: JsonRpcRequest/Response/Notification +
  ToolsListResult / ToolsCallResult + serialize_request /
  parse_response_line (PH-MCP-PROTO; D-009)

### Web (`tool/web_*.rs`)
- tool.web_fetch / tool.web_get / tool.web_search /
  tool.web_extract / tool.web.robots_check

### PDF
- tool.pdf

---

## Highest-leverage next moves (decision-free)

1. **PH-WEB-MARKDOWN** — `tool.web.extract_markdown`. Convert
   HTML to Markdown via a small hand-rolled pass or
   `pulldown-cmark` (already a transitive dep candidate; verify
   before adding). Decision-free.

2. **PH-WEB-METADATA** — `tool.web.extract_metadata`. OpenGraph
   + Twitter Card + standard `<meta>` extraction. Decision-free.

3. **PH-FS-METADATA-RING** — extend `tool.fs.audit_recent` with
   a `?op=` filter so operators can pull only writes / only
   appends. Trivial.

4. **PH-CAP-RISK** — add `risk_level` field to
   `CapabilityDescriptor` and audit every existing descriptor
   to set it. Broad change but mechanical; touches every tool
   module + the manifest exposure.

5. **PH-PDF-CHUNK** — `tool.pdf.chunk_text`. Splits a PDF's
   extracted text into bounded chunks. Decision-free.

6. **PH-MCP-CLI** — `relix-cli ops mcp` subcommand that mirrors
   `tool.mcp.list_servers / list_tools`. Pure CLI add. Pairs
   well with the existing `ops capabilities` / `ops events` /
   `ops route-test` / `ops providers-health` family.

7. **PH-TERM-CLI** — `relix-cli ops terminal-sessions /
   terminal-audit`. Same shape.

Recommend taking 2-3 in a single session, all decision-free.

---

## Exact commands to resume

```bash
cd D:/DATA/WORK/OpenPrem/Apps/Relix

# Confirm clean state
git status
git log --oneline -10

# Re-run full gates to confirm nothing rotted while paused
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
# Expected: 995 tests passing, 0 failures.

# Read the recovery state to understand what shipped this session
cat docs/internal/recovered-execution-state.md

# Pick the next milestone. Recommended order if user hasn't
# answered D-008 / D-009:
#   1. PH-WEB-MARKDOWN  (web extraction parity, no new dep)
#   2. PH-PDF-CHUNK     (document chunking parity)
#   3. PH-MCP-CLI       (CLI surface for MCP registry)
#   4. PH-TERM-CLI      (CLI surface for terminal sessions/audit)
#   5. PH-CAP-RISK      (broad: risk_level on descriptors)
```

---

## Honesty contract (preserved across the session)

1. **No fabricated graph edges** — only real producer events.
2. **No fake hard-preemption** — cooperative cancel only.
3. **No fake provider failover** — operator-visible state with
   restart-required messaging.
4. **No fake backend success** — `tool.browser.*` returns
   `BackendNotConnected`; `tool.mcp.invoke` returns
   `RuntimeNotConnected`. Both refuse to fake.
5. **Append-only chronicle** — capabilities don't write
   chronicle directly. Audit rings (PH-FS-PARITY4,
   PH-TERM-AUDIT) are process-local observability surfaces.
6. **Honest limitations documented** — every capability with a
   gap (no PTY, no streaming-with-drain, no command-boundary
   tracking) carries that limitation in its module doc + the
   relevant tracking decision (D-008/9/10).

---

## Architectural warnings / things to watch

1. **`tool.terminal.shell.*` uses pipe stdin, not PTY.** Programs
   that check `isatty()` see false. Programs that rely on TTY-
   driven signal translation (interactive bash converting `0x03`
   into SIGINT for the foreground job) do not see the signals.
   D-010 logged; recommendation is to defer until a concrete
   isatty-blocked workflow appears.

2. **`tool.terminal.tail` is read-only.** It does NOT advance
   the drainer's write head, so a > 1 MiB producer still stalls
   when the bounded buffer fills. A future ring-with-consumer-
   cursor would relax this.

3. **`tool.fuzzy_replace` refuses on multi-match.** Operators
   who need to disambiguate must rephrase the search block
   with more surrounding context. This is intentional — no
   automatic disambiguation.

4. **`SpawnedRun.stdin` is only Some in shell mode.** The
   `validate_and_spawn` function branches on `SpawnMode`
   (Run / Shell). When adding new spawn modes (e.g., Pty),
   extend the enum and the stdio configuration branch
   alongside.

5. **`backend.allowed_shells` is empty by default.** Operators
   must explicitly list shells in `[tool.terminal] allowed_shells`
   for `shell.open` to succeed. Backend construction now
   requires at least one of `allowed_commands` / `allowed_shells`
   to be non-empty (was: only `allowed_commands` required).

---

## WHEN USER SAYS CONTINUE, START HERE

```
1. cd D:/DATA/WORK/OpenPrem/Apps/Relix
2. git pull --ff-only origin main
3. git status   (confirm clean)
4. cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
5. cargo test --workspace
   (Expected: 995 tests, 0 failures)
6. Read THIS file (docs/internal/continuation-state.md) for full context.
7. Read docs/internal/recovered-execution-state.md for the recovery snapshot.
8. Read docs/internal/decisions-pending.md — operator decisions D-001..D-010.
9. Read docs/internal/hermes-capability-map.md — Hermes parity status.
10. Read docs/capabilities.md — canonical capability index.
11. Pick the next milestone. If the user has answered D-008 / D-009,
    take the unblocked backend track first. Otherwise pick from the
    "Highest-leverage next moves" list above.
12. Maintain the honesty contract (above).
13. After each milestone:
    - cargo fmt && cargo clippy --workspace --all-targets -- -D warnings
    - cargo test --workspace
    - git add <files> && git commit -m "..." && git push origin main
    - Tally into docs/internal/recovered-execution-state.md.
14. NO Claude attribution in commits. NO co-author trailers.
15. NO hardcoded provider tokens. NO secrets in committed files.
16. If a decision is genuinely needed and you can't act safely:
    - log to docs/internal/decisions-pending.md
    - pivot to another track
    - never block the night on a question the user can answer tomorrow.
```
