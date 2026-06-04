import { Fragment, useState } from "react";
import { api, tryGet } from "../api";
import { Empty, Section, useAsync } from "../components/common";

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

// One transcript event (`/v1/runs/:id/events`).
interface RunEvent {
  event_id?: number;
  ts?: number;
  kind?: string;
  source?: string;
  message?: string;
  payload_json?: string;
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

// Run status → badge tone. `running` is in-flight; the rest are terminal.
const TONE: Record<string, string> = {
  running: "in_progress",
  done: "done",
  failed: "blocked",
  cancelled: "blocked",
  continued: "todo",
};

// Transcript event kind → a small color cue (no theme change, just dots).
const EVENT_TONE: Record<string, string> = {
  error: "#c0392b",
  permission_denied: "#c0392b",
  failed: "#c0392b",
  cancelled: "#c0392b",
  cancel_requested: "#b9770e",
  result: "#1e7e34",
  assistant_message: "#2d6cdf",
  tool_use: "#7d3cc0",
  command: "#7d3cc0",
  file_change: "#7d3cc0",
};

function fmtDuration(r: RunRecord): string {
  if (r.status === "running") {
    const s = Math.max(0, Math.floor(Date.now() / 1000) - (r.started_at ?? 0));
    return `${s}s…`;
  }
  if (typeof r.duration_secs === "number") return `${r.duration_secs}s`;
  return "—";
}

const FILTERS = ["all", "running", "done", "failed", "cancelled", "continued"] as const;
const TRIGGERS = ["all", "manual", "heartbeat"] as const;

export function Runs() {
  const [filter, setFilter] = useState<(typeof FILTERS)[number]>("all");
  const [triggerFilter, setTriggerFilter] = useState<(typeof TRIGGERS)[number]>("all");
  const [expanded, setExpanded] = useState<string | null>(null);
  const [events, setEvents] = useState<RunEvent[]>([]);
  const [eventsLoading, setEventsLoading] = useState(false);
  const [artifacts, setArtifacts] = useState<RunArtifact[]>([]);
  const [preview, setPreview] = useState<{ id: number; data: ArtifactPreview } | null>(null);
  const [diff, setDiff] = useState<RunDiff | null>(null);
  const [banner, setBanner] = useState<string | null>(null);

  const { data, loading, error, reload } = useAsync(async () => {
    const [runs, adapters] = await Promise.all([
      tryGet<RunRecord[]>("/v1/runs", []),
      tryGet<Adapter[]>("/v1/adapters", []),
    ]);
    return {
      runs: Array.isArray(runs) ? runs : [],
      adapters: Array.isArray(adapters) ? adapters : [],
    };
  }, []);

  async function loadEvents(runId: string) {
    setEventsLoading(true);
    try {
      const ev = await tryGet<RunEvent[]>(`/v1/runs/${encodeURIComponent(runId)}/events`, []);
      setEvents(Array.isArray(ev) ? ev : []);
    } finally {
      setEventsLoading(false);
    }
  }

  async function loadArtifacts(runId: string) {
    const a = await tryGet<RunArtifact[]>(`/v1/runs/${encodeURIComponent(runId)}/artifacts`, []);
    setArtifacts(Array.isArray(a) ? a : []);
  }

  async function showPreview(runId: string, artifactId: number) {
    if (preview?.id === artifactId) {
      setPreview(null);
      return;
    }
    const data = await tryGet<ArtifactPreview>(
      `/v1/runs/${encodeURIComponent(runId)}/artifacts/${artifactId}/preview`,
      {},
    );
    setPreview({ id: artifactId, data: data ?? {} });
  }

  async function loadDiff(runId: string) {
    const d = await tryGet<RunDiff | null>(`/v1/runs/${encodeURIComponent(runId)}/diff`, null);
    setDiff(d ?? null);
  }

  async function toggle(runId: string) {
    if (expanded === runId) {
      setExpanded(null);
      return;
    }
    setExpanded(runId);
    setEvents([]);
    setArtifacts([]);
    setPreview(null);
    setDiff(null);
    await Promise.all([loadEvents(runId), loadArtifacts(runId), loadDiff(runId)]);
  }

  async function apply(runId: string) {
    setBanner(null);
    try {
      const r = await api.post<{ apply_status?: string; applied_files?: number; failed_files?: number }>(
        `/v1/runs/${encodeURIComponent(runId)}/apply`,
        {},
      );
      setBanner(`Apply ${r.apply_status ?? "done"}: ${r.applied_files ?? 0} applied, ${r.failed_files ?? 0} failed.`);
      reload();
      await Promise.all([loadDiff(runId), loadEvents(runId)]);
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
      if (expanded === runId) await loadEvents(runId);
    } catch (e) {
      setBanner(e instanceof Error ? e.message : "Cancel failed");
    }
  }

  const allRuns = data?.runs ?? [];
  const runs = allRuns
    .filter((r) => filter === "all" || r.status === filter)
    .filter((r) => triggerFilter === "all" || (r.trigger ?? "manual") === triggerFilter);
  const adaptersAvail = (data?.adapters ?? []).filter((a) => a.probe?.status === "available");
  const activeCount = allRuns.filter((r) => r.status === "running").length;
  const autoCount = allRuns.filter((r) => r.trigger === "heartbeat").length;
  const COLS = 11;

  return (
    <div className="grid">
      <Section
        title="Active runs"
        action={<button className="btn ghost sm" onClick={reload}>Refresh</button>}
      >
        {error && <div className="banner err">{error}</div>}
        {banner && <div className="banner info">{banner}</div>}
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

        <div className="card">
          <div className="row" style={{ marginBottom: 8 }}>
            <h3 style={{ margin: 0 }}>Execution runs</h3>
            <div className="spacer" style={{ flex: 1 }} />
            <div className="row" style={{ gap: 4 }}>
              {FILTERS.map((f) => (
                <button
                  key={f}
                  className={"btn sm " + (filter === f ? "" : "ghost")}
                  onClick={() => setFilter(f)}
                >
                  {f}
                </button>
              ))}
              <span className="muted" style={{ margin: "0 4px" }}>·</span>
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
                          <td colSpan={COLS} style={{ background: "rgba(0,0,0,0.02)" }}>
                            <div className="row" style={{ marginBottom: 6 }}>
                              <strong style={{ fontSize: 12 }}>Transcript</strong>
                              <span className="muted mono" style={{ fontSize: 11, marginLeft: 8 }}>{rid}</span>
                              <div className="spacer" style={{ flex: 1 }} />
                              <button className="btn ghost sm" onClick={(e) => { e.stopPropagation(); loadEvents(rid); }}>Refresh</button>
                              {r.status === "running" && (
                                <button className="btn sm" style={{ marginLeft: 6 }} onClick={(e) => { e.stopPropagation(); cancel(rid); }}>Cancel run</button>
                              )}
                            </div>
                            {r.workspace && <div className="muted mono" style={{ fontSize: 11, marginBottom: 6 }}>workspace: {r.workspace}</div>}
                            {eventsLoading ? (
                              <div className="loading">Loading transcript…</div>
                            ) : events.length === 0 ? (
                              <div className="muted" style={{ fontSize: 12 }}>No transcript events recorded.</div>
                            ) : (
                              <div style={{ maxHeight: 320, overflow: "auto", fontSize: 12 }}>
                                {events.map((ev, j) => (
                                  <div key={ev.event_id ?? j} style={{ padding: "2px 0", borderBottom: "1px solid rgba(0,0,0,0.05)" }}>
                                    <span style={{ display: "inline-block", width: 8, height: 8, borderRadius: 4, marginRight: 6, background: EVENT_TONE[ev.kind ?? ""] ?? "#999" }} />
                                    <span className="muted" style={{ fontSize: 10 }}>{ev.ts ? new Date(ev.ts * 1000).toLocaleTimeString() : ""}</span>{" "}
                                    <span className="mono" style={{ fontSize: 11 }}>{ev.source}/{ev.kind}</span>{" — "}
                                    <span style={{ whiteSpace: "pre-wrap", wordBreak: "break-word" }}>{ev.message}</span>
                                    {ev.payload_json && <div className="muted mono" style={{ fontSize: 10, paddingLeft: 14, whiteSpace: "pre-wrap", wordBreak: "break-word" }}>{ev.payload_json}</div>}
                                  </div>
                                ))}
                              </div>
                            )}

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
                                  <div key={a.artifact_id ?? j} style={{ padding: "2px 0", borderBottom: "1px solid rgba(0,0,0,0.05)" }}>
                                    <span className={"badge " + (ARTIFACT_TONE[a.kind ?? ""] ?? "todo")} style={{ fontSize: 10 }}>{a.kind}</span>{" "}
                                    <span className="mono" style={{ fontSize: 11 }}>{a.rel_path}</span>{" "}
                                    <span className="muted" style={{ fontSize: 10 }}>{fmtBytes(a.size)}</span>
                                    {a.is_text && a.kind !== "deleted" && a.artifact_id != null && (
                                      <button className="btn ghost sm" style={{ marginLeft: 8, fontSize: 10, padding: "1px 6px" }} onClick={(e) => { e.stopPropagation(); showPreview(rid, a.artifact_id!); }}>
                                        {preview?.id === a.artifact_id ? "hide" : "preview"}
                                      </button>
                                    )}
                                    {preview && preview.id === a.artifact_id && (
                                      <pre style={{ margin: "4px 0 4px 14px", padding: 8, background: "rgba(0,0,0,0.04)", maxHeight: 220, overflow: "auto", fontSize: 11, whiteSpace: "pre-wrap", wordBreak: "break-word" }}>
                                        {preview.data.available ? (preview.data.content || "(empty)") + (preview.data.truncated ? "\n…[truncated]" : "") : `(no preview: ${preview.data.reason ?? "unavailable"})`}
                                      </pre>
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
                                      <div key={(it.rel_path ?? "") + j} style={{ padding: "2px 0", borderBottom: "1px solid rgba(0,0,0,0.05)" }}>
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
