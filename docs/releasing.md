# Releasing — beta & stable channels

_Current workspace version: 0.4.3-beta.1_

Relix ships through two release channels, both produced by the same
`.github/workflows/release.yml` build (all five targets, cosign-signed).
The **channel is derived from the shape of the git tag** — there is no
separate workflow to maintain.

The release workflow uses the same pinned Rust toolchain as
`rust-toolchain.toml` and the local release gate. Do not switch it back to a
floating `stable`; a moving compiler can make packaging behave differently
from the checkout you validated.

| Channel | Tag shape | GitHub release | Marked "Latest" |
|---|---|---|---|
| **Stable** | `vMAJOR.MINOR.PATCH` (e.g. `v0.4.3`) | normal release | yes |
| **Beta** | pre-release suffix (e.g. `v0.4.3-beta.2`, `-rc.1`, `-alpha.1`) | **pre-release** | no |

The workspace version in `Cargo.toml` is the binary-reported version.
Keep it in lockstep with the release tag. A beta build reports the
pre-release suffix too (`0.4.3-beta.2` -> tag `v0.4.3-beta.2`); a stable
build reports the clean version (`0.4.3` -> tag `v0.4.3`).

Before tagging, run the local release gate:

```powershell
relix release readiness --require-clean --run-local-gate
```

This runs the Windows-local gate (`scripts/ci-local.ps1`): boot-policy
coverage, fmt, clippy, dashboard dist parity, serial workspace tests,
`cargo deny`, and the isolated first-release live smoke with the no-spend
echo Rig. It does **not** enable GitHub Actions, create a tag, or call a
model provider. The command also prints the current binary version, expected
tag, current git HEAD, working-tree status, and whether that tag already
exists locally or on `origin`.

## Cut a beta (default path for testing new changes)

Beta is where new changes go first. Tag the commit you want to test with
a pre-release suffix and push the tag:

```sh
git tag v0.4.3-beta.2
git push origin v0.4.3-beta.2
```

If the current checkout still reports an older beta (`relix release readiness`
prints the exact tag and checks whether it already exists), bump `Cargo.toml`
first so the binary-reported version and tag match. Do not reuse an
already-published beta tag for new commits.

`release.yml` builds + signs all five targets and publishes a GitHub
**pre-release**. Pre-releases are never shown as "Latest", so installers
and users tracking the stable channel are unaffected. Iterate with
`-beta.2`, `-beta.3`, … as you push more test changes.

## Promote a beta to stable

When a beta is good, promote it by cutting the **clean** version tag (no
suffix):

```sh
git tag v0.4.3
git push origin v0.4.3
```

This rebuilds + signs from source and publishes a stable release marked
"Latest". (Bump `[workspace.package] version` and the internal workspace
dependency version pins in `Cargo.toml` to match the tag before promoting,
then re-run `relix release readiness --run-local-gate`.)

> The promotion rebuilds from source rather than copying the beta's
> binaries, so make sure the commit you tag `vX.Y.Z` is the same commit
> (or a no-op superset of) the beta you validated.

## Installing from each channel

The installers (`install.sh` / `install.ps1`) pick the channel from two
env vars; an explicit `RELIX_VERSION` always wins over `RELIX_CHANNEL`.

| Want | Mac/Linux | Windows (PowerShell) |
|---|---|---|
| Latest **stable** (default) | `curl -fsSL …/install.sh \| bash` | `irm …/install.ps1 \| iex` |
| Latest **beta** | `curl -fsSL …/install.sh \| RELIX_CHANNEL=beta bash` | `$env:RELIX_CHANNEL='beta'; irm …/install.ps1 \| iex` |
| **Exact** tag | `curl -fsSL …/install.sh \| RELIX_VERSION=v0.4.3-beta.2 bash` | `$env:RELIX_VERSION='v0.4.3-beta.2'; irm …/install.ps1 \| iex` |

(`…` = `https://raw.githubusercontent.com/itsramananshul/Relix/main`.)

How resolution works: stable hits the GitHub `releases/latest` endpoint
(which excludes pre-releases); `RELIX_CHANNEL=beta` walks the `releases`
list and takes the newest non-draft pre-release; `RELIX_VERSION` pins the
tag verbatim. Every channel downloads the same per-OS binaries (Linux
x86_64/arm64, macOS x86_64/arm64, Windows x86_64), SHA256- and
cosign-verified identically — beta builds are not less verified, just not
marked "Latest". The full per-OS one-liners live in the README **Install**
section.

## Manual trigger

You can also (re)run a release from the **Actions → Release** tab with a
`tag` input — `vX.Y.Z` for stable, `vX.Y.Z-beta.N` for beta. Re-running
an existing tag is idempotent: the release is reused and assets are
re-uploaded with `--clobber`.

## CI

CI (`ci.yml`) is the required gate when repository workflows are enabled.
It runs on every push to `main` and every pull request: `cargo fmt --check`,
`cargo clippy -D warnings` and `cargo test` on the ubuntu + macOS + windows
matrix, plus `cargo deny check`.

For the operator-controlled release path, the local gate is mandatory even
when hosted workflows are disabled to save minutes:

```powershell
relix release readiness --require-clean --run-local-gate
```

If workflows are manually disabled in GitHub, enable the **Release** workflow
only when you are ready to push the tag (or run it via `workflow_dispatch`),
then disable it again after the assets are published.
