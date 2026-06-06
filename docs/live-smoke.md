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
5. The review → apply tail closes the loop: `run.diff` reports the
   `pending_review` run as **not yet apply-eligible**, `POST /v1/runs/<id>/review`
   accepts it, `run.diff` then reports it **eligible**, and
   `POST /v1/runs/<id>/apply` reaches `apply_status: "applied"`. For an echo
   Shift this is a safe **no-op** apply (echo writes nothing → 0 changed files),
   so it proves the governed gate + lifecycle terminal without touching the real
   project root. The in-process loop test
   (`starter_crew_closes_the_positive_local_loop_through_prime_start`) now
   asserts the same `done → accept → applied` tail.

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
#   POST /v1/runs/<run_id>/apply                           -> apply_status:"applied"

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
#   POST /v1/agents/<qa agent_id>/approve-hire     -> pending -> active (governed; owner-gated)
#   PATCH /v1/agents/<qa agent_id> {rig:"echo"}    -> the operator configures the new hire's Rig
#   POST /v1/spine/prime/start     {proposal_id}  -> assigned:[<qa track>], started:[{run_id,rig:"echo"}]
```

- `POST /v1/agents/:id/approve-hire` (+ `.../reject-hire`) is the governed
  affordance the Action Center's **"Approve the hire"** item points at — a
  Prime/`route=direct` pending hire carries no spawn Clearance, so it is
  activated here (not via `/v1/approvals/.../decide`). Its boot-policy allow
  rule is `agent_approve_hire` / `agent_reject_hire`.
- A freshly-filed hire has **no Rig**, so it is not runnable until the operator
  configures one (`PATCH /v1/agents/:id {rig}`) — exactly the §12.6
  "switch an Operative's Rig" step; for the safe-local loop that Rig is `echo`.
- The full dependent-unblock tail (every blocking track reviewed to board
  `done` → the `integrate` Brief unblocks and runs) is pinned by the
  in-process test `prime_start_reconciles_a_greenlit_hire_so_dependent_work_unblocks`.

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
