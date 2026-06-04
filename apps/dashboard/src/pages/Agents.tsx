import { useState } from "react";
import { api, tryGet } from "../api";
import { Badge, Empty, Section, useAsync } from "../components/common";

interface Agent {
  agent_id?: string;
  name?: string;
  role?: string;
  status?: string;
  reports_to?: string | null;
  title?: string;
  rig?: string | null;
}
interface Adapter {
  name?: string;
  display_name?: string;
  probe?: { status?: string; detail?: string; install_hint?: string | null };
}
interface CompanyStatus {
  initialized?: boolean;
  founder?: Agent | null;
  operative_count?: number;
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
  const [busy, setBusy] = useState(false);
  const [founderName, setFounderName] = useState("Founder");
  const [founderRig, setFounderRig] = useState("echo");

  const { data, loading, error, reload } = useAsync(async () => {
    const [company, ops, adapters] = await Promise.all([
      tryGet<CompanyStatus>("/v1/spine/company", {}),
      tryGet<Agent[]>("/v1/spine/operatives", []),
      tryGet<Adapter[]>("/v1/adapters", []),
    ]);
    return {
      company: company ?? {},
      agents: Array.isArray(ops) ? ops : [],
      adapters: Array.isArray(adapters) ? adapters : [],
    };
  }, []);

  const company = data?.company ?? {};
  const agents = data?.agents ?? [];
  const adapters = data?.adapters ?? [];
  const byName = new Map(adapters.map((a) => [a.name ?? "", a]));
  const availCount = adapters.filter((a) => a.probe?.status === "available").length;
  const initialized = company.initialized ?? agents.length > 0;

  async function initCompany() {
    setBanner(null);
    setBusy(true);
    try {
      const r = await api.post<{ founder?: Agent; created?: boolean }>("/v1/spine/company/init", {
        name: founderName.trim() || "Founder",
        rig: founderRig || "echo",
      });
      setBanner({
        kind: "ok",
        msg: r.created
          ? `Company initialized — Founder "${r.founder?.name}" created on adapter ${r.founder?.rig}.`
          : `Company already initialized — Founder "${r.founder?.name}" is in place.`,
      });
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Initialize failed" });
    } finally {
      setBusy(false);
    }
  }

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

  // First-run: no Founder yet. Make the path forward obvious.
  if (!loading && !initialized) {
    return (
      <Section title="Crew">
        {error && <div className="banner err">{error}</div>}
        {banner && <div className={"banner " + banner.kind}>{banner.msg}</div>}
        <div className="card" style={{ maxWidth: 620 }}>
          <h3>Welcome — initialize your company</h3>
          <p className="muted" style={{ marginTop: -4 }}>
            Relix has no Operatives yet. Create the <strong>Founder</strong> — the first Operative who
            can own Briefs, run them through an adapter, and hire the rest of the team. You can do
            everything else from here once the Founder exists.
          </p>
          <label className="field">
            <span>Founder name</span>
            <input
              className="input"
              value={founderName}
              onChange={(e) => setFounderName(e.target.value)}
              placeholder="Founder"
            />
          </label>
          <label className="field">
            <span>Default adapter (Rig)</span>
            <select className="select" value={founderRig} onChange={(e) => setFounderRig(e.target.value)}>
              <option value="echo">echo — built-in, always available</option>
              {adapters
                .filter((a) => a.name && a.name !== "echo")
                .map((a) => {
                  const av = a.probe?.status === "available";
                  return (
                    <option key={a.name} value={a.name}>
                      {a.name}{av ? "" : " ⚠ (" + (STATUS_LABEL[a.probe?.status ?? ""] ?? "unavailable") + ")"}
                    </option>
                  );
                })}
            </select>
          </label>
          <p className="muted" style={{ fontSize: 12 }}>
            {availCount
              ? `${availCount}/${adapters.length} adapter(s) available. echo is recommended to start — switch the Founder to a coding agent once it is installed + logged in.`
              : "echo is recommended to start. Install + log in to a coding-agent CLI (Claude, Codex) on the Settings page to use a real adapter."}
          </p>
          <button className="btn" onClick={initCompany} disabled={busy}>
            {busy ? "Initializing…" : "Initialize Company"}
          </button>
        </div>
      </Section>
    );
  }

  return (
    <Section title="Crew">
      {error && <div className="banner err">{error}</div>}
      {banner && <div className={"banner " + banner.kind}>{banner.msg}</div>}
      <div className={"banner " + (availCount ? "ok" : "info")}>
        {availCount
          ? `${availCount}/${adapters.length} agent adapter(s) available — an Operative with an available adapter can execute Briefs.`
          : "No agent adapters available. Install + log in to a coding-agent CLI (Claude, Codex) — see Settings. echo always works for testing."}
      </div>
      <div className="card">
        {loading ? (
          <div className="loading">Loading crew…</div>
        ) : agents.length === 0 ? (
          <Empty>No Operatives yet.</Empty>
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
                const id = a.agent_id ?? "";
                return (
                  <tr key={id || i}>
                    <td>
                      <strong>{a.name ?? id.slice(0, 10) ?? "operative"}</strong>
                      {a.role === "founder" && <span className="badge done" style={{ marginLeft: 6 }}>Founder</span>}
                    </td>
                    <td className="dim">{a.role ?? a.title ?? "—"}</td>
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
