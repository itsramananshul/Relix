import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link } from "react-router-dom";
import { tryGet } from "../api";
import { Badge, Empty, Section, useAsync } from "../components/common";

// The Lattice — the company's hierarchy view (lexicon: "The Lattice" = the org
// chart; internal edges stay `reports_to`). dashboard-design §9: a dense,
// inspectable reports-to tree (nodes + edges), each node showing role/title/
// status/rig + counts, click → a per-Operative governance detail. B&W aesthetic
// (§12); color is reserved for semantic status only.
//
// Pan/zoom (design §9 asks for a "pan/zoom/pinch tree"): the stage is a true
// transformed viewport — drag-pan (pointer events), cursor-anchored wheel zoom,
// and two-finger pinch — plus explicit −/+/Fit/Reset controls. No dependency:
// the whole tree is one CSS `transform: translate() scale()` over a fixed
// coordinate space, driven by native PointerEvent/WheelEvent, so it stays
// CSP-clean and dependency-free. (Roadmap §P2 slice 4 — the deferred
// drag-pan/pinch/fit gap is now closed.)

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

const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v));

// Tree-layout geometry (px, in unscaled stage coordinates).
const NODE_W = 200;
const NODE_H = 92;
const H_GAP = 28;
const V_GAP = 62;

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

const ZOOM_MIN = 0.3;
const ZOOM_MAX = 2.5;

// The transformed view: scale `s`, translate (`tx`,`ty`) in viewport px. A stage
// point (sx,sy) renders at screen (tx + s*sx, ty + s*sy); transform-origin 0 0.
interface View { s: number; tx: number; ty: number }

// The canonical default view (top-left, 1×). Reset returns here.
const DEFAULT_VIEW: View = { s: 1, tx: 24, ty: 24 };

// ── Persistent local viewport (dashboard-design §9) ─────────────────────────
// The org chart's pan/zoom is kept in the browser under a versioned key so a
// refresh/return restores the operator's last view instead of snapping back to
// the auto-fit. This mirrors the Chat page's local-session pattern (§13): it is
// LOCAL UI PREFERENCE ONLY — three plain numbers (scale + pan x/y), never any
// company data. Every read is hard-validated (finite numbers, scale within the
// zoom range, pan within a sane bound); anything corrupt/foreign resets cleanly
// to null so the chart falls back to the default + auto-fit.
const VIEWPORT_STORAGE_KEY = "relix.lattice.viewport.v1";
// Generous absolute bound on a persisted pan offset (px). Real pans never
// approach this; it only rejects absurd/corrupt values that survive the finite
// check.
const PAN_LIMIT = 100000;

function loadViewport(): View | null {
  try {
    const raw = typeof localStorage !== "undefined" ? localStorage.getItem(VIEWPORT_STORAGE_KEY) : null;
    if (!raw) return null;
    const p: unknown = JSON.parse(raw);
    if (!p || typeof p !== "object") return null;
    const { s, tx, ty } = p as Record<string, unknown>;
    if (typeof s !== "number" || typeof tx !== "number" || typeof ty !== "number") return null;
    if (!Number.isFinite(s) || !Number.isFinite(tx) || !Number.isFinite(ty)) return null;
    if (s < ZOOM_MIN || s > ZOOM_MAX) return null;
    if (Math.abs(tx) > PAN_LIMIT || Math.abs(ty) > PAN_LIMIT) return null;
    return { s, tx, ty };
  } catch {
    // Parse/shape error → drop the bad value so we never read it again.
    try {
      localStorage.removeItem(VIEWPORT_STORAGE_KEY);
    } catch {
      /* storage unavailable — fall through to the in-memory default */
    }
    return null;
  }
}

// Persist only the three view numbers. Best-effort — a storage failure (private
// mode / quota) must never break the chart.
function saveViewport(v: View): void {
  try {
    localStorage.setItem(VIEWPORT_STORAGE_KEY, JSON.stringify({ s: v.s, tx: v.tx, ty: v.ty }));
  } catch {
    /* storage unavailable — the viewport stays in-memory for this session */
  }
}

