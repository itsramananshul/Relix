# Relix — working rules (BINDING)

## Design-docs-first protocol (NON-NEGOTIABLE)

Everything built in this repo MUST come from the design docs in `docs/`.
The authoritative product/design docs are:

- `docs/relix-lexicon.md` — the product names (Guild, Operative, Brief, Mandate, Campaign, Shift, Rig, Bench, Macro, Keeper, Snag, Dossier, Chronicle, Desk, Claim, Keys, Allowance, …) and the two-layer rule (product layer uses these names; existing internal identifiers like `tasks`/`agent`/`reports_to` stay stable).
- `docs/relix-company-model.md` — the company/org/spine model (Founder, Prime, Operatives, Lead, Roster/Lattice/Line/Branch; Guild→Mandate→Campaign→Brief→Shift; hiring, strategy gate, Keys, Allowance).
- `docs/relix-execution-and-issue-design.md` — issue/Brief execution: board states, Claim/lease, dispatch, sub-issues/blockers, supervisory wakes, recovery.
- `docs/relix-dashboard-design.md` — the dashboard IA, views, B&W aesthetic, interactions. (`docs/dashboard-redesign.md` is SUPERSEDED — describes the legacy `/dashboard` only.)
- `docs/relix-hermes-integration.md` + `docs/relix-agent-adapters.md` — the universal Rig adapter system, governance model, subscription CLIs, bridge-back token, Hermes seam, Tether plugin-hook system, and the Pillar-1 transplants (Keeper/Macro/Bench).
- `docs/product-spine-roadmap.md` — the phased roadmap.
- `docs/product-spine-implementation.md` — the implementation map + the **audited divergence ledger** (what currently differs from the docs; keep it honest).

### Before writing ANY code

1. **Read the relevant design-doc section FIRST.** Identify it explicitly.
2. In the response, state up front:
   - **Section:** `<doc file> §<section>` — the exact part being implemented.
   - **Files changed:** the precise list.
   - **Not changed / out of scope:** what is deliberately untouched (and why), so no unrequested layout/behavior changes leak in.
3. Implement EXACTLY what the section specifies — no more, no less.

### Hard rules

- **Do NOT add features that are not in the design docs.** If it isn't in a doc, it doesn't get built. If something seems needed but isn't documented, surface it as a question / propose a doc change — do not silently invent it.
- **Do NOT change layout, IA, naming, or behavior unless the design doc requires it.** Match the doc's structure and the lexicon names.
- **Lexicon:** product-facing surfaces (capability wire names, UI copy, docs) use the `relix-lexicon.md` names. Stable internal identifiers are preserved per the doc's two-layer rule.
- **LOCKED decisions in the docs are binding.** Do not silently diverge from a decision the doc marks LOCKED (e.g. the two-pointer Claim). If a divergence already exists, record it in the divergence ledger and only change it when explicitly directed.

### After EVERY change (self-check)

- Re-open the cited design-doc section and verify the change conforms to it.
- If the change deviates from the doc in any way, **fix the deviation** before moving on (or, if intentional/blocked, record it in the divergence ledger in `docs/product-spine-implementation.md` — never leave an undocumented divergence and never overclaim conformance).
- Keep `docs/product-spine-implementation.md` accurate: do not describe something as done/enforced/conformant if the code doesn't actually do it.

### Verification bar

- `cargo test` (the touched crate, then the workspace) must pass; `cargo clippy` clean on touched crates.
- New behavior gets a test that pins the doc-specified semantics.

## Git / attribution (BINDING)

- Commit + push as **Anshul Raman <ramanal@mail.uc.edu>** only. **No AI/Claude attribution** of any kind (no "Generated with…", no "Co-Authored-By: Claude", no Anthropic mention).
- Stage with **explicit paths**, never `git add -A`.
- Work on `codex/product-spine-roadmap` (or the branch in play). Commit/push each green, doc-conformant slice. Commit messages should cite the design-doc section being implemented.
- `references/` clones are gitignored and never tracked; no secrets are ever committed.
