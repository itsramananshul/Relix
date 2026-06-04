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

const COLUMNS = ["backlog", "todo", "in_progress", "in_review", "done"];

export function Overview() {
  const { data } = useAsync(async () => {
    const [board, inbox, roster, info, events] = await Promise.all([
      tryGet<BoardSummary>("/v1/spine/board", {}),
      tryGet<Inbox>("/v1/spine/inbox?limit=50", {}),
      tryGet<Roster>("/v1/spine/roster", {}),
      tryGet<Record<string, unknown>>("/v1/info", {}),
      tryGet<unknown>("/v1/tasks/events/recent?limit=12", {}),
    ]);
    return { board, inbox, roster, info, events: extractList<EventRow>(events) };
  }, []);

  const board = data?.board ?? {};
  const inbox = data?.inbox ?? {};
  const active = (board.todo ?? 0) + (board.in_progress ?? 0) + (board.in_review ?? 0);
  const done = board.done ?? 0;
  const attention = (inbox.blocked?.length ?? 0) + (inbox.overdue?.length ?? 0) + (inbox.unassigned?.length ?? 0);
  const crew = data?.roster?.active ?? data?.roster?.total ?? 0;

  return (
    <div className="grid">
      <div className="grid cols-4">
        <Stat n={active} label="Active Briefs" to="/briefs" />
        <Stat n={crew} label="Crew (Operatives)" to="/agents" />
        <Stat n={attention} label="Needs Attention" to="/runs" tone={attention ? "warn" : undefined} />
        <Stat n={done} label="Completed" />
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
            <table className="table">
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
            <span className="muted">Spine board empty.</span>
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

function Stat({ n, label, to, tone }: { n: number; label: string; to?: string; tone?: "warn" }) {
  const body = (
    <div className="card">
      <div className="stat" style={tone === "warn" && n > 0 ? { color: "var(--warn)" } : undefined}>{n}</div>
      <div className="stat-label">{label}</div>
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
