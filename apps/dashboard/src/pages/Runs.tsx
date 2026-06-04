import { Fragment, useState } from "react";
import { api, tryGet } from "../api";
import { Empty, Section, useAsync } from "../components/common";

interface Adapter { name?: string; display_name?: string; probe?: { status?: string } }

// A durable run record from the `brief_runs` ledger (`/v1/runs`).
interface RunRecord {
  run_id?: string;
  brief_id?: string;
  agent_id?: string;
  rig?: string;
  status?: string;
  started_at?: number;
  finished_at?: number;
  duration_secs?: number;
  summary?: string;
  workspace?: string;
  workspace_context?: string;
  workspace_files?: number;
  workspace_bytes?: number;
}

// One transcript event (`/v1/runs/:id/events`).
interface RunEvent {
  event_id?: number;
  ts?: number;
  kind?: string;
  source?: string;
  message?: string;
  payload_json?: string;
}

// Short label for the scoped per-run workspace: the leaf folder (the
// run_id segment), with the full path on hover. "inherited CWD" when a
// run executed without a scoped workspace (legacy / inherit mode).
function wsLabel(ws?: string): string {
  if (!ws) return "inherited CWD";
  const parts = ws.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? ws;
}

// Compact "empty" / "copy_repo · 12 files · 34 KB" context badge.
function ctxLabel(r: RunRecord): string {
  if (!r.workspace_context) return "—";
  if (r.workspace_context !== "copy_repo") return r.workspace_context;
  const files = r.workspace_files ?? 0;
  const kb = Math.round((r.workspace_bytes ?? 0) / 1024);
  return `copy_repo · ${files} files · ${kb} KB`;
}

// Run status → badge tone. `running` is in-flight; the rest are terminal.
const TONE: Record<string, string> = {
  running: "in_progress",
  done: "done",
  failed: "blocked",
  cancelled: "blocked",
  continued: "todo",
};

// Transcript event kind → a small color cue (no theme change, just dots).
const EVENT_TONE: Record<string, string> = {
  error: "#c0392b",
  permission_denied: "#c0392b",
  failed: "#c0392b",
  cancelled: "#c0392b",
  cancel_requested: "#b9770e",
  result: "#1e7e34",
  assistant_message: "#2d6cdf",
  tool_use: "#7d3cc0",
  command: "#7d3cc0",
  file_change: "#7d3cc0",
};

function fmtDuration(r: RunRecord): string {
  if (r.status === "running") {
    const s = Math.max(0, Math.floor(Date.now() / 1000) - (r.started_at ?? 0));
    return `${s}s…`;
  }
  if (typeof r.duration_secs === "number") return `${r.duration_secs}s`;
  return "—";
}

const FILTERS = ["all", "running", "done", "failed", "cancelled", "continued"] as const;

