import { useState } from "react";
import { runtimeState, tryGet, type RuntimeStateRow } from "../api";
import { useAuth } from "../auth";
import { asArray, Empty, useAsync } from "../components/common";
import { MaintenancePanel } from "../components/MaintenancePanel";
import { HealthPanel } from "../components/HealthPanel";

interface Provider { name?: string; id?: string; configured?: boolean; enabled?: boolean; model?: string }
interface Adapter {
  name?: string;
  display_name?: string;
  governance?: string;
  billing?: { mode?: string; provider?: string };
  probe?: { status?: string; detail?: string; install_hint?: string | null };
}

const STATUS_LABEL: Record<string, string> = {
  available: "available",
  missing_binary: "not installed",
  not_authenticated: "needs login",
  unsupported_version: "version issue",
  interactive_only: "needs a TTY",
  probe_failed: "probe failed",
};

interface RunConfig {
  context?: string;
  project_root?: string;
  workspace_root?: string;
  max_bytes?: number;
  max_files?: number;
  inherit?: boolean;
  heartbeat_enabled?: boolean;
  heartbeat_interval_secs?: number;
  autonomous_recovery_enabled?: boolean;
  autonomous_recovery_max?: number;
}

function extractProviders(v: unknown): Provider[] {
  if (Array.isArray(v)) return v as Provider[];
  if (v && typeof v === "object") {
    const o = v as Record<string, unknown>;
    for (const k of ["providers", "items", "results"]) if (Array.isArray(o[k])) return o[k] as Provider[];
  }
  return [];
}

