import { tryGet } from "../api";
import { Badge, Empty, extractList, Section, useAsync } from "../components/common";

interface Task {
  task_id?: string;
  id?: string;
  title?: string;
  status?: string;
  board_status?: string;
  updated_at?: number;
  assignee_agent_id?: string | null;
}
interface EventRow { task_id?: string; event_type?: string; ts?: number; payload?: string }
interface Adapter { name?: string; display_name?: string; probe?: { status?: string } }

function extractTasks(v: unknown): Task[] {
  if (Array.isArray(v)) return v as Task[];
  if (v && typeof v === "object") {
    const o = v as Record<string, unknown>;
    for (const k of ["tasks", "items", "results"]) if (Array.isArray(o[k])) return o[k] as Task[];
  }
  return [];
}

// Execution-run lifecycle events the dispatcher / brief.run writes.
const RUN_EVENTS: Record<string, string> = {
  "brief.run_started": "running",
  "brief.shift_done": "succeeded",
  "brief.dispatch_failed": "failed",
  "brief.continued": "continued",
  "brief.budget_refused": "over budget",
};

// Run events carry `[adapter] detail` in their payload — pull the adapter.
function parseAdapter(payload?: string): { adapter: string; detail: string } {
  const m = payload?.match(/^\[([^\]]+)\]\s*([\s\S]*)$/);
  return m ? { adapter: m[1], detail: m[2] } : { adapter: "", detail: payload ?? "" };
}

export function Runs() {
  const { data, loading, error, reload } = useAsync(async () => {
    const [tasks, events, stuck, adapters] = await Promise.all([
      tryGet<unknown>("/v1/tasks?limit=50", []),
      tryGet<unknown>("/v1/tasks/events/recent?limit=40", {}),
      tryGet<unknown>("/v1/tasks/stuck?limit=20", {}),
      tryGet<Adapter[]>("/v1/adapters", []),
    ]);
    return {
      tasks: extractTasks(tasks),
      events: extractList<EventRow>(events),
      stuck: extractTasks(stuck),
      adapters: Array.isArray(adapters) ? adapters : [],
    };
  }, []);

  const runEvents = (data?.events ?? []).filter((e) => e.event_type && RUN_EVENTS[e.event_type]);
  const adaptersAvail = (data?.adapters ?? []).filter((a) => a.probe?.status === "available");

  const tasks = data?.tasks ?? [];

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
        {data?.stuck && data.stuck.length > 0 && (
          <div className="banner info">{data.stuck.length} run(s) look stuck — they may need recovery.</div>
        )}

        <div className="card" style={{ marginBottom: 14 }}>
          <h3>Recent execution runs</h3>
          {loading ? (
            <div className="loading">Loading…</div>
          ) : runEvents.length === 0 ? (
            <Empty>No runs yet. Hit “Run” on a Brief to execute it through its adapter.</Empty>
          ) : (
            <table className="table">
              <thead>
                <tr><th>Status</th><th>Adapter</th><th>Brief</th><th>Detail</th><th>When</th></tr>
              </thead>
              <tbody>
                {runEvents.map((e, i) => {
                  const { adapter, detail } = parseAdapter(e.payload);
                  const status = RUN_EVENTS[e.event_type ?? ""] ?? e.event_type;
                  const tone = status === "succeeded" ? "done" : status === "failed" ? "blocked" : status === "running" ? "in_progress" : "todo";
                  return (
                    <tr key={i}>
                      <td><span className={"badge " + tone}>{status}</span></td>
                      <td className="muted">{adapter || "—"}</td>
                      <td className="mono">{(e.task_id ?? "").slice(0, 12)}</td>
                      <td className="muted" style={{ maxWidth: 360, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{detail}</td>
                      <td className="muted">{e.ts ? new Date(e.ts * 1000).toLocaleTimeString() : ""}</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          )}
        </div>

        <div className="card">
          {loading ? (
            <div className="loading">Loading runs…</div>
          ) : tasks.length === 0 ? (
            <Empty>No runs yet. Start a Brief to kick off execution.</Empty>
          ) : (
            <table className="table">
              <thead>
                <tr>
                  <th>Run</th>
                  <th>Status</th>
                  <th>Assignee</th>
                  <th>Updated</th>
                </tr>
              </thead>
              <tbody>
                {tasks.map((t, i) => {
                  const id = t.task_id ?? t.id ?? "";
                  return (
                    <tr key={id || i}>
                      <td><strong>{t.title ?? id.slice(0, 12)}</strong></td>
                      <td><Badge status={t.status ?? t.board_status} /></td>
                      <td className="muted">{t.assignee_agent_id ? t.assignee_agent_id.slice(0, 10) : "—"}</td>
                      <td className="muted">{t.updated_at ? new Date(t.updated_at * 1000).toLocaleString() : "—"}</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          )}
        </div>
      </Section>

      <div className="card">
        <h3>Activity stream</h3>
        {(data?.events ?? []).length === 0 ? (
          <Empty>No recent events.</Empty>
        ) : (
          <table className="table">
            <tbody>
              {(data?.events ?? []).map((e, i) => (
                <tr key={i}>
                  <td><span className="badge">{e.event_type ?? "event"}</span></td>
                  <td className="mono">{(e.task_id ?? "").slice(0, 12)}</td>
                  <td className="muted">{e.ts ? new Date(e.ts * 1000).toLocaleTimeString() : ""}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}
