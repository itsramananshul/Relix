import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { ApiError, api, primeDriver, subscribeCompanyActions, tryGet, tryGetReport, type PrimeNextStep } from "../api";
import { Badge, extractList, useAsync } from "../components/common";
import { HealthPanel } from "../components/HealthPanel";
import { invalidate, useInvalidate } from "../invalidate";

// The board summary arrives as an object keyed by board status, e.g.
// `{ "backlog": 1, "todo": 2, "total": 3 }`.
type BoardSummary = Record<string, number>;
interface Card { task_id?: string; id?: string; title?: string; board_status?: string; priority?: string }
interface Inbox { blocked?: Card[]; overdue?: Card[]; unassigned?: Card[]; review?: Card[]; stale?: Card[] }
interface Roster { active?: number; total?: number }
interface EventRow { task_id?: string; event_type?: string; ts?: number; payload?: string }
interface Founder { name?: string; rig?: string | null }
// The read-only operations summary embedded in `company.status` (company-model
// §5.4/§8.2; dashboard-design §5). Tenant-scoped, derived only from existing
// stores; every bucket is an honest count (or 0) — never a fabricated figure.
interface Operations {
  briefs?: {
    total?: number;
    by_board?: Record<string, number>;
    in_review?: number;
    ready_to_start?: number;
    unassigned?: number;
    blocked?: number;
    stale?: number;
  };
  runs?: { window?: number; recent?: number; running?: number; failed_or_refused?: number; pending_review?: number };
  approvals?: { pending_clearances?: number; pending_hires?: number };
  mandates?: { total?: number; by_status?: Record<string, number>; strategy_proposed?: number };
}
interface CompanyStatus {
  initialized?: boolean;
  founder?: Founder | null;
  prime?: Founder | null;
  operative_count?: number;
  crew?: { total?: number; active?: number; pending?: number };
  // Present when the bridge has the spine + task stores wired (always, live);
  // absent only on the agent-only fallback read — render an honest unavailable.
  operations?: Operations | null;
}
interface Adapter { name?: string; probe?: { status?: string } }
interface RunRow {
  run_id?: string;
  brief_id?: string;
  status?: string;
  trigger?: string;
  rig?: string;
  started_at?: number;
  review?: string;
}
interface RunConfig {
  context?: string;
  project_root?: string;
  inherit?: boolean;
  heartbeat_enabled?: boolean;
}
interface MaintSummary {
  workspace?: { count?: number; total_bytes?: number };
  warnings?: { level?: string; message?: string }[];
}
interface MandateRow { mandate_id?: string; id?: string; title?: string; name?: string; status?: string }
// Compact live Prime-session view (GET /v1/spine/prime/proposals/:id/status).
interface ProposalRow { proposal_id?: string; status?: string; mandate_title?: string | null }
interface SessionCounts {
  total_briefs?: number; running?: number; done?: number; blocked?: number;
  needs_review?: number; refused?: number; failed?: number; ready?: number; unassigned?: number;
}
interface SessionStatus {
  proposal_id?: string;
  status?: string;
  mandate_title?: string | null;
  counts?: SessionCounts;
  recommended_next_actions?: string[];
}
// Action Center (GET /v1/spine/company/actions) — the operator's next-actions
// feed computed from live state. Read-only; each item links to its existing
// action route.
interface ActionItem {
  id?: string;
  category?: string;
  severity?: string;
  title?: string;
  reason?: string;
  target_type?: string;
  target_id?: string;
  target_title?: string;
  action_label?: string;
  route?: string;
  // A machine-actionable endpoint the client can POST to directly (vs. the
  // human `route`). Today only the `hire` card sets it
  // (`POST /v1/agents/:id/approve-hire`), so the Inbox can approve inline.
  action_api?: string;
  // The safe-local Rig to pass when acting on this item (the `hire` card
  // suggests `echo` so the approved Operative is immediately runnable).
  suggested_rig?: string;
  // Guarded retry target — set on a `failed_or_refused` card ONLY when the
  // source run is retry-eligible (retryable + budget + no existing retry child).
  // Pairs with `action_api` = `POST /v1/runs/<run_id>/retry`, the already-
  // implemented guarded route, so the Action Center can open one guarded retry
  // directly. Absent when not safely retryable from here.
  run_id?: string;
}
interface CompanyActions {
  actions?: ActionItem[];
  counts?: { total?: number; by_category?: Record<string, number>; by_severity?: Record<string, number> };
  truncated?: boolean;
}

const COLUMNS = ["backlog", "todo", "in_progress", "in_review", "done"];
const RUN_TONE: Record<string, string> = {
  running: "in_progress",
  done: "done",
  failed: "blocked",
  cancelled: "blocked",
  refused: "blocked",
  interrupted: "blocked",
  continued: "todo",
};

interface Warn {
  tone: "err" | "info";
  msg: string;
  to?: string;
  cta?: string;
}

// Which work object the cockpit's guided-driver next step was computed for, so
// "Advance one step" hits the matching route (proposal vs Mandate twin).
type NextStepRef = { kind: "proposal" | "mandate"; id: string };

// Best-effort wrapper for an optional driver read: a failure degrades to null
// (so a missing/unavailable next step never blanks the Overview), exactly like
// `tryGet` does for path GETs.
async function bestEffort<T>(fn: () => Promise<T>): Promise<T | null> {
  try {
    return await fn();
  } catch {
    return null;
  }
}

