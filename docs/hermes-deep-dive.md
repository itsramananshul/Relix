# Hermes Agent — Complete Technical Analysis
# How Every System Works, Line by Line

---

## PART 1: MEMORY SYSTEM

---

### Storage Layout

Two plain Markdown files:
- `~/.hermes/memories/MEMORY.md` — general memories
- `~/.hermes/memories/USER.md` — user profile facts

Entries are separated by the delimiter `\n§\n` (section sign, not a slash).

**Size limits** (char-based, NOT token-based — intentionally model-independent):
- MEMORY.md: 2,200 characters max
- USER.md: 1,375 characters max

---

### The `MemoryStore` Class

Two internal states:
1. **Live state** — the current contents of both files, kept in memory
2. **`_system_prompt_snapshot`** — a frozen copy taken at session start, never updated mid-session

The frozen snapshot is what gets injected into the system prompt. This is deliberate: keeping the snapshot stable across a session means the LLM's prefix cache KV is never invalidated mid-session. If you updated the system prompt every time a memory was added, you'd bust the cache on every turn.

---

### `add()` — How a Memory is Written

Every memory write goes through this pipeline in order:

1. **Security scan** — `_scan_memory_content(content)` blocks:
   - 8 invisible unicode codepoints: U+200B, U+200C, U+200D, U+FEFF, U+2060, U+2061, U+2062, U+2069 (zero-width, word-joiners, directional overrides)
   - 11 prompt injection regex patterns: "ignore previous instructions", "you are now", role hijack phrases, etc.

2. **File lock** — `fcntl.flock(LOCK_EX)` on Unix, `msvcrt.locking` on Windows. Cross-platform mutual exclusion.

3. **Reload from disk** — re-reads both files under the lock to get the freshest state (another process may have written between the lock request and acquisition)

4. **Duplicate check** — if the exact string already exists in the file, skip write

5. **Budget check** — if adding would exceed the char limit, trim the oldest entry (entries are separated by `\n§\n`, so trimming = remove everything before the first `§` delimiter)

6. **Append** — appends `\n§\n` + new content to the file

7. **Atomic write** via `_write_file()`

---

### `_write_file()` — Atomic Write

```
tempfile.mkstemp(dir=same_directory_as_target)
→ write content
→ f.flush()
→ os.fsync(fd)
→ os.replace(tmp_path, target_path)
```

The tempfile is in the **same directory** as the target. This is critical — `os.replace()` is only atomic when source and destination are on the same filesystem. Co-locating the temp file guarantees that.

---

### System Prompt Injection

Three-tier system prompt architecture:

| Tier | When Built | What's In It |
|------|------------|-------------|
| `stable` | Once at agent init | Tool schemas, core instructions |
| `context` | Once at session start | Session ID, user profile from USER.md |
| `volatile` | Per-turn | Frozen memory snapshot from MEMORY.md, ephemeral reminders |

Memory is injected in the `volatile` tier — but from the **frozen snapshot**, not from live state. The date injected into the prompt is **date-only** (no hour/minute) specifically to keep the prefix cache stable. If you include the time, the prefix changes every minute and you never get a cache hit.

---

### SQLite Session DB

Location: `~/.hermes/state.db`  
Schema version: `SCHEMA_VERSION = 11`  
WAL mode enabled.

**Tables**:
- `sessions` — 30+ columns: session_id, model, started_at, ended_at, parent_session_id, child compression chain fields, etc.
- `messages` — 20+ columns: message_id, session_id, role, content, tool_name, tool_calls, created_at, etc.
- `state_meta` — key/value store for schema metadata
- `schema_version` — single-row table

**Dual FTS5 index**:
- `messages_fts` — uses `unicode61` tokenizer (handles English and most scripts)
- `messages_fts_trigram` — uses `trigram` tokenizer (handles CJK, partial-word matches, typos)

**Indexed content** for both:
```sql
COALESCE(content,'') || ' ' || COALESCE(tool_name,'') || ' ' || COALESCE(tool_calls,'')
```
Tool names and tool call arguments are searchable, not just message text.

**Three-way CJK routing** in search:
1. Try standard FTS5 (unicode61) — fast
2. If zero results, try trigram FTS5
3. If still zero results, fall back to LIKE with `%query%`

**Concurrency**: `BEGIN IMMEDIATE` + 15 retries with 20–150ms random jitter per retry. Handles multiple processes writing to the same DB without deadlocking.

**Multimodal content**: base64 images stored as `\x00json:<json_payload>` — the null byte prefix is a sentinel that distinguishes binary-embedded JSON from plain text.

**Session compression chain**: `parent_session_id` links compressed sessions to their parent. Discriminated by `child.started_at >= parent.ended_at`. Walking the chain gives you the full conversation history across compressions.

