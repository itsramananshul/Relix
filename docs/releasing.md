# Releasing — beta & stable channels

_Version: 0.4.1_

Relix ships through two release channels, both produced by the same
`.github/workflows/release.yml` build (all five targets, cosign-signed).
The **channel is derived from the shape of the git tag** — there is no
separate workflow to maintain.

| Channel | Tag shape | GitHub release | Marked "Latest" |
|---|---|---|---|
| **Stable** | `vMAJOR.MINOR.PATCH` (e.g. `v0.4.2`) | normal release | yes |
| **Beta** | pre-release suffix (e.g. `v0.4.2-beta.1`, `-rc.1`, `-alpha.1`) | **pre-release** | no |

The workspace version in `Cargo.toml` is the stable version. Beta tags
carry a `-beta.N` (or `-rc`/`-alpha`) suffix on top of the next target
version.

## Cut a beta (default path for testing new changes)

Beta is where new changes go first. Tag the commit you want to test with
a pre-release suffix and push the tag:

```sh
git tag v0.4.2-beta.1
git push origin v0.4.2-beta.1
```

`release.yml` builds + signs all five targets and publishes a GitHub
**pre-release**. Pre-releases are never shown as "Latest", so installers
and users tracking the stable channel are unaffected. Iterate with
`-beta.2`, `-beta.3`, … as you push more test changes.

## Promote a beta to stable

When a beta is good, promote it by cutting the **clean** version tag (no
suffix):

```sh
git tag v0.4.2
git push origin v0.4.2
```

This rebuilds + signs from source and publishes a stable release marked
"Latest". (Bump `[workspace.package] version` in `Cargo.toml` to match
the stable tag before promoting, the same as any stable release — see
the version note in the root `Cargo.toml`.)

> The promotion rebuilds from source rather than copying the beta's
> binaries, so make sure the commit you tag `vX.Y.Z` is the same commit
> (or a no-op superset of) the beta you validated.

## Manual trigger

You can also (re)run a release from the **Actions → Release** tab with a
`tag` input — `vX.Y.Z` for stable, `vX.Y.Z-beta.N` for beta. Re-running
an existing tag is idempotent: the release is reused and assets are
re-uploaded with `--clobber`.

## CI

CI (`ci.yml`) does **not** run on every commit — it is manual-only
(`workflow_dispatch`). Run the full gate locally with
`scripts/ci-local.ps1` before tagging a release.
