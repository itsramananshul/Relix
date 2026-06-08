# Live positive-Shift HTTP smoke

The repeatable contract for proving a **fresh local user** can boot Relix, log
in through the dashboard path, create the starter crew, and drive an empty
company to a real Shift through live HTTP routes (not just unit tests).

This complements the in-process loop test
(`starter_crew_closes_the_positive_local_loop_through_prime_start`): that test
bypasses the mesh admission pipeline, so it cannot catch a missing **policy
allow rule**. The live smoke does — see the note at the end.

## What it proves

1. The bridge serves the real React dashboard shell at `/dashboard`.
2. First-run admin setup + session-cookie auth works; unauthenticated
   protected `/v1/*` JSON routes return `401`; authenticated read routes
   (`/v1/info`, `/v1/spine/company`, `/v1/adapters`) return real data.
3. `POST /v1/spine/company/starter-crew` is reachable (owner-gated), creates
   the Founder + safe-local **echo** Operatives on the first call and is a
   no-op (no duplicates) on the second.
4. Empty company → `prime.propose` → `prime.approve` → `prime.start` runs at
   least one Brief through the **echo** Rig to a terminal `done` run that lands
   in `pending_review` (company-model §12.6 / §12.5B), visible in the Shift
   Room (`prime.status` + the SSE stream), `/v1/runs`, the Brief Chronicle, and
   the Action Center.
5. The review → apply tail closes the loop **on the board**: `run.diff` reports
   the `pending_review` run as **not yet apply-eligible**, `POST /v1/runs/<id>/review`
   accepts it, `run.diff` then reports it **eligible**, and
   `POST /v1/runs/<id>/apply` reaches `apply_status: "applied"` **and returns
   `brief_status: "done"`** — the clean apply is the operator's review-to-done,
   so it advances the run's Brief from `in_review` to `done` (company-model
   §12.5B/§12.6) and any dependent track (e.g. *integrate*) unblocks on a repeat
   `prime.start`, with **no** separate manual `brief.move done`. For an echo
   Shift the apply is a safe **no-op** on the filesystem (echo writes nothing →
   0 changed files) yet still completes the Brief, so it proves the governed gate
   + lifecycle terminal + board close without touching the real project root. The
   in-process loop test
   (`starter_crew_closes_the_positive_local_loop_through_prime_start`) now
   asserts the same `done → accept → applied → Brief done` tail.

## Provider / chat readiness (the first-release boot smoke, step 4b)

`scripts/smoke-first-release.{ps1,sh}` add one check the broader Shift loop
above does **not** cover: the **AI provider seam** the dashboard's Chat
companion ("Use AI") and Prime "Use AI" ride on (`relix-dashboard-design.md`
§13). The core read routes and the **echo** Rig flow can all be green while the
`ai` peer is down or misconfigured — and then the chat surface dies with
`502 / "ai peer unreachable"`. A green board read hides that whole class.

So the smoke drives **one real `ai.chat` round trip over HTTP** and asserts the
AI peer **answered**:

```
#   POST /v1/spine/companion  {"message":"what needs attention","mode":"ai"}
#         -> 200, ai_mode in {fallback, llm_used}   (the AI peer answered)
```

With `-Provider mock` (zero model spend) the `ai` peer returns a deterministic
reply that does **not** validate as a companion action, so the companion
honestly reports `ai_mode="fallback"` (model answered, choice unusable) and
falls back to the rule-based parser. An **unreachable** `ai` peer instead
reports `ai_mode="unavailable"`. That `fallback`-vs-`unavailable` distinction is
the readiness signal: `chat.provider_ready` PASSes only when the peer answered,
so a dead AI seam **fails** the gate (a bounded ~20s retry tolerates the AI node
coming up a beat after the bridge). The `ai.chat` capability already carries its
boot-policy allow rule in both `relix-mesh-up.ps1` and `relix-mesh-up.sh`, so no
new capability is introduced.

## Isolation (do not touch the operator's real state)

