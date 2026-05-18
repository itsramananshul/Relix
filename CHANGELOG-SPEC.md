# Spec Changelog

Per-spec changes to documents under `specs/`. Implementation PRs reference entries here when they pick up a spec change.

Format: dated entries, newest first. Each entry references the spec file(s) and summarizes the change. Substantive amendments link to the originating RFC issue.

---

## 2026-05-18 — Alpha simplifications recorded

- **Added** `specs/alpha-simplifications.md` enumerating the deltas between the alpha implementation and the frozen substrate specs. Every item lists the responsible gate for resolution. This file is the bridge between "what's shipping this week" and "what the substrate target is."

## 2026-05-18 — Substrate freeze imported

- **Added** `specs/RELIX-1` through `specs/RELIX-8` as the substrate freeze captured during the architecture phase. These documents are normative for the production target.
- **Added** `specs/identity-employees.md` (formerly §H of the architecture doc) covering the agents-as-employees identity model.
- **Added** `specs/threat-model.md` (initial draft; expanded at each gate per `SECURITY.md`).
- **Added** `specs/README.md` describing the spec system, governance, and reading order.
