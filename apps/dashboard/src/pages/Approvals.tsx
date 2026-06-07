import { useCallback, useState } from "react";
import { Link } from "react-router-dom";
import {
  api,
  clearances,
  companyActions,
  type Clearance,
  type CompanyActionItem,
} from "../api";
import { useAsync } from "../components/common";
import { invalidate } from "../invalidate";

// The Approvals hub (dashboard-design §10): the one place the operator decides
// the company's pending governance gates. Everything here is REAL — pending
// Clearances from `/v1/spine/clearances` (decided inline via the spine decide
// route) and the direct-hire / budget items from the `company.actions` feed.
// No mock approvals; an unavailable backend shows an honest state with the
// route + reason, never a fabricated row.

const SPAWN_CLEARANCE_METHOD = "agent.activate_hire";

// Humanize a Clearance `method` into an operator-facing type label. The raw
// method is still shown (mono) so the underlying gate stays legible.
function clearanceType(method: string): string {
  const m = (method ?? "").toLowerCase();
  if (m === SPAWN_CLEARANCE_METHOD) return "Hire";
  if (m.includes("strategy")) return "Strategy gate";
  if (m.includes("budget") || m.includes("allowance")) return "Budget override";
  if (m.includes("spawn") || m.includes("hire")) return "Hire";
  return "Clearance";
}

