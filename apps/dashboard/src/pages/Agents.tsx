import { useState } from "react";
import { api, tryGet } from "../api";
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
interface Adapter {
  name?: string;
  display_name?: string;
  probe?: { status?: string; detail?: string; install_hint?: string | null };
}

// Friendly labels for the rich readiness statuses.
const STATUS_LABEL: Record<string, string> = {
  available: "available",
  missing_binary: "not installed",
  not_authenticated: "needs login",
  unsupported_version: "version issue",
  interactive_only: "needs a TTY",
  probe_failed: "probe failed",
};

export function Agents() {
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);

  const { data, loading, error, reload } = useAsync(async () => {
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
  const byName = new Map(adapters.map((a) => [a.name ?? "", a]));
  const availCount = adapters.filter((a) => a.probe?.status === "available").length;

  async function setRig(agentId: string, rig: string) {
    const adapter = byName.get(rig);
    const avail = adapter?.probe?.status === "available";
    if (rig && !avail) {
      const label = STATUS_LABEL[adapter?.probe?.status ?? ""] ?? "unavailable";
      if (!confirm(`Adapter "${rig}" is ${label}. Assign it anyway? Runs will be refused until it is ready.`)) {
        reload();
        return;
      }
    }
    setBanner(null);
    try {
      await api.patch(`/v1/agents/${encodeURIComponent(agentId)}`, { rig });
      setBanner({ kind: "ok", msg: `Adapter set to ${rig || "(none)"}.` });
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Update failed" });
    }
  }

  function rigStatusCell(rig?: string | null) {
    if (!rig) return <span className="muted">no adapter</span>;
    const a = byName.get(rig);
    const status = a?.probe?.status ?? "unknown";
    const ok = status === "available";
    return (
      <span>
        <span className={"badge " + (ok ? "done" : "blocked")}>{STATUS_LABEL[status] ?? status}</span>
        {!ok && a?.probe?.install_hint && (
          <div className="muted" style={{ fontSize: 11, marginTop: 3 }}>{a.probe.install_hint}</div>
        )}
      </span>
    );
  }

  return (
    <Section title="Crew">
      {error && <div className="banner err">{error}</div>}
      {banner && <div className={"banner " + banner.kind}>{banner.msg}</div>}
      <div className={"banner " + (availCount ? "ok" : "info")}>
        {availCount
          ? `${availCount}/${adapters.length} agent adapter(s) available — an Operative with an available adapter can execute Briefs.`
          : "No agent adapters available. Install + log in to a coding-agent CLI (Claude, Codex) — see Settings."}
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
                <th>Readiness</th>
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
                    <td>
                      <select
                        className="select"
                        style={{ fontSize: 12, padding: "3px 6px", minWidth: 120 }}
                        value={a.rig ?? ""}
                        onChange={(e) => setRig(id, e.target.value)}
                      >
                        <option value="">(none)</option>
                        {adapters.map((ad) => {
                          const av = ad.probe?.status === "available";
                          return (
                            <option key={ad.name} value={ad.name}>
                              {ad.name}{av ? "" : " ⚠"}
                            </option>
                          );
                        })}
                      </select>
                    </td>
                    <td>{rigStatusCell(a.rig)}</td>
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
