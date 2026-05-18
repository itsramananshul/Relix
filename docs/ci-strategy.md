# CI Strategy

Relix uses layered CI: a fast lane for every push, a strict lane for milestone gates, and a nightly security pass. The goal is to keep iteration cheap without surrendering hygiene.

## Layer 1 — `fast-ci.yml` (every push / PR)

Runs unconditionally on every push to `main` and every pull request.

| Job          | Purpose                                              | Target runtime |
|--------------|------------------------------------------------------|----------------|
| `fmt`        | `cargo fmt --all -- --check`                         | ~30 s          |
| `check`      | `cargo check --workspace --all-targets`              | ~2 min (warm)  |
| `test`       | `cargo test --workspace` (unit + offline integration) | ~3 min (warm)  |
| `secret-scan`| Grep for committed API keys + AI-coauthor tags       | ~10 s          |

**Total budget:** under 5 minutes with a warm `Swatinem/rust-cache` shared key. The cache is saved only on `main` (per the action's recommendation) to avoid PR cache thrash.

**What fast CI deliberately does NOT run:**

- `cargo clippy --all-targets` (slow first build; lints flap on minor refactors).
- `cargo deny check` (licenses are reviewed at heavy CI; not every push needs the audit-db pull).
- `cargo audit` (advisory database changes daily; nightly catches drift).
- End-to-end multi-process integration scripts (real libp2p ports + processes are heavy and flaky in shared CI).

These belong in `heavy-ci.yml` and `nightly-security.yml`.

## Layer 2 — `heavy-ci.yml` (manual or `heavy`-labeled PR)

Triggers:

- `workflow_dispatch` — operator runs it manually before milestone push.
- Pull request labeled `heavy` — applied when the PR touches the substrate (transport, dispatch, identity, policy, eventlog, codec) or when the author wants a strict lane.

| Job           | Purpose                                                  |
|---------------|----------------------------------------------------------|
| `clippy`      | `cargo clippy --workspace --all-targets -- -D warnings`  |
| `deny`        | `cargo deny check licenses bans sources`                 |
| `audit`       | `cargo audit` — advisory **visibility** (continue-on-error; nightly is the hard gate) |
| `integration` | `bash scripts/alpha-bringup-m5.sh` end-to-end demo       |

Workflow permissions:

```yaml
permissions:
  contents: read
  checks: write              # rustsec/audit-check publishes check-runs
  security-events: write     # advisories appear in the Security tab
```

The `checks: write` block fixes the historical `Resource not accessible by integration` error from `rustsec/audit-check@v2.0.0`. The `audit` job is intentionally `continue-on-error: true` here — its purpose in the heavy lane is to surface new advisories to reviewers, not to block iteration. Hard enforcement is in the nightly lane.

## Layer 3 — `nightly-security.yml` (scheduled + manual)

Triggers:

- Daily cron (`0 6 * * *` UTC).
- `workflow_dispatch`.

| Job           | Purpose                                          |
|---------------|--------------------------------------------------|
| `deny-strict` | `cargo deny check` (all categories incl. advisories) — hard gate |
| `audit-strict`| `cargo audit` as a hard gate                     |
| `full-tests`  | `cargo test --workspace --release`               |

`issues: write` permission allows future automation to open tracking issues on new advisories.

## Per-advisory exceptions

If `audit-strict` or `deny-strict` flags an advisory that we have evaluated and decided to accept (transitive, unreachable, or no fix available), the exception is recorded in two coordinated places:

1. **`docs/security-advisories.md`** — human review notes: advisory ID, direct/transitive, reachability assessment, severity in context, mitigation plan, review condition.
2. **`deny.toml`** `[advisories] ignore = [...]` — machine-readable, with an inline comment naming the advisory ID and pointing at the docs entry.

No silent suppressions. Every entry has a removal condition (e.g., "remove after libp2p ≥ 0.55").

## Local-first workflow

The CI lanes mirror the recommended local-dev pre-push order:

```
# Fast — before every push:
cargo fmt --all
cargo check --workspace --all-targets
cargo test --workspace

# Heavy — before a milestone push (~5–10 min):
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check licenses bans sources

# Security review — periodically, or before security-impacting PRs:
cargo deny check
cargo audit
```

Engineers running these locally catch issues before they consume Actions minutes.

## Toolchain

`rust-toolchain.toml` pins **Rust 1.95** with `rustfmt` + `clippy`. Every workflow uses `dtolnay/rust-toolchain@stable` plus `Swatinem/rust-cache@v2` for layered caching of the registry, git database, and target directory. Cache keys are per-lane (`fast-ci`, `heavy-ci`, `nightly`) to avoid cross-lane invalidation.

MSRV is documented in `README.md`.

## Actions-minute budget

| Lane    | Trigger              | Typical run  | Approx minutes / push |
|---------|----------------------|--------------|------------------------|
| Fast    | push + PR            | 3–5 min      | 4 (4 jobs ~1 min)     |
| Heavy   | manual + `heavy` PR  | 10–20 min    | 0 most pushes         |
| Nightly | daily 06:00 UTC      | 10–25 min    | ~50 min/week          |

Estimated baseline: under **10 minutes of Actions time per typical push**, vs the prior all-in-one workflow that burned 20+ minutes per push regardless of change scope.

## Discipline rules

- Do NOT add expensive jobs to `fast-ci.yml`. The cost of fast CI matters because it runs on every push.
- Do NOT skip heavy CI before a milestone push to `main`. The `heavy` label or a manual dispatch is mandatory before any PR that touches substrate paths is merged.
- Do NOT add `cargo deny check advisories` to `fast-ci.yml`. The advisory database changes daily; that lane belongs to nightly.
- Do NOT remove `secret-scan` from `fast-ci.yml`. It is cheap and catches the most expensive class of mistake.
