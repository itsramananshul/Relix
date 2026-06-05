import { useState } from "react";
import { Link } from "react-router-dom";
import { api, tryGet } from "../api";
import { extractList, Section, useAsync } from "../components/common";

interface Mandate { mandate_id?: string; id?: string; title?: string; name?: string; status?: string; description?: string }
interface Card { task_id?: string; id?: string; title?: string; board_status?: string; assignee_agent_id?: string | null }
interface Operative { agent_id?: string; name?: string; role?: string; rig?: string | null }
interface Adapter { name?: string; probe?: { status?: string } }

// The orchestration result (`POST …/orchestrate` + `…/orchestration/latest`).
interface Orchestration {
  mode?: string;
  dry_run?: boolean;
  ready?: boolean;
  status?: string;
  blockers?: unknown[];
  next_actions?: string[];
  created_briefs?: unknown[];
  existing_briefs?: unknown[];
  assigned_briefs?: unknown[];
  skipped?: unknown[];
  placeholder_tracks_created?: unknown[];
  placeholder_tracks_existing?: unknown[];
  placeholder_tracks_omitted?: unknown[];
}

const MODES = [
  { v: "plan_only", label: "Plan only", hint: "compute the plan, create nothing" },
  { v: "create_briefs", label: "Create Briefs", hint: "create the Brief tree, no assignment" },
  { v: "assign_ready", label: "Create + assign", hint: "create + assign ready work to the active team" },
] as const;

const COLS = ["backlog", "todo", "in_progress", "in_review", "done"];

function mid(m: Mandate): string { return m.mandate_id ?? m.id ?? ""; }
function len(v?: unknown[]): number { return Array.isArray(v) ? v.length : 0; }
function blockerText(b: unknown): string {
  if (typeof b === "string") return b;
  if (b && typeof b === "object") {
    const o = b as Record<string, unknown>;
    return String(o.reason ?? o.message ?? o.blocker ?? JSON.stringify(o));
  }
  return String(b);
}

