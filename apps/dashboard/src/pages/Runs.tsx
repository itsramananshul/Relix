import { useState } from "react";
import { tryGet } from "../api";
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
  continued: "todo",
};

function fmtDuration(r: RunRecord): string {
  if (r.status === "running") {
    const s = Math.max(0, Math.floor(Date.now() / 1000) - (r.started_at ?? 0));
    return `${s}s…`;
  }
  if (typeof r.duration_secs === "number") return `${r.duration_secs}s`;
  return "—";
}

const FILTERS = ["all", "running", "done", "failed", "continued"] as const;

export function Runs() {
  const [filter, setFilter] = useState<(typeof FILTERS)[number]>("all");

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

  const allRuns = data?.runs ?? [];
  const runs = filter === "all" ? allRuns : allRuns.filter((r) => r.status === filter);
  const adaptersAvail = (data?.adapters ?? []).filter((a) => a.probe?.status === "available");
  const activeCount = allRuns.filter((r) => r.status === "running").length;

  return (
    <div className="grid">
      <Section
        title="Active runs"
        action={<button className="btn ghost sm" onClick={reload}>Refresh</button>}
      >
        {error && <div className="banner err">{error}</div>}
        <div className={"banner " + (adaptersAvail.length ? "ok" : "info")}>
          {adaptersAvail.length
            ? `${adaptersAvail.length} agent adapter(s) available: ${adaptersAvail.map((a) => a.name).join(", ")}.`
            : "No agent adapters installed — install a coding-agent CLI (Claude, Codex) to execute Briefs. See Settings."}
        </div>
        {activeCount > 0 && (
          <div className="banner info">{activeCount} run(s) in flight — runs execute in the background; refresh to watch them finish.</div>
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
                {runs.map((r, i) => (
                  <tr key={r.run_id ?? i}>
                    <td><span className={"badge " + (TONE[r.status ?? ""] ?? "todo")}>{r.status ?? "—"}</span></td>
                    <td className="muted">{r.rig || "—"}</td>
                    <td className="mono">{(r.brief_id ?? "").slice(0, 12)}</td>
                    <td className="muted">{(r.agent_id ?? "").slice(0, 10) || "—"}</td>
                    <td className="mono" style={{ fontSize: 11 }} title={r.workspace ?? "ran in the coordinator working directory (no scoped workspace)"}>{wsLabel(r.workspace)}</td>
                    <td className="muted" style={{ fontSize: 11 }}>{ctxLabel(r)}</td>
                    <td className="muted" style={{ maxWidth: 280, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{r.summary || (r.status === "running" ? "…" : "—")}</td>
                    <td className="muted">{fmtDuration(r)}</td>
                    <td className="muted">{r.started_at ? new Date(r.started_at * 1000).toLocaleTimeString() : ""}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </Section>
    </div>
  );
}
