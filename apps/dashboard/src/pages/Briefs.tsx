import { Fragment, useEffect, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { api, tryGet, tryGetReport } from "../api";
import { asArray, extractList, Section, useAsync } from "../components/common";
import { BriefDetail } from "../components/BriefDetail";
import { invalidate, useInvalidate } from "../invalidate";

interface Card {
  task_id?: string;
  id?: string;
  title?: string;
  board_status?: string;
  priority?: string;
  assignee_agent_id?: string | null;
  mandate_id?: string | null;
  // The Brief's UNRESOLVED blockers (Snags whose blocker isn't `done`), as
  // their human-ref where set else id — same-Guild only, served by the board
  // route so the card can show a "Blocked by X" chip without opening the
  // detail (relix-dashboard-design §6). Absent/empty when nothing blocks it.
  blocked_by?: string[];
}

// The bounded slice of a Brief's full detail (`GET /v1/spine/briefs/:id`) the
// Plan view needs to render the goal-facing workflow checklist (dashboard-design
// §6/§7). Only RELATION + state fields are read here — board status / priority /
// assignee come from the already-loaded board card, latest-run from `/v1/runs`,
// so the Plan view adds no per-card run fetch. `parents`/`subbriefs` are
// same-Guild relation ids the server already tenant-filters.
interface PlanDetail {
  parents?: string[];
  subbriefs?: string[];
  blocking?: string[];
  snags?: string[];
  blocked?: boolean;
  delegation_depth?: number;
}
// One cached Plan-detail fetch outcome (mirrors Agents.tsx's detail-cache +
// in-flight-guard pattern): a present entry — even with `detail:null` + an
// `error` — means "loaded / attempted", so the bounded loader never re-fetches.
interface PlanEntry {
  detail: PlanDetail | null;
  error: string | null;
}

interface Operative {
  agent_id?: string;
  name?: string;
  role?: string;
  rig?: string | null;
}

interface Adapter {
  name?: string;
  probe?: { status?: string };
}

// One run record from the shared ledger (`/v1/runs`).
interface RunRow {
  run_id?: string;
  brief_id?: string;
  status?: string;
  trigger?: string;
  rig?: string;
  started_at?: number;
  review?: string;
  apply_status?: string;
  applied_files?: number;
}

interface RunReport {
  brief_id: string;
  status: string;
  rig: string;
  summary: string;
  install_hint?: string | null;
  run_id?: string | null;
  workspace?: string | null;
  workspace_context?: string | null;
  workspace_files?: number | null;
}

const REFUSALS: Record<string, string> = {
  running: "run started — executing in the background",
  unassigned: "assign an Operative first",
  no_adapter: "no adapter configured for this Operative",
  adapter_unavailable: "adapter not installed",
  already_running: "already running",
  not_found: "brief not found",
  workspace_error: "could not prepare a run workspace",
  workspace_context_error: "could not copy project context into the workspace",
  done: "run complete",
  failed: "run failed",
  continued: "run continued (more work to do)",
};

// Plan view bounds its best-effort detail fetches to the first N visible cards
// (dashboard-design §6: the list owns the brains, but stays honest about cost) —
// beyond this the rows still render flat from the board, with a capped note.
const PLAN_DETAIL_CAP = 80;

const COLUMNS = ["backlog", "todo", "in_progress", "in_review", "done"];
const COLUMN_LABEL: Record<string, string> = {
  backlog: "Backlog",
  todo: "To do",
  in_progress: "In progress",
  in_review: "In review",
  done: "Done",
};
const RUN_TONE: Record<string, string> = {
  running: "in_progress",
  done: "done",
  failed: "blocked",
  cancelled: "blocked",
  refused: "blocked",
  interrupted: "blocked",
  continued: "todo",
};

function cardId(c: Card): string {
  return c.task_id ?? c.id ?? "";
}

// The product state a Brief's latest run is in — drives the small status
// chip + the "what next" hint on the card.
function runOutcome(r: RunRow): { label: string; tone: string } | null {
  if (r.apply_status === "applied") return { label: "applied", tone: "done" };
  if (r.apply_status === "conflicted") return { label: "apply conflicted", tone: "blocked" };
  if (r.apply_status === "failed") return { label: "apply failed", tone: "blocked" };
  if (r.status === "done" && r.review === "pending_review") return { label: "needs review", tone: "in_progress" };
  if (r.status === "done" && r.review === "accepted") return { label: "ready to apply", tone: "todo" };
  if (r.status === "done" && r.review === "rejected") return { label: "rejected", tone: "blocked" };
  if (r.status === "failed") return { label: "failed", tone: "blocked" };
  if (r.status === "running") return { label: "running", tone: "in_progress" };
  return null;
}

// The clearest verb for the deep link into a Brief's latest run, based on
// what the operator would do next there.
function runAction(r: RunRow): string {
  if (r.status === "done" && r.review === "pending_review") return "Review run";
  if (r.status === "done" && r.review === "accepted" && r.apply_status !== "applied") return "Apply run";
  return "View run";
}

export function Briefs() {
  const [creating, setCreating] = useState(false);
  const [title, setTitle] = useState("");
  const [priority, setPriority] = useState("normal");
  const [mandateFilter, setMandateFilter] = useState("all");
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);
  // Drag/drop board movement (desktop): the card being dragged + its source
  // column, and the column currently hovered as a drop target. The select +
  // buttons below remain the keyboard/mobile fallback. A drop reuses the same
  // real `brief.move` route — no optimistic mutation, so a backend gate refusal
  // simply leaves the card where it is and we surface the reason.
  const [drag, setDrag] = useState<{ card: Card; from: string } | null>(null);
  const [overCol, setOverCol] = useState<string | null>(null);
  // A transient note pinned right above the board for the LAST move result
  // (success or refusal) — distinct from the section banner, so a refused drop
  // reports next to the columns where the drop happened.
  const [moveNote, setMoveNote] = useState<{ kind: string; msg: string } | null>(null);
  // The open Brief detail/Chronicle panel is URL-driven (`/briefs?brief=<id>`),
  // so the Action Center's ready/blocked/stale cards (and any shared deep link)
  // land on the exact Brief — selected, highlighted, and scrolled into view —
  // mirroring the Runs page's `?run=` pattern. Writing the param preserves any
  // other query params already present.
  const [searchParams, setSearchParams] = useSearchParams();
  const selected = searchParams.get("brief");
  function setSelected(id: string | null) {
    const next = new URLSearchParams(searchParams);
    if (id) next.set("brief", id);
    else next.delete("brief");
    setSearchParams(next, { replace: true });
  }
  // Scroll the deep-linked / selected card into view once the board has
  // rendered it. If the Brief is not in the loaded board (filtered out or
  // beyond the page), the ref stays null and we simply leave the board as-is —
  // the detail panel still opens (it fetches the Brief by id on its own).
  const selectedRef = useRef<HTMLDivElement | null>(null);

  // Board ↔ Plan view is URL-driven (`/briefs?view=plan`) so the goal-facing
  // checklist is shareable and survives refresh/back-forward, alongside the
  // existing `?brief=` selection + the mandate filter (dashboard-design §6).
  // Board is the default; an absent/unknown `view` falls back to it.
  const isPlan = searchParams.get("view") === "plan";
  function setView(plan: boolean) {
    const next = new URLSearchParams(searchParams);
    if (plan) next.set("view", "plan");
    else next.delete("view");
    setSearchParams(next, { replace: true });
  }
  // Plan-view detail cache + in-flight guard — the exact pattern Agents.tsx
  // uses: a present entry (even null/error) marks "loaded", and the in-flight
  // set stops a re-render from starting a duplicate fetch before the entry
  // lands. Persists across Board/Plan toggles so re-opening Plan is instant and
  // never re-fetches; relations are structural and change rarely (caveat: a
  // relation added mid-session shows on the next full board reload of new ids).
  const [planCache, setPlanCache] = useState<Record<string, PlanEntry>>({});
  const planInflight = useRef<Set<string>>(new Set());

  // Boundedly fetch one visible card's relation detail via the existing
  // `/v1/spine/briefs/:id` route. `tryGetReport` so a per-card failure is
  // recorded (not silently dropped) — the Plan view still renders that row flat
  // and raises ONE inline warning. Guarded by the cache + in-flight set.
  async function loadPlanDetail(id: string) {
    if (!id || id in planCache || planInflight.current.has(id)) return;
    planInflight.current.add(id);
    const r = await tryGetReport<PlanDetail>(
      `/v1/spine/briefs/${encodeURIComponent(id)}`,
      {},
    );
    setPlanCache((m) => ({ ...m, [id]: { detail: r.error ? null : r.data, error: r.error } }));
    planInflight.current.delete(id);
  }

  const { data, loading, error, reload } = useAsync(async () => {
    const byCol: Record<string, Card[]> = {};
    const [, ops, adapters, runs, mandates] = await Promise.all([
      Promise.all(
        COLUMNS.map(async (col) => {
          byCol[col] = asArray<Card>(await tryGet<Card[]>(`/v1/spine/board/${col}?limit=50`, []));
        }),
      ),
      tryGet<Operative[]>("/v1/spine/operatives", []),
      tryGet<Adapter[]>("/v1/adapters", []),
      tryGet<RunRow[]>("/v1/runs", []),
      tryGet<unknown>("/v1/spine/mandates?limit=50", {}),
    ]);
    return {
      board: byCol,
      operatives: Array.isArray(ops) ? ops : [],
      adapters: Array.isArray(adapters) ? adapters : [],
      runs: Array.isArray(runs) ? runs : [],
      mandates: extractList<{ mandate_id?: string; id?: string; title?: string }>(mandates, ["mandates"]),
    };
  }, []);

  // Client invalidation bus (dashboard-design §11): reload the board when the
  // CO-MOUNTED Brief detail panel reports a change that touches a Brief or its
  // Shift — a Run started/reviewed/applied, or a `suggest_tasks` accept that
  // materialized new Sub-briefs — so the columns + run badges stay in lockstep
  // with the panel beside them. The board's OWN mutations refetch locally and
  // emit the keys the OTHER surfaces (the open panel, Action Center) need.
  useInvalidate(["briefs"], reload);

  const operatives = data?.operatives ?? [];
  const adapters = data?.adapters ?? [];
  const runs = data?.runs ?? [];
  const mandates = data?.mandates ?? [];
  const mandateTitle = new Map(mandates.map((m) => [m.mandate_id ?? m.id ?? "", m.title ?? ""]));

  const opById = new Map(operatives.map((o) => [o.agent_id ?? "", o]));
  const adapterStatus = new Map(adapters.map((a) => [a.name ?? "", a.probe?.status ?? "unknown"]));
  const availCount = adapters.filter((a) => a.probe?.status === "available").length;
  // `/v1/runs` is newest-first → the FIRST run we see per Brief is its latest.
  const latestRun = new Map<string, RunRow>();
  for (const r of runs) {
    const b = r.brief_id ?? "";
    if (b && !latestRun.has(b)) latestRun.set(b, r);
  }

  // The single mandate-filter predicate, shared by the board columns and the
  // Plan view so both show exactly the same visible cards.
  const mandateMatch = (c: Card) =>
    mandateFilter === "all"
      ? true
      : mandateFilter === "none"
        ? !c.mandate_id
        : c.mandate_id === mandateFilter;

  // ── Plan-view model (dashboard-design §6 "workflow checklist") ────────────
  // A flat visible-card index built from ALL loaded board columns AFTER the
  // mandate filter, plus a parent→child forest assembled from the bounded Plan
  // detail cache. Computed every render (cheap: a few maps over the loaded
  // board) and reused by BOTH the Plan checklist and the small progress cue on
  // board cards — so a board card only shows a cue once the Plan cache exists.
  const allVisibleCards = COLUMNS.flatMap((col) => data?.board?.[col] ?? []).filter(mandateMatch);
  const visibleById = new Map<string, Card>();
  const orderIndex = new Map<string, number>();
  allVisibleCards.forEach((c, i) => {
    const id = cardId(c);
    if (id) {
      visibleById.set(id, c);
      orderIndex.set(id, i);
    }
  });
  // Edges, deduped, from BOTH directions in the cache (a card's `parents` and
  // its `subbriefs`) but only when BOTH ends are visible — so we never invent a
  // child outside the loaded window. A card with no loaded detail simply has no
  // edges and renders as a flat root (honest fallback).
  const childrenByParent = new Map<string, string[]>();
  const childIds = new Set<string>();
  const addEdge = (parent: string, child: string) => {
    if (!parent || !child || parent === child) return;
    const arr = childrenByParent.get(parent) ?? [];
    if (!arr.includes(child)) {
      arr.push(child);
      childrenByParent.set(parent, arr);
    }
    childIds.add(child);
  };
  for (const c of allVisibleCards) {
    const id = cardId(c);
    const pd = planCache[id]?.detail;
    if (!pd) continue;
    for (const p of pd.parents ?? []) if (visibleById.has(p)) addEdge(p, id);
    for (const s of pd.subbriefs ?? []) if (visibleById.has(s)) addEdge(id, s);
  }
  // Keep children in board order (backlog→done, then board position).
  for (const arr of childrenByParent.values()) {
    arr.sort((a, b) => (orderIndex.get(a) ?? 0) - (orderIndex.get(b) ?? 0));
  }
  const planRoots = allVisibleCards.filter((c) => !childIds.has(cardId(c)));

  // The workflow state of one card, for progress counts + the row chip. `done`
  // wins first (a done card isn't "blocked"); then a real blocker (board's
  // unresolved same-Guild blockers, or a loaded Snag); then live/in-flight; else
  // remaining (backlog/todo). Drawn only from REAL signals — never fabricated.
  const cardState = (c: Card): "done" | "running" | "blocked" | "remaining" => {
    const id = cardId(c);
    if (c.board_status === "done") return "done";
    if ((c.blocked_by?.length ?? 0) > 0 || planCache[id]?.detail?.blocked) return "blocked";
    if (
      latestRun.get(id)?.status === "running" ||
      c.board_status === "in_progress" ||
      c.board_status === "in_review"
    )
      return "running";
    return "remaining";
  };
  // The VISIBLE descendant ids of a card (its loaded subtree), cycle-guarded.
  const descendantIds = (id: string): string[] => {
    const out: string[] = [];
    const seen = new Set<string>();
    const stack = [...(childrenByParent.get(id) ?? [])];
    while (stack.length) {
      const x = stack.pop()!;
      if (seen.has(x)) continue;
      seen.add(x);
      out.push(x);
      for (const k of childrenByParent.get(x) ?? []) stack.push(k);
    }
    return out;
  };
  // Progress over a card's VISIBLE LOADED subtree (null when it has none) — the
  // honest "computed only from visible children" rule (dashboard-design §6).
  const progressOf = (id: string) => {
    const ds = descendantIds(id);
    if (ds.length === 0) return null;
    let done = 0,
      running = 0,
      blocked = 0,
      remaining = 0;
    for (const d of ds) {
      const c = visibleById.get(d);
      if (!c) continue;
      const s = cardState(c);
      if (s === "done") done++;
      else if (s === "running") running++;
      else if (s === "blocked") blocked++;
      else remaining++;
    }
    return { done, running, blocked, remaining, total: ds.length };
  };
  // Honest coverage of the Plan view's bounded detail load.
  const planTargets = allVisibleCards.slice(0, PLAN_DETAIL_CAP).map(cardId).filter(Boolean);
  const planCapped = allVisibleCards.length > PLAN_DETAIL_CAP;
  const planHadError = planTargets.some((id) => planCache[id]?.error);

  async function assign(c: Card, agentId: string) {
    setBanner(null);
    try {
      await api.post(`/v1/spine/briefs/${encodeURIComponent(cardId(c))}/set`, {
        field: "assignee",
        value: agentId,
      });
      setBanner({ kind: "ok", msg: agentId ? "Operative assigned." : "Operative cleared." });
      reload();
      // Mirror into the open detail panel + clear the "assign an Operative"
      // Action Center item (dashboard-design §11).
      invalidate(["brief", "actions"], { briefId: cardId(c) });
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Assign failed" });
    }
  }

  async function create() {
    if (!title.trim()) return;
    setBanner(null);
    try {
      await api.post("/v1/spine/briefs", { title: title.trim(), priority });
      setTitle("");
      setCreating(false);
      setBanner({ kind: "ok", msg: "Brief created." });
      reload();
      // A new unassigned Brief raises an Action Center item (dashboard-design §11).
      invalidate(["actions"]);
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Create failed" });
    }
  }

  // Move a Brief to a board column via the real `brief.move` route. Used by
  // both the select fallback and a drag/drop drop. We do NOT optimistically
  // re-place the card: the coordinator's state-machine guards (reviewer/
  // assignee/dependency gates, §1.3) can refuse, so we wait for the server and
  // only reload on success — a refused move leaves the card in place and shows
  // why, right above the board.
  async function move(c: Card, status: string) {
    const label = COLUMN_LABEL[status] ?? status;
    const name = c.title ?? "Brief";
    setMoveNote({ kind: "info", msg: `Moving “${name}” → ${label}…` });
    try {
      await api.post(`/v1/spine/briefs/${encodeURIComponent(cardId(c))}/move`, { status });
      setMoveNote({ kind: "ok", msg: `Moved “${name}” → ${label}.` });
      reload();
      // Mirror the new column into the open detail panel + refresh the Action
      // Center (a move can clear a blocked/review item) (dashboard-design §11).
      invalidate(["brief", "actions"], { briefId: cardId(c) });
    } catch (e) {
      const why = e instanceof Error ? e.message : "Move failed";
      setMoveNote({ kind: "err", msg: `Couldn't move “${name}” → ${label}: ${why}` });
    }
  }

  // Finish a drag: move the dragged card to the dropped column via the real
  // route. A same-column drop is a no-op; a missing drag is ignored.
  function handleDrop(targetCol: string) {
    const d = drag;
    setOverCol(null);
    setDrag(null);
    if (!d || d.from === targetCol) return;
    move(d.card, targetCol);
  }

  async function run(c: Card, rig?: string) {
    setBanner({ kind: "info", msg: `Running ${c.title ?? "brief"}${rig ? ` (${rig})` : ""}…` });
    try {
      const r = await api.post<RunReport>(
        `/v1/spine/briefs/${encodeURIComponent(cardId(c))}/run`,
        rig ? { rig } : {},
      );
      const accepted = r.status === "running" || r.status === "done";
      const refusal = ["unassigned", "no_adapter", "adapter_unavailable", "already_running", "not_found"].includes(r.status);
      const kind = accepted ? "ok" : refusal ? "info" : "err";
      const label = REFUSALS[r.status] ?? r.status;
      let msg = `${c.title ?? "Brief"}: ${label}`;
      if (r.rig) msg += ` · adapter ${r.rig}`;
      if (r.summary && r.status !== "running") msg += ` — ${r.summary}`;
      if (r.install_hint) msg += ` (${r.install_hint})`;
      if (r.status === "running") msg += " — see Active Runs";
      setBanner({ kind, msg });
      reload();
      // Mirror the started Shift into the open detail panel + the Runs ledger
      // (dashboard-design §11).
      invalidate(["brief", "runs"], { briefId: cardId(c) });
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Run failed" });
    }
  }

  // Why a Brief cannot run right now (null = it can). Used to disable the
  // Run button with a helpful reason rather than letting it silently refuse.
  function runBlock(c: Card): string | null {
    const op = c.assignee_agent_id ? opById.get(c.assignee_agent_id) : undefined;
    if (!c.assignee_agent_id) return "Assign an Operative first";
    if (!op?.rig) return "Operative has no adapter — set one on the Crew page";
    if (adapterStatus.get(op.rig) && adapterStatus.get(op.rig) !== "available")
      return `Adapter "${op.rig}" is not available — see Settings`;
    if (latestRun.get(cardId(c))?.status === "running") return "Already running";
    return null;
  }

  const initialized = operatives.length > 0;

  // While Plan view is active, lazily + boundedly fetch relation detail for the
  // first N visible cards (the cap), each guarded so nothing re-fetches. Runs on
  // entering Plan view, on a board reload (new ids), and on a mandate-filter
  // change (a different visible set). Re-fetches are no-ops (cache + in-flight).
  useEffect(() => {
    if (!isPlan) return;
    for (const id of planTargets) loadPlanDetail(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isPlan, data, mandateFilter]);

  // After the board renders (data load or reload), bring the selected card into
  // view. `block: "nearest"` avoids jumping when it is already visible.
  useEffect(() => {
    if (selected && selectedRef.current) {
      selectedRef.current.scrollIntoView({ behavior: "smooth", block: "nearest" });
    }
  }, [selected, data]);

  // Render one Plan row + its visible children, recursively (dashboard-design
  // §6 numbered workflow checklist). `number` is the dotted plan index (1, 1.1,
  // …); `depth` drives the indent. `seen` guards against a relation cycle and
  // stops a multi-parent child from rendering twice. Every chip is a REAL signal
  // from the loaded board card / run ledger / cached relations — never invented.
  function renderPlanRow(
    c: Card,
    number: string,
    depth: number,
    seen: Set<string>,
  ): JSX.Element | null {
    const id = cardId(c);
    if (seen.has(id)) return null;
    seen.add(id);
    const op = c.assignee_agent_id ? opById.get(c.assignee_agent_id) : undefined;
    const mTitle = c.mandate_id ? mandateTitle.get(c.mandate_id) || c.mandate_id.slice(0, 8) : null;
    const lr = latestRun.get(id);
    const prog = progressOf(id);
    const pd = planCache[id]?.detail;
    const bb = c.blocked_by ?? [];
    const snagN = pd?.snags?.length ?? 0;
    const hiddenChildren = (pd?.subbriefs ?? []).filter((s) => !visibleById.has(s)).length;
    const kids = (childrenByParent.get(id) ?? [])
      .map((k) => visibleById.get(k))
      .filter((x): x is Card => !!x);
    return (
      <Fragment key={id}>
        <div
          className={"plan-row" + (selected === id ? " selected" : "")}
          style={{ paddingLeft: 8 + Math.min(depth, 6) * 18 }}
          ref={selected === id ? selectedRef : undefined}
        >
          <span className="plan-num mono">{number}</span>
          <div className="plan-main">
            <div className="plan-title-row">
              <span
                className="plan-title"
                title="Open Brief detail + Chronicle"
                onClick={() => setSelected(selected === id ? null : id)}
              >
                {c.title ?? "(untitled)"}
              </span>
              {c.board_status && (
                <span className={"badge " + c.board_status} style={{ fontSize: 10 }}>
                  {COLUMN_LABEL[c.board_status] ?? c.board_status}
                </span>
              )}
              {c.priority && <span className="badge" style={{ fontSize: 10 }}>{c.priority}</span>}
              {op ? (
                <span className="muted" style={{ fontSize: 11 }} title={c.assignee_agent_id ?? ""}>
                  {op.name ?? "operative"}{op.role === "founder" ? " (Founder)" : ""}
                </span>
              ) : c.assignee_agent_id ? (
                <span className="muted mono" style={{ fontSize: 10 }}>{c.assignee_agent_id.slice(0, 8)}</span>
              ) : (
                <span className="muted" style={{ fontSize: 11 }}>unassigned</span>
              )}
              {mTitle && (
                <Link to="/mandates" className="muted" style={{ fontSize: 10 }} title={"part of mandate " + c.mandate_id}>
                  ◎ {mTitle}
                </Link>
              )}
              {(bb.length > 0 || snagN > 0) && (
                <span
                  className="badge blocked"
                  style={{ fontSize: 10, maxWidth: "100%", overflow: "hidden", textOverflow: "ellipsis" }}
                  title={
                    bb.length
                      ? `Blocked by ${bb.length} Brief${bb.length === 1 ? "" : "s"}: ${bb.join(", ")}`
                      : `${snagN} unresolved Snag${snagN === 1 ? "" : "s"} — open the Brief to see them`
                  }
                >
                  ⛔ {bb.length === 1
                    ? `Blocked by ${bb[0]}`
                    : bb.length > 1
                      ? `Blocked by ${bb.length}`
                      : `${snagN} snag${snagN === 1 ? "" : "s"}`}
                </span>
              )}
              {lr && (
                <span className="plan-run">
                  <span className={"badge " + (RUN_TONE[lr.status ?? ""] ?? "todo")} style={{ fontSize: 10 }}>
                    {lr.status ?? "—"}
                  </span>
                  {lr.run_id && (
                    <Link to={`/runs?run=${encodeURIComponent(lr.run_id)}`} className="link" style={{ fontSize: 11 }}>
                      run →
                    </Link>
                  )}
                </span>
              )}
            </div>
            {prog && (
              <div className="plan-progress">
                <span className="badge done" style={{ fontSize: 10 }}>{prog.done} done</span>
                {prog.running > 0 && <span className="badge in_progress" style={{ fontSize: 10 }}>{prog.running} running</span>}
                {prog.blocked > 0 && <span className="badge blocked" style={{ fontSize: 10 }}>{prog.blocked} blocked</span>}
                <span className="muted" style={{ fontSize: 11 }}>
                  {prog.remaining} remaining · {prog.total} visible child{prog.total === 1 ? "" : "ren"}
                </span>
              </div>
            )}
            {hiddenChildren > 0 && (
              <div className="muted" style={{ fontSize: 10 }}>
                + {hiddenChildren} child Brief{hiddenChildren === 1 ? "" : "s"} not in this view
              </div>
            )}
          </div>
        </div>
        {kids.map((k, i) => renderPlanRow(k, `${number}.${i + 1}`, depth + 1, seen))}
      </Fragment>
    );
  }

  return (
    <div className="grid">
      <Section
        title="Issue board"
        action={
          <div className="row" style={{ gap: 8 }}>
            {/* Board ↔ Plan toggle (dashboard-design §6). Board keeps the
                kanban; Plan reads the same cards as a goal-facing checklist. */}
            <div className="seg" role="tablist" aria-label="Briefs view">
              <button
                className={"seg-btn" + (!isPlan ? " active" : "")}
                role="tab"
                aria-selected={!isPlan}
                onClick={() => setView(false)}
                title="Kanban board — drag/drop, assign, run"
              >
                Board
              </button>
              <button
                className={"seg-btn" + (isPlan ? " active" : "")}
                role="tab"
                aria-selected={isPlan}
                onClick={() => setView(true)}
                title="Goal-facing plan — numbered workflow checklist with nesting + progress"
              >
                Plan
              </button>
            </div>
            {mandates.length > 0 && (
              <select className="select" style={{ width: 180, fontSize: 12 }} value={mandateFilter} onChange={(e) => setMandateFilter(e.target.value)} title="Filter by Mandate">
                <option value="all">All mandates</option>
                <option value="none">— no mandate —</option>
                {mandates.map((m) => (
                  <option key={m.mandate_id ?? m.id} value={m.mandate_id ?? m.id}>{m.title ?? (m.mandate_id ?? m.id ?? "").slice(0, 10)}</option>
                ))}
              </select>
            )}
            <button className="btn" onClick={() => setCreating((v) => !v)}>
              {creating ? "Cancel" : "+ New Brief"}
            </button>
          </div>
        }
      >
        {error && (
          <div className="banner err">Could not load the board: {error}. <span className="link" onClick={reload}>Retry</span></div>
        )}
        {banner && <div className={"banner " + banner.kind}>{banner.msg}</div>}

        {!loading && !initialized && (
          <div className="banner info banner-action">
            <span>No Operatives yet — create Briefs now, but to assign + run them you need a Founder.</span>
            <Link to="/agents" className="banner-cta">Initialize company →</Link>
          </div>
        )}
        {!loading && initialized && availCount === 0 && (
          <div className="banner info banner-action">
            <span>No agent adapter is available — Briefs can be assigned but a Run needs an installed + authenticated adapter (echo always works).</span>
            <Link to="/settings" className="banner-cta">Open Settings →</Link>
          </div>
        )}

        {creating && (
          <div className="card" style={{ marginBottom: 14 }}>
            <div className="row wrap">
              <input
                className="input"
                style={{ flex: 3, minWidth: 240 }}
                placeholder="Brief title — what needs doing?"
                value={title}
                autoFocus
                onChange={(e) => setTitle(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && create()}
              />
              <select className="select" style={{ flex: 1, minWidth: 120 }} value={priority} onChange={(e) => setPriority(e.target.value)}>
                <option value="low">low</option>
                <option value="normal">normal</option>
                <option value="high">high</option>
                <option value="urgent">urgent</option>
              </select>
              <button className="btn" onClick={create}>Create</button>
            </div>
          </div>
        )}

        {/* Split workspace (design §2/§7): the board on the left, the selected
            Brief's detail in a stable contextual panel on the right. The
            `split-*` classes apply ONLY while a Brief is open, so with nothing
            selected the board keeps full width and the panel slides away. */}
        <div className={selected ? "split-workspace" : undefined}>
          <div className={selected ? "split-main" : undefined}>
        {loading ? (
          <div className="loading">Loading board…</div>
        ) : COLUMNS.every((c) => (data?.board?.[c] ?? []).length === 0) ? (
          <div className="empty">
            No Briefs yet. Click <strong>+ New Brief</strong> to create your first unit of work,
            then assign it to an Operative and run it.
          </div>
        ) : isPlan ? (
          /* Plan view (dashboard-design §6) — the SAME visible board cards read
             as a goal-facing, numbered workflow checklist: nesting + progress
             from the bounded relation cache, run state from the run ledger, each
             row deep-linking the existing detail panel via `?brief=`. */
          <div className="plan-list">
            <div className="plan-caption">
              Visible workflow — numbered from the loaded board, after the mandate filter. Nesting
              + progress are computed only from the Briefs in view
              {planCapped
                ? `; relation detail loaded for the first ${PLAN_DETAIL_CAP} of ${allVisibleCards.length} cards (the rest render flat).`
                : "."}
            </div>
            {planHadError && (
              <div className="banner err" style={{ marginBottom: 10 }}>
                Some Brief details couldn't load — those rows render flat (no nesting/progress).
                The visible list itself is still complete.
              </div>
            )}
            {(() => {
              const seen = new Set<string>();
              return planRoots.map((r, i) => renderPlanRow(r, `${i + 1}`, 0, seen));
            })()}
          </div>
        ) : (
          <>
            {moveNote && (
              <div className={"banner " + moveNote.kind} style={{ display: "flex", alignItems: "center", gap: 12 }}>
                <span style={{ flex: 1 }}>{moveNote.msg}</span>
                <span className="link" style={{ whiteSpace: "nowrap" }} onClick={() => setMoveNote(null)}>dismiss ✕</span>
              </div>
            )}
            <div className="board">
            {COLUMNS.map((col) => {
              const cards = (data?.board?.[col] ?? []).filter(mandateMatch);
              // A column accepts a drop when a card from a DIFFERENT column is
              // being dragged. Same-column hover gets no affordance.
              const droppable = !!drag && drag.from !== col;
              return (
                <div
                  className={"board-col" + (droppable && overCol === col ? " drop-over" : "")}
                  key={col}
                  onDragOver={(e) => {
                    if (!droppable) return;
                    e.preventDefault();
                    e.dataTransfer.dropEffect = "move";
                    if (overCol !== col) setOverCol(col);
                  }}
                  onDrop={(e) => {
                    if (!drag) return;
                    e.preventDefault();
                    handleDrop(col);
                  }}
                >
                  <h4>
                    {COLUMN_LABEL[col]} <span className="muted">{cards.length}</span>
                  </h4>
                  {cards.map((c) => {
                    const op = c.assignee_agent_id ? opById.get(c.assignee_agent_id) : undefined;
                    const lr = latestRun.get(cardId(c));
                    const outcome = lr ? runOutcome(lr) : null;
                    const block = runBlock(c);
                    const mTitle = c.mandate_id ? (mandateTitle.get(c.mandate_id) || c.mandate_id.slice(0, 8)) : null;
                    return (
                      <div
                        className={
                          "board-card" +
                          (selected === cardId(c) ? " selected" : "") +
                          (drag?.card && cardId(drag.card) === cardId(c) ? " dragging" : "")
                        }
                        key={cardId(c)}
                        ref={selected === cardId(c) ? selectedRef : undefined}
                        draggable
                        aria-roledescription="Draggable Brief card — drag to another column to move it, or use the move control below."
                        onDragStart={(e) => {
                          // Don't hijack a drag that begins on an interactive
                          // control (assign/move selects, Run/echo buttons,
                          // links) — those stay clickable and are the keyboard/
                          // mobile fallback path.
                          const t = e.target as HTMLElement;
                          if (t.closest("select, button, a, input, textarea, label")) {
                            e.preventDefault();
                            return;
                          }
                          setMoveNote(null);
                          setDrag({ card: c, from: col });
                          e.dataTransfer.effectAllowed = "move";
                          e.dataTransfer.setData("text/plain", cardId(c));
                        }}
                        onDragEnd={() => {
                          setDrag(null);
                          setOverCol(null);
                        }}
                      >
                        <div
                          className="t"
                          style={{ cursor: "pointer" }}
                          title="Open Brief detail + Chronicle"
                          onClick={() => setSelected(selected === cardId(c) ? null : cardId(c))}
                        >
                          {c.title ?? "(untitled)"}
                        </div>
                        {mTitle && (
                          <Link to="/mandates" className="muted" style={{ fontSize: 10, display: "block", marginBottom: 4 }} title={"part of mandate " + c.mandate_id}>◎ {mTitle}</Link>
                        )}
                        <div className="m">
                          {c.priority && <span>{c.priority}</span>}
                          {op ? (
                            <span title={c.assignee_agent_id ?? ""}>
                              · {op.name ?? "operative"}
                              {op.role === "founder" ? " (Founder)" : ""}
                              {op.rig ? ` · ${op.rig}` : " · no adapter"}
                            </span>
                          ) : c.assignee_agent_id ? (
                            <span className="mono">· {c.assignee_agent_id.slice(0, 8)}</span>
                          ) : (
                            <span className="muted">· unassigned</span>
                          )}
                        </div>

                        {/* Blocked-by chip — a compact, REAL reason the card
                            can't advance, drawn straight from the board row's
                            unresolved same-Guild blockers (no faked ids). Shows
                            the single blocker's ref, else a count; the full list
                            is in the title + the Brief detail's Relations.
                            Omitted entirely when nothing blocks it, so clear
                            cards stay uncluttered (relix-dashboard-design §6). */}
                        {(c.blocked_by?.length ?? 0) > 0 && (
                          <div className="m" style={{ marginTop: 2 }}>
                            <span
                              className="badge blocked"
                              style={{ fontSize: 10, maxWidth: "100%", overflow: "hidden", textOverflow: "ellipsis" }}
                              title={`Blocked by ${c.blocked_by!.length} Brief${c.blocked_by!.length === 1 ? "" : "s"}: ${c.blocked_by!.join(", ")} — open the Brief to see its Snags`}
                            >
                              ⛔ {c.blocked_by!.length === 1
                                ? `Blocked by ${c.blocked_by![0]}`
                                : `Blocked by ${c.blocked_by!.length} Briefs`}
                            </span>
                          </div>
                        )}

                        {/* Sub-brief progress cue — shown ONLY when the Plan
                            view has already populated the relation cache (board
                            cards never fetch detail on their own), computed from
                            the same visible-subtree counts as the Plan view. */}
                        {(() => {
                          const p = progressOf(cardId(c));
                          return p ? (
                            <div className="plan-cue" title="Visible sub-brief progress (from the Plan view)">
                              ▣ {p.done}/{p.total} done{p.blocked > 0 ? ` · ${p.blocked} blocked` : ""}
                            </div>
                          ) : null;
                        })()}

                        {lr && (
                          <div className="card-run">
                            <span className={"badge " + (RUN_TONE[lr.status ?? ""] ?? "todo")}>{lr.status ?? "—"}</span>
                            <span className="muted" style={{ fontSize: 10 }}>{lr.trigger === "heartbeat" ? "auto" : lr.trigger ?? "manual"}</span>
                            {outcome && <span className={"badge " + outcome.tone} style={{ fontSize: 10 }}>{outcome.label}</span>}
                            {(lr.applied_files ?? 0) > 0 && <span className="muted" style={{ fontSize: 10 }}>{lr.applied_files} applied</span>}
                            {lr.run_id && (
                              <Link to={`/runs?run=${encodeURIComponent(lr.run_id)}`} className="link" style={{ fontSize: 11, marginLeft: "auto" }}>
                                {runAction(lr)} →
                              </Link>
                            )}
                          </div>
                        )}

                        <label className="row" style={{ marginTop: 8 }}>
                          <select
                            className="select"
                            style={{ fontSize: 11, padding: "3px 6px", width: "100%" }}
                            value={c.assignee_agent_id ?? ""}
                            onChange={(e) => assign(c, e.target.value)}
                            title="Assign an Operative"
                          >
                            <option value="">— unassigned —</option>
                            {operatives.map((o) => (
                              <option key={o.agent_id} value={o.agent_id}>
                                {o.name}{o.role === "founder" ? " (Founder)" : ""}{o.rig ? ` · ${o.rig}` : ""}
                              </option>
                            ))}
                          </select>
                        </label>
                        <div className="row" style={{ marginTop: 6, gap: 6, flexWrap: "wrap" }}>
                          <select
                            className="select"
                            style={{ fontSize: 11, padding: "3px 6px", flex: 1, minWidth: 110 }}
                            value={col}
                            onChange={(e) => move(c, e.target.value)}
                            title="Move to a board column (keyboard / touch fallback for drag-and-drop)"
                          >
                            {COLUMNS.map((s) => (
                              <option key={s} value={s}>→ {COLUMN_LABEL[s]}</option>
                            ))}
                          </select>
                          <button
                            className="btn sm"
                            disabled={!!block}
                            title={block ?? "Run this Brief through its Operative's adapter now"}
                            onClick={() => run(c)}
                          >
                            Run
                          </button>
                          {/* Golden-path smoke: echo always works once a Brief
                              is assigned — even if the real adapter is missing. */}
                          <button
                            className="btn ghost sm"
                            disabled={!c.assignee_agent_id || latestRun.get(cardId(c))?.status === "running"}
                            title={
                              !c.assignee_agent_id
                                ? "Assign an Operative first"
                                : latestRun.get(cardId(c))?.status === "running"
                                  ? "Already running"
                                  : "Run with the echo Rig (no real adapter needed) — verifies the pipeline end to end"
                            }
                            onClick={() => run(c, "echo")}
                          >
                            echo
                          </button>
                        </div>
                        {block && <div className="muted" style={{ fontSize: 10, marginTop: 4 }}>⚠ {block} — or hit <strong>echo</strong> to smoke the pipeline.</div>}
                      </div>
                    );
                  })}
                  {cards.length === 0 && <div className="muted" style={{ fontSize: 12, padding: 6 }}>empty</div>}
                </div>
              );
            })}
            </div>
          </>
        )}
          </div>
          {/* The contextual properties panel — real BriefDetail data
              (status / assignee / reviewer / relations / Latest Shift controls
              / Requests / Conversation / Chronicle). Deep-linked via `?brief=`;
              `.context-panel` is a layout wrapper, not a card, so the detail
              card isn't nested. On mobile it stacks below the board. */}
          {selected && (
            <div className="context-panel">
              <BriefDetail
                briefId={selected}
                onClose={() => setSelected(null)}
              />
            </div>
          )}
        </div>
      </Section>
    </div>
  );
}