export function Mandates() {
  const [selected, setSelected] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [title, setTitle] = useState("");
  const [spec, setSpec] = useState("");
  const [mode, setMode] = useState<string>("plan_only");
  const [maxBriefs, setMaxBriefs] = useState(8);
  const [result, setResult] = useState<Orchestration | null>(null);
  const [busy, setBusy] = useState(false);
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);

  const { data, loading, reload } = useAsync(async () => {
    const [mandates, ops, adapters] = await Promise.all([
      tryGet<unknown>("/v1/spine/mandates?limit=50", {}),
      tryGet<Operative[]>("/v1/spine/operatives", []),
      tryGet<Adapter[]>("/v1/adapters", []),
    ]);
    return {
      mandates: extractList<Mandate>(mandates, ["mandates"]),
      operatives: Array.isArray(ops) ? ops : [],
      adapters: Array.isArray(adapters) ? adapters : [],
    };
  }, []);

  const detail = useAsync(async () => {
    if (!selected) return { briefs: [] as Card[], latest: null as Orchestration | null };
    const [briefs, latest] = await Promise.all([
      tryGet<unknown>(`/v1/spine/mandates/${encodeURIComponent(selected)}/briefs`, {}),
      tryGet<Orchestration | null>(`/v1/spine/mandates/${encodeURIComponent(selected)}/orchestration/latest`, null),
    ]);
    return { briefs: extractList<Card>(briefs, ["briefs"]), latest: latest ?? null };
  }, [selected]);

  const mandates = data?.mandates ?? [];
  const operatives = data?.operatives ?? [];
  const adapters = data?.adapters ?? [];
  const availAdapters = adapters.filter((a) => a.probe?.status === "available").length;
  const hasOps = operatives.length > 0;
  const briefs = detail.data?.briefs ?? [];
  const latest = result ?? detail.data?.latest ?? null;

  const byCol: Record<string, number> = {};
  for (const b of briefs) byCol[b.board_status ?? "todo"] = (byCol[b.board_status ?? "todo"] ?? 0) + 1;
  const total = briefs.length;
  const done = byCol.done ?? 0;
  const unassigned = briefs.filter((b) => !b.assignee_agent_id).length;

  async function create() {
    if (!title.trim()) return;
    setBanner(null);
    try {
      const r = await api.post<{ mandate_id?: string }>("/v1/spine/mandates", {
        title: title.trim(),
        description: spec.trim(),
      });
      setBanner({ kind: "ok", msg: "Mandate created. Now run orchestration to turn it into Briefs." });
      setTitle(""); setSpec(""); setCreating(false);
      reload();
      if (r.mandate_id) { setSelected(r.mandate_id); setResult(null); }
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Create failed" });
    }
  }

  async function orchestrate(dryRun: boolean) {
    if (!selected) return;
    setBusy(true); setBanner(null);
    try {
      const r = await api.post<Orchestration>(`/v1/spine/mandates/${encodeURIComponent(selected)}/orchestrate`, {
        mode, max_briefs: maxBriefs, dry_run: dryRun,
      });
      setResult(r);
      const created = len(r.created_briefs), assigned = len(r.assigned_briefs);
      setBanner({
        kind: dryRun ? "info" : "ok",
        msg: dryRun
          ? `Preview: would create ${created} Brief(s)${mode === "assign_ready" ? `, assign ${len(r.assigned_briefs)}` : ""}. Nothing created.`
          : `Orchestration ${r.status ?? "done"}: ${created} Brief(s) created, ${assigned} assigned.`,
      });
      detail.reload();
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Orchestrate failed" });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="grid">
      <Section
        title="Mandates"
        action={<button className="btn" onClick={() => setCreating((v) => !v)}>{creating ? "Cancel" : "+ New Mandate"}</button>}
      >
        {banner && <div className={"banner " + banner.kind}>{banner.msg}</div>}
        {!loading && !hasOps && (
          <div className="banner info banner-action">
            <span>No Operatives yet — create a Mandate now, but to assign + run its Briefs you need a Founder.</span>
            <Link to="/agents" className="banner-cta">Initialize company →</Link>
          </div>
        )}
        {!loading && hasOps && availAdapters === 0 && (
          <div className="banner info banner-action">
            <span>No agent adapter is available — Briefs can be created + assigned, but not run until an adapter is installed.</span>
            <Link to="/settings" className="banner-cta">Open Settings →</Link>
          </div>
        )}

        {creating && (
          <div className="card" style={{ marginBottom: 14 }}>
            <label className="field">
              <span>Mandate title — the big goal</span>
              <input className="input" autoFocus placeholder="e.g. Build a login page and wire it to auth" value={title} onChange={(e) => setTitle(e.target.value)} />
            </label>
            <label className="field">
              <span>Spec / description (optional)</span>
              <textarea className="input" rows={3} placeholder="What does done look like? Constraints, acceptance criteria…" value={spec} onChange={(e) => setSpec(e.target.value)} />
            </label>
            <button className="btn" onClick={create} disabled={!title.trim()}>Create Mandate</button>
          </div>
        )}

        <div className="grid cols-2">
          {/* Mandate list */}
          <div className="card">
            <h3>Goals</h3>
            {loading ? (
              <div className="loading">Loading…</div>
            ) : mandates.length === 0 ? (
              <div className="empty">Create a Mandate to turn a big goal into Briefs.</div>
            ) : (
              <div>
                {mandates.map((m) => {
                  const id = mid(m);
                  const sel = selected === id;
                  return (
                    <div
                      key={id}
                      className="mandate-row"
                      onClick={() => { setSelected(id); setResult(null); }}
                      style={sel ? { borderColor: "var(--text-faint)", background: "var(--bg-elev)" } : undefined}
                    >
                      <div className="row" style={{ justifyContent: "space-between" }}>
                        <strong style={{ fontSize: 13 }}>{m.title ?? m.name ?? "(untitled)"}</strong>
                        <span className={"badge " + (m.status ?? "todo")} style={{ fontSize: 9 }}>{m.status ?? "—"}</span>
                      </div>
                      <div className="mono" style={{ fontSize: 10 }}>{id.slice(0, 16)}</div>
                    </div>
                  );
                })}
              </div>
            )}
          </div>

          {/* Selected mandate detail + orchestration */}
          <div className="card">
            {!selected ? (
              <div className="empty">Select a Mandate to plan + decompose it into Briefs.</div>
            ) : (
              <>
                <h3>Decompose into Briefs</h3>
                <div className="row wrap" style={{ gap: 8, alignItems: "flex-end", marginBottom: 10 }}>
                  <label className="field" style={{ margin: 0, flex: 1, minWidth: 150 }}>
                    <span>Mode</span>
                    <select className="select" value={mode} onChange={(e) => setMode(e.target.value)}>
                      {MODES.map((m) => <option key={m.v} value={m.v}>{m.label}</option>)}
                    </select>
                  </label>
                  <label className="field" style={{ margin: 0, width: 110 }}>
                    <span>Max Briefs</span>
                    <input className="input" type="number" min={1} value={maxBriefs} onChange={(e) => setMaxBriefs(Math.max(1, Number(e.target.value) || 1))} />
                  </label>
                </div>
                <div className="muted" style={{ fontSize: 11, marginBottom: 8 }}>{MODES.find((m) => m.v === mode)?.hint}</div>
                <div className="row" style={{ gap: 8 }}>
                  <button className="btn ghost" disabled={busy} onClick={() => orchestrate(true)}>{busy ? "…" : "Dry-run preview"}</button>
                  <button className="btn" disabled={busy || mode === "plan_only"} title={mode === "plan_only" ? "Pick Create Briefs or Create + assign" : ""} onClick={() => orchestrate(false)}>
                    {mode === "assign_ready" ? "Create & assign" : "Create Briefs"}
                  </button>
                </div>

                {/* Latest orchestration result */}
                {latest && (
                  <div style={{ marginTop: 14, borderTop: "1px solid var(--border-soft)", paddingTop: 10 }}>
                    <div className="row" style={{ gap: 8, marginBottom: 6 }}>
                      <strong style={{ fontSize: 12 }}>Latest plan</strong>
                      <span className={"badge " + (latest.ready ? "done" : "in_progress")} style={{ fontSize: 9 }}>{latest.status ?? (latest.ready ? "ready" : "planning")}</span>
                      {latest.dry_run && <span className="badge todo" style={{ fontSize: 9 }}>dry-run</span>}
                      <span className="muted" style={{ fontSize: 11 }}>mode: {latest.mode}</span>
                    </div>
                    <div className="row wrap" style={{ gap: 6, fontSize: 11, marginBottom: 6 }}>
                      <span className="badge done">{len(latest.created_briefs)} created</span>
                      <span className="badge todo">{len(latest.existing_briefs)} existing</span>
                      <span className="badge in_progress">{len(latest.assigned_briefs)} assigned</span>
                      <span className="badge backlog">{len(latest.skipped)} skipped</span>
                      <span className="badge blocked">{len(latest.blockers)} blocker(s)</span>
                    </div>
                    {len(latest.blockers) > 0 && (
                      <div className="banner err" style={{ fontSize: 11 }}>
                        Blockers: {(latest.blockers ?? []).slice(0, 4).map(blockerText).join("; ")}
                      </div>
                    )}
                    {(latest.next_actions ?? []).length > 0 && (
                      <ul className="next-steps" style={{ fontSize: 12 }}>
                        {(latest.next_actions ?? []).slice(0, 4).map((a, i) => <li key={i}>{a}</li>)}
                      </ul>
                    )}
                  </div>
                )}

                {/* Brief progress */}
                <div style={{ marginTop: 14, borderTop: "1px solid var(--border-soft)", paddingTop: 10 }}>
                  <div className="row" style={{ marginBottom: 6 }}>
                    <strong style={{ fontSize: 12 }}>Briefs</strong>
                    <span className="muted" style={{ fontSize: 11, marginLeft: 8 }}>{done}/{total} done{unassigned > 0 ? ` · ${unassigned} unassigned` : ""}</span>
                    <div className="spacer" style={{ flex: 1 }} />
                    <Link to="/briefs" className="link" style={{ fontSize: 11 }}>board →</Link>
                  </div>
                  {detail.loading ? (
                    <div className="loading">Loading Briefs…</div>
                  ) : total === 0 ? (
                    <div className="muted" style={{ fontSize: 12 }}>No Briefs yet — run orchestration above to create them.</div>
                  ) : (
                    <>
                      <div className="progress-bar"><div className="progress-fill" style={{ width: `${total ? Math.round((done / total) * 100) : 0}%` }} /></div>
                      <div className="pill-row" style={{ marginTop: 8 }}>
                        {COLS.filter((c) => (byCol[c] ?? 0) > 0).map((c) => (
                          <span key={c} className="row" style={{ gap: 5 }}>
                            <span className={"badge " + c} style={{ fontSize: 9 }}>{c}</span><strong style={{ fontSize: 12 }}>{byCol[c]}</strong>
                          </span>
                        ))}
                      </div>
                    </>
                  )}
                </div>
              </>
            )}
          </div>
        </div>
      </Section>
    </div>
  );
}
