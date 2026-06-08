# Dependency Policy

How Relix manages workspace dependencies, version drift, and security review. Companion to `docs/security-critical-deps.md`, `docs/security-advisories.md`, and `docs/dependency-duplicates.md`.

## Workspace dependency strategy

All shared third-party crates AND internal crates are declared exactly once, in the root `Cargo.toml` under `[workspace.dependencies]`. Member crates inherit via:

```toml
[dependencies]
relix-core.workspace    = true
serde.workspace         = true
tokio.workspace         = true
```

Internal crates carry an explicit `version` (matching the workspace version, currently `0.4.1`) in the workspace declaration so cargo-deny does not flag them as wildcard dependencies:

```toml
[workspace.dependencies]
relix-core    = { path = "crates/relix-core",    version = "0.4.1" }
relix-runtime = { path = "crates/relix-runtime", version = "0.4.1" }
```

**Rule:** member crates never use bare `path = "..."` for internal deps. The workspace is the source of truth for versions.

## Workspace metadata inheritance

Every member crate inherits `version`, `edition`, `rust-version`, `license`, `publish`, `repository`, and `authors` from `[workspace.package]`. This keeps `cargo metadata` consistent and means a single edit to the workspace manifest propagates everywhere.

## Adding a new dependency

1. Add to `[workspace.dependencies]` in the root manifest with the pinned version.
2. If the dependency is security-critical (crypto, transport, parser, runtime), add an entry to `docs/security-critical-deps.md` naming why it was chosen and what alternatives were considered.
3. Consume from member crates via `<crate>.workspace = true`.
4. Run `cargo deny check licenses bans sources` locally. New licenses require an explicit allow + a rationale paragraph in `docs/security-critical-deps.md` under "License Exceptions."
5. Open the PR. Reviewer confirms steps 1–4.

## Duplicate dependency policy

`cargo deny check bans` flags any crate present at more than one version. Most duplicates in a libp2p-based workspace are ecosystem-level version drift (rand, thiserror, getrandom, socket2, hashbrown, windows-sys, etc.). The policy:

| Class                       | Action |
|-----------------------------|--------|
| Ecosystem split (rand 0.8/0.9, thiserror 1/2, etc.) | Leave alone; document in `docs/dependency-duplicates.md`. |
| libp2p-internal (yamux, multistream-select)        | Leave alone; wait for upstream convergence. |
| Dev-dependency only         | Leave alone unless trivially fixable.                    |
| Security-sensitive (crypto, TLS, parsers)          | **Investigate.** Older version with known issues forces a fix path. |
| One-line fix (workspace can pin to converge both consumers) | Fix.                                                     |

**Do not destabilize the dependency graph** to silence duplicate warnings. The `[bans] multiple-versions = "warn"` setting reflects this: warnings are visible, not fatal. Hard fail is reserved for `wildcards = "deny"` and banned licenses.

## Transitive dependency handling

Relix accepts that the libp2p ecosystem brings a long tail of transitives. Policy:

- Direct dependencies are reviewed and listed in `docs/security-critical-deps.md`.
- Transitive dependencies are not enumerated, but are subject to:
  - License check via cargo-deny (see allowlist in `deny.toml`).
  - Advisory check via cargo-deny on every PR plus cargo-audit nightly hard gate.
- A transitive advisory that proves reachable in Relix code becomes a release blocker. A transitive advisory that is documented as unreachable goes into `docs/security-advisories.md` with rationale + review condition, and a matching `deny.toml [advisories] ignore` entry.

## Security review process

Three trigger points:

1. **Per-PR (automatic):** `ci.yml` runs `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test` on the ubuntu + macOS + windows matrix, plus `cargo deny check` (all categories). All are required gates.
2. **Per-milestone (manual):** before pushing a milestone commit, run the full `cargo deny check` and `cargo audit` locally.
3. **Nightly (automated):** `nightly-security.yml` runs `cargo deny check` (all categories, hard gates) and `cargo audit` (hard gate) against `main`. New advisories surface within 24 hours.

## Deferred optional feature graph

The first release ships the default feature graph. `cargo deny check` is the
release supply-chain gate and is expected to pass there.

Do **not** use `cargo deny --all-features check` as a release blocker until the
deferred optional feature families below are reviewed or replaced:

| Feature family | Current blocker | Release status |
|----------------|-----------------|----------------|
| `browser-headless-chrome` | `headless_chrome -> auto_generate_cdp`, whose synthesized license metadata resolves to `GPL-3.0-or-later`. GPL-family licenses are not allowed for Relix without an explicit legal/product decision. | Deferred; not in the default first-release graph. |
| `browser-webdriver` | `fantoccini -> webdriver`, licensed `MPL-2.0`, which is not currently in the allowlist. | Deferred; not in the default first-release graph. |
| `terminal-pty` | `portable-pty -> serial`, flagged by `RUSTSEC-2017-0008` as unmaintained. | Deferred; not in the default first-release graph. |

The correct fix is dependency replacement, feature redesign, or an explicit
documented approval. Do not broaden `deny.toml` just to make
`--all-features` green.

## Why libp2p ecosystems naturally create version skew

libp2p 0.54 bundles dozens of small protocol crates (`libp2p-tcp`, `libp2p-noise`, `libp2p-yamux`, `libp2p-kad`, `libp2p-request-response`, `libp2p-dns`, `libp2p-allow-block-list`, ...). Each of these has its own dependency tree. The libp2p organization releases them on a coordinated schedule but with different acceleration rates — e.g., `yamux` 0.12 and 0.13 both ship in different sub-crates because the breaking-change migration is mid-flight.

The right response is patience, not pinning. Forcing a single version across the graph either breaks libp2p compilation or pulls in patched forks; both are worse than the warning. Reviewing duplicates at every milestone keeps the picture honest without thrashing the build.

## Toolchain

`rust-toolchain.toml` pins Rust 1.95 with `rustfmt` + `clippy`. The pin is intentional:

- Avoids surprise compiler bumps that break CI mid-iteration.
- Lets contributors reproduce builds without flipping toolchains.
- CI uses `dtolnay/rust-toolchain@stable`, which reads the pin file and skips reinstall when the cache is warm.

The pin moves only when a deliberate need arises (new language feature, stable advisory, edition bump). Pin updates land in their own PR with a short rationale.

## CI policy at a glance

`ci.yml` (every push/PR, required) runs fmt, clippy `-D warnings` and test on the ubuntu + macOS + windows matrix, plus `cargo deny check` (all categories).

`heavy-ci.yml` (manual or `heavy` label) runs the end-to-end mesh demo. It is not a required gate and does not duplicate the per-push hygiene checks.

`nightly-security.yml` (daily 06:00 UTC + manual) is the backstop hard gate for cargo-deny (all categories) and cargo-audit. Failures here open tracking issues.

See `docs/ci-strategy.md` for the full breakdown.

## Failure modes the policy protects against

- A new dependency lands without license review → cargo-deny licenses check blocks.
- A crate gets yanked → cargo-deny yanked check warns.
- An advisory is published against a dependency we rely on → nightly catches it within 24 h.
- A reviewer accepts an advisory without rationale → policy requires the `deny.toml ignore` entry to point at a `docs/security-advisories.md` row.
- An author tries to hand-pin a transitive to silence a duplicate warning → blocked by review per "Do not destabilize the dependency graph."
