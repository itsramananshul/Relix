# The Relix Lexicon — our names for everything

> **Status:** Canonical. This is the single source of truth for what Relix's concepts are *called*. The product (dashboard labels, docs, API names for new things, product copy) speaks this lexicon everywhere. Where an older doc still uses a borrowed word (Issue, CEO, Inbox…), the **Relix name here wins** — reconcile the doc to this, not the reverse.
>
> **The world (so the names cohere):** Relix is a secure **Guild** of **Operatives** — owned by the **Founder**, led by the **Prime** — who work **Briefs** in **Shifts** at their **Bench**, hold **Keys**, escalate up the **Line**, sharpen their **Tradecraft** (kept tidy by the **Keeper**), and can run on any **Rig** tethered into the mesh. It reads like an elite secure agency — which is exactly Relix's DNA (a signed mesh running a company of agents).

---

## Two layers (important — this keeps the rename cheap, not a rewrite)

1. **Product layer** — dashboard labels, docs, product copy, and **API/handler names for new things** use the lexicon directly. This is what the Founder and users see.
2. **Internal code** — existing identifiers (`agent`, `agent_profiles`, `tasks`, `reports_to`, …) keep their working names to avoid churning a 313K-line codebase; this table is the map. **Net-new code adopts the lexicon directly** (e.g. the spine store ships as `Mandate`/`Campaign`, the adapter layer ships as `Rig`).

So: the `reports_to` column stays `reports_to` in SQL, but in the product it is an Operative's **Lead**. Same power, our word.

---

## The map

### Work spine
| Concept | Old / borrowed | **Relix name** | Internal identifier (code) |
|---|---|---|---|
| The org / tenant | Company | **Guild** | `tenant` (existing) |
| The durable "why" | Goal / Initiative | **Mandate** | `Mandate` (net-new) |
| A workstream | Project | **Campaign** | `Campaign` (net-new) |
| The atom of work **+ its conversation** | Issue | **Brief** | `task` / `tasks` (existing ledger) |
| A child of a Brief | Sub-issue | **Sub-brief** *(a.k.a.* **Sliver***)* | `task_edges` type `spawned` |
| One working episode | Run | **Shift** | `task_attempts` (existing) |
| A dependency | Blocker | **Snag** | `task_edges` type `blocked_on` |
| A durable artifact (plan/design/deliverable) | Document | **Dossier** | net-new `task_documents` |
| The conversation on a Brief | Comment thread | *the Brief's thread* | `task_events` (existing) |

### People & org
| Concept | Old | **Relix name** | Internal |
|---|---|---|---|
| An AI employee | Agent | **Operative** | `agent` / `agent_profiles` |
| The apex AI agent | CEO | **Prime** | role `prime` |
| The human owner (you) | Board | **Founder** | `operator` identity |
| An Operative's boss | reports_to / manager | **Lead** | `reports_to` column |
| The people view | Agent list | **The Roster** | — |
| The hierarchy view | Org chart | **The Lattice** | — |
| Everyone under a manager | Manager subtree | **Branch** | `manager_subtree()` |
| The escalation path up | Chain of command | **The Line** | `chain_of_command()` |

### Governance
| Concept | Old | **Relix name** | Internal |
|---|---|---|---|
| The action center ("what needs you") | Inbox | **The Desk** | — |
| An approval gate | Approval | **Clearance** (you **greenlight** it) | `approval_requests` |
| Per-agent powers | Permissions | **Keys** | the agent gate / categories |
| Spend cap | Budget | **Allowance** | budget enforcer |
| The immutable record | Audit / activity | **Chronicle** | hash-chained audit |

### Execution & Relix-native
| Concept | Old | **Relix name** | Internal |
|---|---|---|---|
| The event that starts work | Heartbeat / wake | **Pulse** | — |
| Taking exclusive ownership of a Brief | Checkout | **Claim** | — |
| Where work runs | Execution workspace | **Bench** | — |
| The security sandbox box | The box / sandbox | **Cell** | tool-node jail |
| What powers an Operative (the agent backend) | Adapter | **Rig** | net-new |
| The plugin that makes a plugged-in agent Relix-native | relix-bridge | **Tether** | net-new |
| One script that runs N tool-steps in one cheap turn | execute_code | **Macro** | net-new |

### Pillar-1 powers (native to Relix)
| Concept | Old | **Relix name** | Internal |
|---|---|---|---|
| The self-improvement loop | Learning loop | **Tradecraft** | net-new |
| The skill-library janitor | Curator | **The Keeper** | net-new |
| A learned skill | Skill | **Knack** *(or keep "Skill")* | `skill` store |
| Long-term recall | Memory | **Memory** (unchanged) | memory node |

---

## Usage rules
- **New surface → new word.** Any new dashboard view, API method, doc, or net-new module names its concepts with the lexicon (e.g. `mandate.create`, the `Rig` trait, the **Desk** view).
- **Old internals → keep + map.** Don't rename existing tables/structs just for vocabulary; map them here. Rename only when a file is already being substantially rewritten.
- **One word per concept.** No synonyms drifting in. If a new concept appears, add it here first.

---

*This lexicon is the brand layer of [`relix-company-model.md`](relix-company-model.md), [`relix-agent-adapters.md`](relix-agent-adapters.md), and [`relix-hermes-integration.md`](relix-hermes-integration.md). When those docs say "Issue / CEO / Inbox / Adapter," read "Brief / Prime / The Desk / Rig."*