- Point `USERPROFILE` (and `HOME`) at a throwaway dir before boot, so the
  bridge token + `dashboard-admin.json` land there instead of `~/.relix`
  (`resolve_bridge_token_path` falls back to `~/.relix/bridge-token`, and the
  admin record sits next to it).
- Use a unique `-Run <label>` so data/keys/pidfile isolate under
  `dev-data/<label>` and `dev-keys/<label>-*`. The boot regenerates
  `configs/policies/<label>.toml`; delete it after the run (only `dev.toml` /
  `local.toml` are gitignored).
- Use `-Provider mock` and the **echo** Rig: no external/paid CLI is invoked.

## Run it (PowerShell)

```powershell
$env:USERPROFILE = Join-Path $env:TEMP 'relix-smoke-home'
$env:HOME = $env:USERPROFILE
# Boot (blocks; background it, e.g. Start-Job / a separate terminal):
.\scripts\relix-mesh-up.ps1 -Run smoke -Provider mock `
  -BridgePort 19850 -MemPort 19851 -AiPort 19852 -ToolPort 19853 -CoordinatorPort 19854

# Then, against http://127.0.0.1:19850 with a cookie jar ($sess):
#   POST /v1/auth/setup            {username,password}     -> session cookie
#   GET  /v1/spine/company                                 -> initialized:false
#   POST /v1/spine/company/starter-crew  {rig:"echo",roles:"engineer,designer"}
#   POST /v1/spine/prime/propose   {message:"Build ..."}   -> proposal_id
#   POST /v1/spine/prime/approve   {proposal_id}
#   POST /v1/spine/prime/start     {proposal_id}           -> started:[{run_id,rig:"echo",...}]
#   GET  /v1/spine/prime/proposals/<id>/status             -> needs_review after the Shift
#   GET  /v1/runs                                          -> echo run status=done
#   GET  /v1/spine/briefs/<id>/events                      -> run_started + shift_done
#   GET  /v1/spine/company/actions                         -> "Review a completed Shift"
#   GET  /v1/runs/<run_id>/diff                            -> eligible:false (pending_review)
#   POST /v1/runs/<run_id>/review  {decision:"accepted"}   -> accepted
#   GET  /v1/runs/<run_id>/diff                            -> eligible:true (0 changes, echo no-op)
#   POST /v1/runs/<run_id>/apply                           -> apply_status:"applied", brief_status:"done"
#   GET  /v1/spine/briefs/<id>                              -> board column = done (dependents now unblock)

