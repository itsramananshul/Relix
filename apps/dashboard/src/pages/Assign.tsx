import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { api, tryGet } from "../api";
import { asArray, Section, useAsync } from "../components/common";

interface Card { task_id?: string; id?: string; title?: string; assignee_agent_id?: string | null }
interface Agent { agent_id?: string; id?: string; name?: string; display_name?: string; role?: string }
interface Inbox { unassigned?: Card[] }

export function Assign() {
  const [brief, setBrief] = useState("");
  const [agent, setAgent] = useState("");
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);

  // The assignable Operatives are the real roster (/v1/spine/operatives).
  const { data, loading, reload } = useAsync(async () => {
    const [inbox, ops] = await Promise.all([
      tryGet<Inbox>("/v1/spine/inbox?limit=100", {}),
      tryGet<Agent[]>("/v1/spine/operatives", []),
    ]);
    return {
      unassigned: asArray<Card>(inbox.unassigned),
      agents: Array.isArray(ops) ? ops : [],
    };
  }, []);

  const unassigned = useMemo(() => data?.unassigned ?? [], [data]);
  const agents = data?.agents ?? [];

  async function assign() {
    if (!brief || !agent) {
      setBanner({ kind: "err", msg: "Pick a Brief and an Operative." });
      return;
    }
    setBanner(null);
    try {
      await api.post(`/v1/spine/briefs/${encodeURIComponent(brief)}/set`, {
        field: "assignee",
        value: agent,
      });
      setBanner({ kind: "ok", msg: "Work assigned." });
      setBrief("");
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Assign failed" });
    }
  }

  return (
    <div className="grid">
      <Section title="Assign work">
        {banner && <div className={"banner " + banner.kind}>{banner.msg}</div>}
        <div className="card" style={{ maxWidth: 560 }}>
          {loading ? (
            <div className="loading">Loading…</div>
          ) : agents.length === 0 ? (
            <div className="empty">
              No Operatives yet. <Link to="/agents">Initialize your company</Link> to create the
              Founder, then come back to assign work.
            </div>
          ) : (
            <>
              <label className="field">
                <span>Brief (unassigned)</span>
                <select className="select" value={brief} onChange={(e) => setBrief(e.target.value)}>
                  <option value="">Select a Brief…</option>
                  {unassigned.map((c) => {
                    const id = c.task_id ?? c.id ?? "";
                    return (
                      <option key={id} value={id}>
                        {c.title ?? id.slice(0, 12)}
                      </option>
                    );
                  })}
                </select>
              </label>
              <label className="field">
                <span>Operative</span>
                <select className="select" value={agent} onChange={(e) => setAgent(e.target.value)}>
                  <option value="">Select an Operative…</option>
                  {agents.map((a) => {
                    const id = a.agent_id ?? a.id ?? "";
                    return (
                      <option key={id} value={id}>
                        {(a.name ?? id.slice(0, 10)) + (a.role ? ` — ${a.role}` : "")}
                      </option>
                    );
                  })}
                </select>
              </label>
              <button className="btn" onClick={assign} disabled={!brief || !agent}>
                Assign Brief
              </button>
              {unassigned.length === 0 && (
                <p className="muted" style={{ marginTop: 14 }}>
                  No unassigned Briefs right now. Create one on the Briefs board.
                </p>
              )}
            </>
          )}
        </div>
      </Section>
    </div>
  );
}
