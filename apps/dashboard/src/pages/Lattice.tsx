import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { tryGet } from "../api";
import { Badge, Empty, Section, useAsync } from "../components/common";

// The Lattice — the company's hierarchy view (lexicon: "The Lattice" = the org
// chart; internal edges stay `reports_to`). dashboard-design §9: a dense,
// inspectable reports-to tree (nodes + edges), each node showing role/status/
// rig + counts, click → a per-Operative governance detail. B&W aesthetic (§12);
// color is reserved for semantic status only.
//
// Pan/zoom note (design §9 asks for pan/zoom/pinch): full drag-pan/pinch is
// DEFERRED — this ships a scrollable responsive stage (the overflow:auto wrapper
// IS the pan) with explicit zoom controls (−/reset/+). That keeps the surface
// CSP-clean and dependency-free (no SVG-pan lib) while staying inspectable on
// desktop and phone; true drag-pan/pinch can layer on later without reshaping
// the data. (Recorded in product-spine-implementation.md as a partial.)

interface Op {
  agent_id?: string;
  name?: string;
  role?: string;
  title?: string;
  status?: string;
  rig?: string | null;
  reports_to?: string | null;
  can_spawn_agents?: boolean;
  can_assign_work?: boolean;
  can_manage_work?: boolean;
  can_configure_agents?: boolean;
}
interface CompanyStatus {
  initialized?: boolean;
  founder?: Op | null;
  prime?: Op | null;
}
interface Adapter { name?: string; probe?: { status?: string; install_hint?: string | null } }
interface RunRow { agent_id?: string; status?: string }

// Per-Operative Keys + capability detail (same reads the Roster's permission
// panel uses) — fetched lazily when a node is selected.
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
interface AgentDetail {
  risk_ceiling?: string;
  allow_categories?: string[];
  deny_categories?: string[];
}

function fmtCents(c?: number | null): string {
  if (c == null) return "—";
  return "$" + (c / 100).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
}

// Tree-layout geometry (px, in unscaled stage coordinates).
const NODE_W = 190;
const NODE_H = 86;
const H_GAP = 26;
const V_GAP = 60;

interface Placed { op: Op; x: number; y: number }

// Lay out a reports_to forest: a classic leaf-slot DFS — leaves take sequential
// horizontal slots, parents center over their children, depth → row. Defensive
// against cycles (a visited set) and orphan edges (a `reports_to` pointing at an
// id not in the set is treated as a root).
function layout(ops: Op[], rootOrder: Op[]): { placed: Placed[]; w: number; h: number } {
  const byId = new Map<string, Op>();
  for (const o of ops) if (o.agent_id) byId.set(o.agent_id, o);
  const childrenOf = (id: string) =>
    ops.filter((o) => o.reports_to && o.reports_to === id && o.agent_id !== id);

  const placed: Placed[] = [];
  const visited = new Set<string>();
  let leaf = 0;

  const place = (op: Op, depth: number): number => {
    const id = op.agent_id ?? "";
    visited.add(id);
    const kids = childrenOf(id).filter((k) => k.agent_id && !visited.has(k.agent_id));
    let cx: number;
    if (kids.length === 0) {
      cx = leaf * (NODE_W + H_GAP);
      leaf += 1;
    } else {
      const xs = kids.map((k) => place(k, depth + 1));
      cx = (xs[0] + xs[xs.length - 1]) / 2;
    }
    placed.push({ op, x: cx, y: depth * (NODE_H + V_GAP) });
    return cx;
  };

  // Place the explicit root order first (Founder → Prime → …), then any node
  // that hasn't been reached (a true root, or an orphan-edge node).
  for (const r of rootOrder) {
    if (r.agent_id && !visited.has(r.agent_id)) place(r, 0);
  }
  for (const o of ops) {
    if (o.agent_id && !visited.has(o.agent_id)) place(o, 0);
  }

  let maxX = 0;
  let maxY = 0;
  for (const p of placed) {
    if (p.x > maxX) maxX = p.x;
    if (p.y > maxY) maxY = p.y;
  }
  return { placed, w: maxX + NODE_W, h: maxY + NODE_H };
}

const ZOOM_MIN = 0.5;
const ZOOM_MAX = 1.6;
const ZOOM_STEP = 0.15;

