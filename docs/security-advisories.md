# Security Advisories — Tracked

Per-advisory record for items surfaced by `cargo audit` / `cargo deny check advisories`. Every entry the project chooses to accept (transitive, unreachable, or no fix) has a row here AND a corresponding `ignore` entry in `deny.toml` with a back-reference. No silent suppressions.

Severity ratings reflect the RustSec advisory metadata; reachability ratings are Relix-specific assessments.

## Active

### RUSTSEC-2026-0119 — `hickory-proto` CPU exhaustion via O(n²) name compression

- **Source:** [https://rustsec.org/advisories/RUSTSEC-2026-0119](https://rustsec.org/advisories/RUSTSEC-2026-0119)
- **Direct or transitive?** Transitive.
- **Chain:** `relix-runtime → libp2p 0.54 → libp2p-dns 0.42 → hickory-resolver 0.24 → hickory-proto 0.24.4`.
- **Vulnerable functionality:** DNS name decompression on crafted DNS responses.
- **Reachable in Relix alpha?** **Not in the demonstrated path.** The alpha M5 flow uses `/ip4/.../tcp/<port>` multiaddrs directly; no DNS resolution is performed. `libp2p-dns` is compiled in because it is enabled by the `dns` libp2p feature (inherited from OpenPrem reuse), but it is not exercised on the demo path.
- **Severity in context:** Low while alpha bootstrap uses static multiaddrs. Becomes Medium once Kademlia + bootstrap-list DNS lookups are wired (post-alpha).
- **Mitigation plan:** Upgrade to `hickory-proto >= 0.26.1` when `libp2p` cuts a release that pulls it. Track upstream: [hickory-dns/hickory-dns#GHSA-q2qq-hmj6-3wpp](https://github.com/hickory-dns/hickory-dns/security/advisories/GHSA-q2qq-hmj6-3wpp).
- **Current action:** Accepted as transitive-unreachable. Listed in `deny.toml [advisories] ignore` with this entry's ID as the comment.
- **Review condition:** Re-evaluate when (a) libp2p ships a fix-bearing release, or (b) Relix wires DNS-resolved peers, whichever comes first.

### RUSTSEC-2024-0436 — `paste` unmaintained

- **Source:** [https://rustsec.org/advisories/RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436)
- **Direct or transitive?** Transitive.
- **Chain:** `relix-runtime → libp2p 0.54 → libp2p-tcp 0.42 → if-watch 3.2 → netlink-packet-core 0.8 → paste 1.0.15` (plus parallel chains via `netlink-packet-route` and `netlink-proto`).
- **Vulnerable functionality:** None — this is an **unmaintained** flag, not a vulnerability. The crate author archived the repo and recommends `pastey` or `with_builtin_macros`.
- **Reachable in Relix alpha?** N/A (no exploit; macro-only crate).
- **Severity in context:** Informational. No security impact in the alpha.
- **Mitigation plan:** Wait for the libp2p / `if-watch` ecosystem to migrate to `pastey`. Forking `paste` ourselves is disproportionate.
- **Current action:** Accepted. Listed in `deny.toml [advisories] ignore` with this entry's ID as the comment.
- **Review condition:** Remove the ignore when `if-watch` (or its upstream chain) drops the `paste` dependency.

## How to add a new advisory exception

1. Run `cargo deny check advisories` locally and confirm the failing advisory.
2. Run `cargo tree -i <crate>` to confirm the dependency chain.
3. Assess reachability: is the vulnerable function used? Is the relevant feature compiled? Is the network path exposed?
4. Decide:
   - **Fix:** bump the dependency or replace it. No ignore needed.
   - **Accept transitive-unreachable:** add an entry here and an `ignore` line in `deny.toml` with the advisory ID as the comment.
5. Open a PR with both files updated in the same commit (per CONTRIBUTING.md "Documentation Is Part of the Change").
6. The PR description names the review condition and a target removal date if known.

## Removed exceptions

(none yet)