// Best-effort relative age from a unix-seconds value (string or number).
function ago(raw: string | number | undefined): string {
  const n = typeof raw === "number" ? raw : Number(raw);
  if (!n || !isFinite(n)) return "—";
  const secs = Math.max(0, Math.floor(Date.now() / 1000 - n));
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86400)}d ago`;
}

const SEV_TONE: Record<string, string> = { high: "blocked", medium: "in_progress", low: "backlog" };

export function Approvals() {
  const { data, loading, error, reload } = useAsync(async () => {
    const [clr, acts] = await Promise.all([clearances.list(50), companyActions.list()]);
    return { clr, acts };
  }, []);

  // Inline decision state: which row is mid-decision + the last result banner.
  const [acting, setActing] = useState<string | null>(null);
  const [note, setNote] = useState<{ kind: string; msg: string } | null>(null);

  const refresh = useCallback(() => {
    reload();
  }, [reload]);

  // ── Clearance decisions (real: /v1/spine/clearances/:id/decide) ──────────
  async function decideClearance(c: Clearance, decision: "approve" | "reject") {
    setActing(c.approval_id);
    setNote(null);
    try {
      await clearances.decide(c.approval_id, decision);
      setNote({
        kind: "ok",
        msg: `Clearance ${decision === "approve" ? "approved" : "rejected"} — ${clearanceType(c.method)} for ${c.agent_id || "—"}.`,
      });
      // A decided Clearance changes the roster + Mandate readiness + the
      // Action Center (dashboard-design §11).
      invalidate(["actions", "mandates", "briefs"]);
      refresh();
    } catch (e) {
      const msg = e instanceof Error ? e.message : "Decision failed";
      setNote({ kind: "err", msg });
    } finally {
      setActing(null);
    }
  }

  // ── Direct hire decisions (real: /v1/agents/:id/approve-hire | reject-hire)
  // Reuses the Action Center's exact wiring so a pending hire is approved with
  // the safe-local Rig (immediately runnable) without leaving the hub.
  async function approveHire(a: CompanyActionItem) {
    if (!a.target_id) return;
    setActing(a.target_id);
    setNote(null);
    try {
      const r = await api.post<{ runnable?: boolean; rig?: string; needs_rig?: boolean }>(
        `/v1/agents/${encodeURIComponent(a.target_id)}/approve-hire`,
        a.suggested_rig ? { rig: a.suggested_rig } : {},
      );
      setNote({
        kind: "ok",
        msg: r.needs_rig
          ? `${a.target_title ?? "Operative"} hired — set an adapter to make it runnable.`
          : `${a.target_title ?? "Operative"} hired and runnable on ${r.rig ?? a.suggested_rig ?? "echo"}.`,
      });
      invalidate(["actions", "mandates", "briefs"]);
      refresh();
    } catch (e) {
      const msg = e instanceof Error ? e.message : "Approve hire failed";
      setNote({ kind: "err", msg: /clearance/i.test(msg) ? `${msg} — decide its Clearance above.` : msg });
    } finally {
      setActing(null);
    }
  }

  async function rejectHire(a: CompanyActionItem) {
    if (!a.target_id) return;
    setActing(a.target_id);
    setNote(null);
    try {
      await api.post(`/v1/agents/${encodeURIComponent(a.target_id)}/reject-hire`, {});
      setNote({ kind: "ok", msg: `${a.target_title ?? "Hire"} declined — the role is left unfilled.` });
      invalidate(["actions", "mandates", "briefs"]);
      refresh();
    } catch (e) {
      setNote({ kind: "err", msg: e instanceof Error ? e.message : "Reject hire failed" });
    } finally {
      setActing(null);
    }
  }

  const clrReport = data?.clr;
  const clrList = clrReport?.data ?? [];
  const clrError = clrReport?.error ?? null;
  const feed = data?.acts?.data ?? null;
  const feedError = data?.acts?.error ?? null;
  const allActions = feed?.actions ?? [];
  // Direct hires (no Clearance) — distinct from the spawn-Clearance hires above.
  const hires = allActions.filter((a) => a.category === "hire" && !!a.target_id);
  // Budget alerts — informational; no inline decide route exists.
  const budget = allActions.filter((a) => a.category === "budget");

  const pendingCount = clrList.length + hires.length;
  const empty = !loading && pendingCount === 0 && budget.length === 0;

  return (
    <div className="grid">
      {/* Header — what needs a decision, computed from live state. */}
      <div className="card">
        <div className="row" style={{ marginBottom: 6, alignItems: "center" }}>
          <h3 style={{ margin: 0 }}>Operator decisions</h3>
          {pendingCount > 0 && (
            <span className="badge blocked" style={{ fontSize: 9, marginLeft: 8 }}>
              {pendingCount} pending
            </span>
          )}
          <div className="spacer" style={{ flex: 1 }} />
          <span className="muted" style={{ fontSize: 12, marginRight: 8 }}>computed from live state</span>
          <button className="btn ghost sm" onClick={refresh} disabled={loading}>
            {loading ? "…" : "Refresh"}
          </button>
        </div>
        <p className="muted" style={{ marginTop: -2, marginBottom: note ? 10 : 0, fontSize: 12 }}>
          Pending governance gates — hire Clearances, strategy gates, budget overrides, and high-risk
          approvals. Decisions are forwarded under the bridge's verified identity; the runtime cap
          enforces the real authorisation and applies each side effect exactly once.
        </p>
        {note && <div className={"banner " + note.kind} style={{ fontSize: 12 }}>{note.msg}</div>}
        {error && (
          <div className="banner err" style={{ fontSize: 12 }}>
            Approvals data failed to load: {error}
          </div>
        )}
      </div>

      {/* Pending Clearances — the unified coord.approval queue (real decide). */}
      <div className="card">
        <div className="row" style={{ marginBottom: 8, alignItems: "center" }}>
          <h3 style={{ margin: 0 }}>Clearances</h3>
          {clrList.length > 0 && <span className="muted" style={{ fontSize: 12, marginLeft: 8 }}>{clrList.length} pending</span>}
        </div>
        {loading ? (
          <div className="loading">Loading Clearances…</div>
        ) : clrError ? (
          <div className="banner err" style={{ fontSize: 12 }}>
            Clearances unavailable — <span className="mono">GET /v1/spine/clearances</span>: {clrError}
          </div>
        ) : clrList.length === 0 ? (
          <div className="empty">No pending Clearances.</div>
        ) : (
          <div className="table-scroll">
            <table className="table compact">
              <thead>
                <tr>
                  <th>Type</th>
                  <th>Actor / target</th>
                  <th>Reason</th>
                  <th>Age</th>
                  <th style={{ textAlign: "right" }}>Decide</th>
                </tr>
              </thead>
              <tbody>
                {clrList.map((c) => {
                  const isActing = acting === c.approval_id;
                  return (
                    <tr key={c.approval_id}>
                      <td>
                        <span className="badge in_progress" style={{ fontSize: 9 }}>{clearanceType(c.method)}</span>
                        <div className="mono" style={{ fontSize: 10, marginTop: 2 }}>{c.method}</div>
                      </td>
                      <td>
                        <span className="mono" style={{ fontSize: 11 }}>{c.agent_id || "—"}</span>
                        <div className="muted" style={{ fontSize: 10 }}>{c.approval_id.slice(0, 14)}</div>
                      </td>
                      <td className="muted" style={{ fontSize: 12, maxWidth: 360 }}>{c.reason || "—"}</td>
                      <td className="muted" style={{ fontSize: 11 }}>{ago(c.requested_at)}</td>
                      <td style={{ textAlign: "right" }}>
                        <span className="btn-group" style={{ justifyContent: "flex-end" }}>
                          <button className="btn sm" disabled={isActing} onClick={() => decideClearance(c, "approve")}>
                            {isActing ? "…" : "Approve"}
                          </button>
                          <button className="btn ghost sm" disabled={isActing} onClick={() => decideClearance(c, "reject")}>
                            Reject
                          </button>
                        </span>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* Direct pending hires (no Clearance) — approve with the safe-local Rig. */}
      <div className="card">
        <div className="row" style={{ marginBottom: 8, alignItems: "center" }}>
          <h3 style={{ margin: 0 }}>Pending hires</h3>
          {hires.length > 0 && <span className="muted" style={{ fontSize: 12, marginLeft: 8 }}>{hires.length} pending</span>}
        </div>
        {loading ? (
          <div className="loading">Loading hires…</div>
        ) : feedError ? (
          <div className="banner err" style={{ fontSize: 12 }}>
            Hire feed unavailable — <span className="mono">GET /v1/spine/company/actions</span>: {feedError}
          </div>
        ) : hires.length === 0 ? (
          <div className="empty">No pending hires awaiting approval.</div>
        ) : (
          <div className="table-scroll">
            <table className="table compact">
              <tbody>
                {hires.map((a, i) => {
                  const isActing = acting === a.target_id;
                  return (
                    <tr key={a.id ?? i}>
                      <td style={{ width: 56 }}>
                        <span className="badge in_progress" style={{ fontSize: 9 }}>hire</span>
                      </td>
                      <td>
                        <div style={{ fontSize: 13, fontWeight: 600 }}>{a.title ?? a.target_title ?? "(hire)"}</div>
                        {a.reason && <div className="muted" style={{ fontSize: 11 }}>{a.reason}</div>}
                      </td>
                      <td style={{ textAlign: "right" }}>
                        <span className="btn-group" style={{ justifyContent: "flex-end" }}>
                          <button
                            className="btn sm"
                            disabled={isActing}
                            title={`Approve this hire on the safe-local ${a.suggested_rig ?? "echo"} adapter so it is immediately runnable`}
                            onClick={() => approveHire(a)}
                          >
                            {isActing ? "…" : `Approve · ${a.suggested_rig ?? "echo"}`}
                          </button>
                          <button className="btn ghost sm" disabled={isActing} onClick={() => rejectHire(a)}>
                            Reject
                          </button>
                        </span>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* Budget alerts — informational; the decision lives on its own route. */}
      {budget.length > 0 && (
        <div className="card">
          <h3 style={{ margin: 0, marginBottom: 8 }}>Budget alerts</h3>
          <div className="table-scroll">
            <table className="table compact">
              <tbody>
                {budget.map((a, i) => (
                  <tr key={a.id ?? i}>
                    <td style={{ width: 64 }}>
                      <span className={"badge " + (SEV_TONE[a.severity ?? ""] ?? "todo")} style={{ fontSize: 9 }}>budget</span>
                    </td>
                    <td>
                      <div style={{ fontSize: 13, fontWeight: 600 }}>{a.title ?? "(budget alert)"}</div>
                      {a.reason && <div className="muted" style={{ fontSize: 11 }}>{a.reason}</div>}
                    </td>
                    <td style={{ textAlign: "right" }}>
                      {a.route ? (
                        <Link to={a.route} className="btn sm ghost">{a.action_label ?? "Review"} →</Link>
                      ) : (
                        <span className="muted" style={{ fontSize: 11 }}>{a.action_label}</span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {/* Calm, real empty state. */}
      {empty && !clrError && !feedError && (
        <div className="card">
          <div className="empty">Nothing awaits your decision — no pending Clearances or hires.</div>
        </div>
      )}
    </div>
  );
}