export function Lattice() {
  const [selId, setSelId] = useState<string | null>(null);
  const [zoom, setZoom] = useState(1);
  const [detailCache, setDetailCache] = useState<Record<string, { keys: Keys | null; detail: AgentDetail | null }>>({});

  const { data, loading, error } = useAsync(async () => {
    const [company, ops, adapters, runs] = await Promise.all([
      tryGet<CompanyStatus>("/v1/spine/company", {}),
      tryGet<Op[]>("/v1/spine/operatives", []),
      tryGet<Adapter[]>("/v1/adapters", []),
      tryGet<RunRow[]>("/v1/runs", []),
    ]);
    return {
      company: company ?? {},
      ops: Array.isArray(ops) ? ops : [],
      adapters: Array.isArray(adapters) ? adapters : [],
      runs: Array.isArray(runs) ? runs : [],
    };
  }, []);

  const ops = data?.ops ?? [];
  const company = data?.company ?? {};
  const adapters = data?.adapters ?? [];
  const runs = data?.runs ?? [];

  const byName = useMemo(() => new Map(adapters.map((a) => [a.name ?? "", a])), [adapters]);
  // Currently-running count per Operative (live dot driver).
  const running = useMemo(() => {
    const m = new Map<string, number>();
    for (const r of runs) {
      if (r.status === "running" && r.agent_id) m.set(r.agent_id, (m.get(r.agent_id) ?? 0) + 1);
    }
    return m;
  }, [runs]);

  // Resolve the explicit root order: Founder, then Prime, then the rest by
  // creation order — so the apex reads top-of-tree even if the data isn't sorted.
  const rootOrder = useMemo(() => {
    const founder = ops.find((o) => o.role === "founder") ?? company.founder ?? undefined;
    const prime =
      ops.find((o) => o.role?.toLowerCase() === "prime") ?? company.prime ?? undefined;
    const order: Op[] = [];
    if (founder?.agent_id) order.push(founder);
    if (prime?.agent_id && prime.agent_id !== founder?.agent_id) order.push(prime);
    return order;
  }, [ops, company]);

  const { placed, w, h } = useMemo(() => layout(ops, rootOrder), [ops, rootOrder]);
  const posById = useMemo(() => {
    const m = new Map<string, Placed>();
    for (const p of placed) if (p.op.agent_id) m.set(p.op.agent_id, p);
    return m;
  }, [placed]);

  const nameOf = (id?: string | null) => {
    if (!id) return null;
    const o = ops.find((x) => x.agent_id === id);
    return o?.name ?? id.slice(0, 8);
  };
  const directReports = (id?: string) =>
    id ? ops.filter((o) => o.reports_to === id).length : 0;

  async function select(id: string) {
    setSelId(id);
    if (!(id in detailCache)) {
      const enc = encodeURIComponent(id);
      const [keys, detail] = await Promise.all([
        tryGet<Keys | null>(`/v1/spine/keys/${enc}`, null),
        tryGet<AgentDetail | null>(`/v1/agents/${enc}`, null),
      ]);
      setDetailCache((m) => ({ ...m, [id]: { keys, detail } }));
    }
  }

  const initialized = company.initialized ?? ops.length > 0;

  if (!loading && !initialized) {
    return (
      <Section title="The Lattice">
        {error && <div className="banner err">{error}</div>}
        <div className="card setup-card" style={{ maxWidth: 560 }}>
          <div className="setup-step">No company yet</div>
          <h3 style={{ marginTop: 4 }}>The Lattice is empty</h3>
          <p className="muted">
            Initialize your company on the Crew page — once a Founder and Crew exist, the org tree
            renders here from the live reports-to lattice.
          </p>
          <Link to="/agents"><button className="btn">Go to Crew →</button></Link>
        </div>
      </Section>
    );
  }

  const sel = selId ? posById.get(selId)?.op : undefined;
  const selRig = sel?.rig ? byName.get(sel.rig) : undefined;
  const selRunnable = !!sel?.rig && selRig?.probe?.status === "available";
  const selDetail = selId ? detailCache[selId] : undefined;

  // Role tone for the node chip (semantic color only).
  const roleTone = (role?: string) => {
    const r = (role ?? "").toLowerCase();
    if (r === "founder") return "done";
    if (r === "prime") return "in_progress";
    return "backlog";
  };
  // Status → dot class.
  const statusDot = (status?: string) => {
    const s = (status ?? "").toLowerCase();
    if (s === "active") return "on";
    if (s === "pending") return "warn";
    return "";
  };

  return (
    <Section
      title="The Lattice"
      action={
        <div className="lattice-zoom" role="group" aria-label="Zoom">
          <button className="btn ghost sm" aria-label="Zoom out" onClick={() => setZoom((z) => Math.max(ZOOM_MIN, +(z - ZOOM_STEP).toFixed(2)))}>−</button>
          <button className="btn ghost sm" aria-label="Reset zoom" onClick={() => setZoom(1)}>{Math.round(zoom * 100)}%</button>
          <button className="btn ghost sm" aria-label="Zoom in" onClick={() => setZoom((z) => Math.min(ZOOM_MAX, +(z + ZOOM_STEP).toFixed(2)))}>+</button>
        </div>
      }
    >
      {error && <div className="banner err">{error}</div>}

      <div className={selId ? "split-workspace" : ""}>
        <div className={selId ? "split-main" : ""} style={{ minWidth: 0 }}>
          {loading ? (
            <div className="card"><div className="loading">Loading the Lattice…</div></div>
          ) : ops.length === 0 ? (
            <div className="card"><Empty>No Operatives in the lattice yet.</Empty></div>
          ) : (
            <div className="card lattice-card">
              <div className="lattice-stage-wrap">
                <div
                  className="lattice-stage"
                  style={{ width: w, height: h, transform: `scale(${zoom})` }}
                >
                  <svg
                    className="lattice-edges"
                    width={w}
                    height={h}
                    viewBox={`0 0 ${w} ${h}`}
                    aria-hidden
                  >
                    {placed.map((p) => {
                      const pid = p.op.reports_to;
                      if (!pid) return null;
                      const parent = posById.get(pid);
                      if (!parent) return null;
                      const x1 = parent.x + NODE_W / 2;
                      const y1 = parent.y + NODE_H;
                      const x2 = p.x + NODE_W / 2;
                      const y2 = p.y;
                      const midY = (y1 + y2) / 2;
                      return (
                        <path
                          key={p.op.agent_id}
                          d={`M ${x1} ${y1} C ${x1} ${midY}, ${x2} ${midY}, ${x2} ${y2}`}
                          className="lattice-edge"
                          fill="none"
                        />
                      );
                    })}
                  </svg>
                  {placed.map((p) => {
                    const id = p.op.agent_id ?? "";
                    const run = running.get(id) ?? 0;
                    const reports = directReports(id);
                    return (
                      <button
                        key={id}
                        type="button"
                        className={"lattice-node" + (selId === id ? " selected" : "")}
                        style={{ left: p.x, top: p.y, width: NODE_W, height: NODE_H }}
                        onClick={() => select(id)}
                        title={`${p.op.name ?? id} — ${p.op.role ?? "operative"}`}
                      >
                        <div className="ln-head">
                          <span className={"dot " + statusDot(p.op.status)} />
                          <span className="ln-name">{p.op.name ?? id.slice(0, 10)}</span>
                          {run > 0 && <span className="ln-live" title={`${run} running`}>live</span>}
                        </div>
                        <div className="ln-meta">
                          <span className={"badge " + roleTone(p.op.role)} style={{ fontSize: 9 }}>
                            {p.op.role ?? "operative"}
                          </span>
                          <span className="badge" style={{ fontSize: 9 }}>{p.op.rig ?? "no rig"}</span>
                          {reports > 0 && <span className="ln-count">{reports} report{reports === 1 ? "" : "s"}</span>}
                        </div>
                      </button>
                    );
                  })}
                </div>
              </div>
              <div className="lattice-legend muted">
                <span><span className="dot on" /> active</span>
                <span><span className="dot warn" /> pending</span>
                <span><span className="dot" /> suspended / disabled</span>
                <span>scroll to pan · −/+ to zoom · click a node for detail</span>
              </div>
            </div>
          )}
        </div>

        {selId && sel && (
          <div className="context-panel">
            <div className="card">
              <div className="row" style={{ marginBottom: 8 }}>
                <h3 style={{ margin: 0 }}>{sel.name ?? selId.slice(0, 12)}</h3>
                <div className="spacer" style={{ flex: 1 }} />
                <button className="btn ghost sm" onClick={() => setSelId(null)} aria-label="Close detail">✕</button>
              </div>
              <div className="row wrap" style={{ gap: 6, marginBottom: 10 }}>
                <span className={"badge " + roleTone(sel.role)}>{sel.role ?? "operative"}</span>
                <Badge status={sel.status ?? "active"} />
              </div>
              <div className="mono" style={{ fontSize: 11, marginBottom: 10 }}>{selId.slice(0, 20)}</div>

              <div className="kv"><span className="muted">Title</span><span>{sel.title || "—"}</span></div>
              <div className="kv">
                <span className="muted">Rig (adapter)</span>
                <span>
                  {sel.rig ? (
                    <span className={"badge " + (selRunnable ? "done" : "blocked")}>
                      {sel.rig}{selRunnable ? "" : " · not ready"}
                    </span>
                  ) : <span className="muted">no rig</span>}
                </span>
              </div>
              <div className="kv"><span className="muted">Reports to</span><span>{nameOf(sel.reports_to) ?? <span className="muted">— (apex)</span>}</span></div>
              <div className="kv"><span className="muted">Direct reports</span><span>{directReports(selId)}</span></div>
              <div className="kv"><span className="muted">Running now</span><span>{(running.get(selId) ?? 0) > 0 ? <span className="badge in_progress">{running.get(selId)}</span> : <span className="muted">0</span>}</span></div>

              {/* Keys + allowance + capability — the §9 governance face, read-only. */}
              <div className="op-group" style={{ marginTop: 12 }}>
                <div className="op-group-title">Keys &amp; allowance</div>
                {selDetail === undefined ? (
                  <div className="loading" style={{ fontSize: 12 }}>Loading permissions…</div>
                ) : !selDetail.keys ? (
                  <div className="muted" style={{ fontSize: 12 }}>No Keys recorded for this Operative.</div>
                ) : (
                  <div style={{ fontSize: 12 }}>
                    <div className="kv"><span className="muted">Spawn agents</span><span>{flag(selDetail.keys.can_spawn_agents, selDetail.keys.spawn_route)}</span></div>
                    <div className="kv"><span className="muted">Assign work</span><span>{flag(selDetail.keys.can_assign_work, selDetail.keys.assign_scope)}</span></div>
                    <div className="kv"><span className="muted">Manage work</span><span>{flag(selDetail.keys.can_manage_work, selDetail.keys.manage_scope)}</span></div>
                    <div className="kv"><span className="muted">Configure agents</span><span>{flag(selDetail.keys.can_configure_agents, selDetail.keys.configure_scope)}</span></div>
                    <div className="kv"><span className="muted">Monthly Allowance</span><span>{fmtCents(selDetail.keys.monthly_allowance_cents)}</span></div>
                    <div className="kv"><span className="muted">Max concurrent</span><span>{selDetail.keys.max_concurrent_runs ?? "—"}</span></div>
                  </div>
                )}
              </div>

              {selDetail?.detail && (
                <div className="op-group" style={{ marginTop: 12 }}>
                  <div className="op-group-title">Capability ceiling</div>
                  <div className="kv" style={{ fontSize: 12 }}>
                    <span className="muted">Risk ceiling</span>
                    <span>{selDetail.detail.risk_ceiling ? <span className="badge in_review" style={{ fontSize: 9 }}>{selDetail.detail.risk_ceiling}</span> : "—"}</span>
                  </div>
                </div>
              )}

              <div className="row" style={{ marginTop: 12, gap: 8 }}>
                <Link to="/agents" className="link" style={{ fontSize: 12 }}>Manage on Crew →</Link>
                <Link to="/costs" className="link" style={{ fontSize: 12 }}>Costs →</Link>
              </div>
            </div>
          </div>
        )}
      </div>
    </Section>
  );
}

// Yes/no Key chip with an optional scope/route suffix.
function flag(on?: boolean, scope?: string) {
  return on
    ? <span className="badge done" style={{ fontSize: 9 }}>yes{scope ? ` · ${scope}` : ""}</span>
    : <span className="badge backlog" style={{ fontSize: 9 }}>no</span>;
}
