import { Link } from "react-router-dom";
import { tryGet } from "../api";
import { Badge, extractList, useAsync } from "../components/common";

// The board summary arrives as an object keyed by board status, e.g.
// `{ "backlog": 1, "todo": 2, "total": 3 }`.
type BoardSummary = Record<string, number>;
interface Card { task_id?: string; id?: string; title?: string; board_status?: string; priority?: string }
interface Inbox { blocked?: Card[]; overdue?: Card[]; unassigned?: Card[]; review?: Card[]; stale?: Card[] }
interface Roster { active?: number; total?: number }
interface EventRow { task_id?: string; event_type?: string; ts?: number; payload?: string }
interface Founder { name?: string; rig?: string | null }
interface CompanyStatus { initialized?: boolean; founder?: Founder | null; operative_count?: number }
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

const COLUMNS = ["backlog", "todo", "in_progress", "in_review", "done"];
const RUN_TONE: Record<string, string> = {
  running: "in_progress",
  done: "done",
  failed: "blocked",
  cancelled: "blocked",
  continued: "todo",
};

interface Warn {
  tone: "err" | "info";
  msg: string;
  to?: string;
  cta?: string;
}

export function Overview() {
  const { data, loading } = useAsync(async () => {
    const [board, inbox, roster, company, adapters, runs, runCfg, maint, events] = await Promise.all([
      tryGet<BoardSummary>("/v1/spine/board", {}),
      tryGet<Inbox>("/v1/spine/inbox?limit=50", {}),
      tryGet<Roster>("/v1/spine/roster", {}),
      tryGet<CompanyStatus>("/v1/spine/company", {}),
      tryGet<Adapter[]>("/v1/adapters", []),
      tryGet<RunRow[]>("/v1/runs", []),
      tryGet<RunConfig>("/v1/spine/run-config", {}),
      tryGet<MaintSummary | null>("/v1/maintenance/summary", null),
      tryGet<unknown>("/v1/tasks/events/recent?limit=10", {}),
    ]);
    const mandates = await tryGet<unknown>("/v1/spine/mandates?limit=8", {});
    return {
      board,
      inbox,
      roster,
      company: company ?? {},
      adapters: Array.isArray(adapters) ? adapters : [],
      runs: Array.isArray(runs) ? runs : [],
      runCfg: runCfg ?? {},
      maint: maint ?? null,
      mandates: extractList<MandateRow>(mandates, ["mandates"]),
      events: extractList<EventRow>(events),
    };
  }, []);

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
  if (!loading && !initialized) {
    return (
      <div className="grid">
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
            <li>Initialize the company (create the Founder).</li>
            <li>Create a Brief and assign it to an Operative.</li>
            <li>Run it — Relix executes in a scoped sandbox and records a transcript.</li>
            <li>Review the changed files, then accept &amp; apply them.</li>
          </ol>
        </div>
      </div>
    );
  }

  return (
    <div className="grid">
      {/* Actionable system warnings + next steps */}
      {warnings.length > 0 && (
        <div className="grid" style={{ gap: 8 }}>
          {warnings.map((w, i) => (
            <div key={i} className={"banner " + w.tone + " banner-action"}>
              <span>{w.msg}</span>
              {w.to && (
                <Link to={w.to} className="banner-cta">
                  {w.cta ?? "Open"} →
                </Link>
              )}
            </div>
          ))}
        </div>
      )}

      <div className="grid cols-4">
        <Stat n={active} label="Active Briefs" sub={`${totalBriefs} total`} to="/briefs" />
        <Stat n={running} label="Running now" sub={`${inReview} in review`} to="/runs" tone={running ? "info" : undefined} />
        <Stat n={attention} label="Needs Attention" to="/runs" tone={attention ? "warn" : undefined} />
        <Stat n={done} label="Completed" />
      </div>

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
            <span className="muted">Crew</span>
            <span>{crew} Operative{crew === 1 ? "" : "s"} · <Link to="/agents" className="link">manage</Link></span>
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

function Stat({ n, label, sub, to, tone }: { n: number; label: string; sub?: string; to?: string; tone?: "warn" | "info" }) {
  const color = tone === "warn" && n > 0 ? "var(--warn)" : tone === "info" && n > 0 ? "var(--info)" : undefined;
  const body = (
    <div className="card stat-card">
      <div className="stat" style={color ? { color } : undefined}>{n}</div>
      <div className="stat-label">{label}</div>
      {sub && <div className="muted" style={{ fontSize: 11, marginTop: 2 }}>{sub}</div>}
    </div>
  );
  return to ? <Link to={to}>{body}</Link> : body;
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
