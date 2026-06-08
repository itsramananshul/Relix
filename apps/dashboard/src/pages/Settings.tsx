import { useState } from "react";
import { api, runtimeState, tryGet, type RuntimeStateRow } from "../api";
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
  autonomous_prime_enabled?: boolean;
  autonomous_prime_max?: number;
  autonomous_prime_interval_secs?: number;
}

interface StandingAuthorityCategory {
  category?: string;
  active?: boolean;
  description?: string;
}
interface StandingAuthority {
  authority_id?: string;
  driver_enabled?: boolean;
  hire_rig?: string;
  hire_rig_valid?: boolean;
  categories?: StandingAuthorityCategory[];
  note?: string;
}

interface AutonomyState {
  runtime_enabled?: boolean;
  env_enabled?: boolean;
  effective_enabled?: boolean;
  source?: string; // "env" | "runtime" | "off"
  env_override?: boolean;
  autonomous_prime_max?: number;
  autonomous_prime_interval_secs?: number;
  hire_rig?: string;
  note?: string;
}

// One record from a manual autonomy tick — what the loop considered/did for one
// candidate (Manual Autonomy Tick v1; mirrors the coordinator PrimeAutonomyRecord).
interface TickRecord {
  target_kind?: string;
  target_id?: string;
  mandate_id?: string | null;
  phase?: string;
  action?: string;
  outcome?: string;
  reason?: string;
  // Provenance: how the action was chosen (ai_mode), how the proposed strategy
  // *body* was authored on a propose_strategy row (strategy_ai_mode), and how the
  // tick's candidate ORDER was chosen (priority_ai_mode) with this candidate's rank
  // in that order (priority_rank). All ∈ deterministic_only / llm_used / fallback /
  // unavailable.
  ai_mode?: string;
  ai_reason?: string | null;
  strategy_ai_mode?: string | null;
  strategy_ai_reason?: string | null;
  priority_ai_mode?: string | null;
  priority_ai_reason?: string | null;
  priority_rank?: number | null;
  // How the orchestration Brief TEXT (titles / dossiers / checklists) was authored
  // on an orchestrate_assign_ready row. ∈ deterministic_only / llm_used / fallback
  // / unavailable; null on every other action.
  orchestration_ai_mode?: string | null;
  orchestration_ai_reason?: string | null;
}
interface TickResult {
  tenant?: string;
  max?: number;
  records?: TickRecord[];
  advanced?: number;
  started?: number;
  considered?: number;
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
    const [info, providers, adapters, runConfig, primeAuthority, autonomy] = await Promise.all([
      tryGet<Record<string, unknown>>("/v1/info", {}),
      tryGet<unknown>("/v1/config/providers", {}),
      tryGet<Adapter[]>("/v1/adapters", []),
      tryGet<RunConfig>("/v1/spine/run-config", {}),
      tryGet<StandingAuthority>("/v1/spine/prime/standing-authority", {}),
      tryGet<AutonomyState>("/v1/spine/prime/autonomy", {}),
    ]);
    return {
      info,
      providers: extractProviders(providers),
      adapters: Array.isArray(adapters) ? adapters : [],
      runConfig: runConfig ?? {},
      primeAuthority: primeAuthority ?? {},
      autonomy: autonomy ?? {},
    };
  }, []);

  const info = data?.info ?? {};
  const providers = data?.providers ?? [];
  const adapters = data?.adapters ?? [];
  const runConfig = data?.runConfig ?? {};
  const primeAuthority = data?.primeAuthority ?? {};
  const autonomy = data?.autonomy ?? {};

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
            <tr>
              <td className="muted">Autonomous Prime</td>
              <td>
                <span className={"badge " + (autonomy.effective_enabled ? "done" : "backlog")}>
                  {autonomy.effective_enabled ? "enabled" : "disabled"}
                </span>
                <span className="muted" style={{ marginLeft: 8, fontSize: 12 }}>
                  {autonomy.effective_enabled
                    ? `${autonomy.source === "env" ? "on via env override" : "on via runtime toggle"} — up to ${autonomy.autonomous_prime_max ?? 1} action/tick, every ${autonomy.autonomous_prime_interval_secs ?? 30}s`
                    : "off — toggle it below (no restart needed)"}
                </span>
              </td>
            </tr>
            <tr>
              <td className="muted" />
              <td className="muted" style={{ fontSize: 12 }}>
                {autonomy.effective_enabled
                  ? "Prime drives already-approved work forward on its own — plans the team, orchestrates the Brief tree, and starts ready work through the same governed routes — bounded per tick. Approval gates move only when a matching standing-authority grant is live; budget gates are never delegated."
                  : "approved Prime work waits for an operator to click Advance / Start in the Action Center"}
              </td>
            </tr>
          </tbody>
        </table>
        <p className="muted" style={{ fontSize: 11, marginTop: 8 }}>
          Toggle the heartbeat via <span className="mono">RELIX_HEARTBEAT_ENABLED</span> (off by default);
          pacing via <span className="mono">RELIX_HEARTBEAT_INTERVAL_SECS</span>. The opt-in autonomous
          retry lane is <span className="mono">RELIX_AUTONOMOUS_RECOVERY</span> (off by default), bounded by{" "}
          <span className="mono">RELIX_AUTONOMOUS_RECOVERY_MAX</span>. The autonomous Prime loop is now
          a <strong>runtime toggle</strong> (below) — <span className="mono">RELIX_AUTONOMOUS_PRIME</span> still
          works as a global boot override, paced by{" "}
          <span className="mono">RELIX_AUTONOMOUS_PRIME_INTERVAL_SECS</span> and bounded by{" "}
          <span className="mono">RELIX_AUTONOMOUS_PRIME_MAX</span>. Autonomous runs still honor adapter
          readiness, per-Operative wake/concurrency caps, and budget hard-stops.{" "}
          Prime has <strong>four independent, opt-in LLM switches</strong>, all off by default and all
          falling back deterministically — <em>none ever approves a gate</em>.{" "}
          <strong>Prime Deliberation</strong> (<span className="mono">RELIX_PRIME_LLM_DELIBERATION</span>):
          when on, a model may only <em>confirm or hold</em> the one already-computed legal next step for a
          candidate — never approving a gate, inventing an action, or bypassing budget/Claim/adapter checks.{" "}
          <strong>Prime Strategy Draft authoring</strong> (
          <span className="mono">RELIX_PRIME_LLM_STRATEGY_DRAFT</span>): when on, a model may <em>author the
          text of a PROPOSED Mandate strategy</em> from a bounded, secret-free snapshot (title / status /
          description / active roles / readiness); the body is re-validated + sanitized server-side and is
          only ever <em>proposed</em> — the human <span className="mono">mandate.strategy.approve</span> gate
          is unchanged, and an existing proposed/approved/rejected strategy is never overwritten.{" "}
          <strong>Prime Prioritization</strong> (
          <span className="mono">RELIX_PRIME_LLM_PRIORITIZATION</span>): when on, a model may only{" "}
          <em>choose the ORDER</em> in which a bounded tick spends its action budget among the candidates the
          deterministic classifier has ALREADY computed as legal — or hold the whole queue this tick. It
          cannot invent a candidate, add an action, widen any candidate's action, or bypass standing-
          authority / budget / Claim / adapter / tenant gates; only already-attemptable candidates are
          offered.{" "}
          <strong>Prime Orchestration authoring</strong> (
          <span className="mono">RELIX_PRIME_LLM_ORCHESTRATION</span>): when on, a model may <em>author the
          text</em> — titles, dossiers, checklists — of the orchestration Briefs (parent / role tracks /
          subject executions) for the skeleton the deterministic readiness logic has ALREADY computed. It
          cannot invent a role, agent, Brief id, source marker, dependency, assignee, approval, budget change,
          or tool; the roles, agents, assignments, and gates are unchanged, an existing/hand-edited Brief
          title is never clobbered, and direct one-click{" "}
          <span className="mono">mandate.orchestrate</span> stays deterministic. Any switch falls back
          deterministically if the model is unavailable or its output is invalid. Each tick record shows the
          provenance — action choice (<span className="mono">act:</span>), on a strategy draft the body author
          (<span className="mono">strat:</span>), the queue order with this candidate's rank (
          <span className="mono">ord:</span>), and on an orchestrate row the Brief-text author (
          <span className="mono">orch:</span>) — each ∈{" "}
          <span className="mono">deterministic_only</span> / <span className="mono">llm_used</span> /{" "}
          <span className="mono">fallback</span> / <span className="mono">unavailable</span>. All reuse the
          existing governed <span className="mono">ai.chat</span> mesh path; no provider key enters the
          coordinator, web bridge, or dashboard. Explicit one-click{" "}
          <span className="mono">prime.advance</span> strategy drafting stays deterministic.
        </p>
      </div>

      <AutonomousPrimeSwitchPanel
        autonomy={autonomy}
        authority={primeAuthority}
        loading={loading}
        onChanged={reload}
      />

      <PrimeStandingAuthorityPanel
        authority={primeAuthority}
        autonomy={autonomy}
        loading={loading}
        onChanged={reload}
      />

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

