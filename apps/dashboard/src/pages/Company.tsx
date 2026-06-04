import { useState } from "react";
import { tryGet } from "../api";
import { Empty, extractList, useAsync } from "../components/common";

interface Agent { agent_id?: string; id?: string; name?: string; display_name?: string; role?: string; reports_to?: string }
interface Mandate { mandate_id?: string; id?: string; title?: string; status?: string; name?: string }

export function Company() {
  const [query, setQuery] = useState("");
  // The agent *list* lives at /v1/agents/access ({agents:[…]}); /spine/roster
  // is only a count summary. `mandate.search` requires a non-empty query —
  // there is no list-all mandates endpoint — so mandates load on search.
  const { data, loading } = useAsync(async () => {
    const agentsRes = await tryGet<unknown>("/v1/agents/access", {});
    let mandates: Mandate[] = [];
    if (query.trim()) {
      const m = await tryGet<unknown>(
        `/v1/spine/mandates/search?q=${encodeURIComponent(query.trim())}&limit=50`,
        {},
      );
      mandates = extractList<Mandate>(m, ["mandates"]);
    }
    return { agents: extractList<Agent>(agentsRes, ["agents", "operatives"]), mandates };
  }, [query]);

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
        <input
          className="input"
          style={{ marginBottom: 12 }}
          placeholder="Search mandates by title…"
          defaultValue={query}
          onKeyDown={(e) => {
            if (e.key === "Enter") setQuery((e.target as HTMLInputElement).value);
          }}
        />
        {!query.trim() ? (
          <Empty>Type a query to search mandates. (No list-all endpoint yet.)</Empty>
        ) : loading ? (
          <div className="loading">Loading…</div>
        ) : mandates.length === 0 ? (
          <Empty>No mandates match “{query}”.</Empty>
        ) : (
          <table className="table">
            <tbody>
              {mandates.map((m, i) => (
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
