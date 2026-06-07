# Relix Agent Adapters — the universal "plug in any agent" system

> **Status:** Design / idea layer. *What we're building and why*, not *how the code is written*. No code in this document.
>
> **Grounding:** based on a complete, file-by-file read of **Paperclip's adapter system** (the `ServerAdapterModule` registry, the `claude_local` / `codex_local` / `acpx_local` / `hermes_local` / `gemini_local` / `grok_local` / `cursor` / `opencode_local` / `pi_local` adapters, the subscription-credential handling, the cost ledger's `subscription_included` path, and the per-run local-agent JWT), and on the complete read of Hermes (its provider/credential system, ACP adapter, and plugin API). Where this doc says "Paperclip does X," it was verified at the source level in `references/paperclip`.
>
> **Companion docs:**
> - [`relix-company-model.md`](relix-company-model.md) — the company/work-object spine. (An agent record gains an **adapter** choice; see §8 there for where it slots in.)
> - [`relix-hermes-integration.md`](relix-hermes-integration.md) — the **Hermes adapter**, which is the deepest/richest adapter in this system. That doc is now "the flagship adapter, in detail"; *this* doc is the general frame around it.
> - [`hermes-vs-paperclip-vs-relix.md`](hermes-vs-paperclip-vs-relix.md) — the three-way comparison.
>
> **This document answers two requests:**
> 1. *"Make the plug-in system general — plug in ANY AI agent (Hermes, Claude Code, Codex, anything) and it just works with Relix; assign work directly to that agent."*
> 2. *"Take Paperclip's ability to use the installed Claude / Codex CLI **subscription** and put it on our plan."*
>
> Both are the same feature. The short version: **every agent in Relix is backed by an "adapter." An adapter is a small plugin that knows how to run one kind of agent — an installed Hermes, the `claude` CLI on your Max subscription, the `codex` CLI on your ChatGPT subscription, a remote API agent, or anything else. You assign an Issue to an agent; the agent's adapter does the actual running; the result comes back into the Issue. Hermes is just the richest adapter; the CLI-subscription adapters are first-class too.**

---

## 1. The core idea — agents are pluggable, work is uniform

Today "what runs the agent" is implicit. We make it **explicit and swappable**:

- Every agent (employee) in the company has an **adapter** — a named backend that knows how to *execute a Run* for that agent.
- The rest of Relix — the Issue ledger, assignment, the heartbeat loop, approvals, budgets, the dashboard — **doesn't care which adapter an agent uses.** It assigns an Issue, the adapter runs it, results and cost flow back. One uniform contract; many backends behind it.
- Adding support for a new agent product (a new CLI, a new API, a new framework) is **writing one adapter plugin** — no change to the spine. Operators (and eventually third parties) can drop in adapters the way Paperclip loads external adapter packages.

This is exactly Paperclip's architecture, verified at the source: each agent has an `adapterType` string; a registry maps that to a `ServerAdapterModule`; Paperclip ships ~11 built-ins and loads external ones dynamically. We adopt the pattern wholesale, on top of Relix's secure mesh.

> **The two ideas unify here:** "plug in any agent" *is* the adapter system. "Use my installed Claude/Codex subscription" *is* two specific adapters (`claude` and `codex`) that happen to run a local CLI on your existing login. Same machinery.

### 1b. Two pillars — keep these separate (important)

There are **two different things**, and they must not be conflated:

- **Pillar 1 — Relix's OWN native powers.** Relix builds the best Hermes capabilities *into itself*, as native Relix features: the **self-improving learning loop**, **`execute_code`** (one script runs the N steps instead of N slow, expensive turns), the **sleep-and-wake execution workspaces**, the **rich plugin/extension system**, the **memory discipline**. These are *Relix's own code and Relix's own features* — designed from the Hermes blueprint (we read every line, so we know exactly how), but **not dependent on Hermes**. Relix's native agents are world-class on their own. *(Full mapping in [`relix-hermes-integration.md`](relix-hermes-integration.md) §5 — and the disposition for the top capabilities is "**build native in Relix**," with embedded Hermes available only as an optional fast bootstrap.)*
- **Pillar 2 — Plug in ANY outside agent (this doc).** *Additionally*, Relix can plug in any external agent — OpenClaw, Hermes, **Paperclip itself**, Claude Code, Codex, anything — as a swappable adapter, and assign Issues straight to it. This is a bonus capability, **not** how Relix gets its powers.

