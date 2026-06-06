import { useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import {
  api,
  tryGet,
  tryGetReport,
  subscribeRunEvents,
  runControls,
  briefInteractions,
  briefSuggestions,
  type RunDiff,
  type BriefInteraction,
} from "../api";
import { useAuth } from "../auth";
import { Badge, useAsync } from "./common";

// The structured result of starting a Shift (`POST …/briefs/:id/run`). Mirrors
// the board's run handling so a refusal reads the same everywhere.
interface RunReport {
  status: string; // running / done / continued / failed / a refusal token
  rig?: string;
  summary?: string;
  install_hint?: string | null;
}

// Refusal token → a plain-English reason (shared phrasing with the board).
const REFUSALS: Record<string, string> = {
  running: "Shift started — executing in the background",
  unassigned: "assign an Operative first",
  no_adapter: "no adapter configured for this Operative",
  adapter_unavailable: "adapter not installed",
  already_running: "already running",
  not_found: "brief not found",
  workspace_error: "could not prepare a run workspace",
  done: "Shift complete",
  failed: "Shift failed",
  continued: "Shift continued (more work to do)",
};

// Apply-status → badge tone (mirrors the Runs page).
const APPLY_STATUS_TONE: Record<string, string> = {
  applied: "done",
  ready: "todo",
  conflicted: "blocked",
  failed: "blocked",
  blocked: "blocked",
  discarded: "blocked",
  not_applicable: "todo",
};

// Bounded summary of the Brief's most recent Shift (run), from
// `GET /v1/spine/briefs/:id`'s `latest_run`. Full run on /v1/runs/:id.
interface LatestRun {
  run_id?: string;
  rig?: string;
  status?: string; // running / done / failed / continued / cancelled / interrupted / refused
  trigger?: string;
  started_at?: number;
  finished_at?: number;
  duration_secs?: number;
  summary?: string;
  review?: string;
  apply_status?: string;
  refusal_reason?: string;
  artifact_count?: number;
  total_runs?: number;
}

// Run status → badge tone (mirrors the Runs page).
const RUN_TONE: Record<string, string> = {
  running: "in_progress",
  done: "done",
  failed: "blocked",
  cancelled: "blocked",
  refused: "blocked",
  interrupted: "blocked",
  continued: "todo",
};

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
  // The dedicated `/events` route is a passthrough of the canonical
  // `task.events` shape, which names these fields `id` / `type` (the
  // detail's `chronicle.recent` uses `event_id` / `event_type`). Accept
  // both so the timeline renders an event-type label + tone dot from
  // either source; `normalizeEvent` collapses them to the canonical pair.
  id?: number;
  type?: string;
}

// Collapse either Chronicle field shape (`/events` → `id`/`type`;
// `chronicle.recent` → `event_id`/`event_type`) to the canonical pair the
// timeline renders.
function normalizeEvent(e: ChronicleEntry): ChronicleEntry {
  return {
    event_id: e.event_id ?? e.id,
    ts: e.ts,
    event_type: e.event_type ?? e.type,
    payload: e.payload,
  };
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
  latest_run?: LatestRun | null;
}

