import { tryGet } from "../api";
import { useAuth } from "../auth";
import { asArray, Empty, useAsync } from "../components/common";

interface Provider { name?: string; id?: string; configured?: boolean; enabled?: boolean; model?: string }

function extractProviders(v: unknown): Provider[] {
  if (Array.isArray(v)) return v as Provider[];
  if (v && typeof v === "object") {
    const o = v as Record<string, unknown>;
    for (const k of ["providers", "items", "results"]) if (Array.isArray(o[k])) return o[k] as Provider[];
  }
  return [];
}

export function Settings() {
  const { status, logout } = useAuth();
  const { data, loading } = useAsync(async () => {
    const [info, providers] = await Promise.all([
      tryGet<Record<string, unknown>>("/v1/info", {}),
      tryGet<unknown>("/v1/config/providers", {}),
    ]);
    return { info, providers: extractProviders(providers) };
  }, []);

  const info = data?.info ?? {};
  const providers = data?.providers ?? [];

  return (
    <div className="grid cols-2">
      <div className="card">
        <h3>Account</h3>
        <div className="row" style={{ marginBottom: 10 }}>
          <div className="who avatar" style={{ width: 36, height: 36 }}>
            {(status?.username ?? "?").slice(0, 1).toUpperCase()}
          </div>
          <div>
            <div><strong>{status?.username ?? "operator"}</strong></div>
            <div className="muted">Bridge admin</div>
          </div>
        </div>
        <button className="btn ghost" onClick={() => void logout()}>Sign out</button>
      </div>

      <div className="card">
        <h3>Bridge</h3>
        {loading ? (
          <div className="loading">Loading…</div>
        ) : (
          <table className="table">
            <tbody>
              {Object.entries(info)
                .filter(([, v]) => typeof v !== "object")
                .slice(0, 10)
                .map(([k, v]) => (
                  <tr key={k}>
                    <td className="muted">{k}</td>
                    <td className="mono">{String(v)}</td>
                  </tr>
                ))}
              {Object.keys(info).length === 0 && (
                <tr><td className="muted">Bridge info unavailable.</td></tr>
              )}
            </tbody>
          </table>
        )}
      </div>

      <div className="card" style={{ gridColumn: "1 / -1" }}>
        <h3>AI providers</h3>
        {loading ? (
          <div className="loading">Loading providers…</div>
        ) : providers.length === 0 ? (
          <Empty>No providers configured on the AI node.</Empty>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Provider</th>
                <th>Model</th>
                <th>Status</th>
              </tr>
            </thead>
            <tbody>
              {asArray<Provider>(providers).map((p, i) => (
                <tr key={p.name ?? p.id ?? i}>
                  <td><strong>{p.name ?? p.id ?? "provider"}</strong></td>
                  <td className="muted">{p.model ?? "—"}</td>
                  <td>
                    <span className={"badge " + (p.configured || p.enabled ? "done" : "backlog")}>
                      {p.configured || p.enabled ? "configured" : "inactive"}
                    </span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}
