import { tryGet } from "../api";
import { Badge, Empty, Section, useAsync } from "../components/common";

interface Agent {
  agent_id?: string;
  id?: string;
  name?: string;
  display_name?: string;
  role?: string;
  status?: string;
  reports_to?: string;
  tier?: string;
}

// The roster endpoint shape varies; pull the operatives list out of
// whatever wrapper it comes in.
function extractAgents(v: unknown): Agent[] {
  if (Array.isArray(v)) return v as Agent[];
  if (v && typeof v === "object") {
    const o = v as Record<string, unknown>;
    for (const k of ["operatives", "agents", "roster", "members", "active_agents", "crew"]) {
      if (Array.isArray(o[k])) return o[k] as Agent[];
    }
  }
  return [];
}

export function Agents() {
  const { data, loading, error } = useAsync(async () => {
    let agents = extractAgents(await tryGet<unknown>("/v1/spine/roster", {}));
    if (agents.length === 0) {
      agents = extractAgents(await tryGet<unknown>("/v1/agents/access", {}));
    }
    return agents;
  }, []);

  const agents = data ?? [];

  return (
    <Section title="Crew">
      {error && <div className="banner err">{error}</div>}
      <div className="card">
        {loading ? (
          <div className="loading">Loading crew…</div>
        ) : agents.length === 0 ? (
          <Empty>No Operatives yet. Hire crew through the company flow.</Empty>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Operative</th>
                <th>Role</th>
                <th>Status</th>
                <th>Reports to</th>
                <th>ID</th>
              </tr>
            </thead>
            <tbody>
              {agents.map((a, i) => {
                const id = a.agent_id ?? a.id ?? "";
                return (
                  <tr key={id || i}>
                    <td><strong>{a.name ?? a.display_name ?? id.slice(0, 10) ?? "operative"}</strong></td>
                    <td className="dim">{a.role ?? a.tier ?? "—"}</td>
                    <td><Badge status={a.status ?? "active"} /></td>
                    <td className="muted">{a.reports_to ?? "—"}</td>
                    <td className="mono">{id.slice(0, 12)}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>
    </Section>
  );
}
