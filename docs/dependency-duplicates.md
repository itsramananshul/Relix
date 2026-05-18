# Dependency Duplicates

`cargo deny check bans` flags any crate present at more than one version. The duplicates below are the ones currently in the lockfile. They are categorized: **ecosystem-split** (harmless and ubiquitous), **worth deduping** (would require minor effort to fix), and **libp2p-bound** (we cannot change without an upstream release).

Policy: don't destabilize the dependency graph to silence duplicate warnings. Reduce duplicates opportunistically when an unrelated upgrade happens to converge them.

## ecosystem-split (no action)

| Crate           | Versions       | Why duplicated                                                    |
|-----------------|----------------|-------------------------------------------------------------------|
| `rand`          | 0.8, 0.9       | Ecosystem mid-migration. Pinning one breaks transitive consumers. |
| `rand_chacha`   | 0.3, 0.9       | Same as above.                                                    |
| `getrandom`     | 0.2, 0.3       | Same as above.                                                    |
| `thiserror`     | 1.x, 2.x       | Many crates still on 1.x.                                         |
| `socket2`       | 0.4, 0.5       | Same as above; libp2p transitive.                                 |
| `windows-sys`   | 0.x (multiple) | Common on Windows targets; deeply transitive.                     |

## libp2p-bound (cannot fix without upstream change)

| Crate    | Versions     | Path                                                            |
|----------|--------------|-----------------------------------------------------------------|
| `yamux`  | 0.12, 0.13   | `libp2p-yamux 0.46` pulls both via internal API selection.      |

## Worth deduping (revisit at next major bump)

(none today — opportunistic candidates would appear when libp2p 0.55+ lands.)

## Review cadence

Re-check this file (and re-run `cargo deny check bans`) at every milestone push. If a duplicate moves from `ecosystem-split` to genuinely-fixable (e.g., both transitives now expose a way to pin), it migrates here and gets actioned.

Removing a duplicate entry from this file requires running `cargo deny check bans` clean for that crate.
