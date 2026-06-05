import { useState } from "react";
import { api, tryGetReport } from "../api";
import { useAuth } from "../auth";
import { Badge, useAsync } from "./common";

// The full Brief detail (`GET /v1/spine/briefs/:id`) — the canonical
// product object for one Brief: its fields, title, relation graph (each
// tenant-filtered server-side), the current Claim holder, and a Chronicle
// summary. The full paginated timeline stays on `…/events`/`…/thread`.
interface BriefFields {
  task_id?: string;
  human_ref?: string | null;
  assignee_agent_id?: string | null;
  board_status?: string;
  priority?: string;
  reviewer_agent_id?: string | null;
  mandate_id?: string | null;
  campaign_id?: string | null;
}
interface ClaimInfo {
  agent_id?: string;
  expires_at?: number;
}
interface ChronicleEntry {
  event_id?: number;
  ts?: number;
  event_type?: string;
  payload?: string;
}
interface BriefDetailData {
  title?: string;
  fields?: BriefFields;
  subbriefs?: string[];
  snags?: string[];
  blocking?: string[];
  parents?: string[];
  dossiers?: { doc_id?: string; kind?: string; title?: string }[];
  labels?: string[];
  pinned?: boolean;
  due_at?: number | null;
  blocked?: boolean;
  claim?: ClaimInfo | null;
  wakeup_count?: number;
  chronicle?: { total?: number; recent?: ChronicleEntry[] };
}

// A small color cue per Chronicle event family — no theme change, just dots.
function eventTone(type?: string): string {
  const t = type ?? "";
  if (/fail|blocked|dispatch_failed|snag|reject|budget_refused/.test(t)) return "#c0392b";
  if (/cancel/.test(t)) return "#b9770e";
  if (/done|shift_done|applied|accepted|run_reviewed/.test(t)) return "#1e7e34";
  if (/run_started|move|board_moved|created|comment/.test(t)) return "#2d6cdf";
  return "#999";
}

