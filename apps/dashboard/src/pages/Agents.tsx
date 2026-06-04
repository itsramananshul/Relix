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
  rig?: string | null;
}
interface Adapter { name?: string; display_name?: string; probe?: { status?: string; install_hint?: string | null } }

export function Agents() {
  // The Operative list is /v1/agents/access ({agents:[…]}); /spine/roster
  // is only a count summary. Adapters tell each Operative's Rig + whether
  // it is actually installed.
  const { data, loading, error } = useAsync(async () => {
    const [agentsRes, adapters] = await Promise.all([
      tryGet<unknown>("/v1/agents/access", {}),
      tryGet<Adapter[]>("/v1/adapters", []),
    ]);
    return {
      agents: extractList<Agent>(agentsRes, ["agents", "operatives"]),
      adapters: Array.isArray(adapters) ? adapters : [],
    };
  }, []);

  const agents = data?.agents ?? [];
  const adapters = data?.adapters ?? [];
  const availability = new Map(adapters.map((a) => [a.name, a.probe?.status === "available"]));
  const availCount = adapters.filter((a) => a.probe?.status === "available").length;

  function rigCell(rig?: string | null) {
    if (!rig) return <span className="muted">— (no adapter)</span>;
    const ok = availability.get(rig);
    return (
      <span>
        <span className="mono">{rig}</span>{" "}
        <span className={"badge " + (ok ? "done" : "blocked")}>{ok ? "available" : "missing"}</span>
      </span>
    );
  }

  return (
    <Section title="Crew">
      {error && <div className="banner err">{error}</div>}
      <div className={"banner " + (availCount ? "ok" : "info")}>
        {availCount
          ? `${availCount}/${adapters.length} agent adapter(s) available — Operatives with an available Rig can execute Briefs.`
          : "No agent adapters installed. Install a coding-agent CLI (Claude, Codex) so Operatives can execute work — see Settings."}
      </div>
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
                <th>Adapter (Rig)</th>
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
                    <td>{rigCell(a.rig)}</td>
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
