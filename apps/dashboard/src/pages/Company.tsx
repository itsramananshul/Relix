import { tryGet } from "../api";
import { asArray, Empty, useAsync } from "../components/common";

interface Agent { agent_id?: string; id?: string; name?: string; role?: string; reports_to?: string }
interface Mandate { mandate_id?: string; id?: string; title?: string; status?: string; name?: string }

function extract<T>(v: unknown, keys: string[]): T[] {
  if (Array.isArray(v)) return v as T[];
  if (v && typeof v === "object") {
    const o = v as Record<string, unknown>;
    for (const k of keys) if (Array.isArray(o[k])) return o[k] as T[];
  }
  return [];
}

export function Company() {
  const { data, loading } = useAsync(async () => {
    const [roster, mandates] = await Promise.all([
      tryGet<unknown>("/v1/spine/roster", {}),
      tryGet<unknown>("/v1/spine/mandates/search?q=&limit=50", {}),
    ]);
    return {
      agents: extract<Agent>(roster, ["operatives", "agents", "roster", "members", "active_agents"]),
      mandates: extract<Mandate>(mandates, ["mandates", "results", "items"]),
    };
  }, []);

  const agents = data?.agents ?? [];
  const mandates = data?.mandates ?? [];

  // Group by reports_to to render a shallow org tree.
  const roots = agents.filter((a) => !a.reports_to);
  const childrenOf = (id?: string) => agents.filter((a) => a.reports_to && a.reports_to === id);

  return (
    <div className="grid cols-2">
      <div className="card">
        <h3>Org hierarchy</h3>
        {loading ? (
          <div className="loading">Loading…</div>
        ) : agents.length === 0 ? (
          <Empty>No org defined yet.</Empty>
        ) : (
          <div>
            {(roots.length ? roots : agents).map((a) => {
              const id = a.agent_id ?? a.id;
              return (
                <div key={id} style={{ marginBottom: 10 }}>
                  <div className="row">
                    <strong>{a.name ?? id?.slice(0, 10)}</strong>
                    <span className="muted">{a.role ?? ""}</span>
                  </div>
                  {childrenOf(id).map((c) => (
                    <div key={c.agent_id ?? c.id} className="dim" style={{ paddingLeft: 16, fontSize: 13 }}>
                      └ {c.name ?? (c.agent_id ?? c.id)?.slice(0, 10)} <span className="muted">{c.role}</span>
                    </div>
                  ))}
                </div>
              );
            })}
          </div>
        )}
      </div>

      <div className="card">
        <h3>Mandates (goals)</h3>
        {loading ? (
          <div className="loading">Loading…</div>
        ) : mandates.length === 0 ? (
          <Empty>No mandates. Goals organize the work tree.</Empty>
        ) : (
          <table className="table">
            <tbody>
              {asArray<Mandate>(mandates).map((m, i) => (
                <tr key={m.mandate_id ?? m.id ?? i}>
                  <td><strong>{m.title ?? m.name ?? "(untitled)"}</strong></td>
                  <td><span className="badge">{m.status ?? "—"}</span></td>
                  <td className="mono">{(m.mandate_id ?? m.id ?? "").slice(0, 10)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}
