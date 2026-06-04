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
interface EventRow { task_id?: string; event_type?: string; ts?: number }

function extractTasks(v: unknown): Task[] {
  if (Array.isArray(v)) return v as Task[];
  if (v && typeof v === "object") {
    const o = v as Record<string, unknown>;
    for (const k of ["tasks", "items", "results"]) if (Array.isArray(o[k])) return o[k] as Task[];
  }
  return [];
}

export function Runs() {
  const { data, loading, error, reload } = useAsync(async () => {
    const [tasks, events, stuck] = await Promise.all([
      tryGet<unknown>("/v1/tasks?limit=50", []),
      tryGet<unknown>("/v1/tasks/events/recent?limit=20", {}),
      tryGet<unknown>("/v1/tasks/stuck?limit=20", {}),
    ]);
    return {
      tasks: extractTasks(tasks),
      events: extractList<EventRow>(events),
      stuck: extractTasks(stuck),
    };
  }, []);

  const tasks = data?.tasks ?? [];

  return (
    <div className="grid">
      <Section
        title="Active runs"
        action={<button className="btn ghost sm" onClick={reload}>Refresh</button>}
      >
        {error && <div className="banner err">{error}</div>}
        {data?.stuck && data.stuck.length > 0 && (
          <div className="banner info">{data.stuck.length} run(s) look stuck — they may need recovery.</div>
        )}
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