> **Why the split matters:** Relix does **not** borrow its excellence from an embedded Hermes. Relix *is* excellent natively (Pillar 1) — no outside dependency, no licensing risk. Plugging in other agents (Pillar 2) is then pure upside: use Claude/Codex on your subscription, drop in OpenClaw, even run Paperclip as a worker — because you *can*, not because you *must*. This document is Pillar 2; §5 of the Hermes doc catalogs Pillar 1.

---

## 2. What an adapter is (the uniform contract)

An adapter is a plugin that implements a small, fixed surface. Drawn from Paperclip's `ServerAdapterModule`, adapted to Relix:

- **Identity** — a type name (`hermes`, `claude`, `codex`, `gemini`, `acp`, `process`, `http`, …) and a human label.
- **Execute a Run** — given an Issue's work (prompt/context/goal ancestry), a workspace, and config: run the agent, **stream events back** (thinking, tool calls, messages, the transcript), and return an outcome + token usage + cost + a session handle for continuation. This is the one method that matters.
- **Probe / test the environment** — "is this agent installed and logged in?" (e.g. is the `claude` binary present and authenticated; is Hermes reachable). Surfaces a clear "you need to run `claude login`" message instead of a silent failure.
- **Models & model profiles** — which models this backend can run, and their reasoning/effort/fast-mode knobs.
- **Quota / subscription windows** — for subscription backends, report usage (5-hour / weekly / credits windows, plan type) so the dashboard shows "you've used 60% of your Claude Max weekly window."
- **Runtime command spec** — for CLI backends, the command to run and a self-install line (`npm install -g @anthropic-ai/claude-code`) for sandbox/remote targets.
- **Session management** — how to resume a prior session (continue where the agent left off) and how to interrupt/stop a running one.
- **Bridge-back capability flag** — whether the host injects a **scoped per-run Relix token** so the agent can call Relix's own API (comment on the Issue, create sub-issues, request approval). See §5.

That's the whole contract. Anything that can implement "run a Run and stream it back" can be an agent in Relix.

---

## 3. The adapter catalog (what we plug in)

Ordered by integration depth. **The depth of governance scales with the adapter** (§6) — but the *minimum* bar (assign work, get results, talk back via a scoped token) works for all of them.

### 3.1 Hermes adapter — the flagship (deepest)
The full design is [`relix-hermes-integration.md`](relix-hermes-integration.md). In adapter terms: the Hermes adapter runs an installed Hermes worker node, and because Hermes has a **plugin API with lifecycle hooks**, this adapter gets the *richest* integration — per-tool-call gating, in-loop approval routing, memory written to Relix's store, the self-teaching loop. It's the adapter we invest most in, because Hermes is the most capable self-improving worker. **Everything in the Hermes integration doc still stands — it's now "the Hermes adapter, in detail."**

