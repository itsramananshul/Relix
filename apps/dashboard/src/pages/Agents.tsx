import { useState } from "react";
import { Link } from "react-router-dom";
import { api, tryGet } from "../api";
import { asArray, Badge, Empty, Section, useAsync } from "../components/common";

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
interface Card { assignee_agent_id?: string | null }
interface RunRow { agent_id?: string; status?: string }

// Friendly labels for the rich readiness statuses.
const STATUS_LABEL: Record<string, string> = {
  available: "available",
  missing_binary: "not installed",
  not_authenticated: "needs login",
  unsupported_version: "version issue",
  interactive_only: "needs a TTY",
  probe_failed: "probe failed",
};
// Board columns counted as an Operative's open workload.
const WORK_COLUMNS = ["todo", "in_progress", "in_review"];

export function Agents() {
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [founderName, setFounderName] = useState("Founder");
  const [founderRig, setFounderRig] = useState("echo");

  const { data, loading, error, reload } = useAsync(async () => {
    const work: Card[] = [];
    const [company, ops, adapters, runs] = await Promise.all([
      tryGet<CompanyStatus>("/v1/spine/company", {}),
      tryGet<Agent[]>("/v1/spine/operatives", []),
      tryGet<Adapter[]>("/v1/adapters", []),
      tryGet<RunRow[]>("/v1/runs", []),
      Promise.all(
        WORK_COLUMNS.map(async (col) => {
          work.push(...asArray<Card>(await tryGet<Card[]>(`/v1/spine/board/${col}?limit=100`, [])));
        }),
      ),
    ]);
    return {
      company: company ?? {},
      agents: Array.isArray(ops) ? ops : [],
      adapters: Array.isArray(adapters) ? adapters : [],
      runs: Array.isArray(runs) ? runs : [],
      work,
    };
  }, []);

  const company = data?.company ?? {};
  const agents = data?.agents ?? [];
  const adapters = data?.adapters ?? [];
  const runs = data?.runs ?? [];
  const work = data?.work ?? [];
  const byName = new Map(adapters.map((a) => [a.name ?? "", a]));
  const availCount = adapters.filter((a) => a.probe?.status === "available").length;
  const initialized = company.initialized ?? agents.length > 0;

  // Workload (open assigned Briefs) + currently-running counts per Operative.
  const workload = new Map<string, number>();
  for (const c of work) {
    const a = c.assignee_agent_id;
    if (a) workload.set(a, (workload.get(a) ?? 0) + 1);
  }
  const running = new Map<string, number>();
  for (const r of runs) {
    if (r.status === "running" && r.agent_id) running.set(r.agent_id, (running.get(r.agent_id) ?? 0) + 1);
  }

  const founder = agents.find((a) => a.role === "founder") ?? (company.founder ?? undefined);
  const crew = agents.filter((a) => a.role !== "founder");

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

  function rigSelect(a: Agent) {
    const id = a.agent_id ?? "";
    return (
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
    );
  }

  // First-run: no Founder yet. Make the path forward obvious.
  if (!loading && !initialized) {
    return (
      <Section title="Crew">
        {error && <div className="banner err">{error}</div>}
        {banner && <div className={"banner " + banner.kind}>{banner.msg}</div>}
        <div className="card setup-card" style={{ maxWidth: 620 }}>
          <div className="setup-step">First-run setup</div>
          <h3 style={{ marginTop: 4 }}>Initialize your company</h3>
          <p className="muted" style={{ marginTop: -4 }}>
            Relix has no Operatives yet. Create the <strong>Founder</strong> — the first Operative who
            can own Briefs, run them through an adapter, and hire the rest of the team.
          </p>
          <label className="field">
            <span>Founder name</span>
            <input className="input" value={founderName} onChange={(e) => setFounderName(e.target.value)} placeholder="Founder" />
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
      <div className={"banner " + (availCount ? "ok" : "info") + " banner-action"}>
        <span>
          {availCount
            ? `${availCount}/${adapters.length} agent adapter(s) available — an Operative with an available adapter can execute Briefs.`
            : "No agent adapters available. Install + log in to a coding-agent CLI (Claude, Codex). echo always works for testing."}
        </span>
        <Link to="/settings" className="banner-cta">Adapters →</Link>
      </div>

      {/* Founder — shown separately as the org root. */}
      {founder && (
        <div className="card">
          <h3>Founder</h3>
          <div className="row wrap" style={{ gap: 18, alignItems: "flex-start" }}>
            <div>
              <div className="row" style={{ gap: 8 }}>
                <strong>{founder.name ?? "Founder"}</strong>
                <span className="badge done">Founder</span>
                <Badge status={founder.status ?? "active"} />
              </div>
              <div className="mono" style={{ fontSize: 11, marginTop: 4 }}>{(founder.agent_id ?? "").slice(0, 16)}</div>
            </div>
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Adapter</div>
              {rigSelect(founder)}
            </div>
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Readiness</div>
              {rigStatusCell(founder.rig)}
            </div>
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Workload</div>
              <span>{workload.get(founder.agent_id ?? "") ?? 0} open · {running.get(founder.agent_id ?? "") ?? 0} running</span>
            </div>
          </div>
        </div>
      )}

      {/* Operatives roster (the rest of the crew). */}
      <div className="card">
        <div className="row" style={{ marginBottom: 10 }}>
          <h3 style={{ margin: 0 }}>Operatives</h3>
          <div className="spacer" style={{ flex: 1 }} />
          <Link to="/briefs" className="link" style={{ fontSize: 12 }}>assign work →</Link>
        </div>
        {loading ? (
          <div className="loading">Loading crew…</div>
        ) : crew.length === 0 ? (
          <Empty>No other Operatives yet — the Founder can hire more as the company grows.</Empty>
        ) : (
          <div className="table-scroll">
            <table className="table">
              <thead>
                <tr>
                  <th>Operative</th>
                  <th>Role</th>
                  <th>Status</th>
                  <th>Adapter (Rig)</th>
                  <th>Readiness</th>
                  <th>Open</th>
                  <th>Running</th>
                </tr>
              </thead>
              <tbody>
                {crew.map((a, i) => {
                  const id = a.agent_id ?? "";
                  return (
                    <tr key={id || i}>
                      <td>
                        <strong>{a.name ?? id.slice(0, 10) ?? "operative"}</strong>
                        <div className="mono" style={{ fontSize: 10 }}>{id.slice(0, 12)}</div>
                      </td>
                      <td className="dim">{a.role ?? a.title ?? "—"}</td>
                      <td><Badge status={a.status ?? "active"} /></td>
                      <td>{rigSelect(a)}</td>
                      <td>{rigStatusCell(a.rig)}</td>
                      <td>{workload.get(id) ?? 0}</td>
                      <td>
                        {(running.get(id) ?? 0) > 0
                          ? <span className="badge in_progress">{running.get(id)}</span>
                          : <span className="muted">0</span>}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </Section>
  );
}
