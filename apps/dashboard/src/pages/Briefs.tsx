import { useState } from "react";
import { Link } from "react-router-dom";
import { api, tryGet } from "../api";
import { asArray, extractList, Section, useAsync } from "../components/common";
import { BriefDetail } from "../components/BriefDetail";

interface Card {
  task_id?: string;
  id?: string;
  title?: string;
  board_status?: string;
  priority?: string;
  assignee_agent_id?: string | null;
  mandate_id?: string | null;
}

interface Operative {
  agent_id?: string;
  name?: string;
  role?: string;
  rig?: string | null;
}

interface Adapter {
  name?: string;
  probe?: { status?: string };
}

// One run record from the shared ledger (`/v1/runs`).
interface RunRow {
  run_id?: string;
  brief_id?: string;
  status?: string;
  trigger?: string;
  rig?: string;
  started_at?: number;
  review?: string;
  apply_status?: string;
  applied_files?: number;
}

interface RunReport {
  brief_id: string;
  status: string;
  rig: string;
  summary: string;
  install_hint?: string | null;
  run_id?: string | null;
  workspace?: string | null;
  workspace_context?: string | null;
  workspace_files?: number | null;
}

const REFUSALS: Record<string, string> = {
  running: "run started — executing in the background",
  unassigned: "assign an Operative first",
  no_adapter: "no adapter configured for this Operative",
  adapter_unavailable: "adapter not installed",
  already_running: "already running",
  not_found: "brief not found",
  workspace_error: "could not prepare a run workspace",
  workspace_context_error: "could not copy project context into the workspace",
  done: "run complete",
  failed: "run failed",
  continued: "run continued (more work to do)",
};

const COLUMNS = ["backlog", "todo", "in_progress", "in_review", "done"];
const COLUMN_LABEL: Record<string, string> = {
  backlog: "Backlog",
  todo: "To do",
  in_progress: "In progress",
  in_review: "In review",
  done: "Done",
};
const RUN_TONE: Record<string, string> = {
  running: "in_progress",
  done: "done",
  failed: "blocked",
  cancelled: "blocked",
  refused: "blocked",
  continued: "todo",
};

function cardId(c: Card): string {
  return c.task_id ?? c.id ?? "";
}

// The product state a Brief's latest run is in — drives the small status
// chip + the "what next" hint on the card.
function runOutcome(r: RunRow): { label: string; tone: string } | null {
  if (r.apply_status === "applied") return { label: "applied", tone: "done" };
  if (r.apply_status === "conflicted") return { label: "apply conflicted", tone: "blocked" };
  if (r.apply_status === "failed") return { label: "apply failed", tone: "blocked" };
  if (r.status === "done" && r.review === "pending_review") return { label: "needs review", tone: "in_progress" };
  if (r.status === "done" && r.review === "accepted") return { label: "ready to apply", tone: "todo" };
  if (r.status === "done" && r.review === "rejected") return { label: "rejected", tone: "blocked" };
  if (r.status === "failed") return { label: "failed", tone: "blocked" };
  if (r.status === "running") return { label: "running", tone: "in_progress" };
  return null;
}

// The clearest verb for the deep link into a Brief's latest run, based on
// what the operator would do next there.
function runAction(r: RunRow): string {
  if (r.status === "done" && r.review === "pending_review") return "Review run";
  if (r.status === "done" && r.review === "accepted" && r.apply_status !== "applied") return "Apply run";
  return "View run";
}

