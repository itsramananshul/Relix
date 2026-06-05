import { useState } from "react";
import { api, tryGet } from "../api";
import { useAsync } from "./common";

interface Warning { level?: string; message?: string }
interface Summary {
  workspace?: { root?: string; exists?: boolean; count?: number; total_bytes?: number; oldest?: number; newest?: number; truncated?: boolean };
  config?: { context?: string; project_root?: string; inherit?: boolean; heartbeat_enabled?: boolean };
  ledger?: {
    runs?: number; run_events?: number; run_artifacts?: number;
    running?: number; pending_review?: number; accepted?: number; rejected?: number; applied?: number;
    oldest_run?: number; newest_run?: number;
  };
  policy?: { default_older_than_days?: number; default_keep_latest?: number };
  warnings?: Warning[];
}
interface PruneItem { run_id?: string; bytes?: number; age_days?: number }
interface PruneReport {
  dry_run?: boolean;
  to_delete?: PruneItem[];
  to_delete_bytes?: number;
  kept_running?: number;
  kept_latest?: number;
  kept_recent?: number;
  deleted_workspaces?: number;
  deleted_bytes?: number;
  events_deleted?: number;
  artifacts_deleted?: number;
  errors?: string[];
}

function fmtBytes(n?: number): string {
  if (!n) return "0 B";
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${Math.round(n / 1024)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

export function MaintenancePanel() {
  const { data, loading, reload } = useAsync(
    async () => tryGet<Summary | null>("/v1/maintenance/summary", null),
    [],
  );
  const [olderThanDays, setOlderThanDays] = useState(7);
  const [keepLatest, setKeepLatest] = useState(10);
  const [deleteEvents, setDeleteEvents] = useState(false);
  const [deleteArtifacts, setDeleteArtifacts] = useState(false);
  const [report, setReport] = useState<PruneReport | null>(null);
  const [busy, setBusy] = useState(false);
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);
  const [confirm, setConfirm] = useState("");

  const s = data ?? undefined;
  const ws = s?.workspace ?? {};
  const ledger = s?.ledger ?? {};
  const warnings = s?.warnings ?? [];

  async function prune(dryRun: boolean) {
    setBusy(true);
    setBanner(null);
    try {
      const r = await api.post<PruneReport>("/v1/maintenance/prune", {
        dry_run: dryRun,
        older_than_days: olderThanDays,
        keep_latest: keepLatest,
        delete_workspaces: true,
        delete_events: deleteEvents,
        delete_artifacts: deleteArtifacts,
      });
      setReport(r);
      if (!dryRun) {
        setBanner({
          kind: "ok",
          msg: `Pruned ${r.deleted_workspaces ?? 0} workspace(s), reclaimed ${fmtBytes(r.deleted_bytes)}${(r.events_deleted ?? 0) + (r.artifacts_deleted ?? 0) > 0 ? ` · ${r.events_deleted ?? 0} event + ${r.artifacts_deleted ?? 0} artifact row(s) removed` : ""}.`,
        });
        setConfirm("");
        reload();
      }
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Prune failed" });
    } finally {
      setBusy(false);
    }
  }

  const canExecute = confirm.trim().toUpperCase() === "DELETE";

  return (
    <div className="card" style={{ gridColumn: "1 / -1" }}>
      <div className="row" style={{ marginBottom: 8 }}>
        <h3 style={{ margin: 0 }}>Maintenance &amp; storage</h3>
        <div className="spacer" style={{ flex: 1 }} />
        <button className="btn ghost sm" onClick={reload} disabled={loading}>{loading ? "…" : "Refresh"}</button>
      </div>
      <p className="muted" style={{ marginTop: -2, marginBottom: 12 }}>
        Cleanup deletes <strong>old run workspaces</strong> (the per-run sandboxes on disk) and, optionally,
        old transcript/artifact log rows. It never deletes your source repo, the project root, a running
        run, or the run ledger itself.
      </p>

      {banner && <div className={"banner " + banner.kind}>{banner.msg}</div>}
      {!loading && !s && (
        <div className="banner info">Maintenance summary is unavailable (the coordinator may be offline).</div>
      )}
      {warnings.map((w, i) => (
        <div key={i} className={"banner " + (w.level === "error" ? "err" : "info")} style={{ fontSize: 12 }}>{w.message}</div>
      ))}

      {s && (
        <>
          <div className="grid cols-4" style={{ marginBottom: 12 }}>
            <Metric label="Run workspaces" value={String(ws.count ?? 0)} sub={ws.truncated ? "≥ (capped scan)" : undefined} />
            <Metric label="Workspace storage" value={fmtBytes(ws.total_bytes)} />
            <Metric label="Runs" value={String(ledger.runs ?? 0)} sub={`${ledger.running ?? 0} running`} />
            <Metric label="Log rows" value={`${ledger.run_events ?? 0} / ${ledger.run_artifacts ?? 0}`} sub="events / artifacts" />
          </div>
          <div className="kv"><span className="muted">Workspace root</span><span className="mono" style={{ fontSize: 11 }}>{ws.root ?? "—"}{ws.exists === false ? " (none yet)" : ""}</span></div>
          <div className="kv">
            <span className="muted">Review state</span>
            <span style={{ fontSize: 12 }}>
              <span className="badge in_progress">{ledger.pending_review ?? 0} pending</span>{" "}
              <span className="badge done">{ledger.accepted ?? 0} accepted</span>{" "}
              <span className="badge blocked">{ledger.rejected ?? 0} rejected</span>{" "}
              <span className="badge done">{ledger.applied ?? 0} applied</span>
            </span>
          </div>

          {/* Cleanup controls */}
          <div style={{ marginTop: 14, borderTop: "1px solid var(--border-soft)", paddingTop: 12 }}>
            <div className="row wrap" style={{ gap: 12, alignItems: "flex-end" }}>
              <label className="field" style={{ margin: 0 }}>
                <span>Older than (days)</span>
                <input className="input" style={{ width: 110 }} type="number" min={0} value={olderThanDays} onChange={(e) => setOlderThanDays(Math.max(0, Number(e.target.value) || 0))} />
              </label>
              <label className="field" style={{ margin: 0 }}>
                <span>Keep latest N</span>
                <input className="input" style={{ width: 110 }} type="number" min={0} value={keepLatest} onChange={(e) => setKeepLatest(Math.max(0, Number(e.target.value) || 0))} />
              </label>
              <label className="row" style={{ gap: 6, fontSize: 12 }}><input type="checkbox" checked={deleteEvents} onChange={(e) => setDeleteEvents(e.target.checked)} /> also prune transcript rows</label>
              <label className="row" style={{ gap: 6, fontSize: 12 }}><input type="checkbox" checked={deleteArtifacts} onChange={(e) => setDeleteArtifacts(e.target.checked)} /> also prune artifact rows</label>
              <button className="btn ghost" disabled={busy} onClick={() => prune(true)}>{busy ? "…" : "Preview (dry-run)"}</button>
            </div>

            {report && (
              <div style={{ marginTop: 12 }}>
                <div className="row" style={{ marginBottom: 6 }}>
                  <strong style={{ fontSize: 12 }}>{report.dry_run ? "Would delete" : "Deleted"}</strong>
                  <span className="muted" style={{ fontSize: 12, marginLeft: 8 }}>
                    {(report.to_delete?.length ?? 0)} workspace(s) · {fmtBytes(report.to_delete_bytes)} ·
                    kept {report.kept_running ?? 0} running / {report.kept_latest ?? 0} latest / {report.kept_recent ?? 0} recent
                  </span>
                </div>
                {(report.to_delete?.length ?? 0) === 0 ? (
                  <div className="muted" style={{ fontSize: 12 }}>Nothing eligible — no old workspaces to prune.</div>
                ) : (
                  <div style={{ maxHeight: 180, overflow: "auto", fontSize: 12 }}>
                    {(report.to_delete ?? []).map((it, i) => (
                      <div key={it.run_id ?? i} style={{ padding: "2px 0", borderBottom: "1px solid rgba(0,0,0,0.05)" }}>
                        <span className="mono" style={{ fontSize: 11 }}>{it.run_id}</span>{" "}
                        <span className="muted" style={{ fontSize: 10 }}>{fmtBytes(it.bytes)} · {it.age_days}d old</span>
                      </div>
                    ))}
                  </div>
                )}
                {(report.errors?.length ?? 0) > 0 && (
                  <div className="banner err" style={{ fontSize: 11, marginTop: 6 }}>{report.errors!.join("; ")}</div>
                )}

                {report.dry_run && (report.to_delete?.length ?? 0) > 0 && (
                  <div className="row wrap" style={{ marginTop: 10, gap: 8 }}>
                    <span className="muted" style={{ fontSize: 12 }}>Type <strong>DELETE</strong> to confirm permanent removal:</span>
                    <input className="input" style={{ width: 140 }} value={confirm} onChange={(e) => setConfirm(e.target.value)} placeholder="DELETE" />
                    <button className="btn" disabled={busy || !canExecute} onClick={() => prune(false)}>Execute cleanup</button>
                  </div>
                )}
              </div>
            )}
          </div>

          <p className="muted" style={{ fontSize: 11, marginTop: 12 }}>
            Back up local state first with <span className="mono">scripts\relix-local-backup.ps1</span> (Windows) or{" "}
            <span className="mono">./scripts/relix-local-backup.sh</span>. For a consistent DB backup, stop the mesh first.
          </p>
        </>
      )}
    </div>
  );
}

function Metric({ label, value, sub }: { label: string; value: string; sub?: string }) {
  return (
    <div>
      <div style={{ fontSize: 22, fontWeight: 700 }}>{value}</div>
      <div className="stat-label">{label}</div>
      {sub && <div className="muted" style={{ fontSize: 11 }}>{sub}</div>}
    </div>
  );
}