# Tear down (stops ONLY the PIDs this run started) + clean the policy file:
.\scripts\relix-mesh-down.ps1 -Run smoke
Remove-Item configs\policies\smoke.toml -ErrorAction SilentlyContinue
```

Expected first-Shift outcome: two tracks run on echo and reach `done` /
`pending_review`; the dependent "integrate" Brief is correctly **skipped /
blocked** on its dependency.

## Variant: the governed hiring path completes (a missing-role track runs)

Proves the loop does **not stop at a hire** (company-model §12.5B). When a
build plan infers a role with no active Operative (e.g. *qa* from "test
coverage"), `prime.approve` files it as a `pending` hire and leaves that track
unassigned; the operator greenlights the hire and `prime.start` then
**reconciles** the now-active Operative onto its waiting track and runs it.

```
#   POST /v1/spine/company/starter-crew  {rig:"echo",roles:"engineer,designer"}
#   POST /v1/spine/prime/propose   {message:"Build a web app with test coverage"}
#   POST /v1/spine/prime/approve   {proposal_id}  -> hire_requests:[<qa agent_id>]
#   POST /v1/spine/prime/start     {proposal_id}  -> qa track skipped "no Operative ..."
#   POST /v1/agents/<qa agent_id>/approve-hire  {rig:"echo"}
#         -> {runnable:true,rig:"echo",rig_set:true,needs_rig:false} — active AND rigged in one call
#   POST /v1/spine/prime/start     {proposal_id}  -> assigned:[<qa track>], started:[{run_id,rig:"echo"}]
```

- `POST /v1/agents/:id/approve-hire` (+ `.../reject-hire`) is the governed
  affordance the Action Center's **"Approve the hire"** item points at — a
  Prime/`route=direct` pending hire carries no spawn Clearance, so it is
  activated here (not via `/v1/approvals/.../decide`). Its boot-policy allow
  rule is `agent_approve_hire` / `agent_reject_hire`.
- A freshly-filed hire has **no Rig**, so to make it *immediately runnable* the
  approval call now accepts an **optional `{rig}`** body (company-model §12.6):
  the Rig is validated against the known-Rig allowlist and bound **atomically at
  approval**, so the same one call activates *and* rigs the Operative — no
  separate `PATCH /v1/agents/:id {rig}` step. For the safe-local loop that Rig
  is `echo`; `echo` is always accepted, an unknown Rig is refused, and a
  duplicate/conflicting approval never clobbers the bound Rig. Omitting `rig`
  preserves the old behaviour and the response's `needs_rig:true` flags that a
  Rig must still be configured (e.g. `PATCH /v1/agents/:id {rig}`) before the
  Operative can run. The Action Center hire card carries the machine-actionable
  `action_api` (`POST /v1/agents/<id>/approve-hire`) + `suggested_rig:"echo"`.
- The full dependent-unblock tail (every blocking track reviewed to board
  `done` → the `integrate` Brief unblocks and runs) is pinned by the
  in-process test `prime_start_reconciles_a_greenlit_hire_so_dependent_work_unblocks`.

## Variant: the Mandate → Strategy gate → Orchestrate path

Proves the higher-level **company operating model** over live HTTP, not just the
direct Prime-task runner: a Founder/owner creates a **Mandate**, passes it
through the governed **strategy gate**, and **orchestrates** it into the existing
Prime/Brief execution spine — with Mandate linkage preserved end-to-end and one
resulting echo Shift driven to board `done` (company-model §12.5B/§12.6,
execution-and-issue §1.3).

The gate is real: `mandate.orchestrate` **refuses** to materialise anything
until the strategy is approved (`blockers: [{reason: "strategy_not_approved"}]`).
**Existing crew is reused before it is hired (company-model §12.5A/§12.5B):**
because the starter crew already has an active engineer + designer, a team plan
that names those roles **adopts** them (no hire filed) and only a genuinely
**missing** role (e.g. *qa* for "test coverage") is staffed through the governed
hire + echo-Rig affordance. Every orchestrated Brief is stamped with the
Founder/Board reviewer up front, so its completed Shift lands in `in_review`
(not `blocked`) and `run.apply` is the review-to-done.

```
#   POST /v1/auth/setup                  {username,password}            -> session cookie
#   POST /v1/spine/company/starter-crew  {rig:"echo",roles:"engineer,designer"}   (active engineer+designer)
#   POST /v1/spine/mandates              {title,description}            -> mandate_id
#   GET  /v1/spine/mandates/<id>/strategy                               -> status:null
#   POST /v1/spine/mandates/<id>/strategy/propose {doc:"..."}           -> status:"proposed"
#   GET  /v1/spine/company/actions                                      -> approval card target_type:"mandate"
#   POST /v1/spine/mandates/<id>/orchestrate {mode:"assign_ready"}      -> REFUSED: blockers[strategy_not_approved], no Briefs
#   POST /v1/spine/mandates/<id>/strategy/approve                       -> status:"approved"
#   POST /v1/spine/mandates/<id>/team_plan {roles:"engineer:onboard-eng,designer:onboard-design,qa:onboard-qa"}
#         -> adopted:[{engineer},{designer}]  pending_hires:[{<qa agent_id>}]   (starter engineer+designer REUSED, only qa hired)
#   GET  /v1/spine/mandates/<id>/team_readiness                         -> readiness:"staffing"  active_agents:[engineer,designer]  pending_hires:[qa]
#   GET  /v1/spine/company/actions                                      -> exactly one hire card, for the qa agent (none for adopted roles)
#   POST /v1/agents/<qa agent_id>/approve-hire {rig:"echo"}             -> {runnable:true,rig:"echo"}
#   GET  /v1/spine/mandates/<id>/team_readiness                         -> readiness:"ready"  (engineer+designer adopted, qa now active)
#   POST /v1/spine/mandates/<id>/orchestrate {mode:"assign_ready"}      -> parent + role tracks + subject Briefs, assigned to adopted + hired Operatives
#   GET  /v1/spine/company/actions                                      -> mandate strategy card GONE; ready_to_start present
#   GET  /v1/spine/mandates/<id>/briefs                                 -> every Brief carries mandate_id (linkage)
#   POST /v1/spine/briefs/<subject brief>/run                           -> echo run_id, status done -> pending_review
#   GET  /v1/runs/<run_id>/diff                                         -> eligible:false (pending_review)
#   POST /v1/runs/<run_id>/review {decision:"accepted"}                 -> accepted
#   GET  /v1/runs/<run_id>/diff                                         -> eligible:true (0 changes, echo no-op)
#   POST /v1/runs/<run_id>/apply                                        -> apply_status:"applied", brief_status:"done"
#   GET  /v1/spine/mandates/<id>/orchestration/latest                  -> status:"assigned"
```

- The orchestrate Brief tree is three-tier and idempotent (parent →
  role track → subject execution), keyed on stable `mandate:<id>:…` source
  markers — a rerun reuses the tree and never duplicates Briefs. The full
  tier/idempotency/placeholder semantics are pinned by the in-process
  `orchestrate_*` tests; the reviewer-aware tail (a Mandate-orchestrated Shift
  reaching `in_review` so `run.apply` advances it to `done`) is pinned by
  `orchestrate_stamps_founder_reviewer_so_shift_is_review_to_apply_able`.
- All `mandate.*` capabilities used here (`mandate.create`, `mandate.strategy.*`,
  `mandate.team_plan`, `mandate.team_readiness`, `mandate.orchestrate`,
  `mandate.orchestration.latest`) already carry boot-policy allow rules in both
  boot scripts and are in the guard's `$RequiredCapabilities` manifest.

## Caveat that the live smoke caught

Every `/v1/spine/*` capability the bridge forwards is mesh-default-denied unless
the boot policy has a matching `[[rules]]` allow rule. The boot policy is
generated **only** by `scripts/relix-mesh-up.ps1` and `scripts/relix-mesh-up.sh`
(the CLI generates no policy; `relix boot` spawns these). When
`company.starter_crew` shipped, its capability + bridge route + runtime
owner-gate were added but the **allow rule was not**, so the route returned
`deny:default_deny:no allow rule for method company.starter_crew` over real
HTTP while the in-process test stayed green. Fixed by adding the
`spine_company_starter_crew` rule to both boot scripts. When adding a new
product-spine capability, add its allow rule to **both** boot scripts.

### The guard that makes this drift un-shippable

`scripts/check-boot-policy-coverage.ps1` parses the `method = "..."` allow rules
out of **both** boot scripts and fails when:

1. **parity** breaks - a capability is admitted in one script but not the other
   (the class of bug where `.sh` lacked a `run.discard` rule that `.ps1` had); or
2. **coverage** breaks - a capability in its maintained manifest of live
   product/bridge routes is missing from one or both scripts (the
   "added to neither" case, e.g. `company.starter_crew`, `agent.assign_check`).

A parser sanity floor fails loudly if the policy block is renamed/moved so the
guard can never pass vacuously. It runs locally as the first gate in
`scripts/ci-local.ps1` and as the `boot-policy coverage` job in CI
(`.github/workflows/ci.yml`). **When you add a live route that calls a new mesh
capability, add its `[[rules]]` allow entry to both `relix-mesh-up.ps1` and
`relix-mesh-up.sh`** (and, for a product/spine route, to the guard's
`$RequiredCapabilities` manifest) - the guard catches it if you miss either.