### 3.2 Claude Code adapter — your Claude Max subscription (first-class)
- **What it is:** runs the operator's installed `claude` CLI as the agent's brain, on the **Claude subscription** (Max), not a per-token API key.
- **How it works (verified in Paperclip):** spawn `claude --print --output-format stream-json --verbose`, **pipe the Issue's prompt over stdin**, optionally `--resume <session>` to continue, `--model`, `--append-system-prompt-file <the agent's instruction bundle>`, `--add-dir <skill bundle>`. Parse the stream-json back into the live transcript + token usage + the CLI's reported cost.
- **Subscription, not API key:** **no inference key is injected.** The `claude` binary uses its own stored `~/.claude` OAuth login. Subscription mode is detected by the *absence* of an Anthropic API key → billing type `subscription`. The cost ledger records the Run as **$0 (subscription-included) but still tracks tokens + run count** (so you see usage without a dollar charge). Quota is polled from Anthropic's OAuth usage endpoint using the CLI's own token, surfaced as the 5-hour / weekly windows.
- **Tools/approvals caveat:** headless `--print` runs can't answer interactive permission prompts, so the CLI runs either with skip-permissions (trusted box) or a curated `--allowedTools` allowlist (sandbox). This means governance for this adapter is **box-level + Relix-API-level**, not per-tool-call (§6).
- **Implemented (model lane):** an Operative's stored `model_preference` is now **consumed at run time** — the Claude Rig appends `--model <model_preference>` to its argv (discrete argv element, no shell). `reasoning_effort` is **not** mapped for Claude (Claude Code exposes no documented headless effort flag); it applies to Codex only (§3.3).
- **Session resume — intentionally NOT mapped for Claude (stated caveat).** Although `--resume <session>` exists, Claude Code resolves a `--print --resume` session from the run's **working directory**, and Relix runs every Shift in a **fresh per-run scoped workspace**, so a resumed Claude session would not reliably resolve. Until a stable per-line-of-work workspace exists for the Claude Rig, resume stays **Codex-only** (§3.3); the Claude Rig captures + persists the `session_id` (so the recovery table still shows it) but does not replay it. See `docs/current-limitations.md`.

### 3.3 Codex adapter — your ChatGPT/Codex subscription (first-class)
- **What it is:** runs the operator's installed `codex` CLI on the **ChatGPT Plus/Pro/Codex subscription**.
- **How it works (verified):** spawn `codex exec --json [...] -`, **pipe the prompt over stdin**, `--model`, `-c model_reasoning_effort=…`, fast-mode flags, or `codex exec resume [OPTIONS] <session> -` for continuation. Parse JSONL back.
- **Subscription handling — the clever bit:** Codex reads credentials from `$CODEX_HOME/auth.json`. Paperclip gives each company a **managed `CODEX_HOME`** and **symlinks the operator's `~/.codex/auth.json`** into it — so the agent rides the operator's ChatGPT login without copying secrets around. Subscription detected by absence of an OpenAI API key → biller `chatgpt`, cost `$0` subscription-included, tokens tracked. Quota comes from the Codex app-server's JSON-RPC (`account/rateLimits`) or the ChatGPT usage endpoint, surfaced as 5h/weekly/credits windows + plan type.
- **Implemented (model lane):** an Operative's stored `model_preference` and `reasoning_effort` are now **consumed at run time** — the Codex Rig splices `--model <model_preference>` and `-c model_reasoning_effort=<reasoning_effort>` into its argv **before** the trailing `-` stdin marker (so the prompt still reads from stdin), as discrete argv elements (no shell string). Effort is constrained to `minimal`/`low`/`medium`/`high` at write time.
- **Implemented (session lane):** the Codex Rig now **resumes a prior session**. When a run starts, Relix looks up the stored `session_id` for the EXACT `(tenant, Operative, Rig, Brief)` pairing (the same 4-tuple the runtime-state ledger keys on) and, when present + valid, transforms the argv to `codex exec resume [OPTIONS] <session> -` — `resume` is spliced in right after the leading `exec`, existing flags remain before the session id, and the trailing stdin `-` marker remains last. This is wired on every start path (manual `brief.run`, Prime Start-to-Shift, the autonomous heartbeat, and the guarded operator retry — a retry of the same line of work continues the same thread). The lookup is keyed on that 4-tuple, so it can never cross tenants, Operatives, Rigs, or unrelated Briefs; a missing/invalid id (empty, whitespace/control, or a leading `-`) is skipped and the run starts fresh. The session id is **adapter state, not user input**, validated before it becomes a discrete argv element and never logged beyond the existing masked recovery surface. Codex threads live in `$CODEX_HOME` (independent of the cwd), which is what makes resume safe across Relix's per-run scoped workspaces.

### 3.4 ACP adapter — the protocol-based alternative (Claude/Codex/others over a wire)
- **What it is:** instead of wrapping a CLI's stdout, talk the **Agent Client Protocol** to a bundled agent binary (`claude-agent-acp`, `codex-acp`, or a custom ACP command). Paperclip's `acpx_local`.
- **Why it matters:** ACP gives **structured, streamed events** (text deltas, tool calls, status, done/error) and **real permission modes** (approve-all / approve-reads / deny-all) and warm session reuse — a cleaner integration than scraping CLI output, and it uses the *same* subscription credentials. Hermes also speaks ACP (it has an ACP adapter), so this is a second path to several agents. Good for any agent that supports ACP.

### 3.5 Other CLI adapters (cheap to add)
Same spawn-the-binary pattern, each ~one small plugin: **Gemini CLI** (Google subscription), **Grok CLI**, **Cursor** (local agent CLI + Cursor cloud), **opencode**, **pi**. Each is a thin adapter declaring its command, install line, models, and quota source. The point isn't to ship all of them day one — it's that *the system makes each one a small, isolated plugin.*

### 3.6 Generic adapters — literally anything
- **`process`** — run an arbitrary command as an agent (any local tool that takes a prompt and emits output).
- **`http` / remote-API** — an agent that lives behind an HTTP endpoint (a hosted agent, a teammate's service, a future Relix-native Rust worker). This is also the **swappable seam**: a native worker implements the same execute-and-stream contract and slots in as just another adapter.

### 3.7 Whole-framework adapters — OpenClaw, Paperclip, and other agent systems
The adapter system isn't limited to single CLIs — you can plug in an entire agent *framework* as a backend:
- **OpenClaw** — Hermes's sibling/predecessor lineage; plugged in the same way (a gateway/process adapter), so an OpenClaw agent can take Relix Issues. (Hermes already carries an `openclaw_gateway` adapter concept; we generalize it.)
- **Paperclip itself** — yes, you can run Paperclip as a worker behind a Relix agent (via its API / a process adapter). Relix-the-secure-company can assign an Issue to a Paperclip-backed agent if that's useful. (Paperclip is a control plane, so this is more "delegate a workstream to another control plane" than "one worker," but the adapter contract still holds.)
- **Any future agent framework** — the contract is just "execute a Run, stream it back, report cost/usage, support a session handle, expose a probe." Anything that can do that is an adapter. **This is the open-ended promise: plug in *anything*.**

---

## 4. The subscription model (the part you specifically asked for)

Put plainly: **Relix lets an agent run on your existing Claude / ChatGPT / Gemini subscription instead of burning API credits — by driving the CLI you already logged into.** The mechanics, generalized from Paperclip:

1. **No inference key injected.** The CLI authenticates itself with its own stored OAuth login (`~/.claude` for Claude, `$CODEX_HOME/auth.json` for Codex). Relix never handles the model credential.
2. **Subscription detected by absence of an API key.** If an API key *is* configured, that adapter switches to metered API billing instead. The operator chooses per agent.
3. **Credentials shared safely.** For Codex, Relix gives each tenant a managed home and **symlinks** the operator's `auth.json` in (no copying secrets). For Claude, the binary finds its own config dir. This fits Relix's per-tenant isolation: each agent's box gets exactly the subscription login it's entitled to, nothing else.
4. **Cost ledger: $0 but tracked.** Subscription Runs record as **`subscription_included` at zero dollars**, while still logging tokens and run counts. The Costs dashboard splits **metered-API spend** vs **subscription usage** so you can see both: "this agent cost $4.10 in API + ran 22 subscription jobs (1.2M tokens, 60% of weekly Max window)."
5. **Quota awareness + back-off.** Relix polls each provider's usage window and, when a CLI reports a usage-limit hit, parses out the reset time and **reschedules the Run** instead of failing — so a subscription agent that's temporarily capped waits and retries rather than erroring.

This is a real cost lever: a founder running a company of agents can put the heavy coding agents on their Claude Max / Codex subscriptions (flat-rate) and reserve metered API for spillover — and see the whole picture in one Costs view.

---

## 5. The bridge-back token — how any agent "talks to Relix"

For an agent to be a *company employee* (not just a one-shot worker), it has to talk *back* to Relix: comment on its Issue, ask a question, create sub-issues, request approval, report done. Paperclip solves this with a **per-run local-agent JWT** (`PAPERCLIP_API_KEY`) injected into the agent's environment — a scoped token that lets the agent call *Paperclip's own control-plane API* (not the model provider). We adopt the same idea, and it generalizes the Hermes bridge:

- **Rich bridge (Hermes):** the `relix-bridge` plugin uses Hermes's hooks for deep, per-tool-call integration (gate every tool, route every approval, write memory to Relix). Maximum governance.
- **Thin bridge (any CLI / generic agent):** Relix injects a **scoped, per-run Relix token** into the agent's environment and tells the agent — via its instruction bundle / system prompt — how to call Relix's API to comment, create sub-issues, and request approval. The agent talks back over HTTP with a token that's valid only for *this run, this agent, this Issue*. Less deep than the Hermes plugin (we can't intercept its internal tool calls), but it works for **any** agent that can make an HTTP call.

So the integration depth is a spectrum, and the floor is universal: **assign work, stream results back, and let the agent call home with a scoped token.** Every adapter clears that bar; richer adapters (Hermes, ACP) clear more.

---

## 6. Security — governance scales with the adapter, the box is always the floor

The §3 security reconciliation from the Hermes doc generalizes: **every adapter runs inside a Relix-governed sandbox box with a signed identity, scoped credentials, and the gate on everything that crosses the box wall.** What differs is *how deep inside the box* we can govern:

- **Rich adapters (Hermes, ACP with permission modes):** per-tool-call gating — every tool the agent wants to run is checked against Relix's policy *before* it runs, and dangerous actions route to your Inbox mid-run. Strongest.
- **Thin adapters (headless `claude --print`, `codex exec`, generic process):** we **cannot** intercept each internal tool call (the CLI runs them itself, headless). So governance is **box-level**: the sandbox bounds what the agent can reach (filesystem, network, secrets), and the **scoped Relix token** bounds what Relix APIs it can call. Per-tool-call approval isn't available for these — which is an **honest tradeoff to state plainly**.

The consequence is a clear rule: **the less an adapter lets us govern inside it, the more load-bearing its sandbox is.** A headless Claude CLI agent that can reach the open internet inside its box is only as safe as that box. So:

- Thin-adapter agents get **tighter boxes** by default (no raw network egress unless granted; secrets scoped to exactly what they need; the subscription login and nothing else).
- The **subscription credential** is the one thing we deliberately let into the box (it's the whole point) — but *only* that agent's entitled login, isolated per tenant (§4.3 in the Hermes doc).
- For sensitive work (real money, production, cross-tenant), prefer a **rich adapter** (Hermes) where per-tool-call gating is available, or require the thin-adapter agent to route those actions through Relix tools via the bridge token.

This keeps Relix's "nothing crosses a boundary unverified" stance intact regardless of which agent you plug in — the boundary just sits at the box wall for thin adapters and at the tool-call for rich ones.

---

## 7. How this lands in the company model

Small, clean additions to the spine (none disturb the substrate):

- **An agent record gains an "adapter" choice** — which backend runs it (Hermes / Claude / Codex / …), plus that adapter's config (which model, subscription-vs-API, which subscription login, session-resume policy). This sits alongside the agent's instruction bundle, permissions, autonomy, budget, and `reports_to` (company-model §4.5).
- **Assignment is unchanged.** You assign an Issue to an agent exactly as before; the agent's adapter is *how* it runs. "Assign work directly to that agent using the plug-in" (your words) = assign the Issue; the adapter executes it.
- **The Costs view gains the subscription split** — metered-API dollars vs subscription usage/quota windows, per agent.
- **The Agents/Org surface gains a per-agent "backend" line** — "Backed by: Claude Code (Max subscription) · logged in ✓ · 60% of weekly window" — with the probe/login state visible so a not-logged-in agent is obvious, not silently broken.
- **The dashboard's environment probe** surfaces "run `claude login`" / "run `codex login`" when an adapter is configured but unauthenticated.

---

## 8. Phasing (slots into the existing roadmap)

The adapter system is the frame; the Hermes track (H0–H4 in the Hermes doc) is the deepest adapter built inside it. Sequencing:

- **Phase A0 — The adapter contract.** Define the uniform "execute a Run + stream back + report cost/usage + session handle + probe" interface and the registry. Wire *one* adapter end-to-end (Hermes, per H0). Now the spine speaks "adapter," not "Hermes."
- **Phase A1 — The subscription CLI adapters.** Ship the **Claude Code** and **Codex** adapters: spawn the CLI, pipe the prompt, stream back, **subscription-credential handling** (no key injected, login detection, the Codex `CODEX_HOME` symlink), the **cost ledger `subscription_included` $0-but-tracked** path, and **quota polling + back-off**. This is the "use my installed subscription" payoff. (Parallel to company Phase 2–3.)
- **Phase A2 — The thin bridge-back token.** The scoped per-run Relix token + instruction-bundle guidance so a CLI agent can comment / create sub-issues / request approval. Now a subscription CLI agent is a real employee, not a one-shot. (Parallel to company Phase 3–4.)
- **Phase A3 — ACP + more adapters.** The ACP adapter (structured events + permission modes for Claude/Codex/others), then the cheap CLI adapters (Gemini, Grok, Cursor, opencode, pi) and the generic `process`/`http` adapters as demand dictates.
- **Phase A4 — External adapter plugins.** Let operators/third parties drop in their own adapters (the way Paperclip loads external adapter packages), so "plug in any agent" is open-ended.

The Hermes-specific depth (the `relix-bridge` plugin, the learning loop, execute_code) rides on top of A0 as the richest adapter, per the Hermes doc's H1–H4.

---

## 9. Open decisions

1. **Per-tool-call governance for thin adapters.** For headless CLI agents we can't gate each tool. Do we (a) accept box-level governance for them, (b) require them to route sensitive actions through Relix tools via the bridge token, (c) prefer ACP (which *does* expose permission modes) for any agent that supports it, or (d) reserve thin CLI adapters for trusted/low-risk work only? (Leaning: ACP-where-possible + tight boxes + bridge-routing for sensitive actions; accept box-level for the rest, stated clearly.)
2. **Subscription credential isolation.** One operator subscription shared across that operator's agents, or strictly one login per agent box? How do we share `~/.claude` / `~/.codex` into a per-tenant box without leaking it cross-tenant? (Leaning: symlink the *operator's own* login into *that operator's* agent boxes only; never cross-tenant; mirror Paperclip's per-company managed home.)
3. **Subscription terms / ToS.** Running Claude Max / ChatGPT subscriptions through an orchestrator headlessly — is that within each provider's terms for our use case? **Worth checking** before we lean on it commercially (distinct from, but alongside, the Hermes licensing question in the Hermes doc §8.1).
4. **Cost truth for subscriptions.** $0 dollar cost is real, but quota is finite. Do we surface a *synthetic* cost (e.g. equivalent API price) for planning, alongside the real $0? (Leaning: show both — $0 billed + tokens + % of window used.)
5. **Default adapter.** What backs a freshly-hired agent if the operator doesn't choose — Hermes (richest), or whatever subscription CLI the operator has logged in (cheapest)? (Leaning: Hermes default, with a one-click "switch to my Claude/Codex subscription.")
6. **How much of an agent's behavior the adapter vs the instruction bundle owns.** The instruction bundle (job description) should be adapter-agnostic so you can swap an agent's backend (Claude → Hermes) without rewriting its job. Confirm the bundle stays portable across adapters.

---

## 10. Glossary additions

- **Adapter** — a plugin that knows how to run one kind of agent backend (Hermes, Claude Code CLI, Codex CLI, ACP, remote API, …) behind a uniform "execute a Run and stream it back" contract.
- **Adapter registry** — the map from an agent's chosen backend name to its adapter plugin; built-ins plus operator/third-party plug-ins.
- **Subscription adapter** — an adapter that runs an installed CLI on the operator's existing subscription (Claude Max, ChatGPT/Codex, Gemini), using the CLI's own login instead of an inference API key; billed as `$0 subscription-included` with tokens/quota tracked.
- **Bridge-back token** — a scoped, per-run Relix token injected into an agent's environment so it can call Relix's own API (comment, sub-issue, request approval) — the universal "talk back to Relix" mechanism; the thin counterpart to Hermes's `relix-bridge` plugin.
- **Rich vs thin adapter** — rich (Hermes, ACP) allows per-tool-call governance inside the agent; thin (headless CLI, generic process) allows only box-level governance + the bridge token.

---

*Net: "plug in any agent" and "use my installed Claude/Codex subscription" are one system — a uniform adapter contract with a registry of backends. Hermes is the deepest adapter (full plugin-hook governance + self-improvement); the Claude Code and Codex adapters run your existing subscriptions for flat-rate work; ACP and generic adapters cover the rest; and a scoped bridge-back token lets any of them act as a real Relix employee. Governance scales with the adapter, the sandbox is always the floor, and the whole thing rides on Relix's secure mesh — which is still the one thing none of the agents we're plugging in provides for themselves.*