**Schema reconciliation**: `_reconcile_columns()` function compares the current DB schema against the expected schema and runs `ALTER TABLE ADD COLUMN` for any missing columns. Zero-downtime schema migration — never drops columns, only adds them.

**Hidden sources**: tool result messages (role = `"tool"`) are excluded from FTS search and session browse results. The agent can't accidentally search its own tool output history and confuse it with user messages.

---

## PART 2: SKILL SYSTEM

---

### File Layout

```
~/.hermes/skills/
    <name>/
        SKILL.md
        references/   ← .md files only
        templates/    ← .md, .py, .yaml, .yml, .json, .tex, .sh
        scripts/      ← .py, .sh, .bash, .js, .ts, .rb
        assets/       ← any file type
    <category>/
        <name>/
            SKILL.md
            ...
    .hub/
        lock.json       ← installed skills manifest
        quarantine/     ← failed installs staging
        audit.log
        taps.json       ← configured tap sources
        index-cache/    ← per-source index TTL cache (1 hour)
    .archive/           ← archived skills
    .usage.json         ← telemetry sidecar (NOT inside skill dirs)
    .bundled_manifest   ← v2: "name:hash" per line
```

---

### SKILL.md Format

```yaml
---
name: my-skill
description: What this skill does (max 1024 chars)
version: "1.0.0"
license: MIT
platforms: [darwin, linux, windows]
prerequisites:
  env_vars: [API_KEY]
required_environment_variables:
  - name: API_KEY
    description: Your API key
setup:
  collect_secrets:
    - name: SECRET_TOKEN
      prompt: "Enter your token"
required_credential_files:
  - path: ~/.my-creds
    description: Credentials file
metadata:
  hermes:
    tags: [tag1, tag2]
    related_skills: [other-skill]
---

Body text with the actual skill instructions (non-empty after stripping whitespace).
```

**Required**: `name`, `description`, non-empty body.  
**Optional**: everything else.

---

### Skill CRUD — Exact Pipeline

#### CREATE

Constants:
```
MAX_NAME_LENGTH = 64
MAX_DESCRIPTION_LENGTH = 1024
MAX_SKILL_CONTENT_CHARS = 100,000   (~36K tokens at 2.75 chars/token)
MAX_SKILL_FILE_BYTES = 1,048,576    (1 MiB)
VALID_NAME_RE = ^[a-z0-9][a-z0-9._-]*
ALLOWED_SUBDIRS = {"references", "templates", "scripts", "assets"}
```

