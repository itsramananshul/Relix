import { useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import {
  api,
  ApiError,
  tryGet,
  tryGetReport,
  subscribeRunEvents,
  subscribeBriefInteractions,
  runControls,
  briefInteractions,
  briefSuggestions,
  briefPlanConfirms,
  briefDossiers,
  type RunDiff,
  type BriefInteraction,
  type InteractionStreamConn,
  type SuggestChild,
  type DossierMeta,
} from "../api";
import { useAuth } from "../auth";
import { Badge, useAsync } from "./common";
import { RunTranscript } from "./RunTranscript";
import { invalidate, useInvalidate } from "../invalidate";

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

// Interaction terminal-status → badge tone. `expired` (§1.8) is rendered
// distinctly from `rejected`: a plan-bound `confirm` that was superseded by a
// newer `plan` Dossier revision (or a comment) before it could be approved —
// the operator did NOT decline it, the plan moved on. Neutral tone, not the
// red of a rejection.
const IX_STATUS_TONE: Record<string, string> = {
  resolved: "done",
  rejected: "blocked",
  expired: "todo",
};

// Plan-package composer (§1.7/§1.8/§3.1): the priority options a child task
// may carry — the same `low|normal|high|urgent` set the proposal validator
// accepts (`brief::normalize_proposal`); empty ⇒ the child opens at the default.
const PRIORITY_OPTS = ["low", "normal", "high", "urgent"];

// One editable child-task row in the composer. `id` is a stable local key so a
// dependency (`afterId`) survives reorders/removals without index drift; it is
// resolved to the proposal's 0-based `after` index only at submit time.
interface ComposerChild {
  id: number;
  title: string;
  priority: string; // "" = default (omit)
  afterId: number | null; // local id of an EARLIER sibling, or null
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
  dossiers?: DossierMeta[];
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
}: {
  briefId: string;
  onClose: () => void;
}) {
  const { status } = useAuth();
  const [comment, setComment] = useState("");
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);
  const [busy, setBusy] = useState(false);
  // Shift-control state: a busy flag while a run is starting, and the loaded
  // safe-apply plan for the latest accepted run.
  const [runBusy, setRunBusy] = useState(false);
  const [diff, setDiff] = useState<RunDiff | null>(null);
  // Force the embedded live-work transcript to refetch after a Shift mutation
  // (run/re-run/review/apply/cancel) re-shapes the latest run.
  const [txKey, setTxKey] = useState(0);
  // Thread-interaction state: which card is being answered, and the free-text
  // draft for open `ask` cards that have no fixed choices.
  const [ixBusy, setIxBusy] = useState<string | null>(null);
  const [ixDraft, setIxDraft] = useState<Record<string, string>>({});
  // Live interaction-card override (dashboard-design §11 surgical update): the
  // dedicated interactions SSE stream below writes the latest card array here so
  // ask/confirm/suggest/plan-package cards refresh even when no run event fires.
  // `null` ⇒ fall back to the useAsync snapshot; cleared whenever a fresh load
  // arrives (see the `data` effect) so a local answer's reload is never masked,
  // then the stream re-establishes it on its next push. Plus the stream's honest
  // connection state for the subtle "live" cue on the Requests header.
  const [streamIx, setStreamIx] = useState<BriefInteraction[] | null>(null);
  const [ixConn, setIxConn] = useState<InteractionStreamConn>("connecting");
  // Busy flag while a plan-approval confirm is being opened (§1.8).
  const [planBusy, setPlanBusy] = useState(false);
  // Plan-package composer state (§1.7/§1.8/§3.1). A minimal MANUAL composer —
  // a plan title/body + an approval prompt + a small child-task list — that
  // opens a plan package (plan Dossier + suggest_tasks proposal + bound
  // confirm) through `briefPlanConfirms.open`. Collapsed by default to keep the
  // workroom calm; the created bound confirm then answers through the already-
  // safe response path in Requests.
  const [composerOpen, setComposerOpen] = useState(false);
  const [composerBusy, setComposerBusy] = useState(false);
  const [planTitle, setPlanTitle] = useState("");
  const [planBody, setPlanBody] = useState("");
  const [planPrompt, setPlanPrompt] = useState("");
  const childSeq = useRef(0);
  const newChild = (): ComposerChild => ({
    id: childSeq.current++,
    title: "",
    priority: "",
    afterId: null,
  });
  const [planChildren, setPlanChildren] = useState<ComposerChild[]>(() => [newChild()]);
  const updateChild = (id: number, patch: Partial<ComposerChild>) =>
    setPlanChildren((cs) => cs.map((c) => (c.id === id ? { ...c, ...patch } : c)));
  const removeChild = (id: number) =>
    setPlanChildren((cs) => (cs.length === 1 ? cs : cs.filter((c) => c.id !== id)));

  // Documents (Dossiers) editor state (§1.8). A minimal, append-only authoring
  // surface — a kind/title/body textarea, NOT a rich-text editor. `docBaseId`
  // is the loaded latest revision's id, held as the optimistic-lock base: a
  // "Save revision" sends it as `expected_latest_doc_id`, so a save after a
  // newer revision landed is refused (HTTP 409) and the draft is kept for a
  // reload or an explicit fork. `null` base ⇒ authoring the first revision of a
  // kind. `docStale` flags that the last save lost the lock (offer fork).
  const [docOpen, setDocOpen] = useState(false);
  const [docBusy, setDocBusy] = useState(false);
  const [docKind, setDocKind] = useState("plan");
  const [docTitle, setDocTitle] = useState("");
  const [docBody, setDocBody] = useState("");
  const [docBaseId, setDocBaseId] = useState<string | null>(null);
  const [docBaseRev, setDocBaseRev] = useState<number | null>(null);
  const [docStale, setDocStale] = useState(false);

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

  // Dedicated interaction-card stream (dashboard-design §7/§11): while THIS
  // Brief is open, subscribe to its `…/interactions/stream` SSE so the
  // ask/confirm/suggest/plan-package cards refresh the moment the card list
  // changes — a card raised, answered, or superseded — even when no run event
  // fires (the run-event stream above misses pure card changes). We update the
  // card state DIRECTLY (no extra reload) per the §11 surgical-update rule; the
  // run-event reload above still owns the rest of the workroom. Re-subscribes
  // when the Brief changes.
  useEffect(() => {
    setStreamIx(null);
    setIxConn("connecting");
    const unsub = subscribeBriefInteractions(
      briefId,
      (arr) => setStreamIx(arr),
      (state) => setIxConn(state),
    );
    return () => {
      unsub();
      setStreamIx(null);
    };
  }, [briefId]);

  // Drop the live override whenever a fresh useAsync load lands (initial load or
  // any reload — e.g. after answering a card), so the freshly-fetched cards show
  // immediately instead of being masked by a now-stale stream frame. The stream
  // re-establishes the override on its next push (identical content ⇒ no flicker).
  useEffect(() => {
    setStreamIx(null);
  }, [data]);

  // Client invalidation bus (dashboard-design §11): refetch this Brief when a
  // CO-MOUNTED surface (the Issue board beside this panel) reports it changed —
  // e.g. an assign/move on the board card mirrors into the open detail without
  // a manual Refresh. Scoped to THIS Brief so other Briefs' churn is ignored.
  useInvalidate("brief", reload, {
    match: (m) => !m.briefId || m.briefId === briefId,
  });

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
  // Prefer the live stream override when present (dashboard-design §11), else
  // the useAsync snapshot.
  const interactions = streamIx ?? data?.interactions ?? [];
  // suggest_tasks cards render with their own Accept/Reject controls; the
  // ask/confirm cards keep the Yes/No · choice · free-text controls.
  const openInteractions = interactions.filter(
    (i) => i.status === "open" && i.kind !== "suggest_tasks",
  );
  const openSuggestions = interactions.filter(
    (i) => i.status === "open" && i.kind === "suggest_tasks",
  );
  const closedInteractions = interactions.filter((i) => i.status !== "open");
  // Plan-approval control state (§1.8). The detail's `dossiers` list isn't
  // ordered by revision, so it tells us only whether a `plan` Dossier EXISTS
  // locally — enough to enable/label the control honestly; the bridge binds to
  // the true latest revision (and refuses if none). An already-open plan-bound
  // confirm is surfaced so we don't invite a duplicate pending approval.
  const hasPlanDossier = (d.dossiers ?? []).some((x) => x.kind === "plan");
  const openPlanConfirm = interactions.find(
    (i) => i.status === "open" && i.kind === "confirm" && i.bound_doc_kind === "plan",
  );
  // Documents (§1.8): the latest revision of each kind, for the editor list.
  // The detail's `dossiers` array is oldest→newest, so the LAST entry of each
  // kind is its latest; `revision_number` is the count for that kind.
  const latestByKind = new Map<string, DossierMeta>();
  for (const x of d.dossiers ?? []) {
    if (x.kind) latestByKind.set(x.kind, x);
  }
  const dossierKinds = Array.from(latestByKind.values()).sort((a, b) =>
    a.kind.localeCompare(b.kind),
  );

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
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Comment failed" });
    } finally {
      setBusy(false);
    }
  }

  // Request approval for the latest `plan` Dossier (§1.8): open a `confirm`
  // bound to that exact revision. The bridge refuses when the Brief has no
  // `plan` Dossier; we disable the control locally when we can see there's no
  // plan, but still surface a server refusal honestly if the data is stale.
  // Once opened, the bound confirm appears in Requests for a Yes/No answer.
  async function requestPlanApproval() {
    setPlanBusy(true);
    setBanner(null);
    try {
      await briefInteractions.openPlanConfirm(briefId, {
        author: status?.username || "operator",
      });
      setBanner({
        kind: "ok",
        msg: "Plan approval requested — answer the bound confirm in Requests below.",
      });
      reload();
      // A new open confirm can add an Action Center item (§1.9) — refresh it.
      invalidate(["briefs", "actions"], { briefId });
    } catch (e) {
      setBanner({
        kind: "err",
        msg: e instanceof Error ? e.message : "Could not request plan approval",
      });
    } finally {
      setPlanBusy(false);
    }
  }

  // Open a plan package from the dashboard (§1.7/§1.8/§3.1): a `plan` Dossier
  // revision + a `suggest_tasks` proposal + an approval-bound `confirm` linked
  // to both, in one call through the shipped `brief.plan_package_open`. This is
  // a MANUAL composer — not a document editor or LLM planner — that makes the
  // shipped backend usable from the dashboard. Validates locally (non-empty
  // plan body, ≥1 child with a title) before submit; `after` is offered only
  // for an EARLIER titled sibling, so a self/forward dependency can't be
  // expressed in the UI. On success the bound confirm appears in Requests and
  // is approved through the already-safe `briefPlanConfirms.respond` path
  // (which materializes the proposal exactly once).
  async function submitPlanPackage() {
    const body = planBody.trim();
    const rows = planChildren.filter((c) => c.title.trim());
    if (!body) {
      setBanner({ kind: "err", msg: "A plan package needs a plan body." });
      return;
    }
    if (rows.length === 0) {
      setBanner({ kind: "err", msg: "Add at least one child task with a title." });
      return;
    }
    // Resolve each child's local `afterId` to a 0-based index over the KEPT
    // (titled) rows, and only when it points at an earlier sibling — a row whose
    // referenced task was left untitled or removed simply drops its dependency.
    const idToIndex = new Map(rows.map((c, i) => [c.id, i] as const));
    const childrenPayload: SuggestChild[] = rows.map((c, selfIdx) => {
      const out: SuggestChild = { title: c.title.trim() };
      if (c.priority) out.priority = c.priority;
      if (c.afterId != null && idToIndex.has(c.afterId)) {
        const depIdx = idToIndex.get(c.afterId)!;
        if (depIdx < selfIdx) out.after = depIdx;
      }
      return out;
    });
    setComposerBusy(true);
    setBanner(null);
    try {
      const r = await briefPlanConfirms.open(briefId, {
        author: status?.username || "operator",
        plan_title: planTitle.trim() || "Plan",
        plan_body: body,
        prompt: planPrompt.trim() || undefined,
        children: childrenPayload,
      });
      const n = childrenPayload.length;
      setBanner({
        kind: "ok",
        msg:
          `Plan package created — ${n} task${n === 1 ? "" : "s"} proposed. ` +
          `Approve the bound confirm (${r.confirm_id}) in Requests below to materialize them.`,
      });
      // Reset + close the composer for the next plan.
      setPlanTitle("");
      setPlanBody("");
      setPlanPrompt("");
      childSeq.current = 0;
      setPlanChildren([newChild()]);
      setComposerOpen(false);
      reload();
      // A new open confirm + proposal can add Action Center items (§1.9).
      invalidate(["briefs", "actions"], { briefId });
    } catch (e) {
      setBanner({
        kind: "err",
        msg: e instanceof Error ? e.message : "Could not create the plan package",
      });
    } finally {
      setComposerBusy(false);
    }
  }

  // Load the latest revision of a kind into the editor (§1.8). Populates the
  // title/body and keeps the loaded `doc_id` as the optimistic-lock base. An
  // empty result (no Dossier of that kind yet) leaves the body blank with a
  // null base — the next save authors the first revision.
  async function loadDocLatest(kind: string) {
    setDocBusy(true);
    setBanner(null);
    setDocStale(false);
    try {
      const doc = await briefDossiers.latest(briefId, kind);
      setDocKind(kind);
      if (doc && doc.doc_id) {
        setDocTitle(doc.title ?? "");
        setDocBody(doc.body ?? "");
        setDocBaseId(doc.doc_id);
        setDocBaseRev(doc.revision_number ?? null);
      } else {
        setDocTitle("");
        setDocBody("");
        setDocBaseId(null);
        setDocBaseRev(null);
        setBanner({ kind: "ok", msg: `No "${kind}" document yet — your save creates revision 1.` });
      }
      setDocOpen(true);
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Could not load the document" });
    } finally {
      setDocBusy(false);
    }
  }

  // Start a fresh document of a kind (no base — authors revision 1, or revises
  // the latest if one already exists and the save provides no expected id; the
  // backend appends on top in that case).
  function newDoc() {
    setDocTitle("");
    setDocBody("");
    setDocBaseId(null);
    setDocBaseRev(null);
    setDocStale(false);
    setDocOpen(true);
  }

  // Save a revision (§1.8). When a base revision is loaded, send it as
  // `expected_latest_doc_id` so a stale save (a newer revision landed first) is
  // refused with HTTP 409 — we DON'T clear the draft; we mark it stale and
  // invite a reload or a fork. On success the editor rebases onto the new
  // revision so a follow-up save chains cleanly.
  async function saveDocRevision() {
    const kind = docKind.trim();
    const title = docTitle.trim();
    const body = docBody.trim();
    if (!kind || !title || !body) {
      setBanner({ kind: "err", msg: "Document kind, title, and body are all required." });
      return;
    }
    setDocBusy(true);
    setBanner(null);
    try {
      const r = await briefDossiers.author(briefId, {
        kind,
        title,
        body: docBody,
        author: status?.username || "operator",
        mode: "revise",
        expected_latest_doc_id: docBaseId ?? undefined,
      });
      setDocBaseId(r.doc_id);
      setDocBaseRev(r.revision_number);
      setDocStale(false);
      setBanner({
        kind: "ok",
        msg: `Saved "${kind}" revision ${r.revision_number}${
          kind === "plan" ? " — any plan-bound approval is now stale until re-requested." : "."
        }`,
      });
      reload();
      invalidate(["briefs"], { briefId });
    } catch (e) {
      if (e instanceof ApiError && e.status === 409) {
        // Optimistic-lock conflict: a newer revision landed first. Keep the
        // draft, mark it stale, and refresh the list so the operator sees the
        // new latest — they can reload it or fork their draft. Never retry the
        // 409 blindly.
        setDocStale(true);
        setBanner({
          kind: "err",
          msg:
            "This document changed since you loaded it — your draft is kept. " +
            "Reload the latest revision (losing your edits), or fork your draft as a new line.",
        });
        reload();
      } else {
        setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Could not save the revision" });
      }
    } finally {
      setDocBusy(false);
    }
  }

  // Fork the current draft as a new line from the loaded base revision (§1.8).
  // Explicit branch — carries `forked_from_doc_id` and is NOT a stale overwrite
  // (the base may be behind the latest). Requires a loaded base.
  async function forkDocFromLoaded() {
    const kind = docKind.trim();
    const title = docTitle.trim();
    const body = docBody.trim();
    if (!docBaseId) {
      setBanner({ kind: "err", msg: "Load a revision first to fork from it." });
      return;
    }
    if (!kind || !title || !body) {
      setBanner({ kind: "err", msg: "Document kind, title, and body are all required." });
      return;
    }
    setDocBusy(true);
    setBanner(null);
    try {
      const r = await briefDossiers.author(briefId, {
        kind,
        title,
        body: docBody,
        author: status?.username || "operator",
        mode: "fork",
        base_doc_id: docBaseId,
      });
      setDocBaseId(r.doc_id);
      setDocBaseRev(r.revision_number);
      setDocStale(false);
      setBanner({
        kind: "ok",
        msg: `Forked "${kind}" as revision ${r.revision_number} (from ${r.forked_from_doc_id}).`,
      });
      reload();
      invalidate(["briefs"], { briefId });
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Could not fork the document" });
    } finally {
      setDocBusy(false);
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
      // Plan-package confirm (§1.7/§1.8/§3.1): a `confirm` linked to a
      // `suggest_tasks` proposal (`bound_interaction_id`). Answering it through
      // the generic interaction respond would resolve the card WITHOUT
      // materializing the proposal; the safe plan-confirm route ties approval to
      // the decomposition trigger (accept materializes the linked proposal
      // exactly once through the resumable ledger; reject closes both).
      if (it.bound_interaction_id) {
        const accept = verdict === "resolved";
        const r = await briefPlanConfirms.respond(briefId, it.interaction_id, {
          responder: status?.username || "operator",
          accept,
        });
        const n = r?.created?.length ?? 0;
        setBanner({
          kind: "ok",
          msg: accept
            ? `Plan approved — ${n} Sub-brief${n === 1 ? "" : "s"} created` +
              (r?.outcome === "already_approved"
                ? " (already approved — no duplicates)."
                : ".")
            : r?.outcome === "rejected_proposal_already_closed"
              ? "Plan declined (the proposal had already materialized)."
              : "Plan declined.",
        });
        setIxDraft((m) => ({ ...m, [it.interaction_id]: "" }));
        reload();
        invalidate(["briefs", "actions"], { briefId });
        return;
      }
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
      // Answering a Request can clear an Action Center item (§1.9); a `confirm`
      // can also unblock board work — refresh those co-mounted surfaces.
      invalidate(["briefs", "actions"], { briefId });
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
      // Resolution is all-or-nothing, so a successful accept means every
      // child that carried an assignee hint passed the assign-Key gate and is
      // staffed; the rest opened unassigned (the default).
      const assignedN = (it.proposal?.children ?? []).filter(
        (c) => c.assignee_agent_id || c.assignee_role,
      ).length;
      const needN = n - assignedN;
      setBanner({
        kind: "ok",
        msg: accept
          ? `Suggestion accepted — ${n} Sub-brief${n === 1 ? "" : "s"} created` +
            (assignedN > 0 ? `, ${assignedN} assigned through the assign-Key gate` : "") +
            (needN > 0
              ? `. ${needN} open${needN === 1 ? "s" : ""} unassigned — assign an Operative below ` +
                `(also in the Action Center as “Assign an Operative”).`
              : ".")
          : "Suggestion declined.",
      });
      reload();
      // Accept materializes child Sub-briefs on the board and adds
      // "assign an Operative" items to the Action Center (§1.9) — refresh both.
      invalidate(["briefs", "actions"], { briefId });
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
      setTxKey((k) => k + 1);
      // Starting a Shift updates the board card's run badge + the Runs ledger.
      invalidate(["briefs", "runs"], { briefId });
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
      setTxKey((k) => k + 1);
      // Review verdict shows on the board card + the Runs ledger.
      invalidate(["briefs", "runs"], { briefId });
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
      setTxKey((k) => k + 1);
      // Apply can advance the Brief to done — refresh the board card + Runs.
      invalidate(["briefs", "runs"], { briefId });
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
      setTxKey((k) => k + 1);
      // A cancellation request shows on the board card + the Runs ledger.
      invalidate(["briefs", "runs"], { briefId });
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

          {/* Live work — the Shift's run transcript merged into the workroom so
              the agent's work is visible inside the Brief, not only on a
              separate Runs page (dashboard-design §7/§8). Block-grouped + live-
              tailed via the same renderer the Runs page uses; nice/raw toggle. */}
          {lr.run_id && (
            <div style={{ marginTop: 12 }}>
              <RunTranscript runId={lr.run_id} status={lr.status} compact refreshKey={txKey} />
            </div>
          )}
        </div>
      )}

      {/* Plan approval (§1.8; dashboard-design §7 "Request confirmation — used
          for plan approval"). A workroom-native control that opens a `confirm`
          bound to the Brief's latest `plan` Dossier revision. Disabled when no
          plan Dossier is visible locally (the bridge would refuse anyway) or
          when one is already pending — answered as Yes/No in Requests below. */}
      <div
        className="row"
        style={{ marginTop: 14, gap: 8, alignItems: "baseline", flexWrap: "wrap" }}
      >
        <strong style={{ fontSize: 12 }}>Plan approval</strong>
        <span className="muted" style={{ fontSize: 11 }}>
          {openPlanConfirm
            ? "A plan approval is pending below — answer it in Requests."
            : hasPlanDossier
              ? "Bind an approval to the latest plan Dossier revision."
              : "No plan Dossier yet — attach a plan before requesting approval."}
        </span>
        <div className="spacer" style={{ flex: 1 }} />
        <button
          className="btn sm"
          disabled={planBusy || !hasPlanDossier || !!openPlanConfirm}
          title={
            openPlanConfirm
              ? "A plan-bound confirm is already open — answer it in Requests"
              : hasPlanDossier
                ? "Open an approval-bound confirm against the latest plan Dossier"
                : "No plan Dossier to approve — attach one first"
          }
          onClick={requestPlanApproval}
        >
          {planBusy ? "…" : "Request approval"}
        </button>
      </div>

      {/* Plan package composer (§1.7/§1.8/§3.1; dashboard-design §7 planning
          mode). A minimal MANUAL composer — NOT a document editor or LLM
          planner — that opens a plan package (an immutable `plan` Dossier
          revision + a `suggest_tasks` proposal + an approval-bound `confirm`
          linked to both) through the shipped `brief.plan_package_open`. The
          created bound confirm then appears in Requests and is approved through
          the already-safe response path (which materializes the proposal
          exactly once). Collapsed by default; uses the workroom section style
          (no card-in-card nesting). */}
      <div
        className="row"
        style={{ marginTop: 14, gap: 8, alignItems: "baseline", flexWrap: "wrap" }}
      >
        <strong style={{ fontSize: 12 }}>Plan package</strong>
        <span className="muted" style={{ fontSize: 11 }}>
          Draft a plan + child tasks for approval — materializes on Yes.
        </span>
        <div className="spacer" style={{ flex: 1 }} />
        <button
          className="btn ghost sm"
          onClick={() => setComposerOpen((v) => !v)}
          aria-expanded={composerOpen}
        >
          {composerOpen ? "Cancel" : "New plan package"}
        </button>
      </div>

      {composerOpen && (
        <div style={{ display: "flex", flexDirection: "column", gap: 8, marginTop: 8 }}>
          <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
            <span className="muted" style={{ fontSize: 11 }}>Plan title (optional)</span>
            <input
              className="input"
              style={{ boxSizing: "border-box" }}
              placeholder="Plan"
              value={planTitle}
              onChange={(e) => setPlanTitle(e.target.value)}
            />
          </label>
          <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
            <span className="muted" style={{ fontSize: 11 }}>Plan body (required)</span>
            <textarea
              className="input"
              style={{ width: "100%", minHeight: 72, resize: "vertical", boxSizing: "border-box" }}
              placeholder="Describe the plan to be approved…"
              value={planBody}
              onChange={(e) => setPlanBody(e.target.value)}
            />
          </label>
          <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
            <span className="muted" style={{ fontSize: 11 }}>Approval prompt (optional)</span>
            <input
              className="input"
              style={{ boxSizing: "border-box" }}
              placeholder="Approve this plan?"
              value={planPrompt}
              onChange={(e) => setPlanPrompt(e.target.value)}
            />
          </label>

          <div className="row" style={{ marginTop: 2 }}>
            <strong style={{ fontSize: 12 }}>Child tasks</strong>
            <span className="muted" style={{ fontSize: 11, marginLeft: 8 }}>
              {planChildren.filter((c) => c.title.trim()).length} with a title
            </span>
          </div>
          {planChildren.map((c, i) => {
            // `after` may reference only an EARLIER titled sibling — so a
            // self/forward/cyclic dependency can't be expressed in the UI.
            const earlier = planChildren.slice(0, i).filter((p) => p.title.trim());
            const afterValue =
              c.afterId != null && earlier.some((p) => p.id === c.afterId)
                ? String(c.afterId)
                : "";
            return (
              <div key={c.id} className="row wrap" style={{ gap: 6, alignItems: "center" }}>
                <span className="muted mono" style={{ fontSize: 11, width: 22 }}>#{i + 1}</span>
                <input
                  className="input"
                  style={{ flex: "2 1 160px", minWidth: 0, width: "auto", boxSizing: "border-box" }}
                  placeholder={`Task ${i + 1} title`}
                  value={c.title}
                  onChange={(e) => updateChild(c.id, { title: e.target.value })}
                />
                <select
                  className="input"
                  style={{ flex: "0 1 110px", minWidth: 0, width: "auto", boxSizing: "border-box" }}
                  value={c.priority}
                  onChange={(e) => updateChild(c.id, { priority: e.target.value })}
                  title="Priority (optional)"
                >
                  <option value="">priority…</option>
                  {PRIORITY_OPTS.map((p) => (
                    <option key={p} value={p}>{p}</option>
                  ))}
                </select>
                <select
                  className="input"
                  style={{ flex: "1 1 130px", minWidth: 0, width: "auto", boxSizing: "border-box" }}
                  value={afterValue}
                  onChange={(e) =>
                    updateChild(c.id, { afterId: e.target.value ? Number(e.target.value) : null })
                  }
                  disabled={earlier.length === 0}
                  title="Optional dependency: start after an earlier task is done"
                >
                  <option value="">after…</option>
                  {earlier.map((p) => (
                    <option key={p.id} value={String(p.id)}>
                      after #{planChildren.indexOf(p) + 1}
                    </option>
                  ))}
                </select>
                <button
                  className="btn ghost sm"
                  title="Remove this task"
                  onClick={() => removeChild(c.id)}
                  disabled={planChildren.length === 1}
                >
                  ✕
                </button>
              </div>
            );
          })}
          <div className="row wrap" style={{ gap: 8 }}>
            <button
              className="btn ghost sm"
              onClick={() => setPlanChildren((cs) => [...cs, newChild()])}
            >
              + Add task
            </button>
            <div className="spacer" style={{ flex: 1 }} />
            <span className="muted" style={{ fontSize: 11 }}>
              Opens a plan Dossier + a bound approval confirm.
            </span>
            <button
              className="btn"
              onClick={submitPlanPackage}
              disabled={
                composerBusy ||
                !planBody.trim() ||
                planChildren.filter((c) => c.title.trim()).length === 0
              }
            >
              {composerBusy ? "…" : "Create plan package"}
            </button>
          </div>
        </div>
      )}

      {/* Documents (§1.8; dashboard-design §7 work artifacts). An append-only
          authoring surface — a kind/title/body textarea, NOT a rich editor —
          for the Brief's Dossiers (plan/design/notes). Each kind shows its
          latest revision; "Edit latest" loads it under an optimistic lock so a
          "Save revision" after a newer one landed is refused (the draft is kept
          for a reload or fork). Saving a new `plan` revision naturally makes any
          plan-bound approval stale (the approval binds the latest plan id). */}
      <div
        className="row"
        style={{ marginTop: 14, gap: 8, alignItems: "baseline", flexWrap: "wrap" }}
      >
        <strong style={{ fontSize: 12 }}>Documents</strong>
        <span className="muted" style={{ fontSize: 11 }}>
          {dossierKinds.length > 0
            ? `${dossierKinds.length} kind${dossierKinds.length === 1 ? "" : "s"} · append-only revisions`
            : "No documents yet — author a plan, design, or notes."}
        </span>
        <div className="spacer" style={{ flex: 1 }} />
        <button className="btn ghost sm" onClick={() => setDocOpen((v) => !v)} aria-expanded={docOpen}>
          {docOpen ? "Hide editor" : "New / edit document"}
        </button>
      </div>

      {dossierKinds.length > 0 && (
        <div style={{ display: "flex", flexDirection: "column", gap: 4, marginTop: 6 }}>
          {dossierKinds.map((doc) => (
            <div
              key={doc.kind}
              className="row wrap"
              style={{ gap: 8, alignItems: "baseline", fontSize: 12 }}
            >
              <span className="mono" style={{ fontWeight: 600 }}>{doc.kind}</span>
              <span>{doc.title}</span>
              <span className="muted" style={{ fontSize: 11 }}>
                rev {doc.revision_number ?? 1}
                {doc.author ? ` · ${doc.author}` : ""}
                {doc.forked_from_doc_id ? " · forked" : ""}
              </span>
              <div className="spacer" style={{ flex: 1 }} />
              <button
                className="btn ghost sm"
                disabled={docBusy}
                title={`Load the latest "${doc.kind}" revision for editing`}
                onClick={() => loadDocLatest(doc.kind)}
              >
                Edit latest
              </button>
            </div>
          ))}
        </div>
      )}

      {docOpen && (
        <div style={{ display: "flex", flexDirection: "column", gap: 8, marginTop: 8 }}>
          <div className="row wrap" style={{ gap: 6, alignItems: "center" }}>
            <label style={{ display: "flex", flexDirection: "column", gap: 4, flex: "0 1 160px" }}>
              <span className="muted" style={{ fontSize: 11 }}>Kind</span>
              <input
                className="input"
                style={{ boxSizing: "border-box" }}
                placeholder="plan"
                value={docKind}
                onChange={(e) => {
                  setDocKind(e.target.value);
                  // Changing the kind by hand drops the loaded base — the new
                  // kind's latest is a different lock target. Use "Edit latest"
                  // to rebase onto a kind's current revision.
                  setDocBaseId(null);
                  setDocBaseRev(null);
                  setDocStale(false);
                }}
              />
            </label>
            <label style={{ display: "flex", flexDirection: "column", gap: 4, flex: "1 1 200px" }}>
              <span className="muted" style={{ fontSize: 11 }}>Title</span>
              <input
                className="input"
                style={{ boxSizing: "border-box" }}
                placeholder="Document title"
                value={docTitle}
                onChange={(e) => setDocTitle(e.target.value)}
              />
            </label>
            <button
              className="btn ghost sm"
              style={{ alignSelf: "flex-end" }}
              onClick={newDoc}
              disabled={docBusy}
              title="Start a fresh draft (no loaded base)"
            >
              New
            </button>
          </div>
          <span className="muted" style={{ fontSize: 11 }}>
            {docBaseId
              ? `Editing latest revision ${docBaseRev ?? "?"} (locked to this revision — a newer one rejects the save).`
              : "New draft — saving authors the next revision of this kind."}
          </span>
          <label style={{ display: "flex", flexDirection: "column", gap: 4 }}>
            <span className="muted" style={{ fontSize: 11 }}>Body</span>
            <textarea
              className="input"
              style={{ width: "100%", minHeight: 120, resize: "vertical", boxSizing: "border-box" }}
              placeholder="Document body (plain text — append-only)…"
              value={docBody}
              onChange={(e) => setDocBody(e.target.value)}
            />
          </label>
          <div className="row wrap" style={{ gap: 8, alignItems: "center" }}>
            {docStale && (
              <span style={{ color: "#c0392b", fontSize: 11 }}>
                Stale — reload latest or fork your draft.
              </span>
            )}
            <div className="spacer" style={{ flex: 1 }} />
            {docBaseId && (
              <button
                className="btn ghost sm"
                onClick={forkDocFromLoaded}
                disabled={docBusy || !docTitle.trim() || !docBody.trim()}
                title="Branch your draft as a new line from the loaded revision (not a stale overwrite)"
              >
                {docBusy ? "…" : "Fork from loaded revision"}
              </button>
            )}
            <button
              className="btn"
              onClick={saveDocRevision}
              disabled={docBusy || !docKind.trim() || !docTitle.trim() || !docBody.trim()}
              title="Append a new revision (optimistic-locked to the loaded revision)"
            >
              {docBusy ? "…" : "Save revision"}
            </button>
          </div>
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
            {/* Subtle live cue (dashboard-design §11): a muted dot when the
                dedicated interaction stream is connected — cards refresh on
                change without a manual Refresh. Hidden unless live to stay calm. */}
            {ixConn === "live" && (
              <span
                className="muted"
                style={{ fontSize: 10, marginLeft: 8, display: "inline-flex", alignItems: "center", gap: 4 }}
                title="Live — interaction cards refresh automatically when they change"
              >
                <span
                  style={{ display: "inline-block", width: 6, height: 6, borderRadius: 3, background: "#1e7e34" }}
                />
                live
              </span>
            )}
          </div>

          {openInteractions.map((it) => (
            <div
              key={it.interaction_id}
              className="card"
              style={{ borderColor: "var(--warn, #b9770e)", padding: 10, marginBottom: 8 }}
            >
              <div className="row" style={{ gap: 6, alignItems: "baseline", flexWrap: "wrap" }}>
                <span className="badge in_progress" style={{ fontSize: 10 }}>{it.kind}</span>
                {it.bound_doc_kind === "plan" && it.bound_doc_id ? (
                  <span
                    className="badge todo"
                    style={{ fontSize: 9 }}
                    title={`Bound to plan Dossier ${it.bound_doc_id} — approving after the plan changes is refused as stale`}
                  >
                    bound to plan
                  </span>
                ) : null}
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
                <ol style={{ margin: "0 0 8px", paddingLeft: 18, fontSize: 12 }}>
                  {children.map((c, i) => {
                    // A valid `after` references an earlier sibling (0-based);
                    // surface the order so the operator sees the dependency
                    // before accepting (it materializes as a Snag on accept).
                    const dep =
                      typeof c.after === "number" && c.after >= 0 && c.after < children.length
                        ? children[c.after]
                        : null;
                    // The proposed assignee hint (§1.9): an Operative id
                    // (precise) or a role (resolved to the oldest active
                    // same-role Operative). Surfaced BEFORE accept so the
                    // operator sees who each child would be assigned to — it's
                    // still validated through the assign-Key gate on accept.
                    const hint = c.assignee_agent_id
                      ? { label: "→", value: c.assignee_agent_id, kind: "assignee" as const }
                      : c.assignee_role
                        ? { label: "→ role", value: c.assignee_role, kind: "role" as const }
                        : null;
                    return (
                      <li key={i} style={{ wordBreak: "break-word" }}>
                        {c.title}
                        {c.priority ? (
                          <span className="muted" style={{ fontSize: 10, marginLeft: 6 }}>
                            {c.priority}
                          </span>
                        ) : null}
                        {hint ? (
                          <span
                            className="badge in_progress"
                            style={{ fontSize: 9, marginLeft: 6 }}
                            title={
                              hint.kind === "role"
                                ? `Proposed assignee: the oldest active "${hint.value}" Operative (validated on accept)`
                                : `Proposed assignee: ${hint.value} (validated on accept)`
                            }
                          >
                            {hint.label} {hint.value}
                          </span>
                        ) : null}
                        {dep ? (
                          <span
                            className="muted"
                            style={{ fontSize: 10, marginLeft: 6 }}
                            title={`Blocked until #${(c.after as number) + 1} (${dep.title}) is done`}
                          >
                            ↳ after #{(c.after as number) + 1}: {dep.title}
                          </span>
                        ) : null}
                      </li>
                    );
                  })}
                </ol>
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
              {closedInteractions.map((it) => {
                // An accepted `suggest_tasks` card records its created child
                // ids in `response` (comma-joined, in proposal order). Surface
                // each as a deep-link to its board card so the operator can
                // assign it — materialized children open UNASSIGNED on purpose
                // (assignment is governance-gated, not inherited from the
                // parent), so they're inert until staffed. Titles come from the
                // proposal (same index order as the created ids).
                const createdIds =
                  it.kind === "suggest_tasks" && it.status === "resolved"
                    ? (it.response ?? "")
                        .split(",")
                        .map((s) => s.trim())
                        .filter(Boolean)
                    : [];
                const childTitles = it.proposal?.children ?? [];
                // A child that carried an assignee hint (§1.9) WAS assigned on
                // accept — resolution is all-or-nothing, so a resolved card
                // means every hinted child passed the gate and is staffed. The
                // rest opened unassigned (the default). Reflect both honestly.
                const childHint = (i: number): string | null => {
                  const c = childTitles[i];
                  if (!c) return null;
                  if (c.assignee_agent_id) return c.assignee_agent_id;
                  if (c.assignee_role) return `role: ${c.assignee_role}`;
                  return null;
                };
                const assignedN = createdIds.filter((_, i) => childHint(i)).length;
                const needN = createdIds.length - assignedN;
                return (
                  <div
                    key={it.interaction_id}
                    style={{ padding: "4px 0", borderBottom: "1px solid var(--border-soft)" }}
                  >
                    <div className="row" style={{ gap: 6, alignItems: "baseline", flexWrap: "wrap" }}>
                      <span
                        className={"badge " + (IX_STATUS_TONE[it.status] ?? "blocked")}
                        style={{ fontSize: 9 }}
                        title={
                          it.status === "expired"
                            ? "Expired — superseded by a newer plan revision or a comment before it was approved; it never resolved as approved"
                            : undefined
                        }
                      >
                        {it.status}
                      </span>
                      <span className="muted" style={{ fontSize: 11 }}>{it.kind}</span>
                      {it.bound_doc_kind === "plan" && it.bound_doc_id ? (
                        <span
                          className="badge todo"
                          style={{ fontSize: 9 }}
                          title={`Bound to plan Dossier ${it.bound_doc_id}`}
                        >
                          bound to plan
                        </span>
                      ) : null}
                      <span style={{ wordBreak: "break-word" }}>{it.prompt}</span>
                    </div>
                    {createdIds.length > 0 ? (
                      <div style={{ marginTop: 3 }}>
                        <div className="muted" style={{ fontSize: 11 }}>
                          {it.resolved_by ? `${it.resolved_by}: ` : ""}
                          {createdIds.length} Sub-brief{createdIds.length === 1 ? "" : "s"} created
                          {assignedN > 0 ? ` — ${assignedN} assigned` : ""}
                          {needN > 0
                            ? `${assignedN > 0 ? ", " : " — "}${needN} need${
                                needN === 1 ? "s" : ""
                              } an Operative (also in the Action Center as “Assign an Operative”).`
                            : "."}
                        </div>
                        <ul style={{ margin: "3px 0 0", paddingLeft: 16 }}>
                          {createdIds.map((cid, i) => {
                            const assignedTo = childHint(i);
                            return (
                              <li key={cid} style={{ wordBreak: "break-word", marginTop: 2 }}>
                                <Link
                                  to={`/briefs?brief=${encodeURIComponent(cid)}`}
                                  className="link"
                                  title={
                                    assignedTo
                                      ? `Assigned to ${assignedTo} — open this Sub-brief`
                                      : "Open this Sub-brief on the board to assign an Operative"
                                  }
                                >
                                  {childTitles[i]?.title ?? `Sub-brief ${i + 1}`}
                                </Link>{" "}
                                {assignedTo ? (
                                  <span
                                    className="badge done"
                                    style={{ fontSize: 9 }}
                                    title={`Assigned to ${assignedTo}`}
                                  >
                                    assigned: {assignedTo}
                                  </span>
                                ) : (
                                  <span className="badge blocked" style={{ fontSize: 9 }}>
                                    needs assignment
                                  </span>
                                )}
                              </li>
                            );
                          })}
                        </ul>
                      </div>
                    ) : (
                      (it.response || it.resolved_by) && (
                        <div className="muted" style={{ fontSize: 11, marginTop: 2 }}>
                          {it.resolved_by ? `${it.resolved_by}: ` : ""}
                          {it.response ?? ""}
                        </div>
                      )
                    )}
                  </div>
                );
              })}
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