export function Briefs() {
  const [creating, setCreating] = useState(false);
  const [title, setTitle] = useState("");
  const [priority, setPriority] = useState("normal");
  const [mandateFilter, setMandateFilter] = useState("all");
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);
  // The Brief whose detail/Chronicle panel is open (null = none).
  const [selected, setSelected] = useState<string | null>(null);

  const { data, loading, error, reload } = useAsync(async () => {
    const byCol: Record<string, Card[]> = {};
    const [, ops, adapters, runs, mandates] = await Promise.all([
      Promise.all(
        COLUMNS.map(async (col) => {
          byCol[col] = asArray<Card>(await tryGet<Card[]>(`/v1/spine/board/${col}?limit=50`, []));
        }),
      ),
      tryGet<Operative[]>("/v1/spine/operatives", []),
      tryGet<Adapter[]>("/v1/adapters", []),
      tryGet<RunRow[]>("/v1/runs", []),
      tryGet<unknown>("/v1/spine/mandates?limit=50", {}),
    ]);
    return {
      board: byCol,
      operatives: Array.isArray(ops) ? ops : [],
      adapters: Array.isArray(adapters) ? adapters : [],
      runs: Array.isArray(runs) ? runs : [],
      mandates: extractList<{ mandate_id?: string; id?: string; title?: string }>(mandates, ["mandates"]),
    };
  }, []);

  const operatives = data?.operatives ?? [];
  const adapters = data?.adapters ?? [];
  const runs = data?.runs ?? [];
  const mandates = data?.mandates ?? [];
  const mandateTitle = new Map(mandates.map((m) => [m.mandate_id ?? m.id ?? "", m.title ?? ""]));

  const opById = new Map(operatives.map((o) => [o.agent_id ?? "", o]));
  const adapterStatus = new Map(adapters.map((a) => [a.name ?? "", a.probe?.status ?? "unknown"]));
  const availCount = adapters.filter((a) => a.probe?.status === "available").length;
  // `/v1/runs` is newest-first → the FIRST run we see per Brief is its latest.
  const latestRun = new Map<string, RunRow>();
  for (const r of runs) {
    const b = r.brief_id ?? "";
    if (b && !latestRun.has(b)) latestRun.set(b, r);
  }

  async function assign(c: Card, agentId: string) {
    setBanner(null);
    try {
      await api.post(`/v1/spine/briefs/${encodeURIComponent(cardId(c))}/set`, {
        field: "assignee",
        value: agentId,
      });
      setBanner({ kind: "ok", msg: agentId ? "Operative assigned." : "Operative cleared." });
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Assign failed" });
    }
  }

  async function create() {
    if (!title.trim()) return;
    setBanner(null);
    try {
      await api.post("/v1/spine/briefs", { title: title.trim(), priority });
      setTitle("");
      setCreating(false);
      setBanner({ kind: "ok", msg: "Brief created." });
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Create failed" });
    }
  }

  async function move(c: Card, status: string) {
    setBanner(null);
    try {
      await api.post(`/v1/spine/briefs/${encodeURIComponent(cardId(c))}/move`, { status });
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Move failed" });
    }
  }

  async function run(c: Card, rig?: string) {
    setBanner({ kind: "info", msg: `Running ${c.title ?? "brief"}${rig ? ` (${rig})` : ""}…` });
    try {
      const r = await api.post<RunReport>(
        `/v1/spine/briefs/${encodeURIComponent(cardId(c))}/run`,
        rig ? { rig } : {},
      );
      const accepted = r.status === "running" || r.status === "done";
      const refusal = ["unassigned", "no_adapter", "adapter_unavailable", "already_running", "not_found"].includes(r.status);
      const kind = accepted ? "ok" : refusal ? "info" : "err";
      const label = REFUSALS[r.status] ?? r.status;
      let msg = `${c.title ?? "Brief"}: ${label}`;
      if (r.rig) msg += ` · adapter ${r.rig}`;
      if (r.summary && r.status !== "running") msg += ` — ${r.summary}`;
      if (r.install_hint) msg += ` (${r.install_hint})`;
      if (r.status === "running") msg += " — see Active Runs";
      setBanner({ kind, msg });
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Run failed" });
    }
  }

  // Why a Brief cannot run right now (null = it can). Used to disable the
  // Run button with a helpful reason rather than letting it silently refuse.
  function runBlock(c: Card): string | null {
    const op = c.assignee_agent_id ? opById.get(c.assignee_agent_id) : undefined;
    if (!c.assignee_agent_id) return "Assign an Operative first";
    if (!op?.rig) return "Operative has no adapter — set one on the Crew page";
    if (adapterStatus.get(op.rig) && adapterStatus.get(op.rig) !== "available")
      return `Adapter "${op.rig}" is not available — see Settings`;
    if (latestRun.get(cardId(c))?.status === "running") return "Already running";
    return null;
  }

  const initialized = operatives.length > 0;

  return (
    <div className="grid">
      <Section
        title="Issue board"
        action={
          <div className="row" style={{ gap: 8 }}>
            {mandates.length > 0 && (
              <select className="select" style={{ width: 180, fontSize: 12 }} value={mandateFilter} onChange={(e) => setMandateFilter(e.target.value)} title="Filter by Mandate">
                <option value="all">All mandates</option>
                <option value="none">— no mandate —</option>
                {mandates.map((m) => (
                  <option key={m.mandate_id ?? m.id} value={m.mandate_id ?? m.id}>{m.title ?? (m.mandate_id ?? m.id ?? "").slice(0, 10)}</option>
                ))}
              </select>
            )}
            <button className="btn" onClick={() => setCreating((v) => !v)}>
              {creating ? "Cancel" : "+ New Brief"}
            </button>
          </div>
        }
      >
        {error && (
          <div className="banner err">Could not load the board: {error}. <span className="link" onClick={reload}>Retry</span></div>
        )}
        {banner && <div className={"banner " + banner.kind}>{banner.msg}</div>}

        {/* Brief detail + Chronicle — opens when a card title is clicked. */}
        {selected && (
          <BriefDetail
            briefId={selected}
            onClose={() => setSelected(null)}
            onChanged={reload}
          />
        )}

        {!loading && !initialized && (
          <div className="banner info banner-action">
            <span>No Operatives yet — create Briefs now, but to assign + run them you need a Founder.</span>
            <Link to="/agents" className="banner-cta">Initialize company →</Link>
          </div>
        )}
        {!loading && initialized && availCount === 0 && (
          <div className="banner info banner-action">
            <span>No agent adapter is available — Briefs can be assigned but a Run needs an installed + authenticated adapter (echo always works).</span>
            <Link to="/settings" className="banner-cta">Open Settings →</Link>
          </div>
        )}

        {creating && (
          <div className="card" style={{ marginBottom: 14 }}>
            <div className="row wrap">
              <input
                className="input"
                style={{ flex: 3, minWidth: 240 }}
                placeholder="Brief title — what needs doing?"
                value={title}
                autoFocus
                onChange={(e) => setTitle(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && create()}
              />
              <select className="select" style={{ flex: 1, minWidth: 120 }} value={priority} onChange={(e) => setPriority(e.target.value)}>
                <option value="low">low</option>
                <option value="normal">normal</option>
                <option value="high">high</option>
                <option value="urgent">urgent</option>
              </select>
              <button className="btn" onClick={create}>Create</button>
            </div>
          </div>
        )}

        {loading ? (
          <div className="loading">Loading board…</div>
        ) : COLUMNS.every((c) => (data?.board?.[c] ?? []).length === 0) ? (
          <div className="empty">
            No Briefs yet. Click <strong>+ New Brief</strong> to create your first unit of work,
            then assign it to an Operative and run it.
          </div>
        ) : (
          <div className="board">
            {COLUMNS.map((col) => {
              const cards = (data?.board?.[col] ?? []).filter((c) =>
                mandateFilter === "all"
                  ? true
                  : mandateFilter === "none"
                    ? !c.mandate_id
                    : c.mandate_id === mandateFilter,
              );
              return (
                <div className="board-col" key={col}>
                  <h4>
                    {COLUMN_LABEL[col]} <span className="muted">{cards.length}</span>
                  </h4>
                  {cards.map((c) => {
                    const op = c.assignee_agent_id ? opById.get(c.assignee_agent_id) : undefined;
                    const lr = latestRun.get(cardId(c));
                    const outcome = lr ? runOutcome(lr) : null;
                    const block = runBlock(c);
                    const mTitle = c.mandate_id ? (mandateTitle.get(c.mandate_id) || c.mandate_id.slice(0, 8)) : null;
                    return (
                      <div className={"board-card" + (selected === cardId(c) ? " selected" : "")} key={cardId(c)}>
                        <div
                          className="t"
                          style={{ cursor: "pointer" }}
                          title="Open Brief detail + Chronicle"
                          onClick={() => setSelected(selected === cardId(c) ? null : cardId(c))}
                        >
                          {c.title ?? "(untitled)"}
                        </div>
                        {mTitle && (
                          <Link to="/mandates" className="muted" style={{ fontSize: 10, display: "block", marginBottom: 4 }} title={"part of mandate " + c.mandate_id}>◎ {mTitle}</Link>
                        )}
                        <div className="m">
                          {c.priority && <span>{c.priority}</span>}
                          {op ? (
                            <span title={c.assignee_agent_id ?? ""}>
                              · {op.name ?? "operative"}
                              {op.role === "founder" ? " (Founder)" : ""}
                              {op.rig ? ` · ${op.rig}` : " · no adapter"}
                            </span>
                          ) : c.assignee_agent_id ? (
                            <span className="mono">· {c.assignee_agent_id.slice(0, 8)}</span>
                          ) : (
                            <span className="muted">· unassigned</span>
                          )}
                        </div>

                        {lr && (
                          <div className="card-run">
                            <span className={"badge " + (RUN_TONE[lr.status ?? ""] ?? "todo")}>{lr.status ?? "—"}</span>
                            <span className="muted" style={{ fontSize: 10 }}>{lr.trigger === "heartbeat" ? "auto" : lr.trigger ?? "manual"}</span>
                            {outcome && <span className={"badge " + outcome.tone} style={{ fontSize: 10 }}>{outcome.label}</span>}
                            {(lr.applied_files ?? 0) > 0 && <span className="muted" style={{ fontSize: 10 }}>{lr.applied_files} applied</span>}
                            {lr.run_id && (
                              <Link to={`/runs?run=${encodeURIComponent(lr.run_id)}`} className="link" style={{ fontSize: 11, marginLeft: "auto" }}>
                                {runAction(lr)} →
                              </Link>
                            )}
                          </div>
                        )}

                        <label className="row" style={{ marginTop: 8 }}>
                          <select
                            className="select"
                            style={{ fontSize: 11, padding: "3px 6px", width: "100%" }}
                            value={c.assignee_agent_id ?? ""}
                            onChange={(e) => assign(c, e.target.value)}
                            title="Assign an Operative"
                          >
                            <option value="">— unassigned —</option>
                            {operatives.map((o) => (
                              <option key={o.agent_id} value={o.agent_id}>
                                {o.name}{o.role === "founder" ? " (Founder)" : ""}{o.rig ? ` · ${o.rig}` : ""}
                              </option>
                            ))}
                          </select>
                        </label>
                        <div className="row" style={{ marginTop: 6, gap: 6 }}>
                          <select
                            className="select"
                            style={{ fontSize: 11, padding: "3px 6px", flex: 1 }}
                            value={col}
                            onChange={(e) => move(c, e.target.value)}
                            title="Move to a board column"
                          >
                            {COLUMNS.map((s) => (
                              <option key={s} value={s}>→ {COLUMN_LABEL[s]}</option>
                            ))}
                          </select>
                          <button
                            className="btn sm"
                            disabled={!!block}
                            title={block ?? "Run this Brief through its Operative's adapter now"}
                            onClick={() => run(c)}
                          >
                            Run
                          </button>
                          {/* Golden-path smoke: echo always works once a Brief
                              is assigned — even if the real adapter is missing. */}
                          <button
                            className="btn ghost sm"
                            disabled={!c.assignee_agent_id || latestRun.get(cardId(c))?.status === "running"}
                            title={
                              !c.assignee_agent_id
                                ? "Assign an Operative first"
                                : latestRun.get(cardId(c))?.status === "running"
                                  ? "Already running"
                                  : "Run with the echo Rig (no real adapter needed) — verifies the pipeline end to end"
                            }
                            onClick={() => run(c, "echo")}
                          >
                            echo
                          </button>
                        </div>
                        {block && <div className="muted" style={{ fontSize: 10, marginTop: 4 }}>⚠ {block} — or hit <strong>echo</strong> to smoke the pipeline.</div>}
                      </div>
                    );
                  })}
                  {cards.length === 0 && <div className="muted" style={{ fontSize: 12, padding: 6 }}>empty</div>}
                </div>
              );
            })}
          </div>
        )}
      </Section>
    </div>
  );
}