export function Settings() {
  const { status, logout } = useAuth();
  const { data, loading, reload } = useAsync(async () => {
    const [info, providers, adapters, runConfig] = await Promise.all([
      tryGet<Record<string, unknown>>("/v1/info", {}),
      tryGet<unknown>("/v1/config/providers", {}),
      tryGet<Adapter[]>("/v1/adapters", []),
      tryGet<RunConfig>("/v1/spine/run-config", {}),
    ]);
    return {
      info,
      providers: extractProviders(providers),
      adapters: Array.isArray(adapters) ? adapters : [],
      runConfig: runConfig ?? {},
    };
  }, []);

  const info = data?.info ?? {};
  const providers = data?.providers ?? [];
  const adapters = data?.adapters ?? [];
  const runConfig = data?.runConfig ?? {};

  return (
    <div className="grid">
      {/* Live diagnostics first — the fastest way to see what's wrong. */}
      <HealthPanel />
      <div className="grid cols-2">
      <MaintenancePanel />

      <div className="card">
        <h3>Account</h3>
        <div className="row" style={{ marginBottom: 10 }}>
          <div className="who avatar" style={{ width: 36, height: 36 }}>
            {(status?.username ?? "?").slice(0, 1).toUpperCase()}
          </div>
          <div>
            <div><strong>{status?.username ?? "operator"}</strong></div>
            <div className="muted">Bridge admin</div>
          </div>
        </div>
        <button className="btn ghost" onClick={() => void logout()}>Sign out</button>
      </div>

      <div className="card">
        <h3>Bridge</h3>
        {loading ? (
          <div className="loading">Loading…</div>
        ) : (
          <table className="table">
            <tbody>
              {Object.entries(info)
                .filter(([, v]) => typeof v !== "object")
                .slice(0, 10)
                .map(([k, v]) => (
                  <tr key={k}>
                    <td className="muted">{k}</td>
                    <td className="mono">{String(v)}</td>
                  </tr>
                ))}
              {Object.keys(info).length === 0 && (
                <tr><td className="muted">Bridge info unavailable.</td></tr>
              )}
            </tbody>
          </table>
        )}
      </div>

      <div className="card" style={{ gridColumn: "1 / -1" }}>
        <h3>AI providers</h3>
        {loading ? (
          <div className="loading">Loading providers…</div>
        ) : providers.length === 0 ? (
          <Empty>No providers configured on the AI node.</Empty>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Provider</th>
                <th>Model</th>
                <th>Status</th>
              </tr>
            </thead>
            <tbody>
              {asArray<Provider>(providers).map((p, i) => (
                <tr key={p.name ?? p.id ?? i}>
                  <td><strong>{p.name ?? p.id ?? "provider"}</strong></td>
                  <td className="muted">{p.model ?? "—"}</td>
                  <td>
                    <span className={"badge " + (p.configured || p.enabled ? "done" : "backlog")}>
                      {p.configured || p.enabled ? "configured" : "inactive"}
                    </span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      <div className="card" style={{ gridColumn: "1 / -1" }}>
        <h3>Run execution sandbox</h3>
        <p className="muted" style={{ marginTop: -6, marginBottom: 12 }}>
          Every Brief run executes in a dedicated scoped workspace, never in the coordinator/repo
          working directory (that stays explicit + opt-in only, for safety).
        </p>
        {runConfig.inherit && (
          <div className="banner err" style={{ fontSize: 12 }}>
            ⚠ INHERIT mode is active — runs execute in the coordinator working directory, NOT a
            scoped sandbox. An agent can touch real files. Unset <span className="mono">RELIX_RUN_WORKSPACE_MODE</span> to
            return to safe scoped workspaces (empty / copy_repo).
          </div>
        )}
        <table className="table">
          <tbody>
            <tr>
              <td className="muted">Context mode</td>
              <td>
                <span className={"badge " + (runConfig.context === "copy_repo" ? "todo" : "done")}>
                  {runConfig.context ?? "empty"}
                </span>
                <span className="muted" style={{ marginLeft: 8, fontSize: 12 }}>
                  {runConfig.context === "copy_repo"
                    ? "a capped, filtered project snapshot is copied into each run workspace"
                    : "workspaces start empty (only BRIEF.md) — the safest default"}
                </span>
              </td>
            </tr>
            <tr>
              <td className="muted">Workspace root</td>
              <td className="mono" style={{ fontSize: 12 }}>{runConfig.workspace_root ?? "—"}</td>
            </tr>
            <tr>
              <td className="muted">Project root (copy_repo)</td>
              <td className="mono" style={{ fontSize: 12 }}>{runConfig.project_root ?? "—"}</td>
            </tr>
            <tr>
              <td className="muted">Caps</td>
              <td className="muted" style={{ fontSize: 12 }}>
                {(runConfig.max_files ?? 0).toLocaleString()} files ·{" "}
                {Math.round((runConfig.max_bytes ?? 0) / (1024 * 1024))} MB max — a copy exceeding either is refused cleanly
              </td>
            </tr>
          </tbody>
        </table>
        <p className="muted" style={{ fontSize: 11, marginTop: 8 }}>
          Configure via <span className="mono">RELIX_RUN_WORKSPACE_CONTEXT</span> (empty|copy_repo),{" "}
          <span className="mono">RELIX_RUN_PROJECT_ROOT</span>,{" "}
          <span className="mono">RELIX_RUN_WORKSPACE_MAX_FILES</span>,{" "}
          <span className="mono">RELIX_RUN_WORKSPACE_MAX_BYTES</span>. Excludes .git / build caches /
          node_modules / dev-data / secrets.
        </p>
      </div>

      <div className="card" style={{ gridColumn: "1 / -1" }}>
        <h3>Autonomous execution (heartbeat)</h3>
        <p className="muted" style={{ marginTop: -6, marginBottom: 12 }}>
          When the heartbeat is on, a timer auto-runs ready Briefs through their Operative's adapter —
          same pipeline, ledger, transcript, artifacts, and review as a manual run (autonomous runs
          are stamped <span className="mono">heartbeat</span> and never auto-apply). When off, runs are
          operator-triggered only.
        </p>
        <table className="table">
          <tbody>
            <tr>
              <td className="muted">Status</td>
              <td>
                <span className={"badge " + (runConfig.heartbeat_enabled ? "done" : "backlog")}>
                  {runConfig.heartbeat_enabled ? "enabled" : "disabled"}
                </span>
                {runConfig.heartbeat_enabled && (
                  <span className="muted" style={{ marginLeft: 8, fontSize: 12 }}>
                    polling every {runConfig.heartbeat_interval_secs ?? 10}s
                  </span>
                )}
              </td>
            </tr>
            <tr>
              <td className="muted">Mode</td>
              <td className="muted" style={{ fontSize: 12 }}>
                {runConfig.heartbeat_enabled
                  ? "autonomous — ready + assigned Briefs run without an operator click"
                  : "manual — a Brief runs only when you click Run on the board"}
              </td>
            </tr>
            <tr>
              <td className="muted">Autonomous recovery</td>
              <td>
                <span className={"badge " + (runConfig.autonomous_recovery_enabled ? "done" : "backlog")}>
                  {runConfig.autonomous_recovery_enabled ? "enabled" : "disabled"}
                </span>
                {runConfig.autonomous_recovery_enabled && (
                  <span className="muted" style={{ marginLeft: 8, fontSize: 12 }}>
                    up to {runConfig.autonomous_recovery_max ?? 1} retry/tick
                  </span>
                )}
              </td>
            </tr>
            <tr>
              <td className="muted" />
              <td className="muted" style={{ fontSize: 12 }}>
                {runConfig.autonomous_recovery_enabled
                  ? "retryable failed/interrupted Shifts (already diagnosed retryable, with budget) re-run themselves once through the same guarded retry path — bounded per tick, never refusals/budget-stops/non-retryable"
                  : "failed Shifts wait for an operator to click Retry on the Runs page"}
              </td>
            </tr>
          </tbody>
        </table>
        <p className="muted" style={{ fontSize: 11, marginTop: 8 }}>
          Toggle the heartbeat via <span className="mono">RELIX_HEARTBEAT_ENABLED</span> (off by default);
          pacing via <span className="mono">RELIX_HEARTBEAT_INTERVAL_SECS</span>. The opt-in autonomous
          retry lane is <span className="mono">RELIX_AUTONOMOUS_RECOVERY</span> (off by default), bounded by{" "}
          <span className="mono">RELIX_AUTONOMOUS_RECOVERY_MAX</span>. Autonomous runs still honor adapter
          readiness, per-Operative wake/concurrency caps, and budget hard-stops. No LLM diagnosis or
          provider-quota polling.
        </p>
      </div>

      <AdminRecoveryPanel />

      <div className="card" style={{ gridColumn: "1 / -1" }}>
        <div className="row" style={{ marginBottom: 8 }}>
          <h3 style={{ margin: 0 }}>Agent adapters (Rigs)</h3>
          <div className="spacer" style={{ flex: 1 }} />
          <button className="btn ghost sm" onClick={reload} disabled={loading}>
            {loading ? "Probing…" : "Refresh probes"}
          </button>
        </div>
        <p className="muted" style={{ marginTop: -2, marginBottom: 12 }}>
          Local coding-agent backends an Operative can run work through. Readiness is probed live on
          the coordinator (binary + a noninteractive `--version` check) — install + log in to the CLI
          to make it available.
        </p>
        {loading ? (
          <div className="loading">Probing adapters…</div>
        ) : adapters.length === 0 ? (
          <Empty>No adapters registered.</Empty>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Adapter</th>
                <th>Billing</th>
                <th>Governance</th>
                <th>Readiness</th>
                <th>Detail</th>
              </tr>
            </thead>
            <tbody>
              {adapters.map((a, i) => {
                const st = a.probe?.status ?? "unknown";
                const avail = st === "available";
                return (
                  <tr key={a.name ?? i}>
                    <td><strong>{a.display_name ?? a.name}</strong> <span className="mono">{a.name}</span></td>
                    <td className="muted">
                      {a.billing?.mode === "subscription"
                        ? `subscription${a.billing?.provider ? ` (${a.billing.provider})` : ""}`
                        : a.billing?.mode ?? "—"}
                    </td>
                    <td className="muted">{a.governance ?? "—"}</td>
                    <td>
                      <span className={"badge " + (avail ? "done" : "blocked")}>
                        {STATUS_LABEL[st] ?? st}
                      </span>
                    </td>
                    <td className="muted" style={{ fontSize: 12, maxWidth: 320 }}>
                      {a.probe?.detail}
                      {!avail && a.probe?.install_hint && (
                        <div style={{ marginTop: 3, color: "var(--warn)" }}>→ {a.probe.install_hint}</div>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>
      </div>
    </div>
  );
}

// Admin / session recovery (dashboard-design §10): the persisted adapter
// runtime state for the WHOLE Guild — every Operative's resumable session id,
// accumulated usage/cost, and last run status the heartbeat/Rig layer keeps so
// a Shift can resume. The panel auto-loads the global list
// (`GET /v1/runs/runtime-state/list`) so the operator can see and recover any
// wedged session without first knowing an agent id, filter it, inspect safe
// summary fields, and reset a row in place. Reset forgets the rows only; the
// durable run ledger, transcripts, and artifacts are untouched. Tenant-scoped.
//
// A long session id is never shown in full — it is masked to a short fragment.
function maskSession(s?: string): string {
  if (!s) return "—";
  return s.length <= 14 ? s : `${s.slice(0, 8)}…${s.slice(-4)}`;
}

function AdminRecoveryPanel() {
  const { data: rows, loading, error, reload } = useAsync<RuntimeStateRow[]>(async () => {
    const r = await runtimeState.list();
    if (r.error) throw new Error(r.error);
    const d = r.data;
    return Array.isArray(d)
      ? d
      : d && typeof d === "object" && Array.isArray((d as { rows?: RuntimeStateRow[] }).rows)
        ? (d as { rows: RuntimeStateRow[] }).rows
        : [];
  });
  const [filter, setFilter] = useState("");
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);
  const [busy, setBusy] = useState(false);
  // The row queued for reset (confirmation strip) + the typed RESET text used
  // for the dangerous agent-level (whole-Operative) case.
  const [pending, setPending] = useState<RuntimeStateRow | null>(null);
  const [confirm, setConfirm] = useState("");

  const all = rows ?? [];
  const needle = filter.trim().toLowerCase();
  const shown = needle
    ? all.filter((row) =>
        [row.agent_id, row.rig, row.brief_key, row.last_status, row.session_id]
          .some((f) => (f ?? "").toString().toLowerCase().includes(needle)),
      )
    : all;

  function queueReset(row: RuntimeStateRow) {
    setBanner(null);
    setConfirm("");
    setPending(row);
  }

  async function doReset() {
    if (!pending) return;
    const id = (pending.agent_id ?? "").trim();
    if (!id) return;
    const briefKey = (pending.brief_key ?? "").trim() || undefined;
    setBusy(true);
    setBanner(null);
    try {
      const r = await runtimeState.reset(id, briefKey);
      setBanner({
        kind: "ok",
        msg: `Forgot ${r.removed ?? 0} runtime-state row(s) for ${id}${briefKey ? ` · ${briefKey}` : ""}.`,
      });
      setPending(null);
      setConfirm("");
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Reset failed" });
    } finally {
      setBusy(false);
    }
  }

  // The brief-scoped reset clears just one row and is the safe default. The
  // agent-level reset (a row with no brief_key) forgets EVERY session for that
  // Operative, so it stays gated behind a typed RESET confirmation.
  const agentLevel = pending != null && !((pending.brief_key ?? "").trim());
  const canConfirm = !agentLevel || confirm.trim().toUpperCase() === "RESET";

  return (
    <div className="card" style={{ gridColumn: "1 / -1" }}>
      <div className="row wrap" style={{ justifyContent: "space-between", alignItems: "baseline", gap: 8 }}>
        <h3 style={{ margin: 0, marginBottom: 8 }}>Admin · session recovery</h3>
        <button className="btn ghost" disabled={loading} onClick={() => reload()} style={{ fontSize: 12 }}>
          {loading ? "…" : "Refresh"}
        </button>
      </div>
      <p className="muted" style={{ marginTop: -2, marginBottom: 12, fontSize: 12 }}>
        Every persisted adapter session in the Guild — resumable session id (masked), accumulated
        usage/cost, and last status — across all Operatives. Reset forgets a row so a wedged resumable
        session is cleared; it never touches the durable run ledger, transcripts, or artifacts.
        Tenant-scoped via <span className="mono">/v1/runs/runtime-state/list</span>.
      </p>

      {banner && <div className={"banner " + banner.kind} style={{ fontSize: 12 }}>{banner.msg}</div>}

      <div className="row wrap" style={{ gap: 10, alignItems: "flex-end" }}>
        <label className="field" style={{ margin: 0, flex: "1 1 280px" }}>
          <span>Filter (Operative, Rig, Brief, status, or session fragment)</span>
          <input
            className="input"
            value={filter}
            placeholder="filter sessions…"
            onChange={(e) => setFilter(e.target.value)}
          />
        </label>
        {all.length > 0 && (
          <span className="muted" style={{ fontSize: 12 }}>
            {shown.length === all.length ? `${all.length} session(s)` : `${shown.length} of ${all.length}`}
          </span>
        )}
      </div>

      {error && (
        <div className="banner err" style={{ fontSize: 12, marginTop: 10 }}>
          Could not read runtime state — <span className="mono">GET /v1/runs/runtime-state/list</span>: {error}
        </div>
      )}

      {!error && (
        loading && rows == null ? (
          <div className="empty" style={{ marginTop: 10 }}>Loading persisted sessions…</div>
        ) : all.length === 0 ? (
          <div className="empty" style={{ marginTop: 10 }}>No persisted runtime state in this Guild yet.</div>
        ) : shown.length === 0 ? (
          <div className="empty" style={{ marginTop: 10 }}>No sessions match “{filter.trim()}”.</div>
        ) : (
          <div className="table-scroll" style={{ marginTop: 12 }}>
            <table className="table compact">
              <thead>
                <tr>
                  <th>Operative</th><th>Rig</th><th>Brief</th><th>Session</th><th>Status</th>
                  <th>Tokens</th><th>Cost</th><th>Updated</th><th></th>
                </tr>
              </thead>
              <tbody>
                {shown.map((row, i) => {
                  const tokens = (row.input_tokens ?? 0) + (row.output_tokens ?? 0);
                  return (
                    <tr key={i}>
                      <td className="mono" style={{ fontSize: 11 }}>{row.agent_id ?? "—"}</td>
                      <td className="mono" style={{ fontSize: 11 }}>{row.rig ?? "—"}</td>
                      <td className="mono" style={{ fontSize: 11 }}>{row.brief_key ?? "—"}</td>
                      <td className="mono" style={{ fontSize: 11 }} title={row.session_id ? "session id masked" : undefined}>
                        {maskSession(row.session_id)}
                      </td>
                      <td>
                        <span className="badge" style={{ fontSize: 9 }} title={row.last_error || undefined}>
                          {row.last_status ?? "—"}{row.last_error ? " ⚠" : ""}
                        </span>
                      </td>
                      <td className="muted" style={{ fontSize: 11 }}>{tokens}</td>
                      <td className="muted" style={{ fontSize: 11 }}>
                        ${((row.cost_micros ?? 0) / 1_000_000).toFixed(2)}
                      </td>
                      <td className="muted" style={{ fontSize: 11 }}>
                        {row.updated_at ? new Date(row.updated_at * 1000).toLocaleString() : "—"}
                      </td>
                      <td>
                        <button className="btn ghost" style={{ fontSize: 11 }} disabled={busy} onClick={() => queueReset(row)}>
                          Reset
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )
      )}

      {pending && (
        <div className="banner" style={{ marginTop: 12, fontSize: 12 }}>
          {agentLevel ? (
            <div className="row wrap" style={{ gap: 8, alignItems: "center" }}>
              <span>
                Reset <strong>ALL</strong> persisted sessions for{" "}
                <span className="mono">{pending.agent_id}</span> (this row has no Brief scope). Type{" "}
                <strong>RESET</strong> to confirm:
              </span>
              <input className="input" style={{ width: 120 }} value={confirm} placeholder="RESET" onChange={(e) => setConfirm(e.target.value)} />
            </div>
          ) : (
            <span>
              Reset the runtime session for <span className="mono">{pending.agent_id}</span> ·{" "}
              <span className="mono">{pending.brief_key}</span>? This forgets the resumable session only.
            </span>
          )}
          <div className="row wrap" style={{ marginTop: 8, gap: 8 }}>
            <button className="btn" disabled={busy || !canConfirm} onClick={() => void doReset()}>
              {busy ? "…" : "Confirm reset"}
            </button>
            <button className="btn ghost" disabled={busy} onClick={() => { setPending(null); setConfirm(""); }}>
              Cancel
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