Steps:
1. Name regex check — rejects uppercase, spaces, leading hyphens/dots
2. Category regex check — same regex, additionally blocks `/` and `\` (directory traversal prevention)
3. Frontmatter validation — must start with `---`, find closing `---`, valid YAML, has `name` + `description`, non-empty body
4. Size check — `len(content) > 100_000` → error
5. Collision check — `rglob("SKILL.md")` across all skill roots, match by directory name. If found → error "already exists"
6. `mkdir(parents=True, exist_ok=True)` at `skills/<category?>/<name>/`
7. Atomic write: `tempfile.mkstemp(dir=skill_dir)` → write → `os.replace(tmp, SKILL.md)`
8. Security scan (only if `guard.agent_created: true` in config.yaml — off by default)
9. If inside background review ContextVar: `mark_agent_created(name)` — sets `created_by = "agent"` in `.usage.json`
10. `clear_skills_system_prompt_cache(clear_snapshot=True)` — forces rebuild of system prompt on next turn so the new skill appears

After every write action, the system prompt cache is invalidated. Always. The agent sees the new skill on the very next turn.

#### EDIT (full replace)

1. Validate frontmatter (full)
2. Size check
3. `_find_skill(name)` — must find exactly one match
4. Read original into memory (in-memory backup, not to disk)
5. Atomic write new content
6. Security scan — if blocked: write backup content back, return error
7. `bump_patch(name)` in `.usage.json`

#### PATCH (fuzzy find-and-replace)

1. `_find_skill(name)` → skill_dir
2. Target file = `file_path` arg or `SKILL.md` if omitted
3. Path traversal prevention: target must resolve to a path inside skill_dir
4. `fuzzy_find_and_replace(target, old_string, new_string, replace_all)` — tolerant of whitespace/indentation differences
5. If old_string not found at all (even fuzzy): error describing the mismatch — no silent no-op
6. Size check on new content
7. If patching SKILL.md: validate frontmatter — rollback on invalid
8. Atomic write + security scan + `bump_patch()`

#### DELETE

1. `_find_skill(name)` → skill_dir
2. `_pinned_guard(name)` — reads `.usage.json`, if `pinned == True` → refuse "Skill is pinned and cannot be deleted"
3. If `absorbed_into` arg provided: validate that target skill exists (informational — no merge logic)
4. `shutil.rmtree(skill_dir)` — deletes entire directory tree
5. `_clean_empty_category_dirs()` — removes empty parent category dirs
6. `forget(name)` — removes usage record from `.usage.json`
7. NO security scan on delete

#### WRITE_FILE / REMOVE_FILE

- First path component MUST be in `ALLOWED_SUBDIRS` — anything else rejected
- Path traversal prevention: full resolution must remain under skill_dir
- Same atomic write + security scan + `bump_patch()` on write
- On `remove_file`: `target.unlink()` then `_clean_empty_category_dirs()`

---

### Skill Discovery — `_find_skill(name)`

Searches in order:
1. Local `~/.hermes/skills/` via `rglob("SKILL.md")`
2. External skill dirs from config

Match condition: `skill_md.parent.name == name` (directory name, NOT frontmatter name field).

If multiple matches (same name in different categories) → error listing all candidates. Caller must use `category/name` qualified form.

---

### Progressive Disclosure — How the Agent Finds Skills

**Tier 1: `skills_list(category=None)`**
- Reads only the first 4,000 chars of each SKILL.md (fast, no token explosion)
- Parses frontmatter only — name, description, category
- Checks platform compatibility
- De-duplicates by name: local skills win over external skills with the same name
- Returns: list of `{name, description, category}` + categories list + count
- Hint appended: "Use skill_view(name='...') to see full content"

**Tier 2: `skill_view(name, file_path=None)`**
- Four lookup strategies (tried in order):
  1. Direct path if `name` contains `/`
  2. `skills/category/name/SKILL.md` then `skills/name/SKILL.md`
  3. `rglob("SKILL.md")` matching `parent.name == name`
  4. Legacy flat `skills/name.md` format
- Collision detection: if >1 candidate → error listing all matches
- Platform check: mismatch returns `readiness_status: "platform_mismatch"` but still serves the skill
- Security checks (log-only on serve, NEVER blocks serving): path outside trusted dir, injection pattern match
- Env var handling: merges three sources (required_environment_variables + setup.collect_secrets + prerequisites.env_vars), registers missing ones via secret capture callback
- Builds `linked_files` dict for references/, templates/, scripts/, assets/ subdirectories
- **Calling `skill_view()` counts as BOTH a view AND a use event** — bumps both counters

**Plugin skills**: `name` format `plugin:skill` → served from plugin's directory, checks plugin is not disabled, builds bundle context banner with sibling skills list.

---

### Security Scanner — `skills_guard.py` (933 lines)

**60+ threat patterns across 10 categories**:

**Exfiltration**:
- `curl.*\$(?:HOME|HERMES_HOME|.*API_KEY|.*TOKEN|.*SECRET)` — env var exfil via curl
- `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.kube`, `~/.docker`, `~/.hermes/.env` — credential file paths
- `printenv | os\.environ | os\.getenv` — environment dumping
- DNS exfiltration patterns, `/tmp` staging, markdown image/link exfil
- "include the contents of" + sensitive path — context window exfil via instruction

**Injection**:
- "ignore previous instructions", "you are now", "pretend you are"
- "disregard rules", "leak system prompt", HTML hidden divs, HTML comments
- DAN mode, developer mode, "hypothetical bypass", "educational pretext"
- "remove filters", "fake update", "fake policy"

**Destructive**:
- `rm -rf /`, `rm $HOME`, `chmod 777`, `> /etc/`, `mkfs`, `dd if=`, `shutil.rmtree`, `truncate -s 0`

**Persistence**:
- `crontab`, `.bashrc`/`.zshrc`/`.profile`/`/etc/*`, `authorized_keys`
- `systemd`, `/etc/init.d`, `launchctl`, `/etc/sudoers`
- `git config --global`, writing to `AGENTS.md`/`CLAUDE.md`/`.cursorrules`/`.hermes/config.yaml`

**Network**:
- `nc -lp` / `ncat` / `socat`, `ngrok` / `localtunnel`
- Hardcoded IP:port, `0.0.0.0`, `bash /dev/tcp`
- `webhook.site` / `requestbin` / `pipedream`, `pastebin`

**Obfuscation**:
- `base64 decode|pipe`, hex encoding, `eval`/`exec` over strings
- `getattr builtins`, `__import__('os')`, `codecs.decode`
- `chr()` building, unicode escape chains, `String.fromCharCode`, `atob`/`btoa`

**Execution**: `subprocess`, `os.system`, `os.popen`, Node `child_process`, Java `Runtime.exec`, backtick subshell

**Path traversal**: `../../..`, `/etc/passwd`, `/etc/shadow`, `/proc/self`, `/dev/shm`

**Mining**: `xmrig`, `stratum+tcp`, `monero`, `coinhive`, `hashrate`

**Supply chain**: `curl|bash`, `wget|bash`, `curl|python`, unpinned pip/npm, `uv run`, remote fetch, `git clone`, `docker pull`

**Privilege escalation**: `allowed-tools` field, `sudo`, setuid/setgid, `NOPASSWD`, `chmod +s`

**Credentials**:
- Hardcoded `api_key=`/`token=`/`secret=` regex
- `-----BEGIN PRIVATE KEY-----`
- GitHub PATs: `ghp_`, `github_pat_`
- OpenAI keys: `sk-`
- Anthropic keys: `sk-ant-`
- AWS keys: `AKIA...`

**Structural limits**:
- Max 50 files per skill dir
- Max 1 MiB total size
- Max 256 KiB per single file
- Symlink escape detection: resolves symlink target, checks it's not outside the skill dir

**Verdict logic**:
- Any critical finding → `"dangerous"`
- Any high finding → `"caution"`
- Else → `"safe"`

**Trust matrix**:
```
builtin:       (allow on safe, allow on caution, allow on dangerous)
trusted:       (allow on safe, allow on caution, BLOCK on dangerous)
community:     (allow on safe, BLOCK on caution, BLOCK on dangerous)
agent-created: (allow on safe, allow on caution, ASK on dangerous)
```
Agent-created skills only ask on dangerous (not block) because the agent created it for a reason and the user likely knows. Community skills get blocked even on caution because the source is unknown.

`should_allow_install()` returns:
- `(True, None)` on allow
- `(False, reason_string)` on block (with `force=True`: returns True anyway)
- `(None, reason_string)` on ask — `None` is NOT a bool, it signals "ask the user"

---

### Skills Hub — `skills_hub.py`

**Six source types**:

**1. GitHubSource** — default taps:
```
openai/skills
anthropics/skills
huggingface/skills
VoltAgent/awesome-agent-skills
garrytan/gstack
MiniMax-AI/cli
```

Auth priority: `GITHUB_TOKEN`/`GH_TOKEN` env var → `gh auth token` CLI → GitHub App JWT (RS256, 10-min JWT, 58-min installation token cache) → anonymous (60 req/hr)

Fetch strategy: Git Trees API first (single HTTP call for entire repo tree), falls back to Contents API (one call per file — slow for large repos).

**2. WellKnownSkillSource** — reads `/.well-known/skills/index.json` from domains

**3. UrlSource** — bare HTTP(S) URLs ending in `.md`

**4. SkillsShSource** — skills.sh marketplace (frontend only, GitHub for actual file storage)

**5. ClawHubSource** — `https://clawhub.ai/api/v1`
Hardcoded community trust. Comment in code: "ClawHavoc incident showed their vetting is insufficient (341 malicious skills found Feb 2026)"

**6. Custom taps** via `taps.json`

**HTTP safety** on every request: `is_safe_url()` + `check_website_access()` + max 5 redirect hops each validated. Prevents SSRF during skill fetches.

**Bundle format**: `{name: str, files: Dict[str, str|bytes], source: str, ...}`
`files` maps relative paths (`"SKILL.md"`, `"templates/example.py"`) to content.
Must contain `"SKILL.md"` or install is rejected.

**Path normalization** prevents traversal in bundle file paths: rejects absolute paths, Windows drive letters, any `..` components.

---

### Bundled Skill Sync — Content-Addressed Algorithm

Uses MD5 of the entire skill directory (sorted file paths + content) as the hash.

**5-case logic for each bundled skill on startup**:

1. Not in manifest, dest exists, hashes match → record hash, skip (already in sync)
2. Not in manifest, dest doesn't exist → `shutil.copytree()`, record hash, `copied += 1`
3. In manifest, dest exists, user modified it (`user_hash != origin_hash`) → skip, preserve user changes
4. In manifest, dest exists, bundled was updated (`bundled_hash != origin_hash`, user hasn't touched it) → backup with timestamp → overwrite → update manifest hash, `updated += 1`
5. In manifest, dest deleted → respect deletion, don't re-copy, `skipped += 1`

Stale manifest entries (skill no longer bundled) → remove from manifest, `cleaned += 1`.

---

### Usage Telemetry — Write Guard

Sidecar at `~/.hermes/skills/.usage.json` (NOT inside skill dirs).

**Before writing any telemetry**:
- If skill is in `.bundled_manifest` → skip write
- If skill is in `.hub/lock.json` → skip write
- Only agent-created skills (`created_by == "agent"`) accumulate telemetry

This prevents the curator system from auto-archiving bundled or hub-installed skills.

**Record structure**:
```json
{
  "created_by": null | "agent" | "user",
  "use_count": 0,
  "view_count": 0,
  "last_used_at": null,
  "last_viewed_at": null,
  "patch_count": 0,
  "last_patched_at": null,
  "created_at": "<ISO>",
  "state": "active" | "stale" | "archived",
  "pinned": false,
  "archived_at": null
}
```

**Archive**: `os.rename(src, dest)` — atomic on same filesystem. Falls back to `shutil.move()` for cross-device.

**Restore limitation**: original category is NOT reconstructed on restore. Skill lands flat at `skills/<name>/` regardless of where it was.

---

### ContextVar Write Scoping — `skill_provenance.py`

```python
_write_origin: contextvars.ContextVar[str] = contextvars.ContextVar(
    "skill_write_origin",
    default="foreground",
)
```

Set at the top of `run_conversation()`, before any tool call. Reset in a `finally` block.

Background review forks have `_memory_write_origin = "background_review"` set before `run_conversation()` is called on the spawned agent.

Why `ContextVar` and not a global: Python `contextvars.ContextVar` is both thread-safe AND async-safe. Each coroutine gets its own context. Parallel agent forks don't contaminate each other's write origin. A global variable would mean fork A's background review status leaks into fork B's skill writes.

Effect: when `skill_manage(create)` runs inside a background review fork, `is_background_review()` returns True → `mark_agent_created(name)` fires → skill gets `created_by = "agent"` → it becomes curator-managed.

---

## PART 3: AGENT LOOP

---

### `context_engine.py` — The Abstract Base

Pluggable architecture. Drop a custom engine in `plugins/context_engine/<name>/` to replace the default.
Selected via `context.engine` in config.yaml. Default: `"compressor"`.

```python
class ContextEngine(ABC):
    protect_first_n: int = 3    # non-system head messages always preserved
    protect_last_n: int = 6     # tail messages always preserved
    threshold_percent: float = 0.75

    @abstractmethod
    def should_compress(self, prompt_tokens: int) -> bool: ...

    @abstractmethod
    def compress(self, messages, current_tokens=None, focus_topic=None) -> List[Dict]: ...
```

`protect_first_n` = count of non-system head messages beyond the system prompt.
System prompt is always implicitly protected — never summarized.

Lifecycle hooks: `on_session_start()`, `update_from_response()` (after each API response), `on_session_end()` (real session boundaries only — not per-turn).

---

### `context_compressor.py` — The Real Engine (1,749 lines)

**Fires at 50% context window** (not the base class default of 75%). This leaves room for the summary + tail to fit comfortably.

#### Constants
```
_MIN_SUMMARY_TOKENS = 2000
_SUMMARY_RATIO = 0.20             target summary = 20% of compressed content
_SUMMARY_TOKENS_CEILING = 12,000
_PRUNED_TOOL_PLACEHOLDER = "[Old tool output cleared to save context space]"
_CHARS_PER_TOKEN = 4
_IMAGE_TOKEN_ESTIMATE = 1600      tokens per image
_SUMMARY_FAILURE_COOLDOWN_SECONDS = 600   (10 minutes)
```

#### Anti-thrashing

`_ineffective_compression_count` tracks consecutive compressions that saved less than 10%.
After 2 consecutive <10% passes → `should_compress()` returns False regardless of token count.
Clears when a compression saves ≥10% OR when the user runs `/new` or `/compress <topic>`.

#### Three-Pass Tool Result Pruning (no LLM, runs first)

**Pass 1: Dedup identical tool results**
- Compute MD5 of tool result content (>200 chars only)
- Walk backward: keep newest copy of each unique content
- Replace older duplicates with `"[Duplicate tool output — same content as a more recent call]"`

**Pass 2: Replace old tool results with 1-line summaries**
- Only processes messages before the tail boundary
- Per-tool summary format:
  - `terminal`: `[terminal] ran 'npm test' → exit 0, 47 lines output`
  - `read_file`: `[read_file] read config.py from line 1 (3,400 chars)`
  - `write_file`: `[write_file] wrote 2,100 chars to src/app.py`
  - `search_files`: `[search_files] found 12 matches for 'TODO' in *.py`
  - `patch`: `[patch] patched app.py — 2 replacements`
  - `web_search`: `[web_search] searched 'query' — 8 results`
  - `browser_*`: `[browser] screenshot captured`
  - `skill_view`: tailored format with skill name
- Multimodal content (base64 screenshots): image payload stripped, replaced with `text_summary` field

**Pass 3: Truncate oversized tool call arguments**
- Only outside protected tail
- Args >500 chars → JSON-aware truncation (200-char head kept, structure preserved)

#### Tail Boundary Algorithm

`_find_tail_cut_by_tokens(messages, compress_start)`:
- Hard minimum: always protect at least 3 tail messages
- Soft ceiling: allow up to 1.5× the token budget (prevents cutting inside one oversized message)
- Walk backward, accumulate `content_chars / 4 + 10` per message
- `_align_boundary_backward()`: pull cut back to avoid splitting tool_call/result groups
- **`_ensure_last_user_message_in_tail()`**: if the last user message would land in the compressed region, pull the cut back to include it — even if this exceeds the budget

The last rule is a bug fix (bug #10896). Without it, the active task could be summarized away and the agent would stall with no active goal.

#### Summarization Input Preparation

Per-message truncation limits:
```
_CONTENT_MAX = 6,000 chars total per message
_CONTENT_HEAD = 4,000 chars kept from start
_CONTENT_TAIL = 1,500 chars kept from end
_TOOL_ARGS_MAX = 1,500 chars
_TOOL_ARGS_HEAD = 1,200 chars
```

All content passes through `redact_sensitive_text()` BEFORE sending to the summarizer. API keys/tokens/passwords are stripped. The summarizer never sees secrets.

#### LLM Summarization Prompt — 12 Sections

```
## Active Task           ← MOST IMPORTANT: copy user's EXACT request verbatim
## Goal
## Constraints & Preferences
## Completed Actions     ← numbered, with tool names, targets, outcomes
## Active State          ← working dir, branch, modified files, test status
## In Progress
## Blocked               ← include EXACT error messages
## Key Decisions
## Resolved Questions
## Pending User Asks
## Relevant Files
## Remaining Work
## Critical Context      ← never include API keys or passwords
```

**Iterative update**: when `_previous_summary` exists (a prior compression happened), the prompt instructs the model to: preserve existing info, ADD new completed actions, MOVE "In Progress" → "Completed Actions" for finished work.

**Focus topic** (from `/compress <topic>` command): instructs model to give ~60-70% of token budget to that topic, compress everything else aggressively.

#### Summarizer Fallback Chain (5 paths)

1. 404/503/"model_not_found"/"does not exist" + separate summary model + not yet fallen back → retry on main model immediately
2. Timeout (408/429/502/504) → same as above
3. JSON decode error → fallback + 30s cooldown
4. Streaming closed prematurely → fallback + retry
5. RuntimeError (no provider available) → 600s cooldown

Output of the summarizer is passed through `redact_sensitive_text()` again. The LLM may echo back secrets it was told to ignore.

#### Compression Assembly — How the Final Message List Is Built

1. **Head messages**: system prompt + first `protect_first_n` non-system messages, copied verbatim
2. **System prompt gets a note appended**: `"[Note: Some earlier conversation turns have been compacted into a handoff summary...]"`
3. **Summary role selection** (avoids consecutive same-role API errors):
   - If last head message is `assistant` or `tool` → summary role = `user`
   - Otherwise → summary role = `assistant`
   - If chosen role collides with first tail message AND flipping collides with last head → merge summary into first tail message instead
4. **User-role summary gets a footer appended**: `"--- END OF CONTEXT SUMMARY — respond to the message below, not the summary above ---"` — prevents weak models from treating the Active Task verbatim quote as a new user instruction
5. **Tail messages**: last `protect_last_n` messages, copied verbatim
6. **Post-assembly cleanup**:
   - `_sanitize_tool_pairs()`: removes orphaned tool results, inserts stub results for orphaned tool calls: `"[Result from earlier conversation — see context summary above]"`
   - `_strip_historical_media()`: removes image payloads from all messages before the newest image-bearing user turn

---

### `conversation_loop.py` — The ReAct Loop (3,980 lines)

#### Pre-Loop Setup (20+ steps, in order)

1. `_install_safe_stdio()` — prevents OSError from broken pipes
2. `agent._ensure_db_session()` — create/open SQLite session
3. `set_runtime_main(provider, model)` — sets global for auxiliary LLM client
4. `set_session_context(agent.session_id)` — scopes log filtering
5. `set_current_write_origin(...)` — ContextVar for skill provenance
6. `agent._restore_primary_runtime()` — reset any mid-session provider fallback
7. `_sanitize_surrogates(user_message)` — removes Unicode surrogate chars that crash codec
8. `effective_task_id = task_id or str(uuid.uuid4())`
9. `agent._current_task_id = effective_task_id` — set BEFORE any tool dispatch
10. Reset 10 retry counters: `_invalid_tool_retries`, `_invalid_json_retries`, `_empty_content_retries`, `_incomplete_scratchpad_retries`, `_codex_incomplete_retries`, `_thinking_prefill_retries`, `_post_tool_empty_retried`, `truncated_tool_call_retries`, `codex_ack_continuations`
11. `agent._tool_guardrails.reset_for_turn()`
12. Pre-turn TCP connection health check (non-Anthropic providers only)
13. Replay compression warning if previous compression had aux model failure
14. NOTE: `_turns_since_memory` and `_iters_since_skill` are NOT reset — they persist across turns to trigger review nudges at proper intervals
15. `agent.iteration_budget = IterationBudget(agent.max_iterations)`
16. Hydrate todo store + nudge counters from conversation_history
17. `agent._user_turn_count += 1`
18. Memory nudge check — if triggered, inject reminder into system ephemeral
19. Append user message to messages
20. **Preflight compression**: up to 3 passes before first API call if context is already large
21. Fire `pre_llm_call` plugin hook → result stored as `_plugin_user_context`, injected at API call time only (never stored in messages list)

#### Main Loop Condition

```python
while (
    api_call_count < agent.max_iterations
    and agent.iteration_budget.remaining > 0
) or agent._budget_grace_call:
```

`_budget_grace_call`: True when budget is exhausted but the loop needs one more API call to generate a final summary. Set by `_handle_max_iterations()`.

#### Per-Iteration Steps (inside loop)

1. `agent._checkpoint_mgr.new_turn()` — save checkpoint for potential rollback
2. Interrupt check: `agent._interrupt_requested` → break
3. Budget check: consume or set `_budget_grace_call = True`
4. Fire `step_callback` with prior tools info
5. Track `_iters_since_skill` for skill nudge
6. Drain `/steer` messages — if user typed guidance during tool execution, append as `"User guidance: ..."`
7. `agent._sanitize_tool_call_arguments(messages)` — repair corrupted JSON in tool call args
8. `agent._repair_message_sequence(messages)` — enforce role alternation (merge/remove consecutive same-role)
9. Build `api_messages`:
   - Strip internal fields (reasoning, finish_reason, _thinking_prefill, Codex fields)
   - Inject memory context into current user message
   - Inject `_plugin_user_context` into user message
10. Build `effective_system = cached_system_prompt + ephemeral_system`
11. Apply Anthropic cache control breakpoints
12. `agent._sanitize_api_messages()` — strip orphaned tool results, add stubs
13. `agent._drop_thinking_only_and_merge_users()` — prevent Anthropic 400 from thinking-only messages
14. Normalize whitespace for prefix matching

#### API Call — Inner Retry Loop

17 distinct exception/error paths:

1. Surrogate chars → `_sanitize_messages_surrogates()`, retry (max 2 passes)
2. ASCII codec error → `_force_ascii_payload = True`, sanitize all fields, retry
3. Image rejection (4xx response body with image phrases) → `_strip_images_from_messages()`, `_vision_supported = False`
4. `classify_api_error(e)` → `FailoverReason` enum for structured routing
5. 429 → credential pool rotation
6. `FailoverReason.image_too_large` → `_try_shrink_image_parts_in_messages()`
7. `FailoverReason.oauth_long_context_beta_forbidden` → `_oauth_1m_beta_disabled = True`, rebuild client
8. Provider-specific 401 refreshes: Codex/xAI OAuth, Nous, Copilot, Anthropic
9. `FailoverReason.thinking_signature` → strip `reasoning_details` from all messages
10. `FailoverReason.llama_cpp_grammar_pattern` → `strip_pattern_and_format(agent.tools)`
11. Nous genuine rate limit → cross-session breaker record (shared file), skip to max_retries
12. `FailoverReason.payload_too_large` (413) → compress, max 3 attempts
13. `FailoverReason.long_context_tier` → reduce to 200K + compress
14. Rate limit eager fallback
15. `FailoverReason.context_overflow` → parse exact limit from error, compress, max 3 attempts
16. Non-retryable 4xx → try fallback, then abort
17. max_retries exhausted → `_try_recover_primary_transport()` once, then fallback, then abort

**Backoff**: `jittered_backoff(retry_count, base_delay=2.0, max_delay=60.0)`. Capped at `Retry-After` header value (max 120s). Sleep loop checks for interrupts every 0.2s.

**`finish_reason = "length"` (output truncated)**:
- Up to 3 continuation retries with `"[System: Continue exactly where you left off...]"` appended
- 4th time → return partial result
- Accumulated text stored in `truncated_response_parts`

#### Compression Threshold — The Critical Detail

```python
if _compressor.last_prompt_tokens > 0:
    _real_tokens = _compressor.last_prompt_tokens
else:
    _real_tokens = estimate_request_tokens_rough(messages, tools=agent.tools)
```

**Only `last_prompt_tokens` is used — never total tokens.**

Reasoning models (QwQ, R1, DeepSeek, GLM) inflate `completion_tokens` with thinking tokens that do NOT consume the context window (they're in the output, not input). Using total tokens would trigger premature compression on every reasoning turn.

When `last_prompt_tokens` is stale (zero after a disconnect), the rough estimate includes `tools=agent.tools`. 50+ tool schemas add 20–30K tokens that a messages-only estimate would miss. This was a separate bug.

#### Tool Call Handling

1. **Fuzzy repair** — `agent._repair_tool_call(name)` tries to fix typos in tool names BEFORE returning error
2. If still invalid → append error tool result, `_invalid_tool_retries += 1`. After 3 → return partial
3. JSON arg validation — empty/whitespace → `"{}"`. Truncated (doesn't end `}` or `]`) → truncation error
4. After 3 JSON retries → inject recovery tool results with explicit error messages
5. Post-call guardrails:
   - `_cap_delegate_task_calls()` — limits how many delegate tasks can be spawned
   - `_deduplicate_tool_calls()` — removes exact duplicate tool calls in the same turn
6. If ALL tool calls in turn are in `_HOUSEKEEPING_TOOLS = {"memory", "todo", "skill_manage", "session_search"}` → set `_mute_post_response = True` (don't stream housekeeping turns to user)
7. Dispatch via `agent._execute_tool_calls()`
8. Guardrail halt check — if halt decision set → break, build halt response
9. Compression check (AFTER execution, NOT before — let the new tool results factor into the decision)
10. `execute_code` budget refund — if the only tool called was `execute_code` (programmatic) → `iteration_budget.refund()`

#### Empty Response Recovery (7 paths, tried in order)

1. **Partial stream recovery** — if content was streaming before disconnect → use `_current_streamed_assistant_text`
2. **Prior housekeeping content** — if `_last_content_with_tools` exists AND `_last_content_tools_all_housekeeping` → use it
3. **Post-tool nudge** — append synthetic `"(empty)"` assistant + user nudge, set `_post_tool_empty_retried = True`, continue
4. **Thinking-only prefill** (`_thinking_prefill_retries < 2`) — structured reasoning exists but no visible text → append thinking as interim, continue
5. **Empty retry** (`_empty_content_retries < 3`) — retry up to 3 times
6. **Fallback provider** (`_fallback_chain`) — switch to fallback provider
7. **Give up** → `final_response = "(empty)"`, tag `_empty_terminal_sentinel = True`

#### Post-Loop

1. **Budget exhaustion** → `agent._handle_max_iterations(messages)` — strip tools, make one more API call asking for a summary of what was accomplished and what remains. If `HERMES_KANBAN_TASK` env var set → calls `kanban_block` tool
2. `completed = final_response is not None and api_call_count < agent.max_iterations`
3. `agent._save_trajectory()` — training data in ShareGPT format if enabled
4. `agent._cleanup_task_resources()` — release VM/browser resources
5. `agent._drop_trailing_empty_response_scaffolding()` — remove sentinel messages
6. `agent._persist_session()` — save to JSON log + SQLite

**Turn-exit diagnostic log** — `_turn_exit_reason` values:
- `"normal"`, `"interrupted_by_user"`, `"budget_exhausted"`, `"guardrail_halt"`, `"partial"`
- If `_last_msg_role == "tool"` AND not interrupted → logged at WARNING ("agent may appear stuck")

**File-mutation verifier footer**: if any `patch`/`write_file` operations failed → appends footer listing the failed operations so the model can't claim it edited files it actually failed to change.

**Plugin hooks fired after loop**:
1. `transform_llm_output(response_text, ...)` — first hook returning non-empty string wins
2. `post_llm_call(session_id, user_message, assistant_response, ...)` — no return

---

### `agent_runtime_helpers.py` — Training Data Generation

`convert_to_trajectory_format(agent, messages, user_query, completed)` produces ShareGPT-format training data:

- System message: includes full tool definitions in `<tools>` XML tags in Pydantic schema format
- Human turns: plain text
- Assistant turns (`gpt` role): tool calls rendered as `<tool_call>{"name": ..., "arguments": ...}</tool_call>`, reasoning wrapped in `<think>...</think>` tags
- Tool results: collected in sequence, returned as single `{"from": "tool"}` with `<tool_response name="...">...</tool_response>` XML per result
- Multimodal tool results: images replaced with `text_summary` field

Purpose: generates fine-tuning data in a format compatible with standard SFT pipelines.

---

## Key Design Decisions — Why Hermes Made Each Choice

| Decision | Reason |
|----------|--------|
| Char limits on memory (not tokens) | Model-independent — same limit works across GPT, Claude, local models |
| Frozen snapshot for system prompt | Prefix cache KV stability — same system prompt = cache hit every turn |
| Date-only timestamp in prompt | Same reason — minute-precision timestamps bust the prefix cache |
| `os.replace()` atomic writes everywhere | Crash-safe — partial writes never corrupt files |
| ContextVar for write scoping | Thread-safe AND async-safe — parallel forks can't contaminate each other |
| Compression at 50% not 75% | Leaves headroom for summary + tail to fit without another immediate compression |
| Last user message always in tail | Prevents active task loss during compression (bug #10896) |
| Prompt tokens only for compression threshold | Thinking model completion tokens don't consume context window space |
| Tool schemas in rough estimate | 50+ schemas add 20–30K tokens a messages-only estimate would miss |
| Fuzzy tool name repair before error | Model typos in tool names shouldn't waste a full iteration |
| Progressive disclosure (list → view) | With 50+ skills, returning full content on list would blow the context window |
| Telemetry excluded for bundled/hub skills | Prevents auto-archive of skills the user didn't create |
| Four GitHub auth tiers | Maximizes rate limits without requiring setup; `gh` CLI covers most dev machines |
| ClawHub hardcoded community trust | 341 malicious skills discovered Feb 2026 — permanent policy decision |
| Anti-thrashing counter | Without it, compression can loop on an oversized single message forever |
