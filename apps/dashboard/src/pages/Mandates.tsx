import { useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { api, tryGet } from "../api";
import { extractList, Section, useAsync } from "../components/common";
import { invalidate } from "../invalidate";

// The safe-local Rig bound at hire approval so the Operative is immediately
// runnable (company-model §12.6 — `agent.approve_hire` accepts a `rig`).
const SAFE_RIG = "echo";

interface Mandate { mandate_id?: string; id?: string; title?: string; name?: string; status?: string; description?: string }
interface Card { task_id?: string; id?: string; title?: string; board_status?: string; assignee_agent_id?: string | null }
interface Operative { agent_id?: string; name?: string; role?: string; rig?: string | null }
interface Adapter { name?: string; probe?: { status?: string } }
interface Strategy { status?: string | null; approved?: boolean }
interface ActiveAgent { agent_id?: string; name?: string; role?: string; status?: string }
// A minted-but-unapproved hire from team readiness — carries the agent_id the
// operator approves/rejects via `agent.approve_hire` / `agent.reject_hire`.
interface PendingHire { agent_id?: string; role?: string; status?: string; suggested_rig?: string }
interface Readiness {
  planned?: boolean; plan_status?: string | null; readiness?: string; next_action?: string;
  missing_roles?: unknown[]; pending_hires?: PendingHire[]; pending_clearances?: { clearance_id?: string; status?: string }[];
  active_agents?: ActiveAgent[]; blocked_roles?: unknown[];
}
interface Clearance { approval_id?: string; agent_id?: string; method?: string; reason?: string }
interface Orchestration {
  mode?: string; dry_run?: boolean; ready?: boolean; status?: string;
  blockers?: unknown[]; next_actions?: string[];
  created_briefs?: unknown[]; existing_briefs?: unknown[]; assigned_briefs?: unknown[]; skipped?: unknown[];
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
const READY_TONE: Record<string, string> = { ready: "done", staffing: "in_progress", awaiting_clearance: "in_progress", not_planned: "backlog" };

export function Mandates() {
  // The selected Mandate is URL-driven (`/mandates?mandate=<id>`), so the
  // Action Center's "Approve strategy" card (and any deep link) lands on the
  // right Mandate with context — mirroring the Runs page's `?run=` pattern.
  const [searchParams, setSearchParams] = useSearchParams();
  const selected = searchParams.get("mandate");
  const [creating, setCreating] = useState(false);
  const [title, setTitle] = useState("");
  const [spec, setSpec] = useState("");
  const [mode, setMode] = useState<string>("assign_ready");
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

  // Full governance + work bundle for the selected Mandate.
  const detail = useAsync(async () => {
    if (!selected) return null;
    const id = encodeURIComponent(selected);
    const [strategy, readiness, briefs, latest, clearances] = await Promise.all([
      tryGet<Strategy | null>(`/v1/spine/mandates/${id}/strategy`, null),
      tryGet<Readiness | null>(`/v1/spine/mandates/${id}/team_readiness`, null),
      tryGet<unknown>(`/v1/spine/mandates/${id}/briefs`, {}),
      tryGet<Orchestration | null>(`/v1/spine/mandates/${id}/orchestration/latest`, null),
      tryGet<Clearance[]>(`/v1/spine/clearances?limit=50`, []),
    ]);
    return {
      strategy: strategy ?? null,
      readiness: readiness ?? null,
      briefs: extractList<Card>(briefs, ["briefs"]),
      latest: latest ?? null,
      clearances: Array.isArray(clearances) ? clearances : [],
    };
  }, [selected]);

  const mandates = data?.mandates ?? [];
  const operatives = data?.operatives ?? [];
  const adapters = data?.adapters ?? [];
  const availAdapters = adapters.filter((a) => a.probe?.status === "available").length;
  const hasOps = operatives.length > 0;

  const strat = detail.data?.strategy ?? null;
  const rdy = detail.data?.readiness ?? null;
  const briefs = detail.data?.briefs ?? [];
  const clearances = detail.data?.clearances ?? [];
  const latest = result ?? detail.data?.latest ?? null;

  const byCol: Record<string, number> = {};
  for (const b of briefs) byCol[b.board_status ?? "todo"] = (byCol[b.board_status ?? "todo"] ?? 0) + 1;
  const total = briefs.length;
  const done = byCol.done ?? 0;

  // Select a Mandate by writing the `?mandate=` param (or clear it). Resets the
  // local orchestration preview so it never bleeds across Mandates.
  function selectMandate(id: string | null) {
    const next = new URLSearchParams(searchParams);
    if (id) next.set("mandate", id);
    else next.delete("mandate");
    setSearchParams(next, { replace: true });
    setResult(null);
  }

  async function create() {
    if (!title.trim()) return;
    setBanner(null);
    try {
      const r = await api.post<{ mandate_id?: string }>("/v1/spine/mandates", { title: title.trim(), description: spec.trim() });
      setBanner({ kind: "ok", msg: "Mandate created. Now propose + approve a strategy to unblock orchestration." });
      setTitle(""); setSpec(""); setCreating(false);
      reload();
      if (r.mandate_id) selectMandate(r.mandate_id);
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Create failed" });
    }
  }

  // POST an action against the selected Mandate, then reload the bundle.
  async function act(path: string, body: unknown, okMsg: string) {
    if (!selected) return;
    setBusy(true); setBanner(null);
    try {
      await api.post(`/v1/spine/mandates/${encodeURIComponent(selected)}/${path}`, body ?? {});
      setBanner({ kind: "ok", msg: okMsg });
      detail.reload();
      reload();
      // Strategy / team-plan changes shift Mandate readiness + Action Center
      // next-actions — notify those surfaces (dashboard-design §11).
      invalidate(["mandates", "actions"]);
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Action failed" });
    } finally {
      setBusy(false);
    }
  }

  async function decideClearance(approvalId: string, decision: "approve" | "reject") {
    setBusy(true); setBanner(null);
    try {
      await api.post(`/v1/spine/clearances/${encodeURIComponent(approvalId)}/decide`, { decision });
      setBanner({ kind: "ok", msg: `Clearance ${decision}d.` });
      detail.reload();
      invalidate(["mandates", "actions"]);
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Clearance decision failed" });
    } finally {
      setBusy(false);
    }
  }

  // Approve a pending hire inline with the backend-suggested safe-local Rig so
  // the Operative is immediately runnable (company-model §12.6) — the same
  // approve-with-rig behavior as the Overview Action Center / Crew. `rig` comes
  // from the readiness payload's `suggested_rig` (falls back to the safe-local
  // `echo`). A clearance-gated hire is refused server-side and we point the
  // operator at the Clearance step.
  async function approveHire(agentId: string, role?: string, rig: string = SAFE_RIG) {
    setBusy(true); setBanner(null);
    try {
      const r = await api.post<{ runnable?: boolean; rig?: string; needs_rig?: boolean }>(
        `/v1/agents/${encodeURIComponent(agentId)}/approve-hire`,
        { rig },
      );
      setBanner({
        kind: "ok",
        msg: r.needs_rig
          ? `${role ?? "Operative"} hired — set an adapter to make it runnable.`
          : `${role ?? "Operative"} hired and runnable on ${r.rig ?? rig}.`,
      });
      detail.reload();
      reload();
      // A hire fills a seat — the board's Operative list + the Action Center
      // hire item update on the surfaces that show them (dashboard-design §11).
      invalidate(["actions", "briefs", "mandates"]);
    } catch (e) {
      const msg = e instanceof Error ? e.message : "Approve hire failed";
      setBanner({ kind: "err", msg: /clearance/i.test(msg) ? `${msg} — decide its Clearance below.` : msg });
    } finally {
      setBusy(false);
    }
  }

  async function rejectHire(agentId: string, role?: string) {
    setBusy(true); setBanner(null);
    try {
      await api.post(`/v1/agents/${encodeURIComponent(agentId)}/reject-hire`, {});
      setBanner({ kind: "ok", msg: `${role ?? "Hire"} declined — the role is left unfilled.` });
      detail.reload();
      reload();
      invalidate(["actions", "briefs", "mandates"]);
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Reject hire failed" });
    } finally {
      setBusy(false);
    }
  }

  async function orchestrate(dryRun: boolean) {
    if (!selected) return;
    setBusy(true); setBanner(null);
    try {
      const r = await api.post<Orchestration>(`/v1/spine/mandates/${encodeURIComponent(selected)}/orchestrate`, { mode, max_briefs: maxBriefs, dry_run: dryRun });
      setResult(r);
      const created = len(r.created_briefs), assigned = len(r.assigned_briefs);
      setBanner({
        kind: dryRun ? "info" : len(r.blockers) ? "err" : "ok",
        msg: dryRun
          ? `Preview: would create ${created} Brief(s). Nothing created.`
          : len(r.blockers)
            ? `Orchestration ${r.status}: ${len(r.blockers)} blocker(s) — resolve them below.`
            : `Created ${created} Brief(s), assigned ${assigned}.`,
      });
      detail.reload();
      reload();
      // A real (non-dry) orchestration creates/assigns Briefs — refresh the
      // board + the Action Center; a dry-run preview creates nothing (§11).
      if (!dryRun) invalidate(["briefs", "actions", "mandates"]);
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Orchestrate failed" });
    } finally {
      setBusy(false);
    }
  }

  // The single most important next step for this Mandate's workflow.
  const stratStatus = strat?.status ?? null;
  const stratApproved = strat?.approved ?? stratStatus === "approved";
  const ready = rdy?.readiness === "ready";
  let step: { kind: string; label: string } | null = null;
  if (!stratApproved) {
    if (!stratStatus || stratStatus === "rejected") step = { kind: "propose", label: "Propose a strategy" };
    else if (stratStatus === "proposed") step = { kind: "approve", label: "Approve the strategy" };
  } else if (!rdy?.planned) {
    step = { kind: "plan", label: "Plan the team" };
  } else if (!ready) {
    step = { kind: "resolve", label: rdy?.next_action || "Resolve clearances / readiness" };
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
            <span>No Operatives yet — create a Mandate now, but the team plan needs a Founder.</span>
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
                    <div key={id} className="mandate-row" onClick={() => selectMandate(id)} style={sel ? { borderColor: "var(--text-faint)", background: "var(--bg-elev)" } : undefined}>
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

          {/* Workflow for the selected Mandate */}
          <div className="card">
            {!selected ? (
              <div className="empty">Select a Mandate to drive it from blocked → ready → Briefs.</div>
            ) : detail.loading ? (
              <div className="loading">Loading workflow…</div>
            ) : (
              <>
                {/* Next step banner */}
                {step ? (
                  <div className="banner info" style={{ fontSize: 12 }}>Next: <strong>{step.label}</strong></div>
                ) : ready ? (
                  <div className="banner ok" style={{ fontSize: 12 }}>Team is ready — create Briefs below.</div>
                ) : null}

                {/* 1. Strategy */}
                <div className="wf-step">
                  <div className="row" style={{ marginBottom: 4 }}>
                    <strong style={{ fontSize: 12 }}>1 · Strategy</strong>
                    <span className={"badge " + (stratApproved ? "done" : stratStatus === "proposed" ? "in_progress" : stratStatus === "rejected" ? "blocked" : "backlog")} style={{ fontSize: 9, marginLeft: 8 }}>{stratStatus ?? "none"}</span>
                  </div>
                  <div className="row" style={{ gap: 6 }}>
                    {(!stratStatus || stratStatus === "rejected") && (
                      <button className="btn ghost sm" disabled={busy} onClick={() => act("strategy/propose", { doc: `Plan + execute: ${mandates.find((m) => mid(m) === selected)?.title ?? "this Mandate"}` }, "Strategy proposed. Approve it to unblock the team plan.")}>Propose strategy</button>
                    )}
                    {stratStatus === "proposed" && (
                      <>
                        <button className="btn sm" disabled={busy} onClick={() => act("strategy/approve", {}, "Strategy approved.")}>Approve</button>
                        <button className="btn ghost sm" disabled={busy} onClick={() => act("strategy/reject", {}, "Strategy rejected.")}>Reject</button>
                      </>
                    )}
                    {stratApproved && <span className="muted" style={{ fontSize: 11 }}>approved — the orchestration strategy gate is cleared</span>}
                  </div>
                </div>

                {/* 2. Team plan + readiness */}
                <div className="wf-step">
                  <div className="row" style={{ marginBottom: 4 }}>
                    <strong style={{ fontSize: 12 }}>2 · Team &amp; readiness</strong>
                    {rdy && <span className={"badge " + (READY_TONE[rdy.readiness ?? ""] ?? "backlog")} style={{ fontSize: 9, marginLeft: 8 }}>{rdy.readiness ?? "—"}</span>}
                  </div>
                  {!stratApproved ? (
                    <div className="muted" style={{ fontSize: 11 }}>Approve a strategy first.</div>
                  ) : (
                    <>
                      <div className="row" style={{ gap: 6, marginBottom: 6 }}>
                        <button className="btn ghost sm" disabled={busy} onClick={() => act("team_plan", {}, "Team planned. Resolve any pending clearances/hires below.")}>{rdy?.planned ? "Re-plan team" : "Plan team"}</button>
                        {rdy?.next_action && <span className="muted" style={{ fontSize: 11 }}>{rdy.next_action}</span>}
                      </div>
                      {rdy?.planned && (
                        <>
                          <div className="row wrap" style={{ gap: 6, fontSize: 11 }}>
                            <span className="badge done">{len(rdy.active_agents)} active</span>
                            <span className="badge in_progress">{len(rdy.pending_hires)} pending hire(s)</span>
                            <span className="badge in_progress">{len(rdy.pending_clearances)} pending clearance(s)</span>
                            <span className="badge backlog">{len(rdy.missing_roles)} missing role(s)</span>
                            {len(rdy.blocked_roles) > 0 && <span className="badge blocked">{len(rdy.blocked_roles)} blocked</span>}
                          </div>
                          {len(rdy.active_agents) > 0 && (
                            <div className="row wrap" style={{ gap: 6, marginTop: 6 }}>
                              <span className="muted" style={{ fontSize: 11 }}>On the team:</span>
                              {(rdy.active_agents ?? []).map((ag, i) => (
                                <span key={ag.agent_id ?? i} className="badge done" style={{ fontSize: 9 }} title={ag.agent_id}>
                                  {ag.name ?? (ag.agent_id ?? "").slice(0, 8)}{ag.role ? ` · ${ag.role}` : ""}
                                </span>
                              ))}
                            </div>
                          )}
                          {/* Pending hires — approve inline with the safe-local Rig (same
                              approve-with-rig behavior as Overview/Crew) so the seat is
                              filled and runnable without leaving this Mandate. */}
                          {(rdy.pending_hires ?? []).some((h) => h.agent_id) && (
                            <div style={{ marginTop: 6 }}>
                              <span className="muted" style={{ fontSize: 11 }}>Pending hires:</span>
                              {(rdy.pending_hires ?? []).filter((h) => h.agent_id).map((h, i) => {
                                // Use the backend-suggested safe-local Rig (same as the
                                // Action Center hire card); fall back to `echo` if absent.
                                const rig = h.suggested_rig || SAFE_RIG;
                                return (
                                <div key={h.agent_id ?? i} className="row wrap" style={{ gap: 6, padding: "3px 0", borderBottom: "1px solid var(--border-soft)" }}>
                                  <span className="mono" style={{ fontSize: 11 }} title={h.agent_id}>{(h.agent_id ?? "").slice(0, 10)}</span>
                                  <span className="muted" style={{ fontSize: 10, flex: 1, minWidth: 0, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{h.role || "role"}{h.status ? ` · ${h.status}` : ""}</span>
                                  <span className="btn-group">
                                    <button className="btn sm" disabled={busy} title={`Approve this hire on the safe-local ${rig} adapter so it is immediately runnable`} onClick={() => approveHire(h.agent_id!, h.role, rig)}>Approve · {rig}</button>
                                    <button className="btn ghost sm" disabled={busy} title="Decline this hire (the role is left unfilled)" onClick={() => rejectHire(h.agent_id!, h.role)}>Reject</button>
                                  </span>
                                </div>
                                );
                              })}
                            </div>
                          )}
                        </>
                      )}
                    </>
                  )}
                </div>

                {/* 3. Clearances (only if pending) */}
                {clearances.length > 0 && (
                  <div className="wf-step">
                    <strong style={{ fontSize: 12 }}>3 · Pending clearances</strong>
                    <div style={{ marginTop: 4 }}>
                      {clearances.map((c, i) => (
                        <div key={c.approval_id ?? i} className="row wrap" style={{ gap: 6, padding: "3px 0", borderBottom: "1px solid var(--border-soft)" }}>
                          <span className="mono" style={{ fontSize: 11 }}>{(c.agent_id ?? "").slice(0, 10) || (c.approval_id ?? "").slice(0, 10)}</span>
                          <span className="muted" style={{ fontSize: 10, flex: 1, minWidth: 0, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }} title={c.reason}>{c.method} · {c.reason}</span>
                          <span className="btn-group">
                            <button className="btn sm" disabled={busy || !c.approval_id} onClick={() => c.approval_id && decideClearance(c.approval_id, "approve")}>Approve</button>
                            <button className="btn ghost sm" disabled={busy || !c.approval_id} onClick={() => c.approval_id && decideClearance(c.approval_id, "reject")}>Reject</button>
                          </span>
                        </div>
                      ))}
                    </div>
                  </div>
                )}

                {/* 4. Orchestrate */}
                <div className="wf-step">
                  <strong style={{ fontSize: 12 }}>{clearances.length > 0 ? "4" : "3"} · Decompose into Briefs</strong>
                  <div className="row wrap" style={{ gap: 8, alignItems: "flex-end", margin: "6px 0" }}>
                    <label className="field" style={{ margin: 0, flex: 1, minWidth: 140 }}>
                      <span>Mode</span>
                      <select className="select" value={mode} onChange={(e) => setMode(e.target.value)}>{MODES.map((m) => <option key={m.v} value={m.v}>{m.label}</option>)}</select>
                    </label>
                    <label className="field" style={{ margin: 0, width: 100 }}>
                      <span>Max Briefs</span>
                      <input className="input" type="number" min={1} value={maxBriefs} onChange={(e) => setMaxBriefs(Math.max(1, Number(e.target.value) || 1))} />
                    </label>
                  </div>
                  <div className="row" style={{ gap: 8 }}>
                    <button className="btn ghost" disabled={busy} onClick={() => orchestrate(true)}>{busy ? "…" : "Dry-run preview"}</button>
                    <button className="btn" disabled={busy || mode === "plan_only" || !ready} title={!ready ? "Resolve the steps above first" : mode === "plan_only" ? "Pick Create Briefs or Create + assign" : ""} onClick={() => orchestrate(false)}>
                      {mode === "assign_ready" ? "Create & assign" : "Create Briefs"}
                    </button>
                  </div>

                  {latest && (
                    <div style={{ marginTop: 10 }}>
                      <div className="row wrap" style={{ gap: 6, fontSize: 11, marginBottom: 4 }}>
                        <span className={"badge " + (latest.ready ? "done" : "in_progress")} style={{ fontSize: 9 }}>{latest.status ?? "—"}</span>
                        {latest.dry_run && <span className="badge todo" style={{ fontSize: 9 }}>dry-run</span>}
                        <span className="badge done">{len(latest.created_briefs)} created</span>
                        <span className="badge in_progress">{len(latest.assigned_briefs)} assigned</span>
                        <span className="badge blocked">{len(latest.blockers)} blocker(s)</span>
                      </div>
                      {len(latest.blockers) > 0 && (
                        <div className="banner err" style={{ fontSize: 11 }}>Blockers: {(latest.blockers ?? []).slice(0, 4).map(blockerText).join("; ")}</div>
                      )}
                    </div>
                  )}
                </div>

                {/* Brief progress */}
                <div className="wf-step" style={{ borderBottom: "none" }}>
                  <div className="row" style={{ marginBottom: 6 }}>
                    <strong style={{ fontSize: 12 }}>Briefs</strong>
                    <span className="muted" style={{ fontSize: 11, marginLeft: 8 }}>{done}/{total} done</span>
                    <div className="spacer" style={{ flex: 1 }} />
                    {total > 0 && <Link to="/briefs" className="link" style={{ fontSize: 11 }}>view on board →</Link>}
                  </div>
                  {total === 0 ? (
                    <div className="muted" style={{ fontSize: 12 }}>No Briefs yet — once ready, orchestration creates them here.</div>
                  ) : (
                    <>
                      <div className="progress-bar"><div className="progress-fill" style={{ width: `${total ? Math.round((done / total) * 100) : 0}%` }} /></div>
                      <div className="pill-row" style={{ marginTop: 8 }}>
                        {COLS.filter((c) => (byCol[c] ?? 0) > 0).map((c) => (
                          <span key={c} className="row" style={{ gap: 5 }}><span className={"badge " + c} style={{ fontSize: 9 }}>{c}</span><strong style={{ fontSize: 12 }}>{byCol[c]}</strong></span>
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
