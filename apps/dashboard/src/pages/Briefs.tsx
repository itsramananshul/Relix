import { useState } from "react";
import { api, tryGet } from "../api";
import { asArray, Section, useAsync } from "../components/common";

interface Card {
  task_id?: string;
  id?: string;
  title?: string;
  board_status?: string;
  priority?: string;
  assignee_agent_id?: string | null;
  mandate_id?: string | null;
}

const COLUMNS = ["backlog", "todo", "in_progress", "in_review", "done"];
const COLUMN_LABEL: Record<string, string> = {
  backlog: "Backlog",
  todo: "To do",
  in_progress: "In progress",
  in_review: "In review",
  done: "Done",
};

function cardId(c: Card): string {
  return c.task_id ?? c.id ?? "";
}

export function Briefs() {
  const [creating, setCreating] = useState(false);
  const [title, setTitle] = useState("");
  const [priority, setPriority] = useState("normal");
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);

  const { data, loading, reload } = useAsync(async () => {
    const byCol: Record<string, Card[]> = {};
    await Promise.all(
      COLUMNS.map(async (col) => {
        byCol[col] = asArray<Card>(await tryGet<Card[]>(`/v1/spine/board/${col}?limit=50`, []));
      }),
    );
    return byCol;
  }, []);

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

  return (
    <div className="grid">
      <Section
        title="Issue board"
        action={
          <button className="btn" onClick={() => setCreating((v) => !v)}>
            {creating ? "Cancel" : "+ New Brief"}
          </button>
        }
      >
        {banner && <div className={"banner " + banner.kind}>{banner.msg}</div>}

        {creating && (
          <div className="card" style={{ marginBottom: 14 }}>
            <div className="row wrap">
              <input
                className="input"
                style={{ flex: 3, minWidth: 240 }}
                placeholder="Brief title…"
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
        ) : (
          <div className="board">
            {COLUMNS.map((col) => {
              const cards = data?.[col] ?? [];
              return (
                <div className="board-col" key={col}>
                  <h4>
                    {COLUMN_LABEL[col]} <span className="muted">{cards.length}</span>
                  </h4>
                  {cards.map((c) => (
                    <div className="board-card" key={cardId(c)}>
                      <div className="t">{c.title ?? "(untitled)"}</div>
                      <div className="m">
                        {c.priority && <span>{c.priority}</span>}
                        {c.assignee_agent_id && <span>· {c.assignee_agent_id.slice(0, 8)}</span>}
                      </div>
                      <select
                        className="select"
                        style={{ marginTop: 8, fontSize: 11, padding: "3px 6px" }}
                        value={col}
                        onChange={(e) => move(c, e.target.value)}
                      >
                        {COLUMNS.map((s) => (
                          <option key={s} value={s}>
                            → {COLUMN_LABEL[s]}
                          </option>
                        ))}
                      </select>
                    </div>
                  ))}
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
