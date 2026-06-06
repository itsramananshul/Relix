import { Fragment, useState } from "react";
import { Link } from "react-router-dom";
import { api, tryGet } from "../api";
import { asArray, Badge, Empty, Section, useAsync } from "../components/common";

// One Operative's Keys (`/v1/spine/keys/:agent`) — the org/work permissions
// + execution caps the legacy spine board surfaced. Rendered read-only here
// (editing Keys stays out of this parity slice).
interface Keys {
  can_spawn_agents?: boolean;
  spawn_route?: string;
  can_assign_work?: boolean;
  assign_scope?: string;
  can_manage_work?: boolean;
  manage_scope?: string;
  can_configure_agents?: boolean;
  configure_scope?: string;
  max_concurrent_runs?: number;
  monthly_allowance_cents?: number;
  wake_on_timer?: boolean;
  wake_on_demand?: boolean;
  secret_allowlist?: string[];
}

// Guild-committed Allowance (`/v1/spine/allowance/committed`). Field name
// varies; pull the first cents-like number defensively.
function committedCents(v: unknown): number | null {
  if (typeof v === "number") return v;
  if (v && typeof v === "object") {
    const o = v as Record<string, unknown>;
    for (const k of ["committed_cents", "committed", "allowance_cents", "cents", "total_cents"]) {
      if (typeof o[k] === "number") return o[k] as number;
    }
  }
  return null;
}
function fmtCents(c?: number | null): string {
  if (c == null) return "—";
  return "$" + (c / 100).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
}

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
  prime?: Agent | null;
  operative_count?: number;
  crew?: {
    total?: number;
    active?: number;
    pending?: number;
    by_status?: Record<string, number>;
    by_role?: Record<string, number>;
  };
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
  // Per-Operative Keys: which row's Keys are expanded + a small cache so
  // re-opening is instant. `null` value = loaded but no keys returned.
  const [keysOpen, setKeysOpen] = useState<string | null>(null);
  const [keysCache, setKeysCache] = useState<Record<string, Keys | null>>({});

  const { data, loading, error, reload } = useAsync(async () => {
    const work: Card[] = [];
    const [company, ops, adapters, runs, allowance] = await Promise.all([
      tryGet<CompanyStatus>("/v1/spine/company", {}),
      tryGet<Agent[]>("/v1/spine/operatives", []),
      tryGet<Adapter[]>("/v1/adapters", []),
      tryGet<RunRow[]>("/v1/runs", []),
      tryGet<unknown>("/v1/spine/allowance/committed", {}),
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
      allowance: committedCents(allowance),
      work,
    };
  }, []);

  async function toggleKeys(agentId: string) {
    if (keysOpen === agentId) {
      setKeysOpen(null);
      return;
    }
    setKeysOpen(agentId);
    if (!(agentId in keysCache)) {
      const k = await tryGet<Keys | null>(`/v1/spine/keys/${encodeURIComponent(agentId)}`, null);
      setKeysCache((m) => ({ ...m, [agentId]: k }));
    }
  }

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
  // Prime = the planning lead (Founder's right hand). Prefer the server's
  // resolved Prime, else the operative whose role is `prime`.
  const prime =
    agents.find((a) => a.role?.toLowerCase() === "prime") ?? (company.prime ?? undefined);
  // The rest of the Crew, minus the Founder + Prime (shown as their own cards).
  const rest = agents.filter(
    (a) => a.role !== "founder" && a.agent_id !== (prime?.agent_id ?? ""),
  );
  // Separate pending hires (awaiting approval/Clearance) from active Crew so a
  // half-built team reads honestly.
  const pendingHires = rest.filter((a) => a.status === "pending");
  const activeCrew = rest.filter((a) => a.status !== "pending");
  // Resolve a boss agent_id → display name for the reporting line.
  const nameOf = (id?: string | null) => {
    if (!id) return null;
    const a = agents.find((x) => x.agent_id === id);
    return a?.name ?? id.slice(0, 8);
  };

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

  // First-run safe-local on-ramp (company-model §12.6): ensure the Founder +
  // a small echo-backed starter crew so a fresh company can run a real Shift
  // (propose → approve → start) without any external coding-agent auth.
  async function starterCrew() {
    setBanner(null);
    setBusy(true);
    try {
      const r = await api.post<{
        founder?: Agent;
        founder_created?: boolean;
        rig?: string;
        crew?: { role?: string; created?: boolean }[];
      }>("/v1/spine/company/starter-crew", { rig: "echo" });
      const made = (r.crew ?? []).filter((c) => c.created).map((c) => c.role).join(", ");
      const roles = (r.crew ?? []).map((c) => c.role).join(", ");
      setBanner({
        kind: "ok",
        msg: made
          ? `Starter crew ready — safe local Operatives (${made}) on the echo adapter. Ask Prime to plan, then Start the work.`
          : `Starter crew already in place (${roles}) on the echo adapter.`,
      });
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Starter crew failed" });
    } finally {
      setBusy(false);
    }
  }

  // Greenlight a pending hire directly (company-model §12.6): approve + bind
  // the safe-local `echo` Rig atomically so the now-active Operative is
  // immediately runnable. This is the governed `route=direct` affordance — a
  // clearance-gated hire is refused server-side, and we surface that honestly
  // with a pointer to decide its Clearance on Mandates.
  async function approveHire(agentId: string, name?: string) {
    setBanner(null);
    setBusy(true);
    try {
      const r = await api.post<{ runnable?: boolean; rig?: string; needs_rig?: boolean }>(
        `/v1/agents/${encodeURIComponent(agentId)}/approve-hire`,
        { rig: "echo" },
      );
      setBanner({
        kind: "ok",
        msg: r.needs_rig
          ? `${name ?? "Operative"} hired — set an adapter to make it runnable.`
          : `${name ?? "Operative"} hired and runnable on the ${r.rig ?? "echo"} adapter.`,
      });
      reload();
    } catch (e) {
      const msg = e instanceof Error ? e.message : "Approve hire failed";
      setBanner({
        kind: "err",
        msg: /clearance/i.test(msg)
          ? `${msg} — this hire needs a Clearance; decide it on the Mandates page.`
          : msg,
      });
    } finally {
      setBusy(false);
    }
  }

  // Decline a pending hire (pending → disabled). The role stays unfilled so the
  // team plan can re-propose or the operator can hire someone else.
  async function rejectHire(agentId: string, name?: string) {
    setBanner(null);
    setBusy(true);
    try {
      await api.post(`/v1/agents/${encodeURIComponent(agentId)}/reject-hire`, {});
      setBanner({ kind: "ok", msg: `${name ?? "Hire"} declined — the role is left unfilled.` });
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Reject hire failed" });
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

  // Compact read-only render of one Operative's Keys (org/work permissions +
  // execution caps). Mirrors the legacy spine board's Keys panel.
  function keysDetail(agentId: string) {
    const k = keysCache[agentId];
    if (!(agentId in keysCache)) return <div className="loading" style={{ fontSize: 12 }}>Loading Keys…</div>;
    if (!k) return <div className="muted" style={{ fontSize: 12 }}>No Keys recorded for this Operative.</div>;
    const flag = (on?: boolean, scope?: string) =>
      on ? <span className="badge done" style={{ fontSize: 9 }}>yes{scope ? ` · ${scope}` : ""}</span> : <span className="badge backlog" style={{ fontSize: 9 }}>no</span>;
    return (
      <div className="kv-grid" style={{ fontSize: 12 }}>
        <div className="kv"><span className="muted">Spawn agents</span><span>{flag(k.can_spawn_agents, k.spawn_route)}</span></div>
        <div className="kv"><span className="muted">Assign work</span><span>{flag(k.can_assign_work, k.assign_scope)}</span></div>
        <div className="kv"><span className="muted">Manage work</span><span>{flag(k.can_manage_work, k.manage_scope)}</span></div>
        <div className="kv"><span className="muted">Configure agents</span><span>{flag(k.can_configure_agents, k.configure_scope)}</span></div>
        <div className="kv"><span className="muted">Wake</span><span>{k.wake_on_timer ? "timer " : ""}{k.wake_on_demand ? "on-demand" : ""}{!k.wake_on_timer && !k.wake_on_demand ? "—" : ""}</span></div>
        <div className="kv"><span className="muted">Max concurrent runs</span><span>{k.max_concurrent_runs ?? "—"}</span></div>
        <div className="kv"><span className="muted">Monthly Allowance</span><span>{k.monthly_allowance_cents != null ? fmtCents(k.monthly_allowance_cents) : "—"}</span></div>
        <div className="kv"><span className="muted">Secret allowlist</span><span>{(k.secret_allowlist?.length ?? 0) > 0 ? `${k.secret_allowlist!.length} entr${k.secret_allowlist!.length === 1 ? "y" : "ies"}` : "none"}</span></div>
      </div>
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
          <div className="row" style={{ marginTop: 6, gap: 8, flexWrap: "wrap" }}>
            <button className="btn" onClick={initCompany} disabled={busy}>
              {busy ? "Working…" : "Initialize Company"}
            </button>
            <button className="btn ghost" onClick={starterCrew} disabled={busy}>
              {busy ? "Working…" : "Set up starter crew (local · echo)"}
            </button>
          </div>
          <p className="muted" style={{ fontSize: 12, marginTop: 6 }}>
            <strong>Starter crew</strong> also creates a couple of safe, local <em>echo</em> Operatives
            (an Engineer + a Designer) so you can immediately Ask Prime to plan, then <em>Start the
            work</em> and watch a real Shift complete — no external coding-agent login needed. These are
            clearly-labelled local/demo workers, not Claude or Codex.
          </p>
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

      {/* Guild Allowance — the committed monthly budget across the Crew. */}
      <div className="card" style={{ padding: "10px 14px" }}>
        <div className="row">
          <span className="muted">Guild Allowance (committed)</span>
          <span className="spacer" style={{ flex: 1 }} />
          <strong>{fmtCents(data?.allowance)}</strong>
          <span className="muted" style={{ fontSize: 11, marginLeft: 8 }}>
            sum of per-Operative monthly caps · per-Operative limits are in each row's Keys
          </span>
        </div>
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
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Keys</div>
              <button className="btn ghost sm" onClick={() => toggleKeys(founder.agent_id ?? "")}>
                {keysOpen === founder.agent_id ? "Hide" : "View"}
              </button>
            </div>
          </div>
          {keysOpen === founder.agent_id && (
            <div style={{ marginTop: 10 }}>{keysDetail(founder.agent_id ?? "")}</div>
          )}
        </div>
      )}

      {/* Prime — the Founder's planning lead, shown distinctly. */}
      {prime ? (
        <div className="card">
          <h3>Prime</h3>
          <div className="row wrap" style={{ gap: 18, alignItems: "flex-start" }}>
            <div>
              <div className="row" style={{ gap: 8 }}>
                <strong>{prime.name ?? "Prime"}</strong>
                <span className="badge in_progress">Prime</span>
                <Badge status={prime.status ?? "active"} />
              </div>
              <div className="mono" style={{ fontSize: 11, marginTop: 4 }}>{(prime.agent_id ?? "").slice(0, 16)}</div>
              {prime.reports_to && (
                <div className="muted" style={{ fontSize: 11, marginTop: 2 }}>reports to {nameOf(prime.reports_to)}</div>
              )}
            </div>
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Adapter</div>
              {rigSelect(prime)}
            </div>
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Readiness</div>
              {rigStatusCell(prime.rig)}
            </div>
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Workload</div>
              <span>{workload.get(prime.agent_id ?? "") ?? 0} open · {running.get(prime.agent_id ?? "") ?? 0} running</span>
            </div>
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Keys</div>
              <button className="btn ghost sm" onClick={() => toggleKeys(prime.agent_id ?? "")}>
                {keysOpen === prime.agent_id ? "Hide" : "View"}
              </button>
            </div>
          </div>
          {keysOpen === prime.agent_id && (
            <div style={{ marginTop: 10 }}>{keysDetail(prime.agent_id ?? "")}</div>
          )}
        </div>
      ) : founder ? (
        <div className="card" style={{ padding: "10px 14px" }}>
          <div className="row">
            <span className="badge backlog">No Prime yet</span>
            <span className="muted" style={{ fontSize: 12, marginLeft: 8 }}>
              Hire a Prime (planning lead) to propose Mandate strategy + build the team.
            </span>
            <span className="spacer" style={{ flex: 1 }} />
            <Link to="/mandates" className="link" style={{ fontSize: 12 }}>Mandates →</Link>
          </div>
        </div>
      ) : null}

      {/* Pending hires — operatives awaiting approval / Clearance. */}
      {pendingHires.length > 0 && (
        <div className="card">
          <div className="row" style={{ marginBottom: 8 }}>
            <h3 style={{ margin: 0 }}>Pending hires</h3>
            <span className="muted" style={{ fontSize: 12, marginLeft: 8 }}>
              {pendingHires.length} awaiting approval — approve to make it runnable, or decline
            </span>
            <span className="spacer" style={{ flex: 1 }} />
            <Link to="/mandates" className="link" style={{ fontSize: 12 }}>Clearances →</Link>
          </div>
          <div className="table-scroll">
            <table className="table">
              <thead><tr><th>Operative</th><th>Role</th><th>Reports to</th><th>Status</th><th style={{ textAlign: "right" }}>Decision</th></tr></thead>
              <tbody>
                {pendingHires.map((a, i) => {
                  const id = a.agent_id ?? "";
                  return (
                    <tr key={id || i}>
                      <td><strong>{a.name ?? id.slice(0, 10)}</strong><div className="mono" style={{ fontSize: 10 }}>{id.slice(0, 12)}</div></td>
                      <td className="dim">{a.role ?? a.title ?? "—"}</td>
                      <td className="muted">{nameOf(a.reports_to) ?? "—"}</td>
                      <td><Badge status={a.status ?? "pending"} /></td>
                      <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                        <button
                          className="btn sm"
                          disabled={busy || !id}
                          title="Approve this hire and bind the safe-local echo adapter so it is immediately runnable"
                          onClick={() => approveHire(id, a.name)}
                        >
                          Approve · echo
                        </button>
                        <button
                          className="btn ghost sm"
                          style={{ marginLeft: 6 }}
                          disabled={busy || !id}
                          title="Decline this hire (the role is left unfilled)"
                          onClick={() => rejectHire(id, a.name)}
                        >
                          Reject
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {/* Operatives roster (the active crew). */}
      <div className="card">
        <div className="row" style={{ marginBottom: 10 }}>
          <h3 style={{ margin: 0 }}>Operatives</h3>
          <div className="spacer" style={{ flex: 1 }} />
          <Link to="/briefs" className="link" style={{ fontSize: 12 }}>assign work →</Link>
        </div>
        {loading ? (
          <div className="loading">Loading crew…</div>
        ) : activeCrew.length === 0 ? (
          <Empty>No other active Operatives yet — the Founder/Prime can hire more as the company grows.</Empty>
        ) : (
          <div className="table-scroll">
            <table className="table">
              <thead>
                <tr>
                  <th>Operative</th>
                  <th>Role</th>
                  <th>Reports to</th>
                  <th>Status</th>
                  <th>Adapter (Rig)</th>
                  <th>Readiness</th>
                  <th>Open</th>
                  <th>Running</th>
                  <th>Keys</th>
                </tr>
              </thead>
              <tbody>
                {activeCrew.map((a, i) => {
                  const id = a.agent_id ?? "";
                  return (
                    <Fragment key={id || i}>
                    <tr>
                      <td>
                        <strong>{a.name ?? id.slice(0, 10) ?? "operative"}</strong>
                        <div className="mono" style={{ fontSize: 10 }}>{id.slice(0, 12)}</div>
                      </td>
                      <td className="dim">{a.role ?? a.title ?? "—"}</td>
                      <td className="muted">{nameOf(a.reports_to) ?? "—"}</td>
                      <td><Badge status={a.status ?? "active"} /></td>
                      <td>{rigSelect(a)}</td>
                      <td>{rigStatusCell(a.rig)}</td>
                      <td>{workload.get(id) ?? 0}</td>
                      <td>
                        {(running.get(id) ?? 0) > 0
                          ? <span className="badge in_progress">{running.get(id)}</span>
                          : <span className="muted">0</span>}
                      </td>
                      <td>
                        <button className="btn ghost sm" onClick={() => toggleKeys(id)} title="View this Operative's Keys (permissions + caps)">
                          {keysOpen === id ? "Hide" : "View"}
                        </button>
                      </td>
                    </tr>
                    {keysOpen === id && (
                      <tr>
                        <td colSpan={9} style={{ background: "var(--bg)" }}>{keysDetail(id)}</td>
                      </tr>
                    )}
                    </Fragment>
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