export function Runs() {
  const [filter, setFilter] = useState<(typeof FILTERS)[number]>("all");
  const [expanded, setExpanded] = useState<string | null>(null);
  const [events, setEvents] = useState<RunEvent[]>([]);
  const [eventsLoading, setEventsLoading] = useState(false);
  const [banner, setBanner] = useState<string | null>(null);

  const { data, loading, error, reload } = useAsync(async () => {
    const [runs, adapters] = await Promise.all([
      tryGet<RunRecord[]>("/v1/runs", []),
      tryGet<Adapter[]>("/v1/adapters", []),
    ]);
    return {
      runs: Array.isArray(runs) ? runs : [],
      adapters: Array.isArray(adapters) ? adapters : [],
    };
  }, []);

  async function loadEvents(runId: string) {
    setEventsLoading(true);
    try {
      const ev = await tryGet<RunEvent[]>(`/v1/runs/${encodeURIComponent(runId)}/events`, []);
      setEvents(Array.isArray(ev) ? ev : []);
    } finally {
      setEventsLoading(false);
    }
  }

  async function toggle(runId: string) {
    if (expanded === runId) {
      setExpanded(null);
      return;
    }
    setExpanded(runId);
    setEvents([]);
    await loadEvents(runId);
  }

  async function cancel(runId: string) {
    setBanner(null);
    try {
      const r = await api.post<{ active?: boolean; note?: string }>(
        `/v1/runs/${encodeURIComponent(runId)}/cancel`,
        {},
      );
      setBanner(r.active ? "Cancellation signalled — the run will report cancelled." : `Cancel requested: ${r.note ?? "no live process"}`);
      reload();
      if (expanded === runId) await loadEvents(runId);
    } catch (e) {
      setBanner(e instanceof Error ? e.message : "Cancel failed");
    }
  }

  const allRuns = data?.runs ?? [];
  const runs = filter === "all" ? allRuns : allRuns.filter((r) => r.status === filter);
  const adaptersAvail = (data?.adapters ?? []).filter((a) => a.probe?.status === "available");
  const activeCount = allRuns.filter((r) => r.status === "running").length;
  const COLS = 10;

  return (
    <div className="grid">
      <Section
        title="Active runs"
        action={<button className="btn ghost sm" onClick={reload}>Refresh</button>}
      >
        {error && <div className="banner err">{error}</div>}
        {banner && <div className="banner info">{banner}</div>}
        <div className={"banner " + (adaptersAvail.length ? "ok" : "info")}>
          {adaptersAvail.length
            ? `${adaptersAvail.length} agent adapter(s) available: ${adaptersAvail.map((a) => a.name).join(", ")}.`
            : "No agent adapters installed — install a coding-agent CLI (Claude, Codex) to execute Briefs. See Settings."}
        </div>
        {activeCount > 0 && (
          <div className="banner info">{activeCount} run(s) in flight — click a run to follow its transcript; refresh to update.</div>
        )}

        <div className="card">
          <div className="row" style={{ marginBottom: 8 }}>
            <h3 style={{ margin: 0 }}>Execution runs</h3>
            <div className="spacer" style={{ flex: 1 }} />
            <div className="row" style={{ gap: 4 }}>
              {FILTERS.map((f) => (
                <button
                  key={f}
                  className={"btn sm " + (filter === f ? "" : "ghost")}
                  onClick={() => setFilter(f)}
                >
                  {f}
                </button>
              ))}
            </div>
          </div>
          {loading ? (
            <div className="loading">Loading runs…</div>
          ) : runs.length === 0 ? (
            <Empty>
              {filter === "all"
                ? "No runs yet. Hit “Run” on a Brief to execute it through its adapter."
                : `No ${filter} runs.`}
            </Empty>
          ) : (
            <table className="table">
              <thead>
                <tr>
                  <th></th>
                  <th>Status</th>
                  <th>Adapter</th>
                  <th>Brief</th>
                  <th>Operative</th>
                  <th>Workspace</th>
                  <th>Context</th>
                  <th>Result</th>
                  <th>Duration</th>
                  <th>Started</th>
                </tr>
              </thead>
              <tbody>
                {runs.map((r, i) => {
                  const rid = r.run_id ?? "";
                  const open = expanded === rid;
                  return (
                    <Fragment key={rid || i}>
                      <tr style={{ cursor: "pointer" }} onClick={() => rid && toggle(rid)}>
                        <td className="muted">{open ? "▾" : "▸"}</td>
                        <td><span className={"badge " + (TONE[r.status ?? ""] ?? "todo")}>{r.status ?? "—"}</span></td>
                        <td className="muted">{r.rig || "—"}</td>
                        <td className="mono">{(r.brief_id ?? "").slice(0, 12)}</td>
                        <td className="muted">{(r.agent_id ?? "").slice(0, 10) || "—"}</td>
                        <td className="mono" style={{ fontSize: 11 }} title={r.workspace ?? "ran in the coordinator working directory"}>{wsLabel(r.workspace)}</td>
                        <td className="muted" style={{ fontSize: 11 }}>{ctxLabel(r)}</td>
                        <td className="muted" style={{ maxWidth: 260, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{r.summary || (r.status === "running" ? "…" : "—")}</td>
                        <td className="muted">{fmtDuration(r)}</td>
                        <td className="muted">{r.started_at ? new Date(r.started_at * 1000).toLocaleTimeString() : ""}</td>
                      </tr>
                      {open && (
                        <tr>
                          <td colSpan={COLS} style={{ background: "rgba(0,0,0,0.02)" }}>
                            <div className="row" style={{ marginBottom: 6 }}>
                              <strong style={{ fontSize: 12 }}>Transcript</strong>
                              <span className="muted mono" style={{ fontSize: 11, marginLeft: 8 }}>{rid}</span>
                              <div className="spacer" style={{ flex: 1 }} />
                              <button className="btn ghost sm" onClick={(e) => { e.stopPropagation(); loadEvents(rid); }}>Refresh</button>
                              {r.status === "running" && (
                                <button className="btn sm" style={{ marginLeft: 6 }} onClick={(e) => { e.stopPropagation(); cancel(rid); }}>Cancel run</button>
                              )}
                            </div>
                            {r.workspace && <div className="muted mono" style={{ fontSize: 11, marginBottom: 6 }}>workspace: {r.workspace}</div>}
                            {eventsLoading ? (
                              <div className="loading">Loading transcript…</div>
                            ) : events.length === 0 ? (
                              <div className="muted" style={{ fontSize: 12 }}>No transcript events recorded.</div>
                            ) : (
                              <div style={{ maxHeight: 320, overflow: "auto", fontSize: 12 }}>
                                {events.map((ev, j) => (
                                  <div key={ev.event_id ?? j} style={{ padding: "2px 0", borderBottom: "1px solid rgba(0,0,0,0.05)" }}>
                                    <span style={{ display: "inline-block", width: 8, height: 8, borderRadius: 4, marginRight: 6, background: EVENT_TONE[ev.kind ?? ""] ?? "#999" }} />
                                    <span className="muted" style={{ fontSize: 10 }}>{ev.ts ? new Date(ev.ts * 1000).toLocaleTimeString() : ""}</span>{" "}
                                    <span className="mono" style={{ fontSize: 11 }}>{ev.source}/{ev.kind}</span>{" — "}
                                    <span style={{ whiteSpace: "pre-wrap", wordBreak: "break-word" }}>{ev.message}</span>
                                    {ev.payload_json && <div className="muted mono" style={{ fontSize: 10, paddingLeft: 14, whiteSpace: "pre-wrap", wordBreak: "break-word" }}>{ev.payload_json}</div>}
                                  </div>
                                ))}
                              </div>
                            )}
                          </td>
                        </tr>
                      )}
                    </Fragment>
                  );
                })}
              </tbody>
            </table>
          )}
        </div>
      </Section>
    </div>
  );
}
