import { useCallback, useEffect, useMemo, useState } from "react";
import { Link } from "react-router-dom";
import {
  api,
  clearances,
  companyActions,
  subscribeClearances,
  type Clearance,
  type ClearanceStreamConn,
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
//
// Clearances are grouped by TYPE (Hire/Spawn, Strategy, Budget/Allowance,
// High-risk/Other) and each card shows a typed payload summary parsed from the
// fields the bridge now preserves (subject_id, capability_category, expires_at,
// task_id) plus the method/reason. The decide route's authority is unchanged:
// the runtime cap enforces the real authorisation and applies each side effect
// exactly once — this hub never creates approval power or auto-approves.

const SPAWN_CLEARANCE_METHOD = "agent.activate_hire";

type GroupKey = "hire" | "strategy" | "budget" | "other";

interface GroupMeta {
  key: GroupKey;
  label: string;
  tone: string; // badge tone class
  // High-risk groups require a short operator note (typed confirmation) before
  // a decision is allowed. Hire approvals stay fast (no note required).
  sensitive: boolean;
}

const GROUPS: Record<GroupKey, GroupMeta> = {
  hire: { key: "hire", label: "Hire / Spawn", tone: "in_progress", sensitive: false },
  strategy: { key: "strategy", label: "Strategy gates", tone: "in_review", sensitive: true },
  budget: { key: "budget", label: "Budget / Allowance overrides", tone: "blocked", sensitive: true },
  other: { key: "other", label: "High-risk / Other", tone: "blocked", sensitive: true },
};
const GROUP_ORDER: GroupKey[] = ["hire", "strategy", "budget", "other"];

// Classify a Clearance into its operator-facing type group from the method +
// the (optional) capability_category the bridge now preserves.
function groupOf(c: Clearance): GroupMeta {
  const m = (c.method ?? "").toLowerCase();
  const cat = (c.capability_category ?? "").toLowerCase();
  if (m === SPAWN_CLEARANCE_METHOD || m.includes("spawn") || m.includes("hire") || cat === "spawn") {
    return GROUPS.hire;
  }
  if (m.includes("strategy") || cat.includes("strategy")) return GROUPS.strategy;
  if (
    m.includes("budget") ||
    m.includes("allowance") ||
    m.includes("payment") ||
    m.includes("spend") ||
    cat.includes("budget") ||
    cat.includes("payment") ||
    cat.includes("allowance")
  ) {
    return GROUPS.budget;
  }
  return GROUPS.other;
}

// A compact, operational "decision impact" line per group — what each verb does.
function decisionImpact(g: GroupKey): string {
  switch (g) {
    case "hire":
      return "Approve activates the linked pending hire; reject leaves the role unfilled.";
    case "strategy":
      return "Approve clears the strategy gate so the team can be built; reject sends it back.";
    case "budget":
      return "Approve authorises the spend/Allowance override; reject holds the cap.";
    default:
      return "Approve admits the gated action exactly once; reject refuses it.";
  }
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

// Human expiry from a unix-seconds expires_at: "expires in 12m" / "expired".
// Empty / unset → "" (no window recorded).
function expiry(raw: string | number | undefined): { text: string; expired: boolean } {
  const n = typeof raw === "number" ? raw : Number(raw);
  if (!n || !isFinite(n)) return { text: "", expired: false };
  const secs = Math.floor(n - Date.now() / 1000);
  if (secs <= 0) return { text: "expired", expired: true };
  if (secs < 60) return { text: `expires in ${secs}s`, expired: false };
  if (secs < 3600) return { text: `expires in ${Math.floor(secs / 60)}m`, expired: false };
  if (secs < 86400) return { text: `expires in ${Math.floor(secs / 3600)}h`, expired: false };
  return { text: `expires in ${Math.floor(secs / 86400)}d`, expired: false };
}

const SEV_TONE: Record<string, string> = { high: "blocked", medium: "in_progress", low: "backlog" };

// Honest connection-state chip for the dedicated Clearance stream. `unavailable`
// is paired at the call site with a bounded polling fallback, so the hub keeps
// updating even when the live push can't connect — the title says so.
const CLR_CONN_CHIP: Record<ClearanceStreamConn, { label: string; tone: string; title: string }> = {
  connecting: { label: "connecting", tone: "backlog", title: "Opening the live Clearance stream…" },
  live: { label: "live", tone: "done", title: "Live — Clearances refresh the moment the pending queue changes." },
  reconnecting: { label: "reconnecting", tone: "in_progress", title: "Lost the live stream — reconnecting…" },
  unavailable: {
    label: "unavailable",
    tone: "blocked",
    title: "Live stream unavailable — falling back to periodic refresh so the hub still updates.",
  },
};

// A budget action's stable id encodes its KIND (action_center.rs): committed-
// Allowance planning vs money already spent vs a hard-stop. Surface that honestly
// so the operator knows whether it is a spend alert or a planning item.
function budgetKind(a: CompanyActionItem): string {
  const id = a.id ?? "";
  if (id.startsWith("budget:committed")) return "Committed-Allowance plan";
  if (id.includes("hardstop")) return "Allowance hard-stop";
  if (id.includes("spend")) return "Spend alert";
  return "Budget";
}

// Pull the "(NN%)" the action_center reason embeds, for a compact chip. This is
// derived from the same real reason text shown below — not a separate figure.
function pctChip(reason: string | undefined): string | null {
  const m = /\((\d+)%\)/.exec(reason ?? "");
  return m ? `${m[1]}%` : null;
}

export function Approvals() {
  const { data, loading, error, reload } = useAsync(async () => {
    const [clr, acts] = await Promise.all([clearances.list(50), companyActions.list()]);
    return { clr, acts };
  }, []);

  // Inline decision state: which row is mid-decision + the last result banner.
  const [acting, setActing] = useState<string | null>(null);
  const [note, setNote] = useState<{ kind: string; msg: string } | null>(null);
  // Per-Clearance operator notes (required for sensitive groups).
  const [notes, setNotes] = useState<Record<string, string>>({});
  // Filter/search state.
  const [query, setQuery] = useState("");
  const [typeFilter, setTypeFilter] = useState<"all" | GroupKey>("all");

  // ── Live Clearance stream (dashboard-design §11) ─────────────────────────
  // The dedicated polling-backed SSE feed pushes the full pending-Clearance
  // array on connect + on every change, so the queue refreshes without a manual
  // Refresh. `streamClr` is the live override (preferred over the fetched data
  // when present); `clrConn` is the honest connection state for the header chip.
  const [streamClr, setStreamClr] = useState<Clearance[] | null>(null);
  const [clrConn, setClrConn] = useState<ClearanceStreamConn>("connecting");

  const refresh = useCallback(() => {
    reload();
  }, [reload]);

  // Subscribe on mount; tear the EventSource down on unmount.
  useEffect(() => {
    const unsub = subscribeClearances(
      (arr) => setStreamClr(arr),
      (state) => setClrConn(state),
    );
    return () => {
      unsub();
      setStreamClr(null);
    };
  }, []);

  // Drop the live override whenever a fresh fetch lands (initial load or a
  // reload after a decision), so the freshly-fetched queue shows immediately
  // instead of being masked by a now-stale stream frame. The stream
  // re-establishes the override on its next push (identical content ⇒ no flicker).
  useEffect(() => {
    setStreamClr(null);
  }, [data]);

  // Fallback: when the live stream can't connect, poll the hub on a bounded
  // interval so the Clearance queue still updates without the push.
  useEffect(() => {
    if (clrConn !== "unavailable") return;
    const t = setInterval(() => reload(), 7000);
    return () => clearInterval(t);
  }, [clrConn, reload]);

  const setNoteFor = (id: string, v: string) =>
    setNotes((prev) => ({ ...prev, [id]: v }));

  // ── Clearance decisions (real: /v1/spine/clearances/:id/decide) ──────────
  async function decideClearance(c: Clearance, g: GroupMeta, decision: "approve" | "reject") {
    const noteText = (notes[c.approval_id] ?? "").trim();
    if (g.sensitive && !noteText) {
      setNote({ kind: "err", msg: "A short note is required to decide this high-risk Clearance." });
      return;
    }
    setActing(c.approval_id);
    setNote(null);
    try {
      await clearances.decide(c.approval_id, decision, noteText);
      setNote({
        kind: "ok",
        msg: `Clearance ${decision === "approve" ? "approved" : "rejected"} — ${g.label} for ${c.subject_id || c.agent_id || "—"}.`,
      });
      setNoteFor(c.approval_id, "");
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
  // Prefer the live stream snapshot when present; otherwise the fetched data.
  const clrList = streamClr ?? clrReport?.data ?? [];
  // A live stream supersedes a stale fetch error (the queue is being pushed).
  const clrError = streamClr ? null : (clrReport?.error ?? null);
  const feed = data?.acts?.data ?? null;
  const feedError = data?.acts?.error ?? null;
  const allActions = feed?.actions ?? [];
  // Direct hires (no Clearance) — distinct from the spawn-Clearance hires above.
  const hires = allActions.filter((a) => a.category === "hire" && !!a.target_id);
  // Budget alerts — informational; no inline decide route exists.
  const budget = allActions.filter((a) => a.category === "budget");

  // Group + filter the Clearances.
  const grouped = useMemo(() => {
    const q = query.trim().toLowerCase();
    const out: Record<GroupKey, Clearance[]> = { hire: [], strategy: [], budget: [], other: [] };
    for (const c of clrList) {
      const g = groupOf(c);
      if (typeFilter !== "all" && g.key !== typeFilter) continue;
      if (q) {
        const hay = `${c.approval_id} ${c.agent_id} ${c.subject_id ?? ""} ${c.method} ${c.reason} ${c.capability_category ?? ""}`.toLowerCase();
        if (!hay.includes(q)) continue;
      }
      out[g.key].push(c);
    }
    return out;
  }, [clrList, query, typeFilter]);

  const filteredClrCount = GROUP_ORDER.reduce((n, k) => n + grouped[k].length, 0);
  const pendingCount = clrList.length + hires.length;
  const empty = !loading && pendingCount === 0 && budget.length === 0;

  function renderClearanceRow(c: Clearance, g: GroupMeta) {
    const isActing = acting === c.approval_id;
    const exp = expiry(c.expires_at);
    const briefRoute = c.task_id ? `/briefs?brief=${encodeURIComponent(c.task_id)}` : null;
    const noteText = notes[c.approval_id] ?? "";
    const blocked = g.sensitive && !noteText.trim();
    const isSpawnHire = (c.method ?? "").toLowerCase() === SPAWN_CLEARANCE_METHOD;
    return (
      <tr key={c.approval_id}>
        <td style={{ minWidth: 120 }}>
          <span className={"badge " + g.tone} style={{ fontSize: 9 }}>{g.label}</span>
          <div className="mono" style={{ fontSize: 10, marginTop: 2 }}>{c.method}</div>
          {c.capability_category && (
            <div className="muted" style={{ fontSize: 10 }}>category · {c.capability_category}</div>
          )}
        </td>
        <td>
          <div className="muted" style={{ fontSize: 10 }}>requested by</div>
          <span className="mono" style={{ fontSize: 11 }}>{c.agent_id || "—"}</span>
          {c.subject_id && c.subject_id !== c.agent_id && (
            <>
              <div className="muted" style={{ fontSize: 10, marginTop: 2 }}>affects</div>
              <span className="mono" style={{ fontSize: 11 }}>{c.subject_id}</span>
            </>
          )}
          <div className="muted" style={{ fontSize: 10, marginTop: 2 }}>{c.approval_id.slice(0, 14)}</div>
        </td>
        <td style={{ maxWidth: 360 }}>
          <div className="muted" style={{ fontSize: 12 }}>{c.reason || "—"}</div>
          {isSpawnHire && (
            <div className="muted" style={{ fontSize: 10, marginTop: 3 }}>
              Approving activates the linked hire. To bind a specific Rig, use the{" "}
              <Link to="/agents">Operative</Link> page (the Clearance decide cannot set a Rig).
            </div>
          )}
          {briefRoute && (
            <div style={{ fontSize: 10, marginTop: 3 }}>
              <Link to={briefRoute}>Open Brief {c.task_id} →</Link>
            </div>
          )}
        </td>
        <td className="muted" style={{ fontSize: 11, whiteSpace: "nowrap" }}>
          <div>{ago(c.requested_at)}</div>
          {exp.text && (
            <div className={exp.expired ? "err-text" : "muted"} style={{ fontSize: 10 }}>{exp.text}</div>
          )}
        </td>
        <td style={{ textAlign: "right", minWidth: 160 }}>
          {g.sensitive && (
            <input
              className="input"
              style={{ fontSize: 11, marginBottom: 6, width: "100%", maxWidth: 200 }}
              placeholder="note (required)"
              value={noteText}
              disabled={isActing}
              onChange={(e) => setNoteFor(c.approval_id, e.target.value)}
              aria-label={`Decision note for ${c.method}`}
            />
          )}
          <span className="btn-group" style={{ justifyContent: "flex-end" }}>
            <button
              className="btn sm"
              disabled={isActing || blocked}
              title={blocked ? "A note is required to decide this high-risk Clearance" : "Approve this Clearance"}
              onClick={() => decideClearance(c, g, "approve")}
            >
              {isActing ? "…" : "Approve"}
            </button>
            <button
              className="btn ghost sm"
              disabled={isActing || blocked}
              onClick={() => decideClearance(c, g, "reject")}
            >
              Reject
            </button>
          </span>
        </td>
      </tr>
    );
  }

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
          {/* Honest Clearance-stream state (dashboard-design §11): the pending
              queue refreshes live; `unavailable` falls back to periodic refresh. */}
          <span
            className={"badge " + CLR_CONN_CHIP[clrConn].tone}
            style={{ fontSize: 9, marginRight: 8 }}
            title={CLR_CONN_CHIP[clrConn].title}
          >
            {CLR_CONN_CHIP[clrConn].label}
          </span>
          <span className="muted" style={{ fontSize: 12, marginRight: 8 }}>computed from live state</span>
          <button className="btn ghost sm" onClick={refresh} disabled={loading}>
            {loading ? "…" : "Refresh"}
          </button>
        </div>
        <p className="muted" style={{ marginTop: -2, marginBottom: note ? 10 : 0, fontSize: 12 }}>
          Pending governance gates — hire Clearances, strategy gates, budget/Allowance overrides, and
          high-risk approvals. Decisions are forwarded under the bridge's verified identity; the
          runtime cap enforces the real authorisation and applies each side effect exactly once. No
          authority is created here and nothing is auto-approved.
        </p>
        {note && <div className={"banner " + note.kind} style={{ fontSize: 12 }}>{note.msg}</div>}
        {error && (
          <div className="banner err" style={{ fontSize: 12 }}>
            Approvals data failed to load: {error}
          </div>
        )}
      </div>

      {/* Filter / search bar. */}
      {clrList.length > 0 && (
        <div className="card">
          <div className="filter-bar">
            <input
              className="input"
              style={{ fontSize: 12, minWidth: 200, flex: "1 1 220px" }}
              placeholder="Search Clearances (actor, subject, method, reason)…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              aria-label="Search Clearances"
            />
            <span className="btn-group">
              {(["all", ...GROUP_ORDER] as const).map((k) => (
                <button
                  key={k}
                  className={"btn sm" + (typeFilter === k ? "" : " ghost")}
                  onClick={() => setTypeFilter(k)}
                >
                  {k === "all" ? "All" : GROUPS[k as GroupKey].label.split(" ")[0]}
                </button>
              ))}
            </span>
            <div className="spacer" style={{ flex: 1 }} />
            <span className="muted" style={{ fontSize: 11 }}>
              {filteredClrCount} of {clrList.length} Clearance{clrList.length === 1 ? "" : "s"}
            </span>
          </div>
        </div>
      )}

      {/* Clearance availability / loading / empty (single honest state). A live
          stream snapshot satisfies the load even before the initial fetch lands. */}
      {loading && !streamClr ? (
        <div className="card"><div className="loading">Loading Clearances…</div></div>
      ) : clrError ? (
        <div className="card">
          <div className="banner err" style={{ fontSize: 12, marginBottom: 0 }}>
            Clearances unavailable — <span className="mono">GET /v1/spine/clearances</span>: {clrError}
          </div>
        </div>
      ) : clrList.length === 0 ? (
        <div className="card"><div className="empty">No pending Clearances.</div></div>
      ) : filteredClrCount === 0 ? (
        <div className="card"><div className="empty">No Clearances match the current filter.</div></div>
      ) : (
        // Typed Clearance sections — one card per non-empty group.
        GROUP_ORDER.filter((k) => grouped[k].length > 0).map((k) => {
          const g = GROUPS[k];
          return (
            <div className="card" key={k}>
              <div className="row" style={{ marginBottom: 6, alignItems: "center" }}>
                <h3 style={{ margin: 0 }}>{g.label}</h3>
                <span className="muted" style={{ fontSize: 12, marginLeft: 8 }}>{grouped[k].length}</span>
              </div>
              <p className="muted" style={{ marginTop: -2, marginBottom: 8, fontSize: 11 }}>
                {decisionImpact(g.key)}
                {g.sensitive && " A short note is required to decide."}
              </p>
              <div className="table-scroll">
                <table className="table compact">
                  <thead>
                    <tr>
                      <th>Type</th>
                      <th>Actor / affected</th>
                      <th>Reason / target</th>
                      <th>Age</th>
                      <th style={{ textAlign: "right" }}>Decide</th>
                    </tr>
                  </thead>
                  <tbody>{grouped[k].map((c) => renderClearanceRow(c, g))}</tbody>
                </table>
              </div>
            </div>
          );
        })
      )}

      {/* Direct pending hires (no Clearance) — approve with the safe-local Rig. */}
      <div className="card">
        <div className="row" style={{ marginBottom: 6, alignItems: "center" }}>
          <h3 style={{ margin: 0 }}>Pending hires</h3>
          {hires.length > 0 && <span className="muted" style={{ fontSize: 12, marginLeft: 8 }}>{hires.length} pending</span>}
        </div>
        <p className="muted" style={{ marginTop: -2, marginBottom: 8, fontSize: 11 }}>
          A pending Operative with no Clearance to decide — approving binds the suggested Rig at hire
          time so it is immediately runnable; rejecting leaves the role unfilled.
        </p>
        {loading ? (
          <div className="loading">Loading hires…</div>
        ) : feedError ? (
          <div className="banner err" style={{ fontSize: 12, marginBottom: 0 }}>
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
                  const rig = a.suggested_rig ?? "echo";
                  return (
                    <tr key={a.id ?? i}>
                      <td style={{ width: 56 }}>
                        <span className="badge in_progress" style={{ fontSize: 9 }}>hire</span>
                      </td>
                      <td>
                        <div style={{ fontSize: 13, fontWeight: 600 }}>{a.title ?? a.target_title ?? "(hire)"}</div>
                        {a.reason && <div className="muted" style={{ fontSize: 11 }}>{a.reason}</div>}
                        <div className="muted" style={{ fontSize: 10, marginTop: 2 }}>
                          {a.target_id && <span className="mono">{a.target_id}</span>}
                          {" · "}
                          {a.suggested_rig ? `runnable on ${rig}` : "needs a Rig after hire"}
                          {a.route && (
                            <>
                              {" · "}
                              <Link to={a.route}>{a.action_label ?? "Open"} →</Link>
                            </>
                          )}
                        </div>
                      </td>
                      <td style={{ textAlign: "right" }}>
                        <span className="btn-group" style={{ justifyContent: "flex-end" }}>
                          <button
                            className="btn sm"
                            disabled={isActing}
                            title={`Approve this hire on the safe-local ${rig} adapter so it is immediately runnable`}
                            onClick={() => approveHire(a)}
                          >
                            {isActing ? "…" : `Approve · ${rig}`}
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

      {/* Budget alerts — informational; the spend/Allowance decision lives on
          the Costs/Agents surfaces (there is no inline budget-decision route). */}
      {budget.length > 0 && (
        <div className="card">
          <div className="row" style={{ marginBottom: 6, alignItems: "center" }}>
            <h3 style={{ margin: 0 }}>Budget alerts</h3>
            <span className="muted" style={{ fontSize: 12, marginLeft: 8 }}>{budget.length}</span>
          </div>
          <p className="muted" style={{ marginTop: -2, marginBottom: 8, fontSize: 11 }}>
            Informational — these are not decided here. A spend alert is money already used; a
            committed-Allowance plan is capacity reserved. Act on the Costs / Operative surfaces.
          </p>
          <div className="table-scroll">
            <table className="table compact">
              <tbody>
                {budget.map((a, i) => {
                  const pct = pctChip(a.reason);
                  return (
                    <tr key={a.id ?? i}>
                      <td style={{ width: 130 }}>
                        <span className={"badge " + (SEV_TONE[a.severity ?? ""] ?? "todo")} style={{ fontSize: 9 }}>
                          {a.severity ?? "budget"}
                        </span>
                        <div className="muted" style={{ fontSize: 10, marginTop: 2 }}>{budgetKind(a)}</div>
                        {pct && <div className="mono" style={{ fontSize: 10 }}>{pct}</div>}
                      </td>
                      <td>
                        <div style={{ fontSize: 13, fontWeight: 600 }}>{a.title ?? "(budget alert)"}</div>
                        {a.reason && <div className="muted" style={{ fontSize: 11 }}>{a.reason}</div>}
                      </td>
                      <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                        <span className="btn-group" style={{ justifyContent: "flex-end" }}>
                          <Link to="/costs" className="btn sm ghost">Costs →</Link>
                          <Link to={a.route ?? "/agents"} className="btn sm ghost">
                            {a.action_label ?? "Review"} →
                          </Link>
                        </span>
                      </td>
                    </tr>
                  );
                })}
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