export function Overview() {
  const { data, loading, reload } = useAsync(async () => {
    // The board + company are the CORE of the Command Center — if they fail
    // we must say so (not show a blank board). Optional surfaces stay on
    // `tryGet` so one slow panel doesn't blank the page.
    const [boardR, companyR, runsR] = await Promise.all([
      tryGetReport<BoardSummary>("/v1/spine/board", {}),
      tryGetReport<CompanyStatus>("/v1/spine/company", {}),
      tryGetReport<RunRow[]>("/v1/runs", []),
    ]);
    const [inbox, roster, adapters, runCfg, maint, events, actions] = await Promise.all([
      tryGet<Inbox>("/v1/spine/inbox?limit=50", {}),
      tryGet<Roster>("/v1/spine/roster", {}),
      tryGet<Adapter[]>("/v1/adapters", []),
      tryGet<RunConfig>("/v1/spine/run-config", {}),
      tryGet<MaintSummary | null>("/v1/maintenance/summary", null),
      tryGet<unknown>("/v1/tasks/events/recent?limit=10", {}),
      tryGet<CompanyActions | null>("/v1/spine/company/actions", null),
    ]);
    const mandates = await tryGet<unknown>("/v1/spine/mandates?limit=8", {});
    const mandateRows = extractList<MandateRow>(mandates, ["mandates"]);
    // The newest Prime work session — if it's approved, pull its live Shift-Room
    // status for the compact "Active work" card (best-effort, optional surface).
    const proposals = await tryGet<ProposalRow[]>("/v1/spine/prime/proposals?limit=1", []);
    const latestProposal = Array.isArray(proposals) ? proposals[0] : undefined;
    const session =
      latestProposal?.status === "approved" && latestProposal.proposal_id
        ? await tryGet<SessionStatus | null>(
            `/v1/spine/prime/proposals/${latestProposal.proposal_id}/status`,
            null,
          )
        : null;
    // Cockpit guided-driver next step (dashboard-design §5; roadmap §2 slice 9b).
    // Prefer the latest Prime proposal — the driver classifies its whole
    // lifecycle (approval → strategy → team plan → orchestrate → board). If
    // there's no proposal but a Mandate exists, use the Mandate twin route. Both
    // are best-effort: a failure degrades to null and never blanks the Overview.
    let nextStep: PrimeNextStep | null = null;
    let nextStepRef: NextStepRef | null = null;
    if (latestProposal?.proposal_id) {
      nextStep = await bestEffort(() => primeDriver.nextStep(latestProposal.proposal_id!));
      if (nextStep) nextStepRef = { kind: "proposal", id: latestProposal.proposal_id };
    }
    if (!nextStep) {
      const mid = mandateRows[0]?.mandate_id ?? mandateRows[0]?.id;
      if (mid) {
        nextStep = await bestEffort(() => primeDriver.mandateNextStep(mid));
        if (nextStep) nextStepRef = { kind: "mandate", id: mid };
      }
    }
    const coreError =
      boardR.error || companyR.error || runsR.error
        ? (boardR.error ?? companyR.error ?? runsR.error)
        : null;
    return {
      board: boardR.data,
      inbox,
      roster,
      company: companyR.data ?? {},
      adapters: Array.isArray(adapters) ? adapters : [],
      runs: Array.isArray(runsR.data) ? runsR.data : [],
      runCfg: runCfg ?? {},
      maint: maint ?? null,
      mandates: mandateRows,
      events: extractList<EventRow>(events),
      session: session ?? null,
      actions: actions ?? null,
      nextStep,
      nextStepRef,
      coreError,
    };
  }, []);

  // Keep the Action Center less stale (company-model §8.2; dashboard §5) WITHOUT
  // a new event bus: subscribe to the EXISTING run-event SSE as a cheap
  // change-trigger and fall back to a low-frequency poll so approval/hire/prime
  // changes still converge and the surface stays fresh if the stream is absent.
  // This refreshes ONLY the Action Center feed — it never touches the page's
  // load state and only updates on a SUCCESSFUL fetch, so a transient blip can
  // never blank it. The rest of the Overview stays a mount-load snapshot.
  const [liveActions, setLiveActions] = useState<CompanyActions | null>(null);
  // Latch the first-run on-ramp open once the operator starts it: a successful
  // run reloads the page (so the counters refresh) which would otherwise flip
  // `isFresh` false and unmount the panel before its result + deep links are
  // seen. Latched state survives the reload; a fresh mount with real work never
  // sets it, so the panel still disappears for an established company.
  const [onRampLatched, setOnRampLatched] = useState(false);
  // Refetch ONLY the Action Center feed (success-only → never clobber with
  // null, so a transient blip can't blank it). Shared by the SSE/poll effect
  // below AND the inline Approve/Reject handlers, so acting on a hire updates
  // the feed immediately.
  const refreshActions = useCallback(async () => {
    const a = await tryGet<CompanyActions | null>("/v1/spine/company/actions", null);
    if (a) setLiveActions(a);
  }, []);
  useEffect(() => {
    // Prefer the dedicated Action Center snapshot stream: it pushes the full
    // feed on every change (fingerprint-gated server-side), so the Command
    // Center updates without re-fetching. Success-only set → a transient blip
    // can't blank it. The bounded poll stays as the convergence fallback for
    // when the stream never connects, so we never lose the existing behavior.
    // onConn is required by the API but no badge is surfaced here; ignore it.
    const unsub = subscribeCompanyActions(
      (feed) => { if (feed) setLiveActions(feed); },
      () => {},
    );
    const poll = setInterval(refreshActions, 20000); // convergence fallback (bounded)
    return () => {
      clearInterval(poll);
      unsub();
    };
  }, [refreshActions]);
  // Client invalidation bus (dashboard-design §11): the EXISTING run-event SSE
  // (above) covers run-lifecycle change-triggers; the bus covers the NON-run
  // mutations the operator performs elsewhere in the app — assign, create,
  // hire, interaction/suggestion answers, orchestration — so the Action Center
  // feed converges on them without waiting for the 20s poll. Refreshes ONLY the
  // feed (success-only), never the page's load state.
  useInvalidate(["actions", "briefs", "mandates"], refreshActions);

  const board = data?.board ?? {};
  const inbox = data?.inbox ?? {};
  const company = data?.company ?? {};
  const adapters = data?.adapters ?? [];
  const runs = data?.runs ?? [];
  const runCfg = data?.runCfg ?? {};

  const active = (board.todo ?? 0) + (board.in_progress ?? 0) + (board.in_review ?? 0);
  const done = board.done ?? 0;
  const totalBriefs = COLUMNS.reduce((n, c) => n + (board[c] ?? 0), 0);
  const attention =
    (inbox.blocked?.length ?? 0) + (inbox.overdue?.length ?? 0) + (inbox.unassigned?.length ?? 0);
  const crew = data?.roster?.active ?? data?.roster?.total ?? company.operative_count ?? 0;
  const availAdapters = adapters.filter((a) => a.probe?.status === "available");
  const initialized = company.initialized ?? crew > 0;
  const running = runs.filter((r) => r.status === "running").length;
  const inReview = board.in_review ?? 0;
  // First-run on-ramp gate (roadmap §6 DoD "Time-to-first-success … discoverable
  // in the UI"): the company is initialized but has done no work yet — no Briefs
  // and no Mandates. Guarded on a clean core read so a down coordinator never
  // masquerades as a fresh company. Latches open once started so the result
  // survives the post-run reload.
  const isFresh =
    initialized && !data?.coreError && totalBriefs === 0 && (data?.mandates?.length ?? 0) === 0;
  const showOnRamp = initialized && !data?.coreError && (isFresh || onRampLatched);

  // System warnings — actionable, ranked. Each can carry a "next action".
  const warnings: Warn[] = [];
  if (loading) {
    // no warnings while still loading
  } else {
    if (!availAdapters.length) {
      warnings.push({
        tone: "info",
        msg:
          "No agent adapter is available — Briefs can be created and assigned, but a Run needs an installed + authenticated coding agent. (echo always works for testing.)",
        to: "/settings",
        cta: "Open Settings",
      });
    }
    if (showOnRamp) {
      // The first-run on-ramp panel supersedes the generic "no Mandates / no
      // Briefs yet" nudges — don't double them up as warnings (design §5: one
      // scannable card, not a tower of banners).
    } else if (initialized && (data?.mandates?.length ?? 0) === 0 && totalBriefs === 0) {
      warnings.push({ tone: "info", msg: "No Mandates yet — turn a big goal into a Brief tree, or create Briefs by hand.", to: "/mandates", cta: "Create a Mandate" });
    } else if (initialized && totalBriefs === 0) {
      warnings.push({ tone: "info", msg: "No Briefs yet — create your first unit of work.", to: "/briefs", cta: "Create a Brief" });
    }
    if ((inbox.unassigned?.length ?? 0) > 0) {
      warnings.push({
        tone: "info",
        msg: `${inbox.unassigned!.length} Brief(s) are unassigned — assign an Operative so they can run.`,
        to: "/briefs",
        cta: "Assign work",
      });
    }
    if ((inbox.blocked?.length ?? 0) > 0) {
      warnings.push({ tone: "err", msg: `${inbox.blocked!.length} Brief(s) are blocked — review why and unblock them.`, to: "/runs", cta: "Inspect runs" });
    }
    if (runCfg.inherit) {
      warnings.push({
        tone: "err",
        msg: "Runs are in INHERIT mode — they execute in the coordinator working directory, not a scoped sandbox. This is unsafe; prefer empty/copy_repo.",
        to: "/settings",
        cta: "Review runtime",
      });
    }
    if (runCfg.context === "copy_repo" && !runCfg.project_root) {
      warnings.push({ tone: "err", msg: "copy_repo context is set but no project root is configured — set RELIX_RUN_PROJECT_ROOT.", to: "/settings", cta: "Review runtime" });
    }
    // Storage/maintenance warnings (dedupe inherit/project-root already above).
    const maint = data?.maint ?? null;
    if (maint) {
      for (const w of maint.warnings ?? []) {
        const m = w.message ?? "";
        if (/inherit|project root/i.test(m)) continue;
        warnings.push({ tone: w.level === "error" ? "err" : "info", msg: m, to: "/settings", cta: "Maintenance" });
      }
    } else {
      warnings.push({ tone: "info", msg: "Maintenance summary unavailable — storage usage can't be checked right now.", to: "/settings", cta: "Settings" });
    }
  }

  // First-run: no Founder yet. The single most important next action.
  // Guard: only treat "no company" as first-run when the core reads actually
  // SUCCEEDED — otherwise a down coordinator would masquerade as first-run.
  if (!loading && !initialized && !data?.coreError) {
    return (
      <div className="grid">
        <HealthPanel compact />
        <div className="card setup-card">
          <div className="setup-step">Step 1 of 2 · First-run setup</div>
          <h2 style={{ margin: "4px 0 8px" }}>Welcome to Relix</h2>
          <p className="muted" style={{ maxWidth: 560 }}>
            Relix is your company operating system: you create <strong>Briefs</strong> (units of work),
            assign them to <strong>Operatives</strong> (your crew), and run them through a coding-agent
            <strong> adapter</strong> in a safe, scoped sandbox — then review and apply the result.
          </p>
          <p className="muted" style={{ maxWidth: 560 }}>
            To begin, initialize your company by creating the <strong>Founder</strong> — the first
            Operative who can own and run work, and hire the rest of the team.
          </p>
          <p className="muted" style={{ maxWidth: 560 }}>
            In a hurry? On the Crew page you can <strong>Set up a starter crew</strong> — the Founder
            plus a couple of safe, local <em>echo</em> Operatives — so you can Ask Prime to plan and
            run a real Shift end-to-end without installing any external coding agent.
          </p>
          <div className="row" style={{ marginTop: 14 }}>
            <Link to="/agents"><button className="btn">Initialize company →</button></Link>
            <span className="muted" style={{ fontSize: 12 }}>
              {availAdapters.length
                ? `${availAdapters.length} adapter(s) ready`
                : "echo adapter works out of the box"}
            </span>
          </div>
        </div>
        <div className="card">
          <h3>What you'll do next</h3>
          <ol className="next-steps">
            <li>Initialize the company (create the Founder), or set up a starter crew to skip ahead.</li>
            <li>Ask Prime to plan, or create a Brief and assign it to an Operative.</li>
            <li>Run it — Relix executes in a scoped sandbox and records a transcript.</li>
            <li>Review the changed files, then accept &amp; apply them.</li>
          </ol>
        </div>
      </div>
    );
  }

  return (
    <div className="grid">
      {/* Command strip — who's running + the live counters + start-work, before
          any banners, so the Overview opens like a cockpit (design §2/§3). */}
      {initialized && (
        <div className="cmd-strip">
          <div className="who-band">
            <span className="title">{company.founder?.name ? `${company.founder.name}'s Guild` : "Your Guild"}</span>
            <div className="meta">
              <span>Founder {company.founder?.name ?? "—"}</span>
              <span>Prime {company.prime?.name ?? "not hired"}</span>
              <span>{crew} Operative{crew === 1 ? "" : "s"}</span>
              <span>{availAdapters.length}/{adapters.length} adapters ready</span>
            </div>
          </div>
          <div className="counters">
            <Link to="/briefs" className="counter" title={`${totalBriefs} Briefs total`}>
              <b className={active ? "info" : ""}>{active}</b><span>Active Briefs</span>
            </Link>
            <Link to="/runs" className="counter">
              <b className={running ? "info" : ""}>{running}</b><span>Running now</span>
            </Link>
            <Link to="/runs" className="counter" title={`${inReview} run(s) awaiting review → apply`}>
              <b className={inReview ? "info" : ""}>{inReview}</b><span>In review</span>
            </Link>
            <Link to="/runs" className="counter">
              <b className={attention ? "warn" : ""}>{attention}</b><span>Needs attention</span>
            </Link>
            <div className="counter"><b>{done}</b><span>Completed</span></div>
          </div>
          <div className="grow" />
          <div className="cta">
            <Link to="/chat"><button className="btn">Plan with Prime →</button></Link>
            <span className="hint">Describe a goal → governed plan</span>
          </div>
        </div>
      )}
      {/* Live system health — only loud when a layer is down. */}
      <HealthPanel compact />
      {data?.coreError && (
        <div className="banner err banner-action">
          <span>Some Command Center data failed to load: {data.coreError}</span>
          <span className="banner-cta" onClick={reload} style={{ cursor: "pointer" }}>Retry →</span>
        </div>
      )}
      {/* First-run on-ramp — the smallest discoverable path to a positive
          Shift for a fresh, initialized company (roadmap §6 DoD; company-model
          §12.6). One click provisions a safe-local echo crew, creates + assigns
          a first Brief, and runs it end-to-end through the built-in echo Rig. */}
      {showOnRamp && (
        <FirstRunOnRamp
          onStarted={() => setOnRampLatched(true)}
          onChanged={() => { void refreshActions(); reload(); }}
        />
      )}
      {/* Company operating status — the cockpit: Prime's ONE next safe step
          (guided driver) + a live pressure strip, so the operator knows what to
          do next without reading internal routes (dashboard-design §5). */}
      {initialized && (
        <OperatingStatus
          nextStep={data?.nextStep ?? null}
          nextStepRef={data?.nextStepRef ?? null}
          actions={liveActions ?? data?.actions ?? null}
          onReload={() => { void refreshActions(); reload(); }}
        />
      )}
      {/* Operations snapshot — a glance summary of the whole company's work,
          straight from `company.operations` (server-computed, tenant-scoped),
          so the cockpit reads as one coherent snapshot instead of stitched
          panels (dashboard-design §5). Read-only; each stat deep-links to where
          it's worked. */}
      {initialized && <OperationsSnapshot ops={company.operations} />}
      {/* Action Center — the one place for what needs the operator now. Prefers
          the live-refreshed feed, falling back to the mount-load snapshot. */}
      {initialized && (
        <ActionCenter
          data={liveActions ?? data?.actions ?? null}
          loading={loading}
          onActed={() => { void refreshActions(); reload(); }}
        />
      )}
      {/* Active work — the latest Prime session's live Shift Room, compact. */}
      {data?.session && <ActiveWork session={data.session} />}
      {/* Setup & warnings — one scannable card, not a tower of banners. */}
      {warnings.length > 0 && (
        <div className="card">
          <h3>Setup &amp; warnings</h3>
          <div className="warn-list">
            {warnings.map((w, i) => (
              <div key={i} className="warn-row">
                <span className={"dot " + w.tone} />
                <span className="msg">{w.msg}</span>
                {w.to && <Link to={w.to} className="link" style={{ whiteSpace: "nowrap" }}>{w.cta ?? "Open"} →</Link>}
              </div>
            ))}
          </div>
        </div>
      )}

      <div className="grid cols-2">
        {/* Company + runtime snapshot */}
        <div className="card">
          <h3>Company &amp; runtime</h3>
          <div className="kv">
            <span className="muted">Founder</span>
            <span>
              {company.founder?.name ?? "—"}
              {company.founder?.rig && <span className="mono" style={{ marginLeft: 6 }}>{company.founder.rig}</span>}
            </span>
          </div>
          <div className="kv">
            <span className="muted">Prime</span>
            <span>
              {company.prime?.name ?? <span className="muted">not hired yet</span>}
              {company.prime?.rig && <span className="mono" style={{ marginLeft: 6 }}>{company.prime.rig}</span>}
            </span>
          </div>
          <div className="kv">
            <span className="muted">Crew</span>
            <span>
              {crew} Operative{crew === 1 ? "" : "s"}
              {(company.crew?.pending ?? 0) > 0 && <span className="badge backlog" style={{ fontSize: 9, marginLeft: 6 }}>{company.crew!.pending} pending</span>}
              {" · "}<Link to="/agents" className="link">manage</Link>
            </span>
          </div>
          <div className="kv">
            <span className="muted">Adapters</span>
            <span>
              <span className={"badge " + (availAdapters.length ? "done" : "blocked")}>
                {availAdapters.length}/{adapters.length} available
              </span>
              <Link to="/settings" className="link" style={{ marginLeft: 8 }}>configure</Link>
            </span>
          </div>
          <div className="kv">
            <span className="muted">Run sandbox</span>
            <span>
              <span className={"badge " + (runCfg.inherit ? "blocked" : "done")}>
                {runCfg.inherit ? "inherit (unsafe)" : (runCfg.context ?? "empty")}
              </span>
            </span>
          </div>
          <div className="kv">
            <span className="muted">Autonomous (heartbeat)</span>
            <span>
              <span className={"badge " + (runCfg.heartbeat_enabled ? "done" : "backlog")}>
                {runCfg.heartbeat_enabled ? "on" : "off"}
              </span>
              <span className="muted" style={{ marginLeft: 8, fontSize: 12 }}>
                {runCfg.heartbeat_enabled ? "ready Briefs auto-run on a timer" : "runs are operator-triggered"}
              </span>
            </span>
          </div>
        </div>

        {/* Latest runs */}
        <div className="card">
          <div className="row" style={{ marginBottom: 10 }}>
            <h3 style={{ margin: 0 }}>Latest runs</h3>
            <div className="spacer" style={{ flex: 1 }} />
            <Link to="/runs" className="link">all runs →</Link>
          </div>
          {runs.length === 0 ? (
            <div className="empty">No runs yet — assign a Brief and hit Run.</div>
          ) : (
            <table className="table compact">
              <tbody>
                {runs.slice(0, 6).map((r, i) => (
                  <tr key={r.run_id ?? i}>
                    <td><span className={"badge " + (RUN_TONE[r.status ?? ""] ?? "todo")}>{r.status ?? "—"}</span></td>
                    <td className="muted" style={{ fontSize: 11 }}>{r.trigger === "heartbeat" ? "auto" : r.trigger ?? "manual"}</td>
                    <td className="mono">{(r.brief_id ?? "").slice(0, 10)}</td>
                    <td className="muted">{r.rig || "—"}</td>
                    <td className="muted" style={{ fontSize: 11 }}>{r.started_at ? new Date(r.started_at * 1000).toLocaleTimeString() : ""}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </div>

      <div className="card">
        <div className="row" style={{ marginBottom: 10 }}>
          <h3 style={{ margin: 0 }}>Active mandates</h3>
          <div className="spacer" style={{ flex: 1 }} />
          <Link to="/mandates" className="link">all mandates →</Link>
        </div>
        {(data?.mandates ?? []).length === 0 ? (
          <div className="empty">No Mandates yet — <Link to="/mandates" className="link">turn a big goal into Briefs</Link>.</div>
        ) : (
          <table className="table compact">
            <tbody>
              {(data?.mandates ?? []).slice(0, 6).map((m, i) => (
                <tr key={m.mandate_id ?? m.id ?? i}>
                  <td><strong style={{ fontSize: 13 }}>{m.title ?? m.name ?? "(untitled)"}</strong></td>
                  <td><span className={"badge " + (m.status ?? "todo")} style={{ fontSize: 9 }}>{m.status ?? "—"}</span></td>
                  <td className="mono" style={{ fontSize: 10 }}>{(m.mandate_id ?? m.id ?? "").slice(0, 10)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      <div className="grid cols-2">
        <div className="card">
          <h3>Needs attention</h3>
          <AttnList label="Blocked" rows={inbox.blocked} tone="blocked" />
          <AttnList label="Overdue" rows={inbox.overdue} tone="in_progress" />
          <AttnList label="Unassigned" rows={inbox.unassigned} tone="todo" />
          {!attention && <div className="empty">Nothing on fire. Nice.</div>}
        </div>

        <div className="card">
          <h3>Recent activity</h3>
          {(data?.events ?? []).length === 0 ? (
            <div className="empty">No recent runtime events.</div>
          ) : (
            <table className="table compact">
              <tbody>
                {(data?.events ?? []).map((e, i) => (
                  <tr key={i}>
                    <td><span className="badge">{e.event_type ?? "event"}</span></td>
                    <td className="mono">{(e.task_id ?? "").slice(0, 10)}</td>
                    <td className="muted">{e.ts ? new Date(e.ts * 1000).toLocaleTimeString() : ""}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </div>

      <div className="card">
        <h3>Board distribution</h3>
        <div className="pill-row">
          {COLUMNS.every((c) => (board[c] ?? 0) === 0) && (
            <span className="muted">Spine board empty — <Link to="/briefs" className="link">create a Brief</Link>.</span>
          )}
          {COLUMNS.filter((c) => (board[c] ?? 0) > 0).map((c) => (
            <span key={c} className="row" style={{ gap: 6 }}>
              <Badge status={c} />
              <strong>{board[c]}</strong>
            </span>
          ))}
        </div>
      </div>
    </div>
  );
}

// Severity → badge tone. Color is reserved for meaning only (design §12):
// high = needs you (blocked tone), medium = actionable, low = informational.
const SEV_TONE: Record<string, string> = { high: "blocked", medium: "in_progress", low: "backlog" };
// A short human label per category for the row chip.
const CAT_LABEL: Record<string, string> = {
  approval: "approval",
  hire: "hire",
  failed_or_refused: "failed",
  needs_review: "review",
  ready_to_start: "ready",
  blocked: "blocked",
  stale: "stale",
};

// Phase → chip tone for the cockpit. The next step's `label`/`reason` are
// already plain language from the driver; this only colors the phase chip.
const PHASE_TONE: Record<string, string> = {
  needs_approval: "backlog",
  needs_strategy: "backlog",
  needs_hire_approval: "blocked",
  needs_team_plan: "todo",
  needs_orchestration: "todo",
  running: "in_progress",
  needs_review: "in_review",
  blocked: "blocked",
  done: "done",
  unknown: "backlog",
};

// The board/run counts the next-step payload carries (`BriefCounts::to_json`).
// [count key, chip label, tone] — only non-zero buckets are shown.
const COCKPIT_COUNTS: [string, string, string][] = [
  ["ready", "ready", "todo"],
  ["running", "running", "in_progress"],
  ["needs_review", "review", "in_review"],
  ["blocked", "blocked", "blocked"],
  ["done", "done", "done"],
  ["unassigned", "unassigned", "backlog"],
  ["failed", "failed", "blocked"],
  ["refused", "refused", "blocked"],
];

// The Action-Center-derived pressure strip — the company's live pressures at a
// glance, each linking to where it gets worked. [category key, label, route, tone].
const PRESSURES: [string, string, string, string][] = [
  ["approval", "approvals", "/approvals", "blocked"],
  ["hire", "hires", "/approvals", "blocked"],
  ["budget", "budget", "/costs", "blocked"],
  ["failed_or_refused", "recovery", "/runs", "blocked"],
  ["needs_review", "review", "/runs", "in_review"],
  ["ready_to_start", "ready", "/briefs", "todo"],
];

// ── First-run on-ramp ──────────────────────────────────────────────────────
// The smallest discoverable path from a fresh-but-initialized company to a
// positive Shift (roadmap §6 DoD "Time-to-first-success … discoverable in the
// UI"; product-spine-implementation.md First-Run Starter Crew Pack;
// company-model §12.6). It chains THREE existing routes — no new backend:
//   1. `company.starter_crew` (echo)  → a couple of safe-local Operatives,
//   2. `brief.create` (with `assignee`) → a first Brief assigned to one of them,
//   3. `brief.run` (echo)             → a real Shift through the built-in echo Rig.
// It NEVER calls a real model provider (echo is the built-in safe stand-in), and
// every step shows honest progress; a refusal/failure surfaces the real reason.
const ONRAMP_BRIEF_TITLE = "First Shift — local echo demo";
const ONRAMP_PLAN = [
  "Set up a safe local crew (echo)",
  "Create your first Brief",
  "Run the Shift (echo)",
];
type OnRampStepState = "todo" | "run" | "ok" | "err";
const STEP_TONE: Record<OnRampStepState, string> = {
  todo: "backlog",
  run: "in_progress",
  ok: "done",
  err: "blocked",
};
const STEP_LABEL: Record<OnRampStepState, string> = {
  todo: "step",
  run: "…",
  ok: "done",
  err: "failed",
};
// Plain-language mapping for the `brief.run` outcome on the safe-local path.
const ONRAMP_REFUSALS: Record<string, string> = {
  unassigned: "the Brief had no Operative to run it",
  no_adapter: "no adapter is configured for the assigned Operative",
  adapter_unavailable: "the echo adapter isn't available",
  already_running: "this Brief is already running",
  not_found: "the Brief could not be found",
  workspace_error: "a run workspace could not be prepared",
  failed: "the run failed",
};

interface StarterCrewResult {
  founder?: { agent_id?: string; name?: string } | null;
  founder_created?: boolean;
  rig?: string;
  safe_local?: boolean;
  crew?: { agent_id?: string; role?: string; name?: string; created?: boolean }[];
}
interface CreateBriefResult { task_id?: string }
interface OnRampRunReport {
  brief_id?: string;
  status?: string;
  rig?: string;
  summary?: string;
  install_hint?: string | null;
  run_id?: string | null;
}

function FirstRunOnRamp({
  onStarted,
  onChanged,
}: {
  onStarted: () => void;
  onChanged: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [steps, setSteps] = useState<{ s: OnRampStepState; note?: string }[] | null>(null);
  const [result, setResult] = useState<{ briefId: string; runId?: string; status?: string; summary?: string } | null>(null);
  const [fatal, setFatal] = useState<string | null>(null);

  const setStep = (i: number, s: OnRampStepState, note?: string) =>
    setSteps((prev) => (prev ? prev.map((x, idx) => (idx === i ? { s, note } : x)) : prev));

  async function runFirstShift() {
    setBusy(true);
    setFatal(null);
    setResult(null);
    setSteps(ONRAMP_PLAN.map(() => ({ s: "todo" as OnRampStepState })));
    onStarted(); // latch the panel open so the result survives the post-run reload
    try {
      // 1) Ensure a safe-local echo crew (idempotent; returns the crew + ids).
      setStep(0, "run");
      const crew = await api.post<StarterCrewResult>("/v1/spine/company/starter-crew", { rig: "echo" });
      const worker = (crew.crew ?? []).find((c) => c.agent_id);
      const assignee = worker?.agent_id ?? crew.founder?.agent_id;
      const who = worker?.name ?? crew.founder?.name ?? "the Founder";
      if (!assignee) throw new Error("No safe-local Operative was available to run the Brief.");
      setStep(0, "ok", `${who} ready on the echo adapter.`);

      // 2) Create the first Brief, assigned to that Operative in one call.
      setStep(1, "run");
      const created = await api.post<CreateBriefResult>("/v1/spine/briefs", {
        title: ONRAMP_BRIEF_TITLE,
        priority: "normal",
        assignee,
      });
      const briefId = created.task_id;
      if (!briefId) throw new Error("The Brief was created but no id was returned.");
      setStep(1, "ok", `Brief created and assigned to ${who}.`);

      // 3) Run it through the built-in echo Rig.
      setStep(2, "run");
      const run = await api.post<OnRampRunReport>(
        `/v1/spine/briefs/${encodeURIComponent(briefId)}/run`,
        { rig: "echo" },
      );
      const ran = run.status === "running" || run.status === "done";
      if (ran) {
        setStep(2, "ok", run.status === "done" ? "Shift complete." : "Shift started — executing now.");
      } else {
        // Honest: the run path refused — show the real reason + any install hint.
        const why = ONRAMP_REFUSALS[run.status ?? ""] ?? run.status ?? "the run did not start";
        setStep(2, "err", run.install_hint ? `${why} (${run.install_hint})` : why);
      }
      setResult({ briefId, runId: run.run_id ?? undefined, status: run.status, summary: run.summary });
      onChanged();
      // Other surfaces (board, Action Center, Mandates) now have a Brief/run.
      invalidate(["briefs", "actions", "mandates"]);
    } catch (e) {
      // Mark whichever step was mid-flight as failed and surface the real error.
      setSteps((prev) => (prev ? prev.map((x) => (x.s === "run" ? { s: "err" } : x)) : prev));
      setFatal(e instanceof Error ? e.message : "Could not complete the first Shift.");
    } finally {
      setBusy(false);
    }
  }

  const done = result?.status === "done";
  return (
    <div className="card setup-card">
      <div className="setup-step">Get started · safe local path</div>
      <h2 style={{ margin: "4px 0 8px" }}>Run your first Shift</h2>
      <p className="muted" style={{ maxWidth: 620 }}>
        Your company is set up but hasn't done any work yet. In one click, Relix will
        provision a couple of safe, local <em>echo</em> Operatives, create a first{" "}
        <strong>Brief</strong>, assign it, and run it end-to-end through the built-in
        echo adapter — so you can watch a Shift reach <strong>done</strong> without
        installing any external coding agent.
      </p>
      {fatal && (
        <div className="banner err" style={{ fontSize: 12 }}>
          {fatal} — you can retry, or set up the crew by hand on{" "}
          <Link to="/agents" className="link">Crew</Link>.
        </div>
      )}
      <div className="row" style={{ marginTop: 12, gap: 12, flexWrap: "wrap", alignItems: "center" }}>
        <button className="btn" disabled={busy} onClick={runFirstShift}>
          {busy ? "Running…" : steps ? "Run again →" : "Run a first local Shift →"}
        </button>
        <Link to="/chat" className="link" style={{ fontSize: 12 }}>
          or plan a real goal with Prime →
        </Link>
      </div>
      {steps && (
        <ol className="next-steps" style={{ marginTop: 12 }}>
          {steps.map((st, i) => (
            <li key={i} style={{ display: "flex", alignItems: "baseline", gap: 8 }}>
              <span
                className={"badge " + STEP_TONE[st.s]}
                style={{ fontSize: 9, minWidth: 44, textAlign: "center" }}
              >
                {STEP_LABEL[st.s]}
              </span>
              <span>
                {ONRAMP_PLAN[i]}
                {st.note && <span className="muted" style={{ marginLeft: 6, fontSize: 12 }}>— {st.note}</span>}
              </span>
            </li>
          ))}
        </ol>
      )}
      {result && (
        <div className={"banner " + (result.status === "err" ? "err" : "ok")} style={{ fontSize: 12 }}>
          {done
            ? "First Shift complete."
            : result.status === "running"
              ? "First Shift is running."
              : "First Shift opened."}
          {result.summary ? ` ${result.summary}` : ""}
          {result.runId && (
            <Link to={`/runs?run=${encodeURIComponent(result.runId)}`} className="link" style={{ marginLeft: 8 }}>
              Open the run →
            </Link>
          )}
          {result.briefId && (
            <Link to={`/briefs?brief=${encodeURIComponent(result.briefId)}`} className="link" style={{ marginLeft: 12 }}>
              View the Brief →
            </Link>
          )}
          <Link to="/agents" className="link" style={{ marginLeft: 12 }}>See the crew →</Link>
        </div>
      )}
      <p className="muted" style={{ fontSize: 11, marginTop: 10, maxWidth: 620 }}>
        This proves the local execution loop — governed crew → Brief → scoped run →
        transcript → review/apply — end-to-end. It does <strong>not</strong> call a real
        model provider: the echo adapter is a built-in stand-in. To run real work,
        install + log in to a coding-agent CLI on{" "}
        <Link to="/settings" className="link">Settings</Link> and switch an Operative's adapter.
      </p>
    </div>
  );
}

// A compact, read-only operations snapshot sourced from `company.operations`
// (dashboard-design §5). One flat card (no nested cards) with three glance
// groups — work in flight, what needs attention, governance — each stat a
// deep link to where it's worked. Honest unavailable state when the summary is
// absent (the agent-only fallback read). Never mutates anything.
function OperationsSnapshot({ ops }: { ops?: Operations | null }) {
  if (!ops) {
    return (
      <div className="card">
        <h3 style={{ margin: 0 }}>Operations snapshot</h3>
        <div className="empty">Operations summary unavailable right now.</div>
      </div>
    );
  }
  const b = ops.briefs ?? {};
  const r = ops.runs ?? {};
  const a = ops.approvals ?? {};
  const m = ops.mandates ?? {};
  const attention =
    (b.unassigned ?? 0) + (b.blocked ?? 0) + (b.stale ?? 0) + (r.failed_or_refused ?? 0);
  // [count, label, route, warn?] — `warn` colors a non-zero value as needs-you.
  const groups: { label: string; items: [number, string, string, boolean][] }[] = [
    {
      label: "Work in flight",
      items: [
        [r.running ?? 0, "running", "/runs", false],
        [b.ready_to_start ?? 0, "ready", "/briefs", false],
        [r.pending_review ?? 0, "in review", "/runs", false],
      ],
    },
    {
      label: "Needs attention",
      items: [
        [b.unassigned ?? 0, "unassigned", "/briefs", true],
        [b.blocked ?? 0, "blocked", "/briefs", true],
        [b.stale ?? 0, "stale", "/briefs", true],
        [r.failed_or_refused ?? 0, "recovery", "/runs", true],
      ],
    },
    {
      label: "Governance",
      items: [
        [a.pending_clearances ?? 0, "approvals", "/approvals", true],
        [a.pending_hires ?? 0, "hires", "/approvals", true],
        [m.strategy_proposed ?? 0, "strategy", "/mandates", true],
        [m.total ?? 0, "mandates", "/mandates", false],
      ],
    },
  ];
  return (
    <div className="card">
      <div className="row" style={{ marginBottom: 8, alignItems: "center" }}>
        <h3 style={{ margin: 0 }}>Operations snapshot</h3>
        <div className="spacer" style={{ flex: 1 }} />
        <span className="muted" style={{ fontSize: 12 }}>
          {b.total ?? 0} Brief{(b.total ?? 0) === 1 ? "" : "s"} · {attention} need
          {attention === 1 ? "s" : ""} attention
        </span>
      </div>
      <div className="row wrap" style={{ gap: 16, alignItems: "flex-start" }}>
        {groups.map((g) => (
          <div key={g.label} style={{ flex: "1 1 180px", minWidth: 0 }}>
            <div className="muted" style={{ fontSize: 11, marginBottom: 6 }}>{g.label}</div>
            <div className="row wrap" style={{ gap: 14 }}>
              {g.items.map(([n, label, to, warn]) => (
                <Link
                  key={label}
                  to={to}
                  title={`${n} ${label}`}
                  style={{
                    display: "inline-flex",
                    flexDirection: "column",
                    alignItems: "center",
                    minWidth: 52,
                    textDecoration: "none",
                  }}
                >
                  <b className={n ? (warn ? "warn" : "info") : ""} style={{ fontSize: 18 }}>{n}</b>
                  <span className="muted" style={{ fontSize: 11 }}>{label}</span>
                </Link>
              ))}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

// The Overview cockpit (dashboard-design §5; roadmap §2 slice 9b). Surfaces the
// SAME bounded guided-driver step the Chat Shift Room shows — Prime's ONE next
// safe governed step over the most relevant active object — plus a live pressure
// strip from the Action Center, so the operator knows what to do next at a
// glance. "Advance one step" runs AT MOST one governed step through the existing
// gated route (proposal or Mandate twin); it never auto-approves a strategy /
// hire / spawn / budget gate, never runs an adapter, and never loops.
function OperatingStatus({
  nextStep,
  nextStepRef,
  actions,
  onReload,
}: {
  nextStep: PrimeNextStep | null;
  nextStepRef: NextStepRef | null;
  actions: CompanyActions | null;
  onReload: () => void;
}) {
  const [advancing, setAdvancing] = useState(false);
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);

  async function advance() {
    if (!nextStep?.can_advance || !nextStep.advance_action || !nextStepRef) return;
    setAdvancing(true);
    setBanner(null);
    try {
      const res =
        nextStepRef.kind === "proposal"
          ? await primeDriver.advance(nextStepRef.id, nextStep.advance_action)
          : await primeDriver.mandateAdvance(nextStepRef.id, nextStep.advance_action);
      if (res.advanced) {
        setBanner({ kind: "ok", msg: `Advanced one step${res.action ? ` · ${res.action}` : ""}.` });
      } else if (res.refused) {
        // The driver refused (e.g. a governance gate) — show its reason verbatim.
        setBanner({ kind: "err", msg: `Refused: ${res.reason ?? res.refused}` });
      }
      onReload();
    } catch (e) {
      // A stale request (409) means the plan moved on between read and click —
      // honest banner + reload the fresh next step; never retry the 409 blindly.
      if (e instanceof ApiError && e.status === 409) {
        setBanner({
          kind: "err",
          msg: "That step is no longer current — the plan moved on. Reloading the latest next step.",
        });
      } else {
        setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Advance failed" });
      }
      onReload();
    } finally {
      setAdvancing(false);
    }
  }

  // The pressure strip is best-effort from whichever Action Center feed we have.
  const byCat = actions?.counts?.by_category ?? {};
  const pressures = PRESSURES.map(([cat, label, to, tone]) => ({
    label,
    to,
    tone,
    n: byCat[cat] ?? 0,
  })).filter((p) => p.n > 0);

  return (
    <div className="card">
      <div className="row" style={{ marginBottom: 8, alignItems: "center" }}>
        <h3 style={{ margin: 0 }}>Company operating status</h3>
        <div className="spacer" style={{ flex: 1 }} />
        <span className="muted" style={{ fontSize: 12 }}>Prime's next safe step</span>
      </div>
      {banner && (
        <div className={"banner " + banner.kind} style={{ fontSize: 12, marginBottom: 8 }}>
          {banner.msg}
        </div>
      )}

      {nextStep ? (
        <>
          <div className="shift-room-row" style={{ alignItems: "flex-start" }}>
            <span
              className={"badge " + (PHASE_TONE[nextStep.phase] ?? "todo")}
              style={{ fontSize: 9, minWidth: 76, textAlign: "center" }}
              title={`phase: ${nextStep.phase}`}
            >
              {nextStep.can_advance ? "next step" : "operator step"}
            </span>
            <div style={{ flex: 1, minWidth: 0 }}>
              <div style={{ fontSize: 13, fontWeight: 600 }}>{nextStep.label}</div>
              {nextStep.reason && (
                <div className="muted" style={{ fontSize: 11 }}>{nextStep.reason}</div>
              )}
              {!nextStep.can_advance && nextStep.route && (
                <div className="muted" style={{ fontSize: 11, marginTop: 2 }}>
                  Take it here: <Link to={nextStep.route} className="link">{nextStep.route}</Link>
                </div>
              )}
            </div>
            {nextStep.can_advance && nextStep.advance_action ? (
              <button
                className="btn"
                disabled={advancing}
                onClick={advance}
                title="Run this ONE governed step now through the existing gated route. The driver advances at most one step and never approves a strategy, hire, spawn, or budget gate."
              >
                {advancing ? "Advancing…" : "Advance one step"}
              </button>
            ) : nextStep.route ? (
              <Link to={nextStep.route} className="btn sm ghost">Open →</Link>
            ) : null}
          </div>

          {/* Board/run counts the next step is computed over (non-zero only). */}
          {COCKPIT_COUNTS.some(([k]) => (nextStep.counts?.[k] ?? 0) > 0) && (
            <div className="row wrap" style={{ gap: 6, marginTop: 8 }}>
              {COCKPIT_COUNTS.filter(([k]) => (nextStep.counts?.[k] ?? 0) > 0).map(([k, label, tone]) => (
                <span key={k} className={"badge " + tone} style={{ fontSize: 9 }}>
                  {nextStep.counts[k]} {label}
                </span>
              ))}
              <span className="muted" style={{ fontSize: 11 }}>
                {nextStep.counts?.total_briefs ?? 0} Brief(s) in plan
              </span>
            </div>
          )}
        </>
      ) : (
        <div className="empty">
          No active Prime plan yet — <Link to="/chat" className="link">plan with Prime</Link> to
          propose a Mandate, crew, and Briefs.
        </div>
      )}

      {/* Pressure strip — live company pressures, each linking to where it's
          worked. Sourced from the Action Center feed; hidden when nothing's hot. */}
      {pressures.length > 0 && (
        <div className="row wrap" style={{ gap: 8, marginTop: 10, alignItems: "center" }}>
          <span className="muted" style={{ fontSize: 11 }}>Pressure</span>
          {pressures.map((p) => (
            <Link key={p.label} to={p.to} className={"badge " + p.tone} style={{ fontSize: 9 }}>
              {p.n} {p.label}
            </Link>
          ))}
        </div>
      )}
    </div>
  );
}

// The Action Center — one ordered, deduped feed of what needs the operator,
// computed server-side from live state (company-model §8.2). Each row links to
// the existing route that performs the action; nothing is mutated here.
function ActionCenter({
  data,
  loading,
  onActed,
}: {
  data: CompanyActions | null;
  loading: boolean;
  onActed: () => void;
}) {
  // Which item is mid-decision (its target_id), and the last inline result —
  // so a hire can be approved/rejected without leaving the Inbox (design §5).
  const [acting, setActing] = useState<string | null>(null);
  // The note may carry an optional deep link (e.g. a retry's child run) so the
  // operator can jump to the new run without leaving the cockpit.
  const [note, setNote] = useState<{ kind: string; msg: string; to?: string; toLabel?: string } | null>(null);
  // Which recovery item's retry is in flight (its source run_id).
  const [retrying, setRetrying] = useState<string | null>(null);

  // Approve a pending hire inline with its suggested safe-local Rig so the
  // Operative is immediately runnable (company-model §12.6); a clearance-gated
  // hire is refused server-side and we say so.
  async function approveHire(a: ActionItem) {
    if (!a.target_id) return;
    setActing(a.target_id);
    setNote(null);
    try {
      const r = await api.post<{ runnable?: boolean; rig?: string; needs_rig?: boolean }>(
        `/v1/agents/${encodeURIComponent(a.target_id)}/approve-hire`,
        a.suggested_rig ? { rig: a.suggested_rig } : {},
      );
      setNote({
        kind: "ok",
        msg: r.needs_rig
          ? `${a.target_title ?? "Operative"} hired — set an adapter to make it runnable.`
          : `${a.target_title ?? "Operative"} hired and runnable on ${r.rig ?? a.suggested_rig ?? "echo"}.`,
      });
      onActed();
      // A hire changes the roster + Mandate readiness — notify those surfaces
      // (dashboard-design §11). `onActed` already refreshes this Action feed.
      invalidate(["briefs", "mandates"]);
    } catch (e) {
      const msg = e instanceof Error ? e.message : "Approve hire failed";
      setNote({ kind: "err", msg: /clearance/i.test(msg) ? `${msg} — decide its Clearance on Mandates.` : msg });
    } finally {
      setActing(null);
    }
  }

  // Guarded operator retry of a failed/interrupted Shift, straight from the
  // Action Center (execution-and-issue §3.3b). Calls the SAME already-implemented
  // guarded route the Runs page uses (`POST /v1/runs/:id/retry`); the runtime
  // re-checks every precondition (terminal failure-like + retryable + budget +
  // no existing child) and refuses honestly if unsafe — this is NOT a blind
  // auto-retry. On success we follow the child run; an already-retried source
  // returns its existing child (200) and we link to it; any refusal surfaces
  // verbatim (we never hide the failure). The card also keeps its deep link to
  // Runs so the operator can still inspect.
  async function retryShift(a: ActionItem) {
    if (!a.run_id) return;
    setRetrying(a.run_id);
    setNote(null);
    try {
      const r = await api.post<{ status?: string; run_id?: string; retry_attempt?: number }>(
        `/v1/runs/${encodeURIComponent(a.run_id)}/retry`,
        {},
      );
      const child = r.run_id;
      const to = child ? `/runs?run=${encodeURIComponent(child)}` : undefined;
      setNote({
        kind: "ok",
        msg:
          r.status === "already_retried"
            ? `This Shift was already retried — child run ${child ?? "?"}.`
            : `Retry started — child run ${child ?? "?"}${r.retry_attempt ? ` (attempt ${r.retry_attempt})` : ""}.`,
        to,
        toLabel: "Open run →",
      });
      onActed();
      // A retry opens a child run and may move the Brief — refresh those surfaces
      // (dashboard-design §11). `onActed` already refreshes this feed + Overview.
      invalidate(["briefs", "mandates"]);
    } catch (e) {
      setNote({ kind: "err", msg: e instanceof Error ? e.message : "Retry failed" });
    } finally {
      setRetrying(null);
    }
  }

  async function rejectHire(a: ActionItem) {
    if (!a.target_id) return;
    setActing(a.target_id);
    setNote(null);
    try {
      await api.post(`/v1/agents/${encodeURIComponent(a.target_id)}/reject-hire`, {});
      setNote({ kind: "ok", msg: `${a.target_title ?? "Hire"} declined — the role is left unfilled.` });
      onActed();
      invalidate(["briefs", "mandates"]);
    } catch (e) {
      setNote({ kind: "err", msg: e instanceof Error ? e.message : "Reject hire failed" });
    } finally {
      setActing(null);
    }
  }

  const actions = data?.actions ?? [];
  const total = data?.counts?.total ?? actions.length;
  const high = data?.counts?.by_severity?.high ?? 0;
  // Calm empty state — once initialized, an empty feed means nothing needs you.
  if (!loading && total === 0) {
    return (
      <div className="card">
        <div className="row" style={{ marginBottom: 6, alignItems: "center" }}>
          <h3 style={{ margin: 0 }}>Action Center</h3>
        </div>
        {note && (
          <div className={"banner " + note.kind} style={{ fontSize: 12 }}>
            {note.msg}
            {note.to && <Link to={note.to} className="link" style={{ marginLeft: 8 }}>{note.toLabel ?? "Open →"}</Link>}
          </div>
        )}
        <div className="empty">Nothing needs you right now — the company is moving on its own.</div>
      </div>
    );
  }
  if (data === null && !loading) {
    // The endpoint was unavailable (optional surface) — say so, don't fake it.
    return (
      <div className="card">
        <h3 style={{ margin: 0 }}>Action Center</h3>
        <div className="empty">Action Center unavailable right now.</div>
      </div>
    );
  }
  const shown = actions.slice(0, 8);
  return (
    <div className="card">
      <div className="row" style={{ marginBottom: 10, alignItems: "center" }}>
        <h3 style={{ margin: 0 }}>Action Center</h3>
        {total > 0 && (
          <span className={"badge " + (high > 0 ? "blocked" : "in_progress")} style={{ fontSize: 9, marginLeft: 8 }}>
            {total} need{total === 1 ? "s" : ""} you
          </span>
        )}
        <div className="spacer" style={{ flex: 1 }} />
        <span className="muted" style={{ fontSize: 12 }}>computed from live state</span>
      </div>
      {note && (
        <div className={"banner " + note.kind} style={{ fontSize: 12 }}>
          {note.msg}
          {note.to && <Link to={note.to} className="link" style={{ marginLeft: 8 }}>{note.toLabel ?? "Open →"}</Link>}
        </div>
      )}
      <div className="table-scroll">
      <table className="table compact">
        <tbody>
          {shown.map((a, i) => {
            // A direct hire is machine-actionable here (`action_api` set) — let
            // the operator Approve (with the safe-local Rig) / Reject without
            // leaving the Inbox (design §5: "inline Approve/Reject").
            const inlineHire = a.category === "hire" && !!a.action_api && !!a.target_id;
            const isActing = acting === a.target_id;
            // A retryable failed/interrupted Shift carries an explicit retry
            // target (`run_id`) ONLY when the backend judged it safe (retryable +
            // budget + no existing child). Render a guarded Retry Shift button —
            // never for items without this explicit recovery metadata.
            const canRetry = a.category === "failed_or_refused" && !!a.run_id;
            const isRetrying = retrying === a.run_id;
            return (
            <tr key={a.id ?? i}>
              <td style={{ width: 64 }}>
                <span className={"badge " + (SEV_TONE[a.severity ?? ""] ?? "todo")} style={{ fontSize: 9 }}>
                  {CAT_LABEL[a.category ?? ""] ?? a.category ?? "action"}
                </span>
              </td>
              <td>
                <div style={{ fontSize: 13, fontWeight: 600 }}>{a.title ?? "(action)"}</div>
                {a.reason && <div className="muted" style={{ fontSize: 11 }}>{a.reason}</div>}
              </td>
              <td style={{ textAlign: "right" }}>
                {inlineHire ? (
                  <span className="btn-group" style={{ justifyContent: "flex-end" }}>
                    <button
                      className="btn sm"
                      disabled={isActing}
                      title={`Approve this hire on the safe-local ${a.suggested_rig ?? "echo"} adapter so it is immediately runnable`}
                      onClick={() => approveHire(a)}
                    >
                      {isActing ? "…" : `Approve · ${a.suggested_rig ?? "echo"}`}
                    </button>
                    <button
                      className="btn ghost sm"
                      disabled={isActing}
                      title="Decline this hire (the role is left unfilled)"
                      onClick={() => rejectHire(a)}
                    >
                      Reject
                    </button>
                  </span>
                ) : canRetry ? (
                  // Guarded retry + the existing deep link to inspect the run.
                  <span className="btn-group" style={{ justifyContent: "flex-end" }}>
                    <button
                      className="btn sm"
                      disabled={isRetrying}
                      title="Open one guarded retry of this Shift through the same governed run path. The runtime re-checks every precondition and refuses if unsafe — this is not a blind auto-retry."
                      onClick={() => retryShift(a)}
                    >
                      {isRetrying ? "…" : "Retry Shift"}
                    </button>
                    {a.route && <Link to={a.route} className="btn ghost sm">{a.action_label ?? "Inspect"} →</Link>}
                  </span>
                ) : a.route ? (
                  <Link to={a.route} className="btn sm ghost">{a.action_label ?? "Open"} →</Link>
                ) : (
                  <span className="muted" style={{ fontSize: 11 }}>{a.action_label}</span>
                )}
              </td>
            </tr>
            );
          })}
        </tbody>
      </table>
      </div>
      {(actions.length > shown.length || data?.truncated) && (
        <div className="muted" style={{ fontSize: 11, marginTop: 6 }}>
          {actions.length - shown.length > 0 ? `+${actions.length - shown.length} more` : "More actions"} —
          {" "}work them from <Link to="/briefs" className="link">Briefs</Link>,{" "}
          <Link to="/mandates" className="link">Mandates</Link>, or{" "}
          <Link to="/runs" className="link">Runs</Link>.
        </div>
      )}
    </div>
  );
}

// Compact live view of the latest Prime work session (Shift Room), sourced
// from the new `prime.status` API. The full interactive room lives on /chat.
function ActiveWork({ session }: { session: SessionStatus }) {
  const c = session.counts ?? {};
  const chips: [keyof SessionCounts, string, string][] = [
    ["ready", "ready", "todo"],
    ["running", "running", "in_progress"],
    ["needs_review", "review", "in_review"],
    ["done", "done", "done"],
    ["blocked", "blocked", "blocked"],
    ["unassigned", "unassigned", "backlog"],
    ["failed", "failed", "blocked"],
    ["refused", "refused", "blocked"],
  ];
  return (
    <div className="card">
      <div className="row" style={{ marginBottom: 8, alignItems: "center" }}>
        <h3 style={{ margin: 0 }}>Active work</h3>
        {session.mandate_title && <span className="muted" style={{ fontSize: 12, marginLeft: 8 }}>· {session.mandate_title}</span>}
        <div className="spacer" style={{ flex: 1 }} />
        <Link to="/chat" className="link">open Shift Room →</Link>
      </div>
      <div className="row wrap" style={{ gap: 6 }}>
        {chips
          .filter(([k]) => (c[k] ?? 0) > 0)
          .map(([k, label, tone]) => (
            <span key={k} className={"badge " + tone} style={{ fontSize: 9 }}>
              {c[k]} {label}
            </span>
          ))}
        <span className="muted" style={{ fontSize: 12 }}>{c.total_briefs ?? 0} Brief(s) in session</span>
      </div>
      {(session.recommended_next_actions ?? []).slice(0, 2).map((a, i) => (
        <div key={i} className="muted" style={{ fontSize: 11, marginTop: 4 }}>• {a}</div>
      ))}
    </div>
  );
}

function AttnList({ label, rows, tone }: { label: string; rows?: Card[]; tone: string }) {
  const list = rows ?? [];
  if (list.length === 0) return null;
  return (
    <div style={{ marginBottom: 10 }}>
      <div className="row" style={{ marginBottom: 6 }}>
        <span className={"badge " + tone}>{label}</span>
        <span className="muted">{list.length}</span>
      </div>
      {list.slice(0, 4).map((c, i) => (
        <div key={i} className="dim" style={{ fontSize: 13, padding: "2px 0" }}>
          {c.title ?? c.task_id ?? c.id ?? "untitled"}
        </div>
      ))}
    </div>
  );
}
