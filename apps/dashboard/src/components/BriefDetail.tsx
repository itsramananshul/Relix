import { useState } from "react";
import { api, tryGetReport } from "../api";
import { useAuth } from "../auth";
import { Badge, useAsync } from "./common";

// The Brief live work thread (`GET /v1/spine/briefs/:id/thread`): the
// Brief detail + its Chronicle timeline + the current Claim holder. This is
// the React parity for the legacy spine board's Brief Properties + thread
// panel — the "center of work" view for one Brief.
interface ThreadDetail {
  task_id?: string;
  id?: string;
  title?: string;
  board_status?: string;
  priority?: string;
  assignee_agent_id?: string | null;
  mandate_id?: string | null;
  labels?: string[];
  snags?: unknown[];
  sub_briefs?: unknown[];
  due_at?: number | null;
  pinned?: boolean;
}
interface ChronicleEvent {
  id?: number;
  ts?: number;
  type?: string;
  payload?: string;
}
interface ClaimHolder {
  agent_id?: string | null;
  holder?: string | null;
  expires_at?: number | null;
}
interface Thread {
  detail?: ThreadDetail | null;
  events?: ChronicleEvent[];
  claim?: ClaimHolder | null;
}

// A small color cue per Chronicle event family — no theme change, just dots.
function eventTone(type?: string): string {
  const t = type ?? "";
  if (/fail|blocked|dispatch_failed|snag|reject|budget_refused/.test(t)) return "#c0392b";
  if (/cancel/.test(t)) return "#b9770e";
  if (/done|shift_done|applied|accepted|run_reviewed/.test(t)) return "#1e7e34";
  if (/run_started|move|board_moved|created/.test(t)) return "#2d6cdf";
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
    () => tryGetReport<Thread>(`/v1/spine/briefs/${encodeURIComponent(briefId)}/thread`, {}),
    [briefId],
  );

  const thread = data?.data ?? {};
  const loadErr = error ?? data?.error ?? null;
  const detail = thread.detail ?? {};
  const events = Array.isArray(thread.events) ? thread.events : [];
  const claim = thread.claim ?? null;

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
        <h3 style={{ margin: 0 }}>{detail.title ?? "Brief"}</h3>
        {detail.board_status && <Badge status={detail.board_status} />}
        {detail.priority && <span className="badge">{detail.priority}</span>}
        {detail.pinned && <span className="badge todo" title="pinned">📌</span>}
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
        <span className="mono" style={{ fontSize: 11 }}>{detail.task_id ?? detail.id ?? briefId}</span>
      </div>
      <div className="kv">
        <span className="muted">Assignee</span>
        <span>{detail.assignee_agent_id ? <span className="mono" style={{ fontSize: 11 }}>{detail.assignee_agent_id}</span> : <span className="muted">unassigned</span>}</span>
      </div>
      {detail.mandate_id && (
        <div className="kv">
          <span className="muted">Mandate</span>
          <span className="mono" style={{ fontSize: 11 }}>{detail.mandate_id}</span>
        </div>
      )}
      <div className="kv">
        <span className="muted">Claim</span>
        <span>
          {claim && (claim.agent_id || claim.holder)
            ? <><span className="badge in_progress">held</span> <span className="mono" style={{ fontSize: 11 }}>{claim.agent_id ?? claim.holder}</span></>
            : <span className="muted">not claimed</span>}
        </span>
      </div>
      {Array.isArray(detail.labels) && detail.labels.length > 0 && (
        <div className="kv">
          <span className="muted">Labels</span>
          <span>{detail.labels.map((l) => <span key={l} className="badge" style={{ marginRight: 4 }}>{l}</span>)}</span>
        </div>
      )}
      {(detail.snags?.length ?? 0) > 0 && (
        <div className="kv">
          <span className="muted">Snags</span>
          <span><span className="badge blocked">{detail.snags!.length} unresolved</span></span>
        </div>
      )}

      {/* Chronicle — the Brief's event timeline, newest first. */}
      <div className="row" style={{ marginTop: 12, marginBottom: 6 }}>
        <strong style={{ fontSize: 12 }}>Chronicle</strong>
        <span className="muted" style={{ fontSize: 11, marginLeft: 8 }}>{events.length} event(s), newest first</span>
      </div>
      {loading ? (
        <div className="loading">Loading Chronicle…</div>
      ) : events.length === 0 ? (
        <div className="muted" style={{ fontSize: 12 }}>No Chronicle events yet for this Brief.</div>
      ) : (
        <div style={{ maxHeight: 300, overflow: "auto", fontSize: 12 }}>
          {events.map((ev, i) => (
            <div key={ev.id ?? i} style={{ padding: "3px 0", borderBottom: "1px solid rgba(0,0,0,0.05)" }}>
              <span style={{ display: "inline-block", width: 8, height: 8, borderRadius: 4, marginRight: 6, background: eventTone(ev.type) }} />
              <span className="muted" style={{ fontSize: 10 }}>{ev.ts ? new Date(ev.ts * 1000).toLocaleString() : ""}</span>{" "}
              <span className="mono" style={{ fontSize: 11 }}>{ev.type}</span>
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