export function Lattice() {
  const [selId, setSelId] = useState<string | null>(null);
  const [detailCache, setDetailCache] = useState<Record<string, { keys: Keys | null; detail: AgentDetail | null }>>({});

  // ── Pan/zoom view state + interaction refs ──────────────────────────────
  const viewportRef = useRef<HTMLDivElement>(null);
  // Read the persisted viewport ONCE at mount (validated; corrupt → null). Held
  // in a ref so the read happens a single time, not on every render.
  const restoredRef = useRef<View | null | undefined>(undefined);
  if (restoredRef.current === undefined) restoredRef.current = loadViewport();
  const [view, setView] = useState<View>(restoredRef.current ?? DEFAULT_VIEW);
  // A live mirror so the native (non-passive) wheel listener + pointer math read
  // the current view without re-binding every frame.
  const viewRef = useRef(view);
  viewRef.current = view;
  // If a saved viewport was restored, the one-time auto-fit is already satisfied —
  // we must NOT fit over the operator's last view.
  const didFitRef = useRef(restoredRef.current !== null);
  // Active touch/mouse pointers (id → last client position) for pinch + pan.
  const pointersRef = useRef<Map<number, { x: number; y: number }>>(new Map());
  const dragRef = useRef<{ sx: number; sy: number; baseTx: number; baseTy: number; active: boolean; moved: boolean } | null>(null);
  const pinchRef = useRef<{ dist: number } | null>(null);
  // True the instant a pan crosses the move threshold — read by a node's click
  // to swallow the click that would otherwise fire after a drag (drag ≠ select).
  const pannedRef = useRef(false);
  const [grabbing, setGrabbing] = useState(false);

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
  const childrenOfSel = useMemo(
    () => (selId ? ops.filter((o) => o.reports_to === selId) : []),
    [ops, selId],
  );
  const directReports = (id?: string) =>
    id ? ops.filter((o) => o.reports_to === id).length : 0;
  // Honest per-Operative Rig readiness: a bound adapter that probes available.
  const rigReady = (rig?: string | null) =>
    !!rig && byName.get(rig)?.probe?.status === "available";

  const initialized = company.initialized ?? ops.length > 0;
  const stageReady = !loading && initialized && ops.length > 0;

  // ── View controls (Fit / Reset / zoom) ──────────────────────────────────
  // Anchor a zoom around a viewport-relative point so that point stays put.
  const zoomAround = useCallback((nextScale: number, cx: number, cy: number) => {
    setView((v) => {
      const s = clamp(nextScale, ZOOM_MIN, ZOOM_MAX);
      const k = s / v.s;
      return { s, tx: cx - k * (cx - v.tx), ty: cy - k * (cy - v.ty) };
    });
  }, []);
  const zoomByCenter = useCallback((factor: number) => {
    const el = viewportRef.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    zoomAround(viewRef.current.s * factor, r.width / 2, r.height / 2);
  }, [zoomAround]);
  // Fit: scale + center so the whole tree (with padding) sits in the viewport.
  // Never enlarges past 1× (a tiny tree shouldn't balloon).
  const fit = useCallback(() => {
    const el = viewportRef.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    if (r.width === 0 || r.height === 0 || w === 0 || h === 0) return;
    const pad = 36;
    const s = clamp(Math.min((r.width - pad * 2) / w, (r.height - pad * 2) / h, 1), ZOOM_MIN, ZOOM_MAX);
    setView({ s, tx: (r.width - w * s) / 2, ty: (r.height - h * s) / 2 });
  }, [w, h]);
  // Reset → the canonical default view. The persist effect mirrors the live view
  // to storage, so Reset OVERWRITES the saved viewport with the default (it does
  // not leave a stale saved view behind) — and Fit likewise persists the fitted
  // view. Semantics: storage always reflects what's on screen.
  const reset = useCallback(() => setView(DEFAULT_VIEW), []);

  // Frame the tree once on first render (so it opens centered, not top-left).
  // Guarded so it never re-fits — the chart must not jump while the operator
  // hovers/clicks (requirement: no resize/jump on interaction).
  useEffect(() => {
    if (!stageReady || didFitRef.current) return;
    const el = viewportRef.current;
    if (!el) return;
    if (el.getBoundingClientRect().width === 0) return;
    didFitRef.current = true;
    fit();
  }, [stageReady, fit]);

  // Persist the viewport locally so a refresh/return keeps the operator's last
  // pan/zoom. Debounced: a drag/pinch/wheel gesture fires many `setView`s, so we
  // write once it settles rather than on every frame. localStorage MIRRORS the
  // live view — that's what makes Fit persist the fitted view and Reset overwrite
  // with the default.
  useEffect(() => {
    const t = setTimeout(() => saveViewport(view), 250);
    return () => clearTimeout(t);
  }, [view]);

  // Cursor-anchored wheel zoom. Bound natively (not via React's passive onWheel)
  // so `preventDefault` actually suppresses the page scroll.
  useEffect(() => {
    const el = viewportRef.current;
    if (!el || !stageReady) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const r = el.getBoundingClientRect();
      const cx = e.clientX - r.left;
      const cy = e.clientY - r.top;
      const factor = Math.exp(-e.deltaY * 0.0015);
      zoomAround(viewRef.current.s * factor, cx, cy);
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, [stageReady, zoomAround]);

  // ── Pointer interaction: drag-pan (1 pointer) + pinch-zoom (2 pointers) ──
  const onPointerDown = (e: React.PointerEvent) => {
    pointersRef.current.set(e.pointerId, { x: e.clientX, y: e.clientY });
    const n = pointersRef.current.size;
    if (n === 1) {
      dragRef.current = {
        sx: e.clientX,
        sy: e.clientY,
        baseTx: viewRef.current.tx,
        baseTy: viewRef.current.ty,
        active: true,
        moved: false,
      };
      pannedRef.current = false;
    } else if (n === 2) {
      // Second finger down → start a pinch; abandon the pan in progress.
      dragRef.current = null;
      const p = [...pointersRef.current.values()];
      pinchRef.current = { dist: Math.hypot(p[0].x - p[1].x, p[0].y - p[1].y) };
    }
  };
  const onPointerMove = (e: React.PointerEvent) => {
    if (!pointersRef.current.has(e.pointerId)) return;
    pointersRef.current.set(e.pointerId, { x: e.clientX, y: e.clientY });
    const n = pointersRef.current.size;
    if (n >= 2 && pinchRef.current) {
      const el = viewportRef.current;
      if (!el) return;
      const r = el.getBoundingClientRect();
      const p = [...pointersRef.current.values()];
      const dist = Math.hypot(p[0].x - p[1].x, p[0].y - p[1].y);
      const mx = (p[0].x + p[1].x) / 2 - r.left;
      const my = (p[0].y + p[1].y) / 2 - r.top;
      const factor = dist / (pinchRef.current.dist || dist);
      zoomAround(viewRef.current.s * factor, mx, my);
      pinchRef.current = { dist };
      return;
    }
    const d = dragRef.current;
    if (d && d.active) {
      const dx = e.clientX - d.sx;
      const dy = e.clientY - d.sy;
      if (!d.moved && Math.hypot(dx, dy) > 4) {
        d.moved = true;
        pannedRef.current = true;
        setGrabbing(true);
        try { viewportRef.current?.setPointerCapture(e.pointerId); } catch { /* capture is best-effort */ }
      }
      if (d.moved) setView({ s: viewRef.current.s, tx: d.baseTx + dx, ty: d.baseTy + dy });
    }
  };
  const endPointer = (e: React.PointerEvent) => {
    pointersRef.current.delete(e.pointerId);
    try { viewportRef.current?.releasePointerCapture(e.pointerId); } catch { /* never had capture */ }
    if (pointersRef.current.size < 2) pinchRef.current = null;
    if (pointersRef.current.size === 0) {
      if (dragRef.current) dragRef.current.active = false;
      setGrabbing(false);
    }
  };

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
  // A node's click: swallow the click that trails a drag (so panning never
  // selects), otherwise open the detail.
  const onNodeClick = (id: string) => {
    if (pannedRef.current) {
      pannedRef.current = false;
      return;
    }
    select(id);
  };

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
  const selRunnable = rigReady(sel?.rig);
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
        <div className="lattice-controls" role="group" aria-label="Org chart view controls">
          <button className="btn ghost sm" aria-label="Zoom out" onClick={() => zoomByCenter(1 / 1.2)}>−</button>
          <button className="btn ghost sm" aria-label="Current zoom" title="Current zoom" onClick={reset}>{Math.round(view.s * 100)}%</button>
          <button className="btn ghost sm" aria-label="Zoom in" onClick={() => zoomByCenter(1.2)}>+</button>
          <button className="btn ghost sm" onClick={fit} title="Fit the whole tree in view (saved locally)">Fit</button>
          <button className="btn ghost sm" onClick={reset} title="Reset to the default view — overwrites the locally-saved viewport">Reset</button>
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
              <div
                ref={viewportRef}
                className={"lattice-stage-wrap" + (grabbing ? " grabbing" : "")}
                onPointerDown={onPointerDown}
                onPointerMove={onPointerMove}
                onPointerUp={endPointer}
                onPointerCancel={endPointer}
                onPointerLeave={endPointer}
              >
                <div
                  className="lattice-stage"
                  style={{ width: w, height: h, transform: `translate(${view.tx}px, ${view.ty}px) scale(${view.s})` }}
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
                    const ready = rigReady(p.op.rig);
                    return (
                      <button
                        key={id}
                        type="button"
                        className={"lattice-node" + (selId === id ? " selected" : "")}
                        style={{ left: p.x, top: p.y, width: NODE_W, height: NODE_H }}
                        onClick={() => onNodeClick(id)}
                        title={`${p.op.name ?? id} — ${p.op.role ?? "operative"}`}
                      >
                        <div className="ln-head">
                          <span className={"dot " + statusDot(p.op.status)} />
                          <span className="ln-name">{p.op.name ?? id.slice(0, 10)}</span>
                          {run > 0 && <span className="ln-live" title={`${run} running`}>live</span>}
                        </div>
                        {p.op.title && <div className="ln-title">{p.op.title}</div>}
                        <div className="ln-meta">
                          <span className={"badge " + roleTone(p.op.role)} style={{ fontSize: 9 }}>
                            {p.op.role ?? "operative"}
                          </span>
                          {p.op.rig ? (
                            <span className={"badge " + (ready ? "" : "blocked")} style={{ fontSize: 9 }}>
                              {ready ? p.op.rig : `${p.op.rig} · not ready`}
                            </span>
                          ) : (
                            <span className="badge backlog" style={{ fontSize: 9 }}>no rig</span>
                          )}
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
                <span>drag to pan · scroll or pinch to zoom · click a node for detail</span>
                <span title="Your pan/zoom is remembered in this browser only — not the server.">view saved locally</span>
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
                {(running.get(selId) ?? 0) > 0 && <span className="badge in_progress">running</span>}
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

              {/* Direct reports — click to walk the tree without losing the panel. */}
              {childrenOfSel.length > 0 && (
                <div className="op-group" style={{ marginTop: 12 }}>
                  <div className="op-group-title">Direct reports ({childrenOfSel.length})</div>
                  <div className="ln-reports">
                    {childrenOfSel.map((c) => (
                      <button
                        key={c.agent_id}
                        type="button"
                        className="link ln-report"
                        onClick={() => c.agent_id && select(c.agent_id)}
                      >
                        <span className={"dot " + statusDot(c.status)} />
                        {c.name ?? c.agent_id?.slice(0, 10)}
                        <span className="muted" style={{ fontSize: 10 }}>{c.role ?? "operative"}</span>
                      </button>
                    ))}
                  </div>
                </div>
              )}

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
                {/* Deep link straight to THIS Operative's governance detail on
                    the Crew page (`/agents?agent=<id>`) — selected, scrolled
                    into view, permissions panel open — not the generic roster. */}
                <Link to={`/agents?agent=${encodeURIComponent(selId)}`} className="link" style={{ fontSize: 12 }}>Govern on Crew →</Link>
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
