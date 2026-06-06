import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { api, subscribeRunEvents, tryGet, tryGetReport } from "../api";
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
interface CompanyStatus {
  initialized?: boolean;
  founder?: Founder | null;
  prime?: Founder | null;
  operative_count?: number;
  crew?: { total?: number; active?: number; pending?: number };
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
      mandates: extractList<MandateRow>(mandates, ["mandates"]),
      events: extractList<EventRow>(events),
      session: session ?? null,
      actions: actions ?? null,
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
  // Refetch ONLY the Action Center feed (success-only → never clobber with
  // null, so a transient blip can't blank it). Shared by the SSE/poll effect
  // below AND the inline Approve/Reject handlers, so acting on a hire updates
  // the feed immediately.
  const refreshActions = useCallback(async () => {
    const a = await tryGet<CompanyActions | null>("/v1/spine/company/actions", null);
    if (a) setLiveActions(a);
  }, []);
  useEffect(() => {
    let debounce: ReturnType<typeof setTimeout> | null = null;
    // Coalesce run-event bursts into one refresh ~1.2s later.
    const ping = () => {
      if (debounce) clearTimeout(debounce);
      debounce = setTimeout(refreshActions, 1200);
    };
    // onConn is required by the API but the badge isn't surfaced here; ignore it.
    const unsub = subscribeRunEvents(ping, () => {});
    const poll = setInterval(refreshActions, 20000); // convergence fallback (bounded)
    return () => {
      if (debounce) clearTimeout(debounce);
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
    if (initialized && (data?.mandates?.length ?? 0) === 0 && totalBriefs === 0) {
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
  const [note, setNote] = useState<{ kind: string; msg: string } | null>(null);

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
        {note && <div className={"banner " + note.kind} style={{ fontSize: 12 }}>{note.msg}</div>}
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
      {note && <div className={"banner " + note.kind} style={{ fontSize: 12 }}>{note.msg}</div>}
      <div className="table-scroll">
      <table className="table compact">
        <tbody>
          {shown.map((a, i) => {
            // A direct hire is machine-actionable here (`action_api` set) — let
            // the operator Approve (with the safe-local Rig) / Reject without
            // leaving the Inbox (design §5: "inline Approve/Reject").
            const inlineHire = a.category === "hire" && !!a.action_api && !!a.target_id;
            const isActing = acting === a.target_id;
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
