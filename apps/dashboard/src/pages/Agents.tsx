import { tryGet } from "../api";
import { Badge, Empty, extractList, Section, useAsync } from "../components/common";

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

export function Agents() {
  // The Operative list is /v1/agents/access ({agents:[…]}); /spine/roster
  // is only a count summary.
  const { data, loading, error } = useAsync(
    async () => extractList<Agent>(await tryGet<unknown>("/v1/agents/access", {}), ["agents", "operatives"]),
    [],
  );

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
