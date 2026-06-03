# CI Strategy

Relix runs one required gate on every push and pull request, plus an
on-demand end-to-end lane and a nightly security pass. The required gate is
deliberately thorough so no regression can land green; the other two lanes
carry only the work that is too slow or too noisy to run on every push.

## Layer 1: `ci.yml` (required, every push / PR)

Runs unconditionally on every push to `main` and every pull request. This is
the gate that must be green before a change reaches `main`.

| Job          | Purpose                                                  | Runs on |
|--------------|----------------------------------------------------------|---------|
| `fmt`        | `cargo fmt --all -- --check`                             | ubuntu  |
| `build-test` | `cargo clippy --workspace --all-targets -- -D warnings` then `cargo test --workspace` | ubuntu + macOS + windows |
| `deny`       | `cargo deny check` (licenses, bans, sources, advisories) | ubuntu  |

`build-test` runs the full ubuntu + macOS + windows matrix on purpose.
OS-gated code (`#[cfg(unix)]`, `#[cfg(target_os = "...")]`, path handling,
line endings) only compiles and lints on its own host, so a Linux-only run
would let an OS-specific regression land green. `fmt` and `deny` read
host-independent inputs (source text and the lockfile), so they run once on
Linux.

`build-test` uses a per-OS `Swatinem/rust-cache` shared key. The cache is
saved only on `main` (per the action's recommendation) to avoid pull-request
cache thrash, and cache failures are `continue-on-error` because the cache is
a speed optimisation, not a correctness gate.

## Layer 2: `heavy-ci.yml` (on-demand end-to-end)

The required hygiene gates now live in `ci.yml`, so `heavy-ci.yml` carries
only the heavy multi-process mesh bring-up, which is too slow and port-bound
to run on every push.

Triggers:

- `workflow_dispatch`: operator runs it before a milestone push.
- Pull request labeled `heavy`: runs the end-to-end demo against the PR.

| Job           | Purpose                                            |
|---------------|----------------------------------------------------|
| `filter`      | gate the run on dispatch or the `heavy` label      |
| `integration` | `bash scripts/alpha-bringup-m5.sh` end-to-end demo |

This lane is not a required gate. It adds end-to-end coverage on request; it
does not duplicate the per-push hygiene checks.

## Layer 3: `nightly-security.yml` (scheduled + manual)

Triggers:

- Daily cron (`0 6 * * *` UTC).
- `workflow_dispatch`.

| Job           | Purpose                                                       |
|---------------|---------------------------------------------------------------|
| `deny-strict` | `cargo deny check` (all categories incl. advisories) hard gate |
| `audit-strict`| `cargo audit` as a hard gate                                  |
| `full-tests`  | `cargo test --workspace --release`                            |

`issues: write` permission lets the lane open a tracking issue when a newly
published advisory breaks the strict pass. `ci.yml` already runs `cargo deny
check` on every PR, so most advisory drift is caught there first; nightly is
the backstop that catches advisories published after a change has merged.

## Per-advisory exceptions

If `audit-strict` or `deny-strict` flags an advisory that we have evaluated
and decided to accept (transitive, unreachable, or no fix available), the
exception is recorded in two coordinated places:

1. **`docs/security-advisories.md`**: human review notes, including the
   advisory ID, direct or transitive status, reachability assessment,
   severity in context, mitigation plan, and review condition.
2. **`deny.toml`** `[advisories] ignore = [...]`: machine-readable, with an
   inline comment naming the advisory ID and pointing at the docs entry.

No silent suppressions. Every entry has a removal condition (for example,
"remove after libp2p reaches 0.55").

## Local-first workflow

The required gate mirrors the recommended local pre-push order:

```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

Running these locally catches issues before they consume Actions minutes.
`scripts/ci-local.ps1` runs the same set on a Windows dev box.

## Toolchain

`rust-toolchain.toml` pins **Rust 1.95** with `rustfmt` and `clippy`. The
required gate (`ci.yml`) installs that exact version via
`dtolnay/rust-toolchain@1.95.0` so fmt output and the clippy lint set are
reproducible; a floating `stable` pulls newer tools whose formatting and
lints drift from the pin and fail the gate for reasons unrelated to the
change under review. When the pin in `rust-toolchain.toml` moves, update the
workflow version in the same PR. `Swatinem/rust-cache@v2` provides layered
caching of the registry, git database, and target directory; cache keys are
per-OS to avoid cross-OS invalidation.

MSRV is documented in `README.md`.

## Discipline rules

- Keep `ci.yml` as the single required gate. Do not move a required check
  (fmt, clippy, test, deny) to a dispatch-only or label-only lane, because
  that lets the check be skipped on an ordinary PR.
- Keep the `build-test` matrix on all three operating systems. Dropping an OS
  reopens the gap where OS-gated regressions land green.
- Secret scanning belongs in an entropy-based scanner with an allowlist
  (gitleaks), tracked separately. A plain grep cannot work here because the
  redaction module and its docs legitimately contain key-shaped strings.
- Keep the heavy end-to-end demo off the per-push path. It is slow and
  port-bound; run it on dispatch or with the `heavy` label.
