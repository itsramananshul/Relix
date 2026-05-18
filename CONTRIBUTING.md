# Contributing to Relix

This document is short and load-bearing. Read it before opening a PR.

## Branching

Trunk-based. `main` is always shippable to the alpha demo. Feature work happens on short-lived branches (`<owner>/<topic>`), reviewed and merged within days.

Squash-merge by default. Multi-commit merges only when the commit structure is useful for history.

## PR Review

Two tiers:

- **Security-critical crates** (`relix-core` identity/bundle/policy/eventlog modules, `relix-runtime` dispatch/admission pipeline): two reviewers, at least one CODEOWNER. Self-merge prohibited.
- **Everything else:** one CODEOWNER review.

All PRs require:

- Green `cargo fmt --check`, `cargo clippy`, `cargo test --workspace`.
- Tests covering the change, or an explicit note explaining why none are possible.
- PR description naming the spec section(s) and/or `docs/code-reuse-map.md` rows the change relates to.
- CHANGELOG entry if a public API or wire format changes.

## Specs Are Source of Truth

The substrate target lives in `specs/RELIX-1..8`. Where the alpha deviates, the deviation is documented in `specs/alpha-simplifications.md` with a deadline (gate) for resolution. Implementation conforms to specs; specs do not retroactively rationalize implementation.

Changes to `specs/` follow a separate PR from implementation. Spec amendments are not bundled with code.

## Coding Standards

- `rustfmt` and `clippy` (deny warnings) enforced in CI.
- `unwrap()` and `expect()` forbidden in non-test code paths. Use typed errors (`thiserror`).
- All errors are typed. No `String` errors crossing crate boundaries.
- `#![forbid(unsafe_code)]` at every crate. Waivers require explicit security review.
- Determinism-relevant code carries `// DETERMINISM:` comments explaining the constraint. Do not silently remove them in refactors.
- `// TODO(alpha):` comments mark known simplifications with a pointer to `specs/alpha-simplifications.md`.

## Existential Properties (Non-Negotiable)

These cannot be weakened, even temporarily, even for "just a demo":

1. Real separate OS processes for separate node types. No in-process fake P2P.
2. Identity verified on every responder before any handler logic. No bypass switch.
3. Policy evaluated on every responder before handler dispatch.
4. Audit record on every responder for every cross-node call.
5. Routing in SOL flows, never in Rust/Python glue.
6. AI provider keys live ONLY in the AI node's local config.
7. The web backend (`relix-web/`) does NOT make LLM provider calls in `RELIX_MODE`.
8. No marketplace work.

Violations of these are blocked at PR review on principle.

## Security Review

The designated security reviewer (rotating responsibility within the team) must approve:

- Any change touching crypto, signature paths, identity verification, policy evaluation.
- Any new dependency in the security-critical set (see `docs/security-critical-deps.md`).
- Any change to the admission pipeline ordering in `relix-runtime`.

## Local Commits, No Push Without Permission

Commits land locally during alpha development. `git push` happens only on explicit instruction.

## Documentation Is Part of the Change

Relix docs must stay real and current. **Every meaningful change updates the relevant docs in the same commit, or in an immediately adjacent commit.** Docs are not marketing text; they are operational truth.

Required doc discipline:

- Architecture changes → update `docs/architecture-overview.md` (and `docs/code-reuse-map.md` if reuse boundaries shift).
- Alpha-scope changes → update `docs/alpha-plan.md`.
- Implementation deviates from a frozen spec → add or amend an entry in `specs/alpha-simplifications.md` with a gate for resolution.
- Commands change (CLI subcommand, flag, binary name) → update `README.md` and `ops/runbooks/alpha-bringup.md`.
- Audit / inspection behavior changes → update `ops/runbooks/audit-query.md`.
- Code reused from OpenPrem / OpenClaw / Hermes / Open WebUI changes (added, removed, retargeted) → update `docs/code-reuse-map.md`.
- Security behavior changes (key handling, trust model, signing) → update `SECURITY.md` and/or `specs/threat-model.md`.
- Dependencies change (add / remove / version bump security-critical) → update `docs/security-critical-deps.md`.

Pre-push checklist (every commit, every push):

1. Did this change affect architecture, behavior, commands, security, setup, or scope?
2. If yes, were docs updated?
3. If no, the push report explains why no doc update was needed.

CI does not enforce this rule directly (yet); reviewers and the on-disk pre-push report do.

## Repo Hygiene

The repo holds the project's intentional artifacts. **Reference material, scratch notes, vendor copies, and temporary working files do NOT belong on `main`.**

Allowed:

- Actual Relix source code, configs actively used by the runtime, production-relevant docs, specs, runbooks, tests, CI, assets actually used by the app, migrations/schemas actually used, flow files actually used, scripts actually used.

Not allowed:

- Copied OpenPrem / OpenClaw / Hermes / Open WebUI source unless actively wired into Relix.
- Random reference repos, transcript dumps, AI brainstorming text files, scratch notes, downloaded documentation, screenshots not used by the app.
- Local logs / databases, build artifacts, temporary generated test files, experimental dead-end code, staging folders.

If reference material is useful: document it in `docs/code-reuse-map.md`, name the upstream path/repo, and import only the necessary code.

Pre-push hygiene check (every push):

1. `git status` — review staged additions.
2. Identify accidental files (logs, keys, vendor copies, scratch dirs).
3. Remove them and update `.gitignore` if a pattern recurs.

A new engineer should be able to clone `main` and understand the project quickly. Junk on `main` actively erodes that property.

## What This Project Refuses

- AI co-author tags in commits.
- "Generated by" attribution in code or commit messages.
- Secrets, API keys, or private keys in the repository.
- Convenience shortcuts that violate the existential properties above.

## Reporting Security Issues

See `SECURITY.md`.