// A comment, extracted from a `brief.comment` Chronicle event. The runtime
// records the payload as `"{author}: {text}"` (see coordinator
// `comment_on_brief`), so the author is the prefix up to the first `": "`;
// anything without that separator renders as a bodied note with no author.
interface BriefComment {
  event_id?: number;
  ts?: number;
  author: string;
  body: string;
}
function parseComment(e: ChronicleEntry): BriefComment {
  const text = e.payload ?? "";
  const idx = text.indexOf(": ");
  return idx > 0
    ? { event_id: e.event_id, ts: e.ts, author: text.slice(0, idx), body: text.slice(idx + 2) }
    : { event_id: e.event_id, ts: e.ts, author: "", body: text };
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
  // Shift-control state: a busy flag while a run is starting, and the loaded
  // safe-apply plan for the latest accepted run.
  const [runBusy, setRunBusy] = useState(false);
  const [diff, setDiff] = useState<RunDiff | null>(null);
  // Thread-interaction state: which card is being answered, and the free-text
  // draft for open `ask` cards that have no fixed choices.
  const [ixBusy, setIxBusy] = useState<string | null>(null);
  const [ixDraft, setIxDraft] = useState<Record<string, string>>({});

  // Load the Brief detail AND the fuller Chronicle timeline together. The
  // detail carries only a bounded `chronicle.recent`; the dedicated `/events`
  // route gives the readable, scrollable history (newest first). Both refresh
  // on the live run-event stream below.
  const EVENT_LIMIT = 120;
  const { data, loading, error, reload } = useAsync(async () => {
    const [detail, events, interactions] = await Promise.all([
      tryGetReport<BriefDetailData>(`/v1/spine/briefs/${encodeURIComponent(briefId)}`, {}),
      tryGet<ChronicleEntry[]>(
        `/v1/spine/briefs/${encodeURIComponent(briefId)}/events?limit=${EVENT_LIMIT}`,
        [],
      ),
      briefInteractions.list(briefId),
    ]);
    return {
      detail,
      events: (Array.isArray(events) ? events : []).map(normalizeEvent),
      interactions: Array.isArray(interactions) ? interactions : [],
    };
  }, [briefId]);

  // Live updates: refresh this Brief's detail (latest_run + Chronicle) when an
  // execution event for THIS Brief arrives on the run-event stream — so the
  // panel reflects a Shift starting / finishing / being refused without a
  // manual refresh. Refs keep the single subscription stable across renders.
  const reloadRef = useRef(reload);
  reloadRef.current = reload;
  const briefIdRef = useRef(briefId);
  briefIdRef.current = briefId;
  useEffect(() => {
    let pending: ReturnType<typeof setTimeout> | null = null;
    const unsub = subscribeRunEvents(
      (ev) => {
        // Only react to events for this Brief (or unlabeled frames).
        if (ev.taskId && ev.taskId !== briefIdRef.current) return;
        if (pending) clearTimeout(pending);
        pending = setTimeout(() => reloadRef.current(), 400);
      },
      () => {},
    );
    return () => {
      if (pending) clearTimeout(pending);
      unsub();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const d = data?.detail.data ?? {};
  const f = d.fields ?? {};
  const loadErr = error ?? data?.detail.error ?? null;
  // Prefer the fuller `/events` timeline; fall back to the detail's bounded
  // `chronicle.recent` if that optional fetch came back empty.
  const events =
    (data?.events.length ?? 0) > 0
      ? data!.events
      : Array.isArray(d.chronicle?.recent)
        ? d.chronicle!.recent!
        : [];
  const claim = d.claim ?? null;
  const lr = d.latest_run ?? null;
  // The Conversation: the Brief's comment thread (human + companion +
  // Operative), extracted from the same Chronicle events the ledger renders.
  // Events arrive newest-first; a conversation reads oldest→newest (newest by
  // the composer). Per the design docs this comment thread *is* the channel —
  // the full Chronicle ledger stays below as execution history.
  const comments = events
    .filter((e) => (e.event_type ?? "") === "brief.comment")
    .map(parseComment)
    .reverse();

  // Thread interactions (§1.9): the open ask/confirm cards needing an answer
  // sit above the Conversation; resolved/rejected ones stay listed but marked.
  const interactions = data?.interactions ?? [];
  // suggest_tasks cards render with their own Accept/Reject controls; the
  // ask/confirm cards keep the Yes/No · choice · free-text controls.
  const openInteractions = interactions.filter(
    (i) => i.status === "open" && i.kind !== "suggest_tasks",
  );
  const openSuggestions = interactions.filter(
    (i) => i.status === "open" && i.kind === "suggest_tasks",
  );
  const closedInteractions = interactions.filter((i) => i.status !== "open");

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
      setBanner({ kind: "ok", msg: "Comment posted to the Conversation." });
      reload();
      onChanged?.();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Comment failed" });
    } finally {
      setBusy(false);
    }
  }

  // Answer an interaction card. `verdict` is the terminal status the runtime
  // records (`resolved` / `rejected`); `response` is the chosen option, the
  // free-text answer, or a yes/no note. A duplicate answer is refused server-
  // side and surfaces here as an honest error banner.
  async function answerInteraction(
    it: BriefInteraction,
    verdict: "resolved" | "rejected",
    response: string,
  ) {
    setIxBusy(it.interaction_id);
    setBanner(null);
    try {
      await briefInteractions.respond(briefId, it.interaction_id, {
        responder: status?.username || "operator",
        status: verdict,
        response,
      });
      setBanner({
        kind: "ok",
        msg: verdict === "resolved" ? "Request answered." : "Request declined.",
      });
      setIxDraft((m) => ({ ...m, [it.interaction_id]: "" }));
      reload();
      onChanged?.();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Response failed" });
    } finally {
      setIxBusy(null);
    }
  }

  // Accept or reject a `suggest_tasks` card (§1.9). Accept materializes the
  // proposed children as real Sub-briefs server-side and returns their ids; a
  // duplicate answer is refused server-side and surfaces as an honest banner.
  async function answerSuggestion(it: BriefInteraction, accept: boolean) {
    setIxBusy(it.interaction_id);
    setBanner(null);
    try {
      const r = await briefSuggestions.respond(briefId, it.interaction_id, {
        responder: status?.username || "operator",
        accept,
      });
      const n = r?.created?.length ?? 0;
      setBanner({
        kind: "ok",
        msg: accept
          ? `Suggestion accepted — ${n} Sub-brief${n === 1 ? "" : "s"} created.`
          : "Suggestion declined.",
      });
      reload();
      onChanged?.();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Response failed" });
    } finally {
      setIxBusy(null);
    }
  }

  // ── Shift (run) lifecycle controls ──────────────────────────────────────
  // Start a Shift through the Operative's adapter (or an explicit `rig`
  // override such as `echo`). Refusals are surfaced honestly — never faked.
  async function runNow(rig?: string) {
    setRunBusy(true);
    setBanner({ kind: "info", msg: `Starting Shift${rig ? ` (${rig})` : ""}…` });
    try {
      const r = await api.post<RunReport>(
        `/v1/spine/briefs/${encodeURIComponent(briefId)}/run`,
        rig ? { rig } : {},
      );
      const accepted = r.status === "running" || r.status === "done";
      const refusal = ["unassigned", "no_adapter", "adapter_unavailable", "already_running", "not_found"].includes(r.status);
      let msg = REFUSALS[r.status] ?? r.status;
      if (r.rig) msg += ` · adapter ${r.rig}`;
      if (r.summary && r.status !== "running") msg += ` — ${r.summary}`;
      if (r.install_hint) msg += ` (${r.install_hint})`;
      setBanner({ kind: accepted ? "ok" : refusal ? "info" : "err", msg });
      reload();
      onChanged?.();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Run failed" });
    } finally {
      setRunBusy(false);
    }
  }

  // Accept / reject the latest done run.
  async function reviewRun(decision: "accepted" | "rejected") {
    if (!lr?.run_id) return;
    setBanner(null);
    try {
      await runControls.review(lr.run_id, decision);
      setBanner({ kind: "ok", msg: `Shift ${decision}.` });
      reload();
      onChanged?.();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Review failed" });
    }
  }

  // Apply an accepted run's changes into the project root.
  async function applyRun() {
    if (!lr?.run_id) return;
    setBanner(null);
    try {
      const r = await runControls.apply(lr.run_id);
      setBanner({
        kind: "ok",
        msg:
          `Apply ${r.apply_status ?? "done"}: ${r.applied_files ?? 0} applied, ${r.failed_files ?? 0} failed` +
          (r.brief_status === "done" ? " — Brief marked done." : "."),
      });
      reload();
      onChanged?.();
      await loadDiff();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Apply failed" });
    }
  }

  // Request cancellation of an in-flight Shift.
  async function cancelRun() {
    if (!lr?.run_id) return;
    setBanner(null);
    try {
      const r = await runControls.cancel(lr.run_id);
      setBanner({
        kind: "info",
        msg: r.active ? "Cancellation signalled — the Shift will report cancelled." : `Cancel requested: ${r.note ?? "no live process"}`,
      });
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Cancel failed" });
    }
  }

  // Load (or refresh) the safe-apply plan for the latest run.
  async function loadDiff() {
    if (!lr?.run_id) return;
    setDiff(await runControls.diff(lr.run_id));
  }

  // Auto-load the apply plan once a run is accepted-but-not-yet-applied, so the
  // operator sees what would change without a manual click. Cleared otherwise.
  const lrRunId = lr?.run_id;
  const lrReview = lr?.review;
  const lrApply = lr?.apply_status;
  useEffect(() => {
    if (lrRunId && lrReview === "accepted" && lrApply !== "applied") {
      void loadDiff();
    } else {
      setDiff(null);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [lrRunId, lrReview, lrApply]);

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

      {/* Latest Shift (run) — the execution lifecycle, operated in place. */}
      <div className="row" style={{ marginTop: 12, marginBottom: 6 }}>
        <strong style={{ fontSize: 12 }}>Latest Shift</strong>
        {(lr?.total_runs ?? 0) > 0 && (
          <span className="muted" style={{ fontSize: 11, marginLeft: 8 }}>{lr!.total_runs} run(s) total</span>
        )}
        <div className="spacer" style={{ flex: 1 }} />
        {lr?.run_id && (
          <Link to={`/runs?run=${encodeURIComponent(lr.run_id)}`} className="link" style={{ fontSize: 11 }}>
            Full transcript →
          </Link>
        )}
      </div>
      {!lr ? (
        <div style={{ fontSize: 12 }}>
          <div className="muted" style={{ marginBottom: 6 }}>
            No Shift yet — start one through the assigned Operative's adapter, or smoke the pipeline with <strong>echo</strong>.
          </div>
          <div className="row wrap" style={{ gap: 6 }}>
            <button className="btn sm" disabled={runBusy} title="Run this Brief through its Operative's adapter now" onClick={() => runNow()}>
              {runBusy ? "…" : "Run now"}
            </button>
            <button className="btn ghost sm" disabled={runBusy} title="Run with the echo Rig (no real adapter needed) — verifies the pipeline end to end" onClick={() => runNow("echo")}>
              echo
            </button>
          </div>
        </div>
      ) : (
        <div style={{ fontSize: 12 }}>
          <div className="row" style={{ gap: 8, flexWrap: "wrap" }}>
            <span className={"badge " + (RUN_TONE[lr.status ?? ""] ?? "todo")}>{lr.status ?? "—"}</span>
            {lr.refusal_reason && <span className="badge blocked" style={{ fontSize: 9 }} title="why the run didn't start">{lr.refusal_reason}</span>}
            {lr.trigger && <span className="muted" style={{ fontSize: 11 }}>{lr.trigger === "heartbeat" ? "auto" : lr.trigger}</span>}
            {lr.rig && <span className="muted">adapter <span className="mono">{lr.rig}</span></span>}
            {lr.review && <span className={"badge " + (lr.review === "accepted" ? "done" : lr.review === "rejected" ? "blocked" : "in_progress")} style={{ fontSize: 9 }}>{lr.review}</span>}
            {lr.apply_status && <span className={"badge " + (APPLY_STATUS_TONE[lr.apply_status] ?? "todo")} style={{ fontSize: 9 }}>apply: {lr.apply_status}</span>}
            {(lr.artifact_count ?? 0) > 0 && <span className="muted" style={{ fontSize: 11 }}>{lr.artifact_count} changed file(s)</span>}
          </div>
          <div className="muted" style={{ fontSize: 11, marginTop: 4 }}>
            {lr.started_at ? `started ${new Date(lr.started_at * 1000).toLocaleString()}` : ""}
            {lr.finished_at ? ` · finished ${new Date(lr.finished_at * 1000).toLocaleTimeString()}` : (lr.status === "running" ? " · in flight…" : "")}
            {typeof lr.duration_secs === "number" ? ` · ${lr.duration_secs}s` : ""}
          </div>
          {lr.summary && (
            <div style={{ marginTop: 4, whiteSpace: "pre-wrap", wordBreak: "break-word" }}>{lr.summary}</div>
          )}

          {/* Shift controls — run/re-run, cancel, review, all wrapping. */}
          <div className="row wrap" style={{ gap: 6, marginTop: 8 }}>
            <button className="btn sm" disabled={runBusy || lr.status === "running"} title="Start a new Shift through the Operative's adapter" onClick={() => runNow()}>
              {runBusy ? "…" : "Re-run"}
            </button>
            <button className="btn ghost sm" disabled={runBusy || lr.status === "running"} title="Run with the echo Rig (no real adapter needed)" onClick={() => runNow("echo")}>
              echo
            </button>
            {lr.status === "running" && lr.run_id && (
              <button className="btn ghost sm" title="Request cancellation of the in-flight Shift" onClick={cancelRun}>
                Cancel run
              </button>
            )}
            {lr.status === "done" && lr.run_id && lr.review !== "accepted" && (
              <button className="btn sm" title="Accept this Shift's output" onClick={() => reviewRun("accepted")}>
                Accept
              </button>
            )}
            {lr.status === "done" && lr.run_id && lr.review !== "rejected" && (
              <button className="btn ghost sm" title="Reject this Shift's output" onClick={() => reviewRun("rejected")}>
                Reject
              </button>
            )}
          </div>

          {/* Apply — copy an accepted Shift's changes into the project root. */}
          {lr.status === "done" && lr.review === "accepted" && (
            <div style={{ marginTop: 10 }}>
              <div className="row wrap" style={{ gap: 6, marginBottom: 4 }}>
                <strong style={{ fontSize: 12 }}>Apply</strong>
                <span className={"badge " + (APPLY_STATUS_TONE[lr.apply_status ?? ""] ?? "todo")} style={{ fontSize: 10 }}>
                  {lr.apply_status ?? "not applied"}
                </span>
                {diff?.plan?.note && <span className="muted" style={{ fontSize: 11 }}>{diff.plan.note}</span>}
                <div className="spacer" style={{ flex: 1 }} />
                <button className="btn ghost sm" onClick={loadDiff}>Refresh plan</button>
                {diff?.plan?.applicable && (diff.plan.changes ?? 0) > 0 && lr.apply_status !== "applied" && (
                  <button className="btn sm" onClick={applyRun}>
                    Apply {diff.plan.changes} change(s)
                  </button>
                )}
              </div>
              {diff?.plan?.project_root && (
                <div className="muted mono" style={{ fontSize: 11, marginBottom: 4 }}>→ {diff.plan.project_root}</div>
              )}
              {diff && diff.eligible === false && (
                <div className="banner info" style={{ fontSize: 11 }}>{diff.reason}</div>
              )}
              {(diff?.plan?.items?.length ?? 0) > 0 && (
                <div style={{ fontSize: 12, maxHeight: 180, overflow: "auto" }}>
                  {diff!.plan!.items!.map((it, j) => (
                    <div key={(it.rel_path ?? "") + j} style={{ padding: "2px 0", borderBottom: "1px solid var(--border-soft)" }}>
                      <span className={"badge " + (!it.can_apply ? "blocked" : it.action === "noop" ? "todo" : "done")} style={{ fontSize: 10 }}>{it.action}</span>{" "}
                      <span className="mono" style={{ fontSize: 11 }}>{it.rel_path}</span>{" "}
                      <span className="muted" style={{ fontSize: 10 }}>{it.reason}</span>
                    </div>
                  ))}
                </div>
              )}
              {diff?.plan && diff.plan.applicable === false && (diff.plan.items?.length ?? 0) > 0 && (
                <div className="banner err" style={{ fontSize: 11, marginTop: 4 }}>
                  Refusing apply: {diff.plan.conflicts ?? 0} conflict(s), {diff.plan.blocked ?? 0} blocked. Resolve these before applying.
                </div>
              )}
            </div>
          )}
        </div>
      )}

      {/* Requests — answerable interaction cards (§1.9; dashboard-design §7).
          Open ask/confirm cards an Operative or the companion raised sit above
          the Conversation so they read as "needs an answer", not a buried
          comment. Resolved/rejected cards stay listed but clearly marked. The
          section is omitted entirely when the Brief has no interactions (an
          honest empty state — no fabricated card). */}
      {interactions.length > 0 && (
        <>
          <div className="row" style={{ marginTop: 14, marginBottom: 6 }}>
            <strong style={{ fontSize: 12 }}>Requests</strong>
            <span className="muted" style={{ fontSize: 11, marginLeft: 8 }}>
              {openInteractions.length + openSuggestions.length} open ·{" "}
              {closedInteractions.length} answered
            </span>
          </div>

          {openInteractions.map((it) => (
            <div
              key={it.interaction_id}
              className="card"
              style={{ borderColor: "var(--warn, #b9770e)", padding: 10, marginBottom: 8 }}
            >
              <div className="row" style={{ gap: 6, alignItems: "baseline", flexWrap: "wrap" }}>
                <span className="badge in_progress" style={{ fontSize: 10 }}>{it.kind}</span>
                <span className="mono" style={{ fontSize: 10 }}>{it.author}</span>
                {it.created_at ? (
                  <span className="muted" style={{ fontSize: 10 }}>
                    {new Date(it.created_at * 1000).toLocaleString()}
                  </span>
                ) : null}
              </div>
              <div style={{ fontSize: 13, margin: "5px 0 8px", whiteSpace: "pre-wrap", wordBreak: "break-word" }}>
                {it.prompt}
              </div>

              {it.kind === "confirm" ? (
                // Yes/No gate: yes resolves, no rejects.
                <div className="row wrap" style={{ gap: 6 }}>
                  <button
                    className="btn sm"
                    disabled={ixBusy === it.interaction_id}
                    onClick={() => answerInteraction(it, "resolved", "yes")}
                  >
                    {ixBusy === it.interaction_id ? "…" : "Yes"}
                  </button>
                  <button
                    className="btn ghost sm"
                    disabled={ixBusy === it.interaction_id}
                    onClick={() => answerInteraction(it, "rejected", "no")}
                  >
                    No
                  </button>
                </div>
              ) : it.choices.length > 0 ? (
                // ask with fixed options: each choice resolves with that label.
                <div className="row wrap" style={{ gap: 6 }}>
                  {it.choices.map((c) => (
                    <button
                      key={c}
                      className="btn ghost sm"
                      disabled={ixBusy === it.interaction_id}
                      onClick={() => answerInteraction(it, "resolved", c)}
                    >
                      {c}
                    </button>
                  ))}
                </div>
              ) : (
                // open-ended ask: free-text answer.
                <div className="row" style={{ gap: 6 }}>
                  <input
                    className="input"
                    style={{ flex: 1, boxSizing: "border-box" }}
                    placeholder="Type an answer…"
                    value={ixDraft[it.interaction_id] ?? ""}
                    onChange={(e) =>
                      setIxDraft((m) => ({ ...m, [it.interaction_id]: e.target.value }))
                    }
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && (ixDraft[it.interaction_id] ?? "").trim()) {
                        e.preventDefault();
                        void answerInteraction(it, "resolved", (ixDraft[it.interaction_id] ?? "").trim());
                      }
                    }}
                  />
                  <button
                    className="btn sm"
                    disabled={ixBusy === it.interaction_id || !(ixDraft[it.interaction_id] ?? "").trim()}
                    onClick={() =>
                      answerInteraction(it, "resolved", (ixDraft[it.interaction_id] ?? "").trim())
                    }
                  >
                    {ixBusy === it.interaction_id ? "…" : "Answer"}
                  </button>
                </div>
              )}
            </div>
          ))}

          {/* suggest_tasks cards (§1.9): a proposed child-Brief tree the
              Operative raised. The operator accepts — materializing each as a
              real Sub-brief — or rejects. The proposed titles are listed so the
              operator sees exactly what they're accepting (no hidden creates). */}
          {openSuggestions.map((it) => {
            const children = it.proposal?.children ?? [];
            return (
              <div
                key={it.interaction_id}
                className="card"
                style={{ borderColor: "var(--warn, #b9770e)", padding: 10, marginBottom: 8 }}
              >
                <div className="row" style={{ gap: 6, alignItems: "baseline", flexWrap: "wrap" }}>
                  <span className="badge in_progress" style={{ fontSize: 10 }}>suggest tasks</span>
                  <span className="mono" style={{ fontSize: 10 }}>{it.author}</span>
                  {it.created_at ? (
                    <span className="muted" style={{ fontSize: 10 }}>
                      {new Date(it.created_at * 1000).toLocaleString()}
                    </span>
                  ) : null}
                </div>
                <div style={{ fontSize: 13, margin: "5px 0 6px", whiteSpace: "pre-wrap", wordBreak: "break-word" }}>
                  {it.proposal?.summary || it.prompt}
                </div>
                <ul style={{ margin: "0 0 8px", paddingLeft: 18, fontSize: 12 }}>
                  {children.map((c, i) => (
                    <li key={i} style={{ wordBreak: "break-word" }}>
                      {c.title}
                      {c.priority ? (
                        <span className="muted" style={{ fontSize: 10, marginLeft: 6 }}>
                          {c.priority}
                        </span>
                      ) : null}
                    </li>
                  ))}
                </ul>
                <div className="row wrap" style={{ gap: 6 }}>
                  <button
                    className="btn sm"
                    disabled={ixBusy === it.interaction_id || children.length === 0}
                    onClick={() => answerSuggestion(it, true)}
                  >
                    {ixBusy === it.interaction_id
                      ? "…"
                      : `Accept ${children.length} task${children.length === 1 ? "" : "s"}`}
                  </button>
                  <button
                    className="btn ghost sm"
                    disabled={ixBusy === it.interaction_id}
                    onClick={() => answerSuggestion(it, false)}
                  >
                    Reject
                  </button>
                </div>
              </div>
            );
          })}

          {closedInteractions.length > 0 && (
            <div style={{ fontSize: 12, marginBottom: 8 }}>
              {closedInteractions.map((it) => (
                <div
                  key={it.interaction_id}
                  style={{ padding: "4px 0", borderBottom: "1px solid var(--border-soft)" }}
                >
                  <div className="row" style={{ gap: 6, alignItems: "baseline", flexWrap: "wrap" }}>
                    <span
                      className={"badge " + (it.status === "resolved" ? "done" : "blocked")}
                      style={{ fontSize: 9 }}
                    >
                      {it.status}
                    </span>
                    <span className="muted" style={{ fontSize: 11 }}>{it.kind}</span>
                    <span style={{ wordBreak: "break-word" }}>{it.prompt}</span>
                  </div>
                  {(it.response || it.resolved_by) && (
                    <div className="muted" style={{ fontSize: 11, marginTop: 2 }}>
                      {it.resolved_by ? `${it.resolved_by}: ` : ""}
                      {it.response ?? ""}
                    </div>
                  )}
                </div>
              ))}
            </div>
          )}
        </>
      )}

      {/* Conversation — the Brief's work thread: every `brief.comment`
          event (human, companion, Operative) read oldest→newest. This thread
          IS the communication channel (execution-and-issue-design §0/§1.9);
          the full Chronicle ledger stays below as execution history. */}
      <div className="row" style={{ marginTop: 14, marginBottom: 6 }}>
        <strong style={{ fontSize: 12 }}>Conversation</strong>
        <span className="muted" style={{ fontSize: 11, marginLeft: 8 }}>
          {comments.length} comment(s)
        </span>
      </div>
      {comments.length === 0 ? (
        <div className="muted" style={{ fontSize: 12, marginBottom: 8 }}>
          No comments yet — add the first note to start the conversation. Comments are shared by you, the companion, and the assigned Operative.
        </div>
      ) : (
        <div style={{ maxHeight: 260, overflow: "auto", marginBottom: 8 }}>
          {comments.map((c, i) => (
            <div key={c.event_id ?? i} style={{ padding: "5px 0", borderBottom: "1px solid var(--border-soft)" }}>
              <div className="row" style={{ gap: 6, alignItems: "baseline", flexWrap: "wrap" }}>
                <span className="mono" style={{ fontSize: 11, fontWeight: 600 }}>{c.author || "—"}</span>
                {c.ts ? <span className="muted" style={{ fontSize: 10 }}>{new Date(c.ts * 1000).toLocaleString()}</span> : null}
              </div>
              <div style={{ fontSize: 12, whiteSpace: "pre-wrap", wordBreak: "break-word" }}>{c.body}</div>
            </div>
          ))}
        </div>
      )}
      {/* Composer — posts via `brief.comment`; the new comment lands in both
          the Conversation and the Chronicle after the refetch below. */}
      <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
        <textarea
          className="input"
          style={{ width: "100%", minHeight: 56, resize: "vertical", boxSizing: "border-box" }}
          placeholder="Write a comment…  (Enter to post · Shift+Enter for a new line)"
          value={comment}
          onChange={(e) => setComment(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              void submitComment();
            }
          }}
        />
        <div className="row" style={{ gap: 8 }}>
          <span className="muted" style={{ fontSize: 11 }}>Posts to the Conversation and the Chronicle.</span>
          <div className="spacer" style={{ flex: 1 }} />
          <button className="btn" onClick={submitComment} disabled={busy || !comment.trim()}>
            {busy ? "…" : "Comment"}
          </button>
        </div>
      </div>

      {/* Chronicle — the readable timeline (newest first) from `/events`,
          merging system notes, run lifecycle, board moves, and comments. */}
      <div className="row" style={{ marginTop: 14, marginBottom: 6 }}>
        <strong style={{ fontSize: 12 }}>Chronicle</strong>
        <span className="muted" style={{ fontSize: 11, marginLeft: 8 }}>
          {d.chronicle?.total ?? 0} event(s) total · showing newest {events.length}
          {events.length >= EVENT_LIMIT ? ` (capped at ${EVENT_LIMIT})` : ""}
        </span>
      </div>
      {loading ? (
        <div className="loading">Loading…</div>
      ) : events.length === 0 ? (
        <div className="muted" style={{ fontSize: 12 }}>No Chronicle events yet for this Brief.</div>
      ) : (
        <div style={{ maxHeight: 240, overflow: "auto", fontSize: 12 }}>
          {events.map((ev, i) => (
            <div key={ev.event_id ?? i} style={{ padding: "3px 0", borderBottom: "1px solid var(--border-soft)" }}>
              <span style={{ display: "inline-block", width: 8, height: 8, borderRadius: 4, marginRight: 6, background: eventTone(ev.event_type) }} />
              <span className="muted" style={{ fontSize: 10 }}>{ev.ts ? new Date(ev.ts * 1000).toLocaleString() : ""}</span>{" "}
              <span className="mono" style={{ fontSize: 11 }}>{ev.event_type}</span>
              {ev.payload && <> — <span style={{ whiteSpace: "pre-wrap", wordBreak: "break-word" }}>{ev.payload}</span></>}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
