import { Fragment, useEffect, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { api, tryGet, subscribeRuns, type RunEvent, type RunsStreamConn } from "../api";
import { Empty, Section, useAsync } from "../components/common";
import { RunTranscript } from "../components/RunTranscript";
import { invalidate } from "../invalidate";

// Live runs-stream connection → a small honest status chip.
const LIVE_LABEL: Record<RunsStreamConn, string> = {
  connecting: "connecting…",
  live: "live",
  reconnecting: "reconnecting…",
  unavailable: "live updates off",
};
const LIVE_TONE: Record<RunsStreamConn, string> = {
  connecting: "todo",
  live: "done",
  reconnecting: "in_progress",
  unavailable: "blocked",
};

// Build the shareable deep link for one run (the SPA is mounted at
// /dashboard, so include the basename for a copy-paste-able URL).
function runDeepLink(runId: string): string {
  return `${window.location.origin}/dashboard/runs?run=${encodeURIComponent(runId)}`;
}

interface Adapter { name?: string; display_name?: string; probe?: { status?: string } }

// A durable run record from the `brief_runs` ledger (`/v1/runs`).
interface RunRecord {
  run_id?: string;
  brief_id?: string;
  agent_id?: string;
  rig?: string;
  status?: string;
  started_at?: number;
  finished_at?: number;
  duration_secs?: number;
  summary?: string;
  workspace?: string;
  workspace_context?: string;
  workspace_files?: number;
  workspace_bytes?: number;
  review?: string;
  review_note?: string;
  reviewed_at?: number;
  apply_status?: string;
  applied_at?: number;
  apply_note?: string;
  applied_files?: number;
  failed_files?: number;
  trigger?: string;
  /// When status === "refused": why the run never started.
  refusal_reason?: string;
  // Recovery DIAGNOSIS (execution-and-issue §3.3b) — stamped on terminal /
  // refused runs. Informs operator decisions; NOT an autonomous retry.
  failure_class?: string;
  retryable?: boolean;
  retry_budget_remaining?: number;
  recovery_action?: string;
  recovery_route?: string;
  // STAGE-2 guarded operator retry lineage: when this run is a retry CHILD,
  // the source failed run it was started from; its attempt number. Absent on a
  // fresh / non-retry run.
  retried_from_run_id?: string;
  retry_attempt?: number;
}

// A failed/interrupted Shift is retry-eligible from the durable diagnosis
// fields alone (terminal failure-like + retryable + budget). The duplicate
// guard is enforced server-side; the UI also hides the button once a child is
// surfaced in the runs list (see `retriedSources`).
function retryEligible(r: RunRecord): boolean {
  return (
    (r.status === "failed" || r.status === "interrupted") &&
    r.retryable === true &&
    (r.retry_budget_remaining ?? 0) > 0
  );
}

// One file in a safe-apply plan (`/v1/runs/:id/diff` → plan.items).
interface ApplyPlanItem {
  rel_path?: string;
  kind?: string;
  action?: string; // create / overwrite / delete / noop / refuse
  can_apply?: boolean;
  conflict?: boolean;
  reason?: string;
  source_size?: number;
  target_exists?: boolean;
}

interface ApplyPlan {
  project_root?: string;
  items?: ApplyPlanItem[];
  applicable?: boolean;
  changes?: number;
  conflicts?: number;
  blocked?: number;
  note?: string;
}

// Safe-apply preview (`/v1/runs/:id/diff`).
interface RunDiff {
  run_id?: string;
  status?: string;
  review?: string;
  apply_status?: string;
  eligible?: boolean;
  reason?: string;
  plan?: ApplyPlan;
}

// One changed file (`/v1/runs/:id/artifacts`).
interface RunArtifact {
  artifact_id?: number;
  rel_path?: string;
  kind?: string;
  size?: number;
  is_text?: boolean;
  hash?: string;
}

// Preview response (`/v1/runs/:id/artifacts/:aid/preview`).
interface ArtifactPreview {
  rel_path?: string;
  kind?: string;
  available?: boolean;
  truncated?: boolean;
  content?: string;
  reason?: string;
}

// Unified-diff response (`/v1/runs/:id/artifacts/:aid/diff`).
interface ArtifactDiff {
  rel_path?: string;
  kind?: string;
  available?: boolean;
  truncated?: boolean;
  baseline?: string; // "project_root" | "empty"
  diff?: string;
  reason?: string;
}

// Run-workspace storage summary (`/v1/maintenance/summary`) — just enough to
// warn when disk usage is high and point at the cleanup panel.
interface StorageSummary {
  workspace?: { count?: number; total_bytes?: number };
  warnings?: { level?: string; message?: string }[];
}

const ARTIFACT_TONE: Record<string, string> = {
  created: "done",
  modified: "todo",
  deleted: "blocked",
};

const APPLY_STATUS_TONE: Record<string, string> = {
  applied: "done",
  ready: "todo",
  conflicted: "blocked",
  failed: "blocked",
  blocked: "blocked",
  discarded: "blocked",
  not_applicable: "todo",
};

// An apply-plan item's badge tone: a refusal is red; a noop is neutral; a
// safe write/delete is green.
function applyActionTone(it: ApplyPlanItem): string {
  if (!it.can_apply) return "blocked";
  if (it.action === "noop") return "todo";
  return "done";
}

function fmtBytes(n?: number): string {
  if (!n) return "0 B";
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${Math.round(n / 1024)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

// Short label for the scoped per-run workspace: the leaf folder (the
// run_id segment), with the full path on hover. "inherited CWD" when a
// run executed without a scoped workspace (legacy / inherit mode).
function wsLabel(ws?: string): string {
  if (!ws) return "inherited CWD";
  const parts = ws.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? ws;
}

// Compact "empty" / "copy_repo · 12 files · 34 KB" context badge.
function ctxLabel(r: RunRecord): string {
  if (!r.workspace_context) return "—";
  if (r.workspace_context !== "copy_repo") return r.workspace_context;
  const files = r.workspace_files ?? 0;
  const kb = Math.round((r.workspace_bytes ?? 0) / 1024);
  return `copy_repo · ${files} files · ${kb} KB`;
}

// What triggered a run. `heartbeat` = autonomous timer dispatch; `manual` =
// an operator hit Run. Same ledger, same pipeline — only the source differs.
const TRIGGER_TONE: Record<string, string> = {
  manual: "todo",
  heartbeat: "in_progress",
  scheduled: "in_progress",
};
function triggerLabel(t?: string): string {
  if (!t || t === "unknown") return "—";
  if (t === "heartbeat") return "auto";
  return t;
}

// Recovery-diagnosis action key → plain-language operator guidance
// (execution-and-issue §3.3b). Mirrors the backend `recovery_action` keys.
function recoveryActionLabel(action?: string): string {
  switch (action) {
    case "assign_agent": return "Assign an Operative";
    case "configure_rig": return "Configure the Rig";
    case "raise_allowance": return "Raise the Allowance";
    case "review_runtime": return "Review runtime settings";
    case "retry_later": return "Retry the Shift later";
    case "inspect_run": return "Inspect the run";
    case "none": return "Inspect the run";
    default: return action ?? "Inspect the run";
  }
}

// Run status → badge tone. `running` is in-flight; the rest are terminal.
const TONE: Record<string, string> = {
  running: "in_progress",
  done: "done",
  failed: "blocked",
  cancelled: "blocked",
  refused: "blocked",
  interrupted: "blocked",
  continued: "todo",
};

function fmtDuration(r: RunRecord): string {
  if (r.status === "running") {
    const s = Math.max(0, Math.floor(Date.now() / 1000) - (r.started_at ?? 0));
    return `${s}s…`;
  }
  if (typeof r.duration_secs === "number") return `${r.duration_secs}s`;
  return "—";
}

const FILTERS = ["all", "running", "done", "failed", "refused", "interrupted", "cancelled", "continued"] as const;
const TRIGGERS = ["all", "manual", "heartbeat"] as const;

export function Runs() {
  const [filter, setFilter] = useState<(typeof FILTERS)[number]>("all");
  const [triggerFilter, setTriggerFilter] = useState<(typeof TRIGGERS)[number]>("all");
  // The expanded run is URL-driven (`/runs?run=<run_id>`), so a Brief card
  // can deep-link straight into a run, refresh preserves it, and
  // back/forward behave. `expanded` mirrors the param for render.
  const [searchParams, setSearchParams] = useSearchParams();
  // Start collapsed; the URL-sync effect below expands (and LOADS) the run
  // named in `?run=` on mount, so a deep link / refresh loads its data.
  const [expanded, setExpanded] = useState<string | null>(null);
  // The expanded run's transcript is rendered by the reusable <RunTranscript>
  // (block-grouped nice/raw, self-tailing). We keep the loaded events ONLY to
  // drive the "Changes" scan-failed banner; `txKey` force-refreshes the
  // transcript after a mutation (apply/cancel/discard) re-shapes the run.
  const [events, setEvents] = useState<RunEvent[]>([]);
  const [txKey, setTxKey] = useState(0);
  const [artifacts, setArtifacts] = useState<RunArtifact[]>([]);
  const [preview, setPreview] = useState<{ id: number; data: ArtifactPreview } | null>(null);
  // Per-file unified diff (workspace output vs baseline), mutually exclusive
  // with the inline preview.
  const [diffView, setDiffView] = useState<{ id: number; data: ArtifactDiff } | null>(null);
  const [diff, setDiff] = useState<RunDiff | null>(null);
  const [banner, setBanner] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  // Live runs-stream connection state (live / reconnecting / off).
  const [liveConn, setLiveConn] = useState<RunsStreamConn>("connecting");
  // The latest run snapshot pushed by `/v1/runs/stream`. When the stream is
  // connected this is the table's source of truth (it pushes the full ledger on
  // every change); otherwise the page falls back to the mount-load + Refresh.
  const [liveRuns, setLiveRuns] = useState<RunRecord[] | null>(null);

  const { data, loading, error, reload } = useAsync(async () => {
    const [runs, adapters, storage] = await Promise.all([
      tryGet<RunRecord[]>("/v1/runs", []),
      tryGet<Adapter[]>("/v1/adapters", []),
      tryGet<StorageSummary | null>("/v1/maintenance/summary", null),
    ]);
    return {
      runs: Array.isArray(runs) ? runs : [],
      adapters: Array.isArray(adapters) ? adapters : [],
      storage: storage ?? null,
    };
  }, []);

  async function loadArtifacts(runId: string) {
    const a = await tryGet<RunArtifact[]>(`/v1/runs/${encodeURIComponent(runId)}/artifacts`, []);
    setArtifacts(Array.isArray(a) ? a : []);
  }

  async function showPreview(runId: string, artifactId: number) {
    if (preview?.id === artifactId) {
      setPreview(null);
      return;
    }
    setDiffView(null);
    const data = await tryGet<ArtifactPreview>(
      `/v1/runs/${encodeURIComponent(runId)}/artifacts/${artifactId}/preview`,
      {},
    );
    setPreview({ id: artifactId, data: data ?? {} });
  }

  // Toggle the per-file unified diff (workspace output vs the run's baseline).
  async function showDiff(runId: string, artifactId: number) {
    if (diffView?.id === artifactId) {
      setDiffView(null);
      return;
    }
    setPreview(null);
    const data = await tryGet<ArtifactDiff>(
      `/v1/runs/${encodeURIComponent(runId)}/artifacts/${artifactId}/diff`,
      {},
    );
    setDiffView({ id: artifactId, data: data ?? {} });
  }

  // Discard a terminal run's output: rejects it + frees the workspace for the
  // normal storage cleanup. Does NOT delete files now.
  async function discard(runId: string) {
    setBanner(null);
    try {
      const r = await api.post<{ apply_status?: string }>(
        `/v1/runs/${encodeURIComponent(runId)}/discard`,
        {},
      );
      setBanner(`Run discarded (${r.apply_status ?? "discarded"}). Its workspace will be reclaimed by cleanup.`);
      reload();
      if (expanded === runId) {
        await loadDiff(runId);
        setTxKey((k) => k + 1);
      }
      // Discarding rejects the run — the board card + open Brief panel update (§11).
      invalidate(["briefs", "brief"], { briefId: data?.runs?.find((x) => x.run_id === runId)?.brief_id });
    } catch (e) {
      setBanner(e instanceof Error ? e.message : "Discard failed");
    }
  }

  async function loadDiff(runId: string) {
    const d = await tryGet<RunDiff | null>(`/v1/runs/${encodeURIComponent(runId)}/diff`, null);
    setDiff(d ?? null);
  }

  // Expand a run + load its transcript/changes/apply plan. Does NOT touch
  // the URL (the caller / effect owns that) so it can run on deep-link too.
  async function openRun(runId: string) {
    setExpanded(runId);
    setEvents([]);
    setArtifacts([]);
    setPreview(null);
    setDiffView(null);
    setDiff(null);
    setCopied(false);
    // The transcript self-loads on runId change; load the changes + apply plan.
    await Promise.all([loadArtifacts(runId), loadDiff(runId)]);
  }

  function setRunParam(runId: string | null) {
    const next = new URLSearchParams(searchParams);
    if (runId) next.set("run", runId);
    else next.delete("run");
    setSearchParams(next, { replace: true });
  }

  // A row click toggles the run AND mirrors it into the URL.
  function toggle(runId: string) {
    if (expanded === runId) {
      setExpanded(null);
      setRunParam(null);
    } else {
      void openRun(runId);
      setRunParam(runId);
    }
  }

  // Sync expansion FROM the URL — handles deep links, refresh, and
  // back/forward. Only acts on a genuine mismatch (no double-load on the
  // same click that set the param).
  const deepRun = searchParams.get("run");
  useEffect(() => {
    if (deepRun && deepRun !== expanded) {
      void openRun(deepRun);
    } else if (!deepRun && expanded) {
      setExpanded(null);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [deepRun]);

  // Live updates: subscribe ONCE to the runs snapshot stream. It pushes the full
  // recent-run ledger on the initial snapshot + every change (start / finish /
  // refuse / recover / review / apply / retry), fingerprint-gated server-side so
  // an unchanged ledger pushes nothing. We render the pushed snapshot directly
  // while the stream is `live` (no re-fetch); when it isn't, the page falls back
  // to the mount-load + manual Refresh. The expanded run's transcript self-tails
  // inside <RunTranscript>.
  useEffect(() => {
    const unsub = subscribeRuns(
      (arr) => setLiveRuns(arr as RunRecord[]),
      (state) => setLiveConn(state),
    );
    return () => unsub();
  }, []);

  async function copyRunLink(runId: string) {
    const url = runDeepLink(runId);
    try {
      await navigator.clipboard?.writeText(url);
      setCopied(true);
      setBanner(null);
    } catch {
      // Clipboard blocked — surface the URL so it can be copied manually.
      setBanner(url);
    }
  }

  async function apply(runId: string) {
    setBanner(null);
    try {
      const r = await api.post<{ apply_status?: string; applied_files?: number; failed_files?: number; brief_status?: string }>(
        `/v1/runs/${encodeURIComponent(runId)}/apply`,
        {},
      );
      // `run.apply` is the operator's review-to-done: when the Brief advances to
      // `done` (company-model §12.5B) say so, so the loop's close is visible.
      setBanner(
        `Apply ${r.apply_status ?? "done"}: ${r.applied_files ?? 0} applied, ${r.failed_files ?? 0} failed` +
          (r.brief_status === "done" ? " — Brief marked done." : "."),
      );
      reload();
      await loadDiff(runId);
      setTxKey((k) => k + 1);
      // Apply can advance the Brief to done — refresh the board card + the open
      // Brief panel on surfaces that show them (dashboard-design §11).
      invalidate(["briefs", "brief"], { briefId: data?.runs?.find((x) => x.run_id === runId)?.brief_id });
    } catch (e) {
      setBanner(e instanceof Error ? e.message : "Apply failed");
    }
  }

  async function review(runId: string, decision: "accepted" | "rejected") {
    setBanner(null);
    try {
      await api.post(`/v1/runs/${encodeURIComponent(runId)}/review`, { decision, note: "" });
      setBanner(`Run ${decision}.`);
      reload();
      // Review verdict shows on the board card + the open Brief panel (§11).
      invalidate(["briefs", "brief"], { briefId: data?.runs?.find((x) => x.run_id === runId)?.brief_id });
    } catch (e) {
      setBanner(e instanceof Error ? e.message : "Review failed");
    }
  }

  async function cancel(runId: string) {
    setBanner(null);
    try {
      const r = await api.post<{ active?: boolean; note?: string }>(
        `/v1/runs/${encodeURIComponent(runId)}/cancel`,
        {},
      );
      setBanner(r.active ? "Cancellation signalled — the run will report cancelled." : `Cancel requested: ${r.note ?? "no live process"}`);
      reload();
      if (expanded === runId) setTxKey((k) => k + 1);
      invalidate(["briefs", "brief"], { briefId: data?.runs?.find((x) => x.run_id === runId)?.brief_id });
    } catch (e) {
      setBanner(e instanceof Error ? e.message : "Cancel failed");
    }
  }

  // Guarded operator retry of a failed/interrupted Shift (execution-and-issue
  // §3.3b). On success the backend opens exactly one child run and we navigate
  // to it; an already-retried source returns its existing child (200); any
  // refusal (not retryable / no budget / claim conflict) surfaces honestly —
  // we never hide the failure.
  async function retry(runId: string) {
    setBanner(null);
    try {
      const r = await api.post<{ status?: string; run_id?: string; retry_attempt?: number }>(
        `/v1/runs/${encodeURIComponent(runId)}/retry`,
        {},
      );
      setBanner(
        r.status === "already_retried"
          ? `This Shift was already retried — child run ${r.run_id}.`
          : `Retry started — child run ${r.run_id}${r.retry_attempt ? ` (attempt ${r.retry_attempt})` : ""}.`,
      );
      reload();
      invalidate(["briefs", "brief"], { briefId: data?.runs?.find((x) => x.run_id === runId)?.brief_id });
      // Follow the retry: open the child run (the URL-sync effect loads it).
      if (r.run_id) setRunParam(r.run_id);
    } catch (e) {
      setBanner(e instanceof Error ? e.message : "Retry failed");
    }
  }

  // While the snapshot stream is connected, the pushed ledger is the source of
  // truth (fresher than the mount-load); otherwise fall back to the fetched data
  // so the table still works with the stream absent (manual Refresh re-fetches).
  const allRuns = liveConn === "live" && liveRuns ? liveRuns : (data?.runs ?? []);
  // Surface retry lineage: map each source run id → its retry child (from the
  // child rows in the list), so a retried source hides its Retry button and
  // links to the child instead.
  const childOf = new Map<string, string>();
  for (const r of allRuns) {
    if (r.retried_from_run_id && r.run_id) childOf.set(r.retried_from_run_id, r.run_id);
  }
  const retriedSources = new Set(childOf.keys());
  // The expanded run is ALWAYS shown (so a deep link survives the filters);
  // everything else respects the status + trigger filters.
  const runs = allRuns.filter(
    (r) =>
      r.run_id === expanded ||
      ((filter === "all" || r.status === filter) &&
        (triggerFilter === "all" || (r.trigger ?? "manual") === triggerFilter)),
  );
  const deepNotFound = !loading && !!deepRun && !allRuns.some((r) => r.run_id === deepRun);
  const adaptersAvail = (data?.adapters ?? []).filter((a) => a.probe?.status === "available");
  const activeCount = allRuns.filter((r) => r.status === "running").length;
  const autoCount = allRuns.filter((r) => r.trigger === "heartbeat").length;
  // Storage hygiene: surface the backend's own warn/error workspace warnings
  // (high disk / too many sandboxes) with a pointer to the cleanup panel.
  const ws = data?.storage?.workspace ?? null;
  const storageWarn = (data?.storage?.warnings ?? []).find(
    (w) => (w.message ?? "").toLowerCase().includes("workspace") && w.level !== "info",
  );
  const COLS = 11;

  return (
    <div className="grid">
      <Section
        title="Active runs"
        action={
          <div className="row wrap" style={{ gap: 8, alignItems: "center" }}>
            <span
              className={"badge " + LIVE_TONE[liveConn]}
              style={{ fontSize: 10 }}
              title="live runs stream (auto-refreshes this table)"
            >
              ● {LIVE_LABEL[liveConn]}
            </span>
            <button className="btn ghost sm" onClick={reload}>Refresh</button>
          </div>
        }
      >
        {error && <div className="banner err">{error}</div>}
        {banner && <div className="banner info">{banner}</div>}
        {deepNotFound && (
          <div className="banner info banner-action">
            <span>Run <span className="mono">{deepRun}</span> isn't in the recent list — it may be older or from another Guild.</span>
            <span className="banner-cta" onClick={() => setRunParam(null)}>Show all runs →</span>
          </div>
        )}
        <div className={"banner " + (adaptersAvail.length ? "ok" : "info")}>
          {adaptersAvail.length
            ? `${adaptersAvail.length} agent adapter(s) available: ${adaptersAvail.map((a) => a.name).join(", ")}.`
            : "No agent adapters installed — install a coding-agent CLI (Claude, Codex) to execute Briefs. See Settings."}
        </div>
        {activeCount > 0 && (
          <div className="banner info">{activeCount} run(s) in flight — click a run to follow its transcript; refresh to update.</div>
        )}
        {autoCount > 0 && (
          <div className="banner info">{autoCount} autonomous (heartbeat) run(s) — same ledger as manual runs; reviewable + applicable.</div>
        )}
        {storageWarn && (
          <div className="banner err banner-action">
            <span>
              ⚠ {storageWarn.message}
              {ws && ws.total_bytes != null && ` (${fmtBytes(ws.total_bytes)} across ${ws.count ?? 0} workspace(s))`}
            </span>
            <Link to="/settings" className="banner-cta">Open cleanup →</Link>
          </div>
        )}

        <div className="card">
          <div className="row wrap" style={{ marginBottom: 8, gap: 8 }}>
            <h3 style={{ margin: 0 }}>Execution runs</h3>
            <div className="spacer" style={{ flex: 1 }} />
            {/* Status + trigger filters wrap as two clusters so the row never
                overflows on narrow viewports (design §2/§12). */}
            <div className="filter-bar">
              <div className="btn-group" role="group" aria-label="Filter by run status">
                {FILTERS.map((f) => (
                  <button
                    key={f}
                    className={"btn sm " + (filter === f ? "" : "ghost")}
                    onClick={() => setFilter(f)}
                  >
                    {f}
                  </button>
                ))}
              </div>
              <div className="btn-group" role="group" aria-label="Filter by trigger source">
                {TRIGGERS.map((t) => (
                  <button
                    key={t}
                    className={"btn sm " + (triggerFilter === t ? "" : "ghost")}
                    onClick={() => setTriggerFilter(t)}
                    title="filter by trigger source"
                  >
                    {t === "heartbeat" ? "auto" : t}
                  </button>
                ))}
              </div>
            </div>
          </div>
          {loading ? (
            <div className="loading">Loading runs…</div>
          ) : runs.length === 0 ? (
            <Empty>
              {filter === "all"
                ? "No runs yet. Hit “Run” on a Brief to execute it through its adapter."
                : `No ${filter} runs.`}
            </Empty>
          ) : (
            <div className="table-scroll">
            <table className="table">
              <thead>
                <tr>
                  <th></th>
                  <th>Status</th>
                  <th>Trigger</th>
                  <th>Adapter</th>
                  <th>Brief</th>
                  <th>Operative</th>
                  <th>Workspace</th>
                  <th>Context</th>
                  <th>Result</th>
                  <th>Duration</th>
                  <th>Started</th>
                </tr>
              </thead>
              <tbody>
                {runs.map((r, i) => {
                  const rid = r.run_id ?? "";
                  const open = expanded === rid;
                  return (
                    <Fragment key={rid || i}>
                      <tr style={{ cursor: "pointer" }} onClick={() => rid && toggle(rid)}>
                        <td className="muted">{open ? "▾" : "▸"}</td>
                        <td>
                          <span className={"badge " + (TONE[r.status ?? ""] ?? "todo")}>{r.status ?? "—"}</span>
                          {r.status === "refused" && r.refusal_reason && (
                            <span className="badge blocked" style={{ fontSize: 9, marginLeft: 4 }} title="why the run didn't start">{r.refusal_reason}</span>
                          )}
                          {r.status !== "refused" && r.failure_class && (
                            <span className="badge blocked" style={{ fontSize: 9, marginLeft: 4 }} title="diagnosed failure class">{r.failure_class}</span>
                          )}
                          {r.status === "done" && r.review && r.review !== "pending_review" && (
                            <span className={"badge " + (r.review === "accepted" ? "done" : "blocked")} style={{ fontSize: 9, marginLeft: 4 }} title={"review: " + r.review}>{r.review === "accepted" ? "✓" : "✕"}</span>
                          )}
                        </td>
                        <td>
                          <span className={"badge " + (TRIGGER_TONE[r.trigger ?? ""] ?? "todo")} style={{ fontSize: 9 }} title={"trigger: " + (r.trigger ?? "unknown")}>
                            {triggerLabel(r.trigger)}
                          </span>
                        </td>
                        <td className="muted">{r.rig || "—"}</td>
                        <td className="mono">{(r.brief_id ?? "").slice(0, 12)}</td>
                        <td className="muted">{(r.agent_id ?? "").slice(0, 10) || "—"}</td>
                        <td className="mono" style={{ fontSize: 11 }} title={r.workspace ?? "ran in the coordinator working directory"}>{wsLabel(r.workspace)}</td>
                        <td className="muted" style={{ fontSize: 11 }}>{ctxLabel(r)}</td>
                        <td className="muted" style={{ maxWidth: 260, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{r.summary || (r.status === "running" ? "…" : "—")}</td>
                        <td className="muted">{fmtDuration(r)}</td>
                        <td className="muted">{r.started_at ? new Date(r.started_at * 1000).toLocaleTimeString() : ""}</td>
                      </tr>
                      {open && (
                        <tr>
                          <td colSpan={COLS} style={{ background: "var(--bg)" }}>
                            {/* Compact run header — the at-a-glance state. */}
                            <div className="run-head">
                              <span className={"badge " + (TONE[r.status ?? ""] ?? "todo")}>{r.status ?? "—"}</span>
                              <span className={"badge " + (TRIGGER_TONE[r.trigger ?? ""] ?? "todo")} style={{ fontSize: 9 }}>{triggerLabel(r.trigger)}</span>
                              {r.review && <span className={"badge " + (r.review === "accepted" ? "done" : r.review === "rejected" ? "blocked" : "in_progress")} style={{ fontSize: 9 }}>{r.review}</span>}
                              {r.apply_status && <span className={"badge " + (APPLY_STATUS_TONE[r.apply_status] ?? "todo")} style={{ fontSize: 9 }}>apply: {r.apply_status}</span>}
                              <span className="muted mono" style={{ fontSize: 11 }}>{rid}</span>
                              {r.retried_from_run_id && (
                                <Link to={`/runs?run=${encodeURIComponent(r.retried_from_run_id)}`} className="muted" style={{ fontSize: 11 }} title="this run is a retry of an earlier failed Shift" onClick={(e) => e.stopPropagation()}>retry of {r.retried_from_run_id.slice(0, 8)}{r.retry_attempt ? ` · attempt ${r.retry_attempt}` : ""} ↗</Link>
                              )}
                              {r.brief_id && <Link to="/briefs" className="link" style={{ fontSize: 11 }} onClick={(e) => e.stopPropagation()}>brief {r.brief_id.slice(0, 8)} ↗</Link>}
                              <div className="spacer" style={{ flex: 1 }} />
                              <button className="btn ghost sm" title="Copy a shareable link to this run" onClick={(e) => { e.stopPropagation(); copyRunLink(rid); }}>{copied ? "✓ copied" : "Copy link"}</button>
                              {r.status !== "running" && r.apply_status !== "discarded" && r.apply_status !== "applied" && (
                                <button className="btn ghost sm" title="Discard this run's output — rejects it and frees the workspace for cleanup" onClick={(e) => { e.stopPropagation(); discard(rid); }}>Discard</button>
                              )}
                              <Link to="/briefs" className="btn ghost sm" onClick={(e) => e.stopPropagation()}>← Briefs</Link>
                            </div>
                            {/* Recovery diagnosis — operational, not a retry engine.
                                Shows the failure class, retryable verdict + remaining
                                budget, and the recommended fix → route. */}
                            {(r.failure_class || r.recovery_action) &&
                              r.status !== "running" &&
                              r.status !== "done" &&
                              r.status !== "continued" && (
                                <div className="run-diag" onClick={(e) => e.stopPropagation()} style={{ display: "flex", flexWrap: "wrap", alignItems: "center", gap: 6, fontSize: 11, marginBottom: 8 }}>
                                  <strong style={{ fontSize: 11 }}>Recovery</strong>
                                  {r.failure_class && (
                                    <span className="badge blocked" style={{ fontSize: 9 }} title="diagnosed failure class">{r.failure_class}</span>
                                  )}
                                  {r.retryable !== undefined && (
                                    <span className={"badge " + (r.retryable ? "in_progress" : "todo")} style={{ fontSize: 9 }} title="whether a retry may help">
                                      {r.retryable ? "retryable" : "not retryable"}
                                    </span>
                                  )}
                                  {typeof r.retry_budget_remaining === "number" && r.retry_budget_remaining > 0 && (
                                    <span className="muted" title="operator-facing retry budget (not an auto-retry)">budget {r.retry_budget_remaining}</span>
                                  )}
                                  {r.recovery_action && (
                                    <span className="muted">→ {recoveryActionLabel(r.recovery_action)}</span>
                                  )}
                                  {r.recovery_route && r.recovery_action !== "inspect_run" && r.recovery_action !== "none" && (
                                    <Link to={r.recovery_route} className="btn ghost sm" onClick={(e) => e.stopPropagation()}>Fix setup ↗</Link>
                                  )}
                                  {/* One-click guarded retry — only for a retryable failed/interrupted
                                      Shift with budget that hasn't already been retried. The runtime
                                      re-checks every precondition and refuses if unsafe; this is NOT a
                                      blind auto-retry. */}
                                  {retryEligible(r) && !retriedSources.has(rid) && (
                                    <button className="btn sm" title="Open a fresh retry of this Shift through the same governed run path" onClick={(e) => { e.stopPropagation(); retry(rid); }}>Retry Shift</button>
                                  )}
                                  {retriedSources.has(rid) && (
                                    <Link to={`/runs?run=${encodeURIComponent(childOf.get(rid)!)}`} className="muted" style={{ fontSize: 11 }} onClick={(e) => e.stopPropagation()}>already retried → {childOf.get(rid)!.slice(0, 12)} ↗</Link>
                                  )}
                                </div>
                              )}
                            {r.status === "running" && (
                              <div className="row" style={{ marginBottom: 6 }}>
                                <div className="spacer" style={{ flex: 1 }} />
                                <button className="btn sm" onClick={(e) => { e.stopPropagation(); cancel(rid); }}>Cancel run</button>
                              </div>
                            )}
                            {r.workspace && <div className="muted mono" style={{ fontSize: 11, marginBottom: 6 }}>workspace: {r.workspace}</div>}
                            <div onClick={(e) => e.stopPropagation()}>
                              <RunTranscript runId={rid} status={r.status} refreshKey={txKey} onEvents={setEvents} />
                            </div>

                            {/* Changes / artifacts */}
                            <div className="row" style={{ marginTop: 12, marginBottom: 6 }}>
                              <strong style={{ fontSize: 12 }}>Changes</strong>
                              <span className="muted" style={{ fontSize: 11, marginLeft: 8 }}>{artifacts.length} file(s) the agent touched</span>
                            </div>
                            {events.some((e) => e.kind === "artifacts.scan_failed") && (
                              <div className="banner err" style={{ fontSize: 11 }}>Artifact scan failed — see the transcript above.</div>
                            )}
                            {artifacts.length === 0 ? (
                              <div className="muted" style={{ fontSize: 12 }}>
                                {r.workspace ? "No files changed in the run workspace." : "No scoped workspace — no change detection."}
                              </div>
                            ) : (
                              <div style={{ fontSize: 12 }}>
                                {artifacts.map((a, j) => (
                                  <div key={a.artifact_id ?? j} style={{ padding: "2px 0", borderBottom: "1px solid var(--border-soft)" }}>
                                    <span className={"badge " + (ARTIFACT_TONE[a.kind ?? ""] ?? "todo")} style={{ fontSize: 10 }}>{a.kind}</span>{" "}
                                    <span className="mono" style={{ fontSize: 11 }}>{a.rel_path}</span>{" "}
                                    <span className="muted" style={{ fontSize: 10 }}>{fmtBytes(a.size)}</span>
                                    {a.is_text && a.kind !== "deleted" && a.artifact_id != null && (
                                      <button className="btn ghost sm" style={{ marginLeft: 8, fontSize: 10, padding: "1px 6px" }} onClick={(e) => { e.stopPropagation(); showPreview(rid, a.artifact_id!); }}>
                                        {preview?.id === a.artifact_id ? "hide" : "preview"}
                                      </button>
                                    )}
                                    {a.is_text && a.artifact_id != null && (
                                      <button className="btn ghost sm" style={{ marginLeft: 4, fontSize: 10, padding: "1px 6px" }} title="unified diff of the run's change vs the baseline" onClick={(e) => { e.stopPropagation(); showDiff(rid, a.artifact_id!); }}>
                                        {diffView?.id === a.artifact_id ? "hide diff" : "diff"}
                                      </button>
                                    )}
                                    {preview && preview.id === a.artifact_id && (
                                      <pre style={{ margin: "4px 0 4px 14px", padding: 8, background: "var(--bg-elev)", maxHeight: 220, overflow: "auto", fontSize: 11, whiteSpace: "pre-wrap", wordBreak: "break-word" }}>
                                        {preview.data.available ? (preview.data.content || "(empty)") + (preview.data.truncated ? "\n…[truncated]" : "") : `(no preview: ${preview.data.reason ?? "unavailable"})`}
                                      </pre>
                                    )}
                                    {diffView && diffView.id === a.artifact_id && (
                                      diffView.data.available ? (
                                        <pre style={{ margin: "4px 0 4px 14px", padding: 8, background: "var(--bg-elev)", maxHeight: 260, overflow: "auto", fontSize: 11, whiteSpace: "pre-wrap", wordBreak: "break-word" }}>
                                          {(diffView.data.diff || "(no textual changes)") + (diffView.data.truncated ? "\n…[truncated]" : "")}
                                        </pre>
                                      ) : (
                                        <div className="banner info" style={{ margin: "4px 0 4px 14px", fontSize: 11 }}>
                                          Diff unavailable: {diffView.data.reason ?? "no baseline"} — use <em>preview</em> to see the file.
                                        </div>
                                      )
                                    )}
                                  </div>
                                ))}
                              </div>
                            )}

                            {/* Review */}
                            {r.status === "done" && (
                              <div className="row" style={{ marginTop: 12 }}>
                                <strong style={{ fontSize: 12 }}>Review</strong>
                                <span className={"badge " + (r.review === "accepted" ? "done" : r.review === "rejected" ? "blocked" : "todo")} style={{ fontSize: 10, marginLeft: 8 }}>
                                  {r.review ?? "pending_review"}
                                </span>
                                <div className="spacer" style={{ flex: 1 }} />
                                {r.review !== "accepted" && (
                                  <button className="btn sm" style={{ marginLeft: 6 }} onClick={(e) => { e.stopPropagation(); review(rid, "accepted"); }}>Accept</button>
                                )}
                                {r.review !== "rejected" && (
                                  <button className="btn ghost sm" style={{ marginLeft: 6 }} onClick={(e) => { e.stopPropagation(); review(rid, "rejected"); }}>Reject</button>
                                )}
                              </div>
                            )}

                            {/* Apply — copy an accepted run's changes into the project root */}
                            {r.status === "done" && r.review === "accepted" && (
                              <div style={{ marginTop: 12 }}>
                                <div className="row" style={{ marginBottom: 6 }}>
                                  <strong style={{ fontSize: 12 }}>Apply</strong>
                                  <span className={"badge " + (APPLY_STATUS_TONE[r.apply_status ?? ""] ?? "todo")} style={{ fontSize: 10, marginLeft: 8 }}>
                                    {r.apply_status ?? "not applied"}
                                  </span>
                                  {diff?.plan?.note && <span className="muted" style={{ fontSize: 11, marginLeft: 8 }}>{diff.plan.note}</span>}
                                  <div className="spacer" style={{ flex: 1 }} />
                                  <button className="btn ghost sm" onClick={(e) => { e.stopPropagation(); loadDiff(rid); }}>Refresh plan</button>
                                  {diff?.plan?.applicable && (diff.plan.changes ?? 0) > 0 && (
                                    <button className="btn sm" style={{ marginLeft: 6 }} onClick={(e) => { e.stopPropagation(); apply(rid); }}>
                                      Apply {diff.plan.changes} change(s)
                                    </button>
                                  )}
                                </div>
                                {diff?.plan?.project_root && (
                                  <div className="muted mono" style={{ fontSize: 11, marginBottom: 6 }}>→ {diff.plan.project_root}</div>
                                )}
                                {diff && diff.eligible === false && (
                                  <div className="banner info" style={{ fontSize: 11 }}>{diff.reason}</div>
                                )}
                                {diff?.plan && (diff.plan.items?.length ?? 0) === 0 ? (
                                  <div className="muted" style={{ fontSize: 12 }}>No artifacts — nothing to apply.</div>
                                ) : (
                                  <div style={{ fontSize: 12 }}>
                                    {(diff?.plan?.items ?? []).map((it, j) => (
                                      <div key={(it.rel_path ?? "") + j} style={{ padding: "2px 0", borderBottom: "1px solid var(--border-soft)" }}>
                                        <span className={"badge " + applyActionTone(it)} style={{ fontSize: 10 }}>{it.action}</span>{" "}
                                        <span className="mono" style={{ fontSize: 11 }}>{it.rel_path}</span>{" "}
                                        <span className="muted" style={{ fontSize: 10 }}>{it.reason}</span>
                                      </div>
                                    ))}
                                  </div>
                                )}
                                {diff?.plan && diff.plan.applicable === false && (diff.plan.items?.length ?? 0) > 0 && (
                                  <div className="banner err" style={{ fontSize: 11, marginTop: 6 }}>
                                    Refusing apply: {diff.plan.conflicts ?? 0} conflict(s), {diff.plan.blocked ?? 0} blocked. Resolve these before applying.
                                  </div>
                                )}
                              </div>
                            )}
                          </td>
                        </tr>
                      )}
                    </Fragment>
                  );
                })}
              </tbody>
            </table>
            </div>
          )}
        </div>
      </Section>
    </div>
  );
}