export function BriefDetail({
  briefId,
  onClose,
  onChanged,
}: {
  briefId: string;
  onClose: () => void;
  onChanged?: () => void;
}) {
  const { status } = useAuth();
  const [comment, setComment] = useState("");
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);
  const [busy, setBusy] = useState(false);

  const { data, loading, error, reload } = useAsync(
    () => tryGetReport<BriefDetailData>(`/v1/spine/briefs/${encodeURIComponent(briefId)}`, {}),
    [briefId],
  );

  const d = data?.data ?? {};
  const f = d.fields ?? {};
  const loadErr = error ?? data?.error ?? null;
  const events = Array.isArray(d.chronicle?.recent) ? d.chronicle!.recent! : [];
  const claim = d.claim ?? null;

  async function submitComment() {
    const text = comment.trim();
    if (!text) return;
    setBusy(true);
    setBanner(null);
    try {
      await api.post(`/v1/spine/briefs/${encodeURIComponent(briefId)}/comment`, {
        author: status?.username || "operator",
        text,
      });
      setComment("");
      setBanner({ kind: "ok", msg: "Comment added to the Chronicle." });
      reload();
      onChanged?.();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Comment failed" });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="card" style={{ borderColor: "var(--info, #2d6cdf)" }}>
      <div className="row" style={{ marginBottom: 8 }}>
        <h3 style={{ margin: 0 }}>{d.title ?? "Brief"}</h3>
        {f.human_ref && <span className="mono" style={{ fontSize: 11 }}>{f.human_ref}</span>}
        {f.board_status && <Badge status={f.board_status} />}
        {f.priority && <span className="badge">{f.priority}</span>}
        {d.pinned && <span className="badge todo" title="pinned">📌</span>}
        {d.blocked && <span className="badge blocked" title="blocked by an unresolved Snag">blocked</span>}
        <div className="spacer" style={{ flex: 1 }} />
        <button className="btn ghost sm" onClick={reload} disabled={loading}>Refresh</button>
        <button className="btn ghost sm" onClick={onClose}>Close ✕</button>
      </div>

      {loadErr && (
        <div className="banner err">Could not load this Brief: {loadErr}. <span className="link" onClick={reload}>Retry</span></div>
      )}
      {banner && <div className={"banner " + banner.kind}>{banner.msg}</div>}

      <div className="kv">
        <span className="muted">Brief id</span>
        <span className="mono" style={{ fontSize: 11 }}>{f.task_id ?? briefId}</span>
      </div>
      <div className="kv">
        <span className="muted">Assignee</span>
        <span>{f.assignee_agent_id ? <span className="mono" style={{ fontSize: 11 }}>{f.assignee_agent_id}</span> : <span className="muted">unassigned</span>}</span>
      </div>
      {f.reviewer_agent_id && (
        <div className="kv"><span className="muted">Reviewer</span><span className="mono" style={{ fontSize: 11 }}>{f.reviewer_agent_id}</span></div>
      )}
      {f.mandate_id && (
        <div className="kv"><span className="muted">Mandate</span><span className="mono" style={{ fontSize: 11 }}>{f.mandate_id}</span></div>
      )}
      {f.campaign_id && (
        <div className="kv"><span className="muted">Campaign</span><span className="mono" style={{ fontSize: 11 }}>{f.campaign_id}</span></div>
      )}
      <div className="kv">
        <span className="muted">Claim</span>
        <span>
          {claim && claim.agent_id
            ? <><span className="badge in_progress">held</span> <span className="mono" style={{ fontSize: 11 }}>{claim.agent_id}</span>{claim.expires_at ? <span className="muted" style={{ fontSize: 11, marginLeft: 6 }}>· expires {new Date(claim.expires_at * 1000).toLocaleTimeString()}</span> : null}</>
            : <span className="muted">not claimed</span>}
        </span>
      </div>
      {d.due_at != null && (
        <div className="kv"><span className="muted">Due</span><span>{new Date(d.due_at * 1000).toLocaleString()}</span></div>
      )}
      {(d.labels?.length ?? 0) > 0 && (
        <div className="kv">
          <span className="muted">Labels</span>
          <span>{d.labels!.map((l) => <span key={l} className="badge" style={{ marginRight: 4 }}>{l}</span>)}</span>
        </div>
      )}

      {/* Relation graph counts (each tenant-filtered server-side). */}
      <div className="kv">
        <span className="muted">Relations</span>
        <span className="muted" style={{ fontSize: 12 }}>
          {(d.subbriefs?.length ?? 0)} sub-brief(s) · {(d.parents?.length ?? 0)} parent(s) ·{" "}
          {(d.snags?.length ?? 0)} snag(s) · {(d.blocking?.length ?? 0)} blocking ·{" "}
          {(d.dossiers?.length ?? 0)} dossier(s) · {(d.wakeup_count ?? 0)} wakeup(s)
        </span>
      </div>

      {/* Chronicle — newest entries + total; full timeline on the events route. */}
      <div className="row" style={{ marginTop: 12, marginBottom: 6 }}>
        <strong style={{ fontSize: 12 }}>Chronicle</strong>
        <span className="muted" style={{ fontSize: 11, marginLeft: 8 }}>
          {d.chronicle?.total ?? 0} event(s) total · showing newest {events.length}
        </span>
      </div>
      {loading ? (
        <div className="loading">Loading…</div>
      ) : events.length === 0 ? (
        <div className="muted" style={{ fontSize: 12 }}>No Chronicle events yet for this Brief.</div>
      ) : (
        <div style={{ maxHeight: 240, overflow: "auto", fontSize: 12 }}>
          {events.map((ev, i) => (
            <div key={ev.event_id ?? i} style={{ padding: "3px 0", borderBottom: "1px solid rgba(0,0,0,0.05)" }}>
              <span style={{ display: "inline-block", width: 8, height: 8, borderRadius: 4, marginRight: 6, background: eventTone(ev.event_type) }} />
              <span className="muted" style={{ fontSize: 10 }}>{ev.ts ? new Date(ev.ts * 1000).toLocaleString() : ""}</span>{" "}
              <span className="mono" style={{ fontSize: 11 }}>{ev.event_type}</span>
              {ev.payload && <> — <span style={{ whiteSpace: "pre-wrap", wordBreak: "break-word" }}>{ev.payload}</span></>}
            </div>
          ))}
        </div>
      )}

      {/* Comment — appends to the Chronicle as a brief.comment event. */}
      <div className="row" style={{ marginTop: 12, gap: 8 }}>
        <input
          className="input"
          style={{ flex: 1 }}
          placeholder="Add a comment to the Chronicle…"
          value={comment}
          onChange={(e) => setComment(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && submitComment()}
        />
        <button className="btn" onClick={submitComment} disabled={busy || !comment.trim()}>
          {busy ? "…" : "Comment"}
        </button>
      </div>
    </div>
  );
}