// Prime Runtime Autonomy Switch (v1): the operator control to turn the
// autonomous Prime LOOP on/off for this Guild at runtime — no restart, no env
// edit. The setting is tenant-scoped + persisted in the coordinator DB. This is
// NOT an approval bypass: ON only wakes the loop over already-approved work;
// each governed approval still needs a live standing grant (the panel below).
// The env var RELIX_AUTONOMOUS_PRIME stays a GLOBAL boot override — while it is
// set the loop runs for every Guild and the OFF control can only clear the
// persisted row (effective stays ON until the env is changed + restart).
function AutonomousPrimeSwitchPanel({
  autonomy,
  authority,
  loading,
  onChanged,
}: {
  autonomy: AutonomyState;
  authority: StandingAuthority;
  loading: boolean;
  onChanged: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);
  const [tickBusy, setTickBusy] = useState(false);
  const [tick, setTick] = useState<TickResult | null>(null);

  const runtimeOn = !!autonomy.runtime_enabled;
  const envOverride = !!autonomy.env_override;
  const effectiveOn = !!autonomy.effective_enabled;
  const grantedCount = (authority.categories ?? []).filter((c) => c.active).length;
  const totalCats = (authority.categories ?? []).length;

  async function setEnabled(enabled: boolean) {
    setBusy(true);
    setBanner(null);
    try {
      await api.put("/v1/spine/prime/autonomy", { enabled });
      setBanner({
        kind: "ok",
        msg: enabled
          ? "Autonomous Prime loop turned ON for this Guild (no restart needed)."
          : "Autonomous Prime loop turned OFF for this Guild.",
      });
      onChanged();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Toggle failed" });
    } finally {
      setBusy(false);
    }
  }

  // Run exactly ONE bounded autonomous Prime tick for this Guild now. This is an
  // explicit operator wake-up of the same governed driver — it does NOT bypass
  // standing approvals or budgets and does not require the loop to be ON.
  async function runTickNow() {
    setTickBusy(true);
    setBanner(null);
    setTick(null);
    try {
      const res = await api.post<TickResult>("/v1/spine/prime/autonomy/tick", {});
      setTick(res);
      setBanner({
        kind: "ok",
        msg: `Prime tick ran: considered ${res.considered ?? 0}, advanced ${res.advanced ?? 0}, started ${res.started ?? 0}.`,
      });
      onChanged();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Tick failed" });
    } finally {
      setTickBusy(false);
    }
  }

  return (
    <div className="card" style={{ gridColumn: "1 / -1" }}>
      <div className="row" style={{ marginBottom: 8 }}>
        <h3 style={{ margin: 0 }}>Autonomous Prime loop</h3>
        <div className="spacer" style={{ flex: 1 }} />
        <span className={"badge " + (effectiveOn ? "done" : "backlog")}>
          {effectiveOn
            ? autonomy.source === "env"
              ? "on (env override)"
              : "on"
            : "off"}
        </span>
      </div>
      <p className="muted" style={{ marginTop: -2, marginBottom: 12 }}>
        Turn the autonomous Prime loop on/off for this Guild at runtime — no restart. When ON, Prime
        drives <strong>already-approved</strong> work forward on a timer (plans the team, orchestrates
        the Brief tree, starts ready work) through the same governed routes, bounded per tick. This is{" "}
        <strong>not</strong> an approval bypass: a governed approval moves only when the matching
        standing-authority grant below is live.
      </p>

      {banner && <div className={"banner " + banner.kind} style={{ fontSize: 12 }}>{banner.msg}</div>}

      {loading ? (
        <div className="loading">Loading…</div>
      ) : (
        <>
          <div className="row wrap" style={{ gap: 10, alignItems: "center" }}>
            {runtimeOn ? (
              <button className="btn ghost" disabled={busy} onClick={() => void setEnabled(false)}>
                {busy ? "…" : "Turn OFF"}
              </button>
            ) : (
              <button className="btn" disabled={busy} onClick={() => void setEnabled(true)}>
                {busy ? "…" : "Turn ON"}
              </button>
            )}
            <span className="muted" style={{ fontSize: 12 }}>
              Runtime setting: <strong>{runtimeOn ? "on" : "off"}</strong>
              {" · "}up to {autonomy.autonomous_prime_max ?? 1} action/tick, every{" "}
              {autonomy.autonomous_prime_interval_secs ?? 30}s
            </span>
          </div>

          {/* Manual Autonomy Tick: wake the loop ONCE for this Guild and see what
              it did. Works even when the loop is off — it is an explicit wake-up,
              not a bypass: it still obeys standing grants, budgets, and bounds. */}
          <div className="row wrap" style={{ gap: 10, alignItems: "center", marginTop: 10 }}>
            <button className="btn ghost" disabled={tickBusy} onClick={() => void runTickNow()}>
              {tickBusy ? "Running…" : "Run Prime now"}
            </button>
            <span className="muted" style={{ fontSize: 12 }}>
              Wakes <strong>exactly one</strong> bounded tick for this Guild (up to{" "}
              {autonomy.autonomous_prime_max ?? 1} action). It does <strong>not</strong> require the
              loop to be on and does <strong>not</strong> bypass standing approvals or budgets.{" "}
              <strong>Run Prime now</strong> uses the same deliberation, strategy-draft, and prioritization
              layers (when their switches are on) whenever the coordinator mesh AI peer is available;
              otherwise it falls back deterministically.
            </span>
          </div>

          {/* The latest tick's records: phase / action / outcome / reason. */}
          {tick && (tick.records ?? []).length > 0 && (
            <div className="card" style={{ marginTop: 10, padding: 8 }}>
              <table className="table" style={{ fontSize: 12 }}>
                <thead>
                  <tr>
                    <th>Target</th>
                    <th>Phase</th>
                    <th>Action</th>
                    <th>Outcome</th>
                    <th>Provenance</th>
                    <th>Reason</th>
                  </tr>
                </thead>
                <tbody>
                  {(tick.records ?? []).slice(0, 5).map((r, i) => (
                    <tr key={i}>
                      <td className="mono">
                        {r.target_kind ?? "?"}
                        {r.target_id ? <span className="muted"> · {r.target_id.slice(0, 8)}</span> : null}
                      </td>
                      <td>{r.phase ?? "—"}</td>
                      <td className="mono">{r.action ?? "—"}</td>
                      <td>
                        <span
                          className={
                            "badge " +
                            (r.outcome === "advanced" || r.outcome === "started"
                              ? "done"
                              : r.outcome === "blocked"
                                ? "backlog"
                                : "")
                          }
                        >
                          {r.outcome ?? "—"}
                        </span>
                      </td>
                      <td className="mono" style={{ fontSize: 11 }}>
                        {/* Action-choice provenance (deliberation), the strategy-body
                            author on a propose_strategy row, and the tick's queue-order
                            provenance + this candidate's rank. */}
                        <span title={r.ai_reason ?? undefined}>act:{r.ai_mode ?? "deterministic_only"}</span>
                        {r.strategy_ai_mode ? (
                          <>
                            <br />
                            <span title={r.strategy_ai_reason ?? undefined}>
                              strat:{r.strategy_ai_mode}
                            </span>
                          </>
                        ) : null}
                        <br />
                        <span title={r.priority_ai_reason ?? undefined}>
                          ord:{r.priority_ai_mode ?? "deterministic_only"}
                          {typeof r.priority_rank === "number" ? `#${r.priority_rank}` : ""}
                        </span>
                        {r.orchestration_ai_mode ? (
                          <>
                            <br />
                            <span title={r.orchestration_ai_reason ?? undefined}>
                              orch:{r.orchestration_ai_mode}
                            </span>
                          </>
                        ) : null}
                      </td>
                      <td className="muted">{r.reason ?? ""}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
              {(tick.records ?? []).length > 5 && (
                <p className="muted" style={{ fontSize: 11, marginTop: 4 }}>
                  Showing 5 of {(tick.records ?? []).length} considered.
                </p>
              )}
            </div>
          )}
          {tick && (tick.records ?? []).length === 0 && (
            <p className="muted" style={{ fontSize: 12, marginTop: 8 }}>
              The tick considered no candidates — nothing approved/active to drive in this Guild.
            </p>
          )}

          {/* Env override: the OFF control can only clear the persisted row. */}
          {envOverride && (
            <div className="banner" style={{ fontSize: 12, marginTop: 10 }}>
              <span className="mono">RELIX_AUTONOMOUS_PRIME</span> is set as a global boot override, so
              the loop is effectively <strong>ON for every Guild</strong>. Turning it off here only
              clears this Guild's persisted runtime setting — it cannot fully disable the loop until the
              env var is unset and the coordinator restarts.
            </div>
          )}

          {/* Cross-hints between the loop switch and standing grants. */}
          {runtimeOn && totalCats > 0 && grantedCount === 0 && (
            <div className="banner" style={{ fontSize: 12, marginTop: 10 }}>
              The loop is <strong>awake</strong> but no standing-authority category is granted — it will
              drive already-approved work, but <strong>cannot approve</strong> a proposal / strategy /
              hire / Clearance until you grant standing authority below.
            </div>
          )}
          {!effectiveOn && grantedCount > 0 && (
            <div className="banner" style={{ fontSize: 12, marginTop: 10 }}>
              {grantedCount} standing-authority {grantedCount === 1 ? "grant is" : "grants are"} live, but
              the loop is <strong>off</strong> — Prime has permission but is asleep. Turn it ON for the
              grants to take effect.
            </div>
          )}
        </>
      )}
    </div>
  );
}

// Prime standing authority (company-model standing-approval semantics): the
// operator control surface for the bounded powers the Board grants the
// autonomous Prime to act on its behalf at specific approval gates. These are
// STANDING APPROVALS, not loop toggles — enabling the runtime/env Prime loop
// only wakes the driver; each category acts ONLY while a `standing_approvals` row exists
// for the synthetic `__relix_autonomous_prime__` authority in this Guild. Grant
// creates a bounded row through the EXISTING standing-approval routes
// (`POST /v1/agents/:id/standing-approvals`); Revoke deletes the matching rows
// (`GET` the list → `DELETE /v1/standing-approvals/:standing_id`). After either,
// the standing-authority read surface is refreshed. No new mutation route was
// invented — the synthetic authority reuses the same routes real Operatives use.

const AUTONOMOUS_PRIME_AUTHORITY = "__relix_autonomous_prime__";
// Bounded, safe-but-usable grant defaults (a practical ops default, not an open
// blank cheque): expires 24h out, capped at 25 autonomous calls, no cost cap
// (these categories are $0-but-tracked governance actions, not paid spend).
const GRANT_TTL_SECS = 24 * 60 * 60;
const GRANT_MAX_CALLS = 25;

// Human-facing labels for the bounded standing-authority categories the backend
// (`prime.standing_authority`) returns. The category wire name stays the source
// of truth (and is shown verbatim); this only adds a readable headline per row.
const CATEGORY_LABELS: Record<string, string> = {
  "prime.proposal.approve": "Approve proposed plans",
  "prime.hire.approve": "Approve pending hires",
  "prime.clearance.approve": "Greenlight spawn Clearances",
  "prime.strategy.approve": "Approve proposed strategies",
  "prime.run.review_accept": "Accept completed Shifts",
  "prime.run.apply": "Apply accepted Shifts",
  "prime.brief.assign_decomposed": "Assign Prime-decomposed children",
};
function categoryLabel(cat: string): string {
  return CATEGORY_LABELS[cat] ?? cat;
}

interface StandingListRow {
  standing_id?: string;
  match_category?: string;
}

function PrimeStandingAuthorityPanel({
  authority,
  autonomy,
  loading,
  onChanged,
}: {
  authority: StandingAuthority;
  autonomy: AutonomyState;
  loading: boolean;
  onChanged: () => void;
}) {
  const authorityId = authority.authority_id ?? AUTONOMOUS_PRIME_AUTHORITY;
  const categories = authority.categories ?? [];
  const loopEffective = !!autonomy.effective_enabled;
  const loopSource = autonomy.source === "env" ? "env override" : "runtime toggle";
  // The category currently being granted/revoked (disables just its button), and
  // a single success/error banner for the last action.
  const [busy, setBusy] = useState<string | null>(null);
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);

  async function grant(category: string) {
    setBusy(category);
    setBanner(null);
    try {
      const expires_at = Math.floor(Date.now() / 1000) + GRANT_TTL_SECS;
      await api.post(`/v1/agents/${encodeURIComponent(authorityId)}/standing-approvals`, {
        category,
        expires_at,
        max_calls: GRANT_MAX_CALLS,
        granted_by: "operator",
        note: "Granted from Settings · Prime standing authority",
      });
      setBanner({
        kind: "ok",
        msg: `Granted ${category} — bounded to ${GRANT_MAX_CALLS} autonomous calls, expires in 24h.`,
      });
      onChanged();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Grant failed" });
    } finally {
      setBusy(null);
    }
  }

  async function revoke(category: string) {
    setBusy(category);
    setBanner(null);
    try {
      // The read surface reports active/inactive but not the row id, so list the
      // synthetic authority's standing approvals and revoke every row for THIS
      // category (an exhausted row alongside a fresh one are both cleared).
      const list = await api.get<{ standing?: StandingListRow[] }>(
        `/v1/agents/${encodeURIComponent(authorityId)}/standing-approvals`,
      );
      const rows = (list?.standing ?? []).filter(
        (r) => r.match_category === category && r.standing_id,
      );
      if (rows.length === 0) {
        setBanner({ kind: "err", msg: `No standing grant found for ${category} to revoke.` });
        onChanged();
        return;
      }
      for (const r of rows) {
        await api.del(`/v1/standing-approvals/${encodeURIComponent(r.standing_id!)}`);
      }
      setBanner({
        kind: "ok",
        msg: `Revoked ${category} (${rows.length} grant${rows.length > 1 ? "s" : ""}).`,
      });
      onChanged();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Revoke failed" });
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="card" style={{ gridColumn: "1 / -1" }}>
      <h3>Prime standing authority</h3>
      <p className="muted" style={{ marginTop: -2, marginBottom: 12 }}>
        Bounded powers the Board can grant the autonomous Prime to act on its behalf at specific
        approval gates. These are <strong>standing approvals, not loop toggles</strong>: granting a
        category here does <em>not</em> wake Prime on its own — the autonomous Prime loop must be ON
        via the runtime switch above or the env override, and even then a category
        acts <em>only</em> while its grant is live. Granting creates a bounded row (25 calls, expires
        in 24h) for the synthetic{" "}
        <span className="mono">{authorityId}</span> authority in this Guild; revoking removes it.
      </p>
      {!loopEffective && (
        <div className="banner" style={{ fontSize: 12 }}>
          The autonomous Prime loop is <strong>off</strong>. Grants below are recorded but stay inert
          until the runtime switch above is turned on (or the env override is set) — they never run
          autonomy by themselves.
        </div>
      )}
      {loopEffective && categories.some((c) => c.active) && (
        <div className="banner ok" style={{ fontSize: 12 }}>
          The loop is ON via {loopSource}; live grants below can be consumed by Prime's bounded
          autonomous approval actions.
        </div>
      )}
      {banner && <div className={"banner " + banner.kind} style={{ fontSize: 12 }}>{banner.msg}</div>}
      {loading ? (
        <div className="loading">Loading…</div>
      ) : (
        <table className="table">
          <tbody>
            {categories.map((c) => {
              const active = !!c.active;
              const cat = c.category ?? "";
              const inFlight = busy === cat;
              return (
                <tr key={cat}>
                  <td style={{ fontSize: 12 }}>
                    <div>{categoryLabel(cat)}</div>
                    <div className="mono muted" style={{ fontSize: 11 }}>{cat}</div>
                  </td>
                  <td>
                    <span className={"badge " + (active ? "done" : "backlog")}>
                      {active ? "enabled" : "disabled"}
                    </span>
                    <span className="muted" style={{ marginLeft: 8, fontSize: 12 }}>{c.description}</span>
                  </td>
                  <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                    {active ? (
                      <button
                        className="btn ghost sm"
                        disabled={busy !== null}
                        onClick={() => void revoke(cat)}
                      >
                        {inFlight ? "…" : "Revoke"}
                      </button>
                    ) : (
                      <button
                        className="btn sm"
                        disabled={busy !== null}
                        onClick={() => void grant(cat)}
                      >
                        {inFlight ? "…" : "Grant"}
                      </button>
                    )}
                  </td>
                </tr>
              );
            })}
            {categories.length === 0 && (
              <tr><td className="muted" colSpan={3}>Prime standing-authority state unavailable.</td></tr>
            )}
            <tr>
              <td className="muted">Hire Rig</td>
              <td>
                <span className="mono" style={{ fontSize: 12 }}>{authority.hire_rig ?? "echo"}</span>
                {authority.hire_rig_valid === false && (
                  <span className="badge todo" style={{ marginLeft: 8 }}>unknown Rig — hires will be skipped</span>
                )}
              </td>
              <td />
            </tr>
          </tbody>
        </table>
      )}
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
