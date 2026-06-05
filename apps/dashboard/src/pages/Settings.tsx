import { tryGet } from "../api";
import { useAuth } from "../auth";
import { asArray, Empty, useAsync } from "../components/common";
import { MaintenancePanel } from "../components/MaintenancePanel";

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
          </tbody>
        </table>
        <p className="muted" style={{ fontSize: 11, marginTop: 8 }}>
          Toggle via <span className="mono">RELIX_HEARTBEAT_ENABLED</span> (off by default); pacing via{" "}
          <span className="mono">RELIX_HEARTBEAT_INTERVAL_SECS</span>. Autonomous runs still honor adapter
          readiness, per-Operative wake/concurrency caps, and budget hard-stops.
        </p>
      </div>

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
  );
}
