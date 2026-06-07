import { useEffect, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { api, tryGet } from "../api";
import { asArray, Badge, Empty, extractList, Section, useAsync } from "../components/common";

// One Operative's Keys (`/v1/spine/keys/:agent`) — the org/work permissions
// + execution caps the legacy spine board surfaced. Rendered read-only here
// (editing Keys stays out of this parity slice).
interface Keys {
  can_spawn_agents?: boolean;
  spawn_route?: string;
  can_assign_work?: boolean;
  assign_scope?: string;
  can_manage_work?: boolean;
  manage_scope?: string;
  can_configure_agents?: boolean;
  configure_scope?: string;
  max_concurrent_runs?: number;
  monthly_allowance_cents?: number;
  wake_on_timer?: boolean;
  wake_on_demand?: boolean;
  secret_allowlist?: string[];
  // The Operative's charter — markdown instruction bundle (company-model §4.5).
  // `agent.keys` serializes the full AgentProfile, so this read carries it even
  // though the pipe-delimited `/v1/agents/:id` detail does not. Operator-authored
  // trusted text composed into the agent's Shift prompt; surfaced read-only.
  instruction_bundle?: string;
}

// Guild-committed Allowance (`/v1/spine/allowance/committed`). Field name
// varies; pull the first cents-like number defensively.
function committedCents(v: unknown): number | null {
  if (typeof v === "number") return v;
  if (v && typeof v === "object") {
    const o = v as Record<string, unknown>;
    for (const k of ["committed_cents", "committed", "allowance_cents", "cents", "total_cents"]) {
      if (typeof o[k] === "number") return o[k] as number;
    }
  }
  return null;
}
function fmtCents(c?: number | null): string {
  if (c == null) return "—";
  return "$" + (c / 100).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
}

// One Operative's full detail (`/v1/agents/:id`). Beyond the Capability-powers
// half of the §9 permission panel (risk ceiling + the category/secret/surface
// gates the admission pipeline enforces), the same read carries the employee
// record the Overview / Budget / Configuration tabs surface (org placement,
// adapter, autonomy flags, allowance, timestamps). Read-only here.
interface AgentDetail {
  agent_id?: string;
  name?: string;
  role?: string;
  title?: string;
  department?: string;
  team?: string;
  created_by?: string;
  status?: string;
  subject_id?: string;
  created_at?: number;
  updated_at?: number;
  risk_ceiling?: string;
  approval_timeout_secs?: number;
  surface_allowlist?: string[];
  allow_categories?: string[];
  deny_categories?: string[];
  allow_sensitivity_tags?: string[];
  deny_sensitivity_tags?: string[];
  approval_required_categories?: string[];
  rig?: string | null;
  monthly_allowance_cents?: number | null;
  max_concurrent_runs?: number;
  wake_on_timer?: boolean;
  wake_on_demand?: boolean;
}

// One standing approval (`/v1/agents/:id/standing-approvals`) — a pre-granted
// clearance so a matching action proceeds without a fresh gate, with its scope,
// expiry, and usage. Rendered read-only (granting/revoking stays out of slice).
interface Standing {
  standing_id?: string;
  match_category?: string;
  match_path_glob?: string | null;
  scope_kind?: string;
  method_prefix?: string | null;
  workspace_path_glob?: string | null;
  expires_at?: number;
  granted_by?: string;
  max_calls?: number | null;
  calls_used?: number;
  note?: string;
}

// The three governance reads for one Operative, fetched together on expand.
interface OpDetail {
  keys: Keys | null;
  detail: AgentDetail | null;
  standing: Standing[];
}

// Render epoch-seconds as a short local date; "—" when absent/zero.
function fmtWhen(secs?: number): string {
  if (!secs) return "—";
  const d = new Date(secs * 1000);
  return Number.isNaN(d.getTime()) ? "—" : d.toLocaleDateString();
}

interface Agent {
  agent_id?: string;
  name?: string;
  role?: string;
  status?: string;
  reports_to?: string | null;
  title?: string;
  rig?: string | null;
}
interface Adapter {
  name?: string;
  display_name?: string;
  probe?: { status?: string; detail?: string; install_hint?: string | null };
}
interface CompanyStatus {
  initialized?: boolean;
  founder?: Agent | null;
  prime?: Agent | null;
  operative_count?: number;
  crew?: {
    total?: number;
    active?: number;
    pending?: number;
    by_status?: Record<string, number>;
    by_role?: Record<string, number>;
  };
}
// A board card (`/v1/spine/board/:col`) — enough to list an Operative's open
// assigned Briefs (title + column + deep link) on the Overview tab.
interface Card {
  task_id?: string;
  id?: string;
  title?: string;
  board_status?: string;
  priority?: string;
  assignee_agent_id?: string | null;
}
// A durable run row (`/v1/runs`) — already fetched for the live/running badge;
// widened so the Runs + Overview tabs can show recent Shifts for an Operative
// (status / trigger / rig / duration + a deep link) with no extra fetch.
interface RunRow {
  run_id?: string;
  brief_id?: string;
  agent_id?: string;
  rig?: string;
  status?: string;
  trigger?: string;
  started_at?: number;
  duration_secs?: number;
  summary?: string;
  review?: string;
}

// Friendly labels for the rich readiness statuses.
const STATUS_LABEL: Record<string, string> = {
  available: "available",
  missing_binary: "not installed",
  not_authenticated: "needs login",
  unsupported_version: "version issue",
  interactive_only: "needs a TTY",
  probe_failed: "probe failed",
};
// Board columns counted as an Operative's open workload.
const WORK_COLUMNS = ["todo", "in_progress", "in_review"];

// The Operative-detail workbench tabs (dashboard-design §9). Overview is the
// default; an unknown `?tab=` value falls back to it (param safety).
const TABS = ["overview", "instructions", "permissions", "runs", "budget", "configuration"] as const;
type Tab = (typeof TABS)[number];
const TAB_LABEL: Record<Tab, string> = {
  overview: "Overview",
  instructions: "Instructions",
  permissions: "Permissions",
  runs: "Runs",
  budget: "Budget",
  configuration: "Configuration",
};

// Run status → badge tone (a compact mirror of the Runs page vocabulary).
const RUN_TONE: Record<string, string> = {
  running: "in_progress",
  done: "done",
  failed: "blocked",
  cancelled: "blocked",
  refused: "blocked",
  interrupted: "blocked",
  continued: "todo",
};
// Trigger source → short label. `heartbeat` is autonomous dispatch.
function runTrigger(t?: string): string {
  if (!t || t === "unknown") return "—";
  return t === "heartbeat" ? "auto" : t;
}
// A run's duration: a live run counts up from started_at; a terminal run shows
// its recorded seconds.
function runDuration(r: RunRow): string {
  if (r.status === "running" && r.started_at) {
    return `${Math.max(0, Math.floor(Date.now() / 1000) - r.started_at)}s…`;
  }
  return typeof r.duration_secs === "number" ? `${r.duration_secs}s` : "—";
}
// Epoch-seconds → short local date+time; "—" when absent.
function fmtDateTime(secs?: number): string {
  if (!secs) return "—";
  const d = new Date(secs * 1000);
  return Number.isNaN(d.getTime()) ? "—" : d.toLocaleString();
}

export function Agents() {
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [founderName, setFounderName] = useState("Founder");
  const [founderRig, setFounderRig] = useState("echo");
  // Per-Operative governance panel: the open Operative is URL-driven
  // (`/agents?agent=<id>`) so Lattice/Agents deep links land on the exact
  // Operative — selected, highlighted, and scrolled into view — and refresh/
  // back/forward preserve the selection (mirrors the Briefs `?brief=` pattern).
  // A small cache keeps re-opening instant; an entry present (even with null
  // parts) = loaded.
  const [searchParams, setSearchParams] = useSearchParams();
  const openId = searchParams.get("agent");
  const [detailCache, setDetailCache] = useState<Record<string, OpDetail>>({});
  // The active workbench tab is URL-driven too (`?tab=runs`) so a deep link can
  // land on a specific tab and back/forward restore it. An unknown value falls
  // back to Overview without rewriting the URL (param safety).
  const tabParam = searchParams.get("tab");
  const activeTab: Tab = (TABS as readonly string[]).includes(tabParam ?? "")
    ? (tabParam as Tab)
    : "overview";
  // Writing the agent param preserves the tab + any other query params already
  // present; clearing the selection drops the tab too (it's meaningless alone).
  function setOpen(id: string | null) {
    const next = new URLSearchParams(searchParams);
    if (id) {
      next.set("agent", id);
    } else {
      next.delete("agent");
      next.delete("tab");
    }
    setSearchParams(next, { replace: true });
  }
  // Switch the workbench tab, preserving the selected Operative + other params.
  function setTab(tab: Tab) {
    const next = new URLSearchParams(searchParams);
    if (tab === "overview") next.delete("tab");
    else next.set("tab", tab);
    setSearchParams(next, { replace: true });
  }
  // In-flight guard so the load effect never starts a duplicate fetch for the
  // same Operative before its cache entry lands.
  const inflightRef = useRef<Set<string>>(new Set());
  // The currently-selected row/card, scrolled into view like Briefs deep links.
  const selectedRef = useRef<HTMLElement | null>(null);

  const { data, loading, error, reload } = useAsync(async () => {
    const work: Card[] = [];
    const [company, ops, adapters, runs, allowance, roster] = await Promise.all([
      tryGet<CompanyStatus>("/v1/spine/company", {}),
      tryGet<Agent[]>("/v1/spine/operatives", []),
      tryGet<Adapter[]>("/v1/adapters", []),
      tryGet<RunRow[]>("/v1/runs", []),
      tryGet<unknown>("/v1/spine/allowance/committed", {}),
      // Authoritative, tenant-scoped Operative counts by status (+ total).
      tryGet<Record<string, number>>("/v1/spine/roster", {}),
      Promise.all(
        WORK_COLUMNS.map(async (col) => {
          work.push(...asArray<Card>(await tryGet<Card[]>(`/v1/spine/board/${col}?limit=100`, [])));
        }),
      ),
    ]);
    return {
      company: company ?? {},
      agents: Array.isArray(ops) ? ops : [],
      adapters: Array.isArray(adapters) ? adapters : [],
      runs: Array.isArray(runs) ? runs : [],
      allowance: committedCents(allowance),
      roster: roster && typeof roster === "object" ? roster : {},
      work,
    };
  }, []);

  // Load one Operative's governance detail — its three reads in parallel
  // (Keys + capability detail + standing approvals). Each read degrades to a
  // null/empty fallback so one unavailable surface shows an honest empty state
  // instead of blanking the panel. Guarded by the cache + an in-flight set so a
  // URL-driven open and a row click never double-fetch.
  async function loadDetail(agentId: string) {
    if (!agentId || agentId in detailCache || inflightRef.current.has(agentId)) return;
    inflightRef.current.add(agentId);
    const enc = encodeURIComponent(agentId);
    const [keys, detail, standing] = await Promise.all([
      tryGet<Keys | null>(`/v1/spine/keys/${enc}`, null),
      tryGet<AgentDetail | null>(`/v1/agents/${enc}`, null),
      tryGet<unknown>(`/v1/agents/${enc}/standing-approvals`, {}),
    ]);
    setDetailCache((m) => ({
      ...m,
      [agentId]: { keys, detail, standing: extractList<Standing>(standing, ["standing"]) },
    }));
    inflightRef.current.delete(agentId);
  }

  // Toggle the governance panel through the URL: clicking View/Hide writes (or
  // clears) `?agent=<id>`. The load + scroll happen in the effects below, so an
  // open from the URL (deep link / refresh / back-forward) behaves identically
  // to a click.
  function toggleDetail(agentId: string) {
    setOpen(openId === agentId ? null : agentId);
  }

  // Fetch the selected Operative's detail when the selection changes — whether
  // it came from a click or straight from the URL on first render.
  useEffect(() => {
    if (openId) loadDetail(openId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [openId]);

  // Copy a shareable deep link to this Operative's workbench. The canonical
  // form stays `?agent=<id>`; the active tab is appended only when it isn't the
  // default Overview, so a shared link reopens exactly what's on screen.
  async function copyLink(agentId: string) {
    const tabSuffix = activeTab === "overview" ? "" : `&tab=${activeTab}`;
    const url = `${window.location.origin}${window.location.pathname}?agent=${encodeURIComponent(agentId)}${tabSuffix}`;
    try {
      await navigator.clipboard.writeText(url);
      setBanner({ kind: "ok", msg: "Deep link copied to clipboard." });
    } catch {
      setBanner({ kind: "info", msg: url });
    }
  }

  const company = data?.company ?? {};
  const agents = data?.agents ?? [];
  const adapters = data?.adapters ?? [];
  const runs = data?.runs ?? [];
  const roster = data?.roster ?? {};
  const work = data?.work ?? [];
  const byName = new Map(adapters.map((a) => [a.name ?? "", a]));
  const availCount = adapters.filter((a) => a.probe?.status === "available").length;
  const initialized = company.initialized ?? agents.length > 0;

  // Bring the workbench panel into view once a known Operative is selected.
  // `block: "nearest"` avoids jumping when it's already visible; an unknown id
  // renders no workbench, leaves the ref null, and the page simply stays put.
  useEffect(() => {
    if (openId && selectedRef.current) {
      selectedRef.current.scrollIntoView({ behavior: "smooth", block: "nearest" });
    }
  }, [openId, data]);

  // Workload (open assigned Briefs) + currently-running counts per Operative.
  const workload = new Map<string, number>();
  for (const c of work) {
    const a = c.assignee_agent_id;
    if (a) workload.set(a, (workload.get(a) ?? 0) + 1);
  }
  const running = new Map<string, number>();
  for (const r of runs) {
    if (r.status === "running" && r.agent_id) running.set(r.agent_id, (running.get(r.agent_id) ?? 0) + 1);
  }

  const founder = agents.find((a) => a.role === "founder") ?? (company.founder ?? undefined);
  // Prime = the planning lead (Founder's right hand). Prefer the server's
  // resolved Prime, else the operative whose role is `prime`.
  const prime =
    agents.find((a) => a.role?.toLowerCase() === "prime") ?? (company.prime ?? undefined);
  // The rest of the Crew, minus the Founder + Prime (shown as their own cards).
  const rest = agents.filter(
    (a) => a.role !== "founder" && a.agent_id !== (prime?.agent_id ?? ""),
  );
  // Separate pending hires (awaiting approval/Clearance) from active Crew so a
  // half-built team reads honestly.
  const pendingHires = rest.filter((a) => a.status === "pending");
  const activeCrew = rest.filter((a) => a.status !== "pending");
  // An Operative is runnable when its bound Rig probes available; count it
  // across the active company (Founder + Prime + Operatives) for the roster
  // readiness line. A runnable adapter is what lets an Operative execute Briefs.
  const runnableOf = (a?: Agent) =>
    !!a?.rig && byName.get(a.rig)?.probe?.status === "available";
  const activeAll = agents.filter((a) => a.status !== "pending" && a.status !== "disabled");
  const runnableCount = activeAll.filter(runnableOf).length;
  // Authoritative status total from the roster summary, falling back to the
  // live agent list when that endpoint is unavailable.
  const rosterTotal =
    typeof roster.total === "number" ? roster.total : agents.length;
  // Resolve a boss agent_id → display name for the reporting line.
  const nameOf = (id?: string | null) => {
    if (!id) return null;
    const a = agents.find((x) => x.agent_id === id);
    return a?.name ?? id.slice(0, 8);
  };

  // The Operative the URL points at (if any). Resolved across the operatives
  // list plus the server-resolved Founder/Prime (which may be carried only on
  // `company.{founder,prime}`), so selecting leadership opens the workbench too.
  // When `?agent=` names an id that isn't in the Crew at all, we render an honest
  // banner rather than silently showing nothing — see `unknownSelection`.
  const selectedAgent = openId
    ? agents.find((a) => a.agent_id === openId) ??
      [founder, prime].find((a) => a?.agent_id === openId) ??
      undefined
    : undefined;
  const unknownSelection = !!openId && !loading && initialized && !selectedAgent;

  async function initCompany() {
    setBanner(null);
    setBusy(true);
    try {
      const r = await api.post<{ founder?: Agent; created?: boolean }>("/v1/spine/company/init", {
        name: founderName.trim() || "Founder",
        rig: founderRig || "echo",
      });
      setBanner({
        kind: "ok",
        msg: r.created
          ? `Company initialized — Founder "${r.founder?.name}" created on adapter ${r.founder?.rig}.`
          : `Company already initialized — Founder "${r.founder?.name}" is in place.`,
      });
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Initialize failed" });
    } finally {
      setBusy(false);
    }
  }

  // First-run safe-local on-ramp (company-model §12.6): ensure the Founder +
  // a small echo-backed starter crew so a fresh company can run a real Shift
  // (propose → approve → start) without any external coding-agent auth.
  async function starterCrew() {
    setBanner(null);
    setBusy(true);
    try {
      const r = await api.post<{
        founder?: Agent;
        founder_created?: boolean;
        rig?: string;
        crew?: { role?: string; created?: boolean }[];
      }>("/v1/spine/company/starter-crew", { rig: "echo" });
      const made = (r.crew ?? []).filter((c) => c.created).map((c) => c.role).join(", ");
      const roles = (r.crew ?? []).map((c) => c.role).join(", ");
      setBanner({
        kind: "ok",
        msg: made
          ? `Starter crew ready — safe local Operatives (${made}) on the echo adapter. Ask Prime to plan, then Start the work.`
          : `Starter crew already in place (${roles}) on the echo adapter.`,
      });
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Starter crew failed" });
    } finally {
      setBusy(false);
    }
  }

  // Greenlight a pending hire directly (company-model §12.6): approve + bind
  // the safe-local `echo` Rig atomically so the now-active Operative is
  // immediately runnable. This is the governed `route=direct` affordance — a
  // clearance-gated hire is refused server-side, and we surface that honestly
  // with a pointer to decide its Clearance on Mandates.
  async function approveHire(agentId: string, name?: string) {
    setBanner(null);
    setBusy(true);
    try {
      const r = await api.post<{ runnable?: boolean; rig?: string; needs_rig?: boolean }>(
        `/v1/agents/${encodeURIComponent(agentId)}/approve-hire`,
        { rig: "echo" },
      );
      setBanner({
        kind: "ok",
        msg: r.needs_rig
          ? `${name ?? "Operative"} hired — set an adapter to make it runnable.`
          : `${name ?? "Operative"} hired and runnable on the ${r.rig ?? "echo"} adapter.`,
      });
      reload();
    } catch (e) {
      const msg = e instanceof Error ? e.message : "Approve hire failed";
      setBanner({
        kind: "err",
        msg: /clearance/i.test(msg)
          ? `${msg} — this hire needs a Clearance; decide it on the Mandates page.`
          : msg,
      });
    } finally {
      setBusy(false);
    }
  }

  // Decline a pending hire (pending → disabled). The role stays unfilled so the
  // team plan can re-propose or the operator can hire someone else.
  async function rejectHire(agentId: string, name?: string) {
    setBanner(null);
    setBusy(true);
    try {
      await api.post(`/v1/agents/${encodeURIComponent(agentId)}/reject-hire`, {});
      setBanner({ kind: "ok", msg: `${name ?? "Hire"} declined — the role is left unfilled.` });
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Reject hire failed" });
    } finally {
      setBusy(false);
    }
  }

  async function setRig(agentId: string, rig: string) {
    const adapter = byName.get(rig);
    const avail = adapter?.probe?.status === "available";
    if (rig && !avail) {
      const label = STATUS_LABEL[adapter?.probe?.status ?? ""] ?? "unavailable";
      if (!confirm(`Adapter "${rig}" is ${label}. Assign it anyway? Runs will be refused until it is ready.`)) {
        reload();
        return;
      }
    }
    setBanner(null);
    try {
      await api.patch(`/v1/agents/${encodeURIComponent(agentId)}`, { rig });
      setBanner({ kind: "ok", msg: `Adapter set to ${rig || "(none)"}.` });
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Update failed" });
    }
  }

  function rigStatusCell(rig?: string | null) {
    if (!rig) return <span className="muted">no adapter</span>;
    const a = byName.get(rig);
    const status = a?.probe?.status ?? "unknown";
    const ok = status === "available";
    return (
      <span>
        <span className={"badge " + (ok ? "done" : "blocked")}>{STATUS_LABEL[status] ?? status}</span>
        {!ok && a?.probe?.install_hint && (
          <div className="muted" style={{ fontSize: 11, marginTop: 3 }}>{a.probe.install_hint}</div>
        )}
      </span>
    );
  }

  // Read-only render of one Operative's governance panel — the §9 per-agent
  // permission "face on machinery that already exists": Keys (org/work powers +
  // caps), Capability powers (risk ceiling + category/secret/surface gates the
  // admission pipeline enforces), and Standing approvals (pre-granted
  // clearances). Each group degrades to an honest empty state on its own.
  function operativeDetail(agentId: string) {
    const d = detailCache[agentId];
    if (!d) return <div className="loading" style={{ fontSize: 12 }}>Loading permissions…</div>;
    const { keys: k, detail, standing } = d;
    const flag = (on?: boolean, scope?: string) =>
      on ? <span className="badge done" style={{ fontSize: 9 }}>yes{scope ? ` · ${scope}` : ""}</span> : <span className="badge backlog" style={{ fontSize: 9 }}>no</span>;
    // Render a category/tag set as small chips, or an em-dash when empty.
    const chips = (vals?: string[], cls = "backlog") =>
      vals && vals.length
        ? <span className="pill-row" style={{ display: "inline-flex" }}>{vals.map((v) => <span key={v} className={"badge " + cls} style={{ fontSize: 9 }}>{v}</span>)}</span>
        : <span className="muted">—</span>;
    return (
      <div className="op-detail">
        {/* Org & work powers (Keys). */}
        <div className="op-group">
          <div className="op-group-title">Keys — org &amp; work powers</div>
          {!k ? (
            <div className="muted" style={{ fontSize: 12 }}>No Keys recorded for this Operative.</div>
          ) : (
            <div className="kv-grid" style={{ fontSize: 12 }}>
              <div className="kv"><span className="muted">Spawn agents</span><span>{flag(k.can_spawn_agents, k.spawn_route)}</span></div>
              <div className="kv"><span className="muted">Assign work</span><span>{flag(k.can_assign_work, k.assign_scope)}</span></div>
              <div className="kv"><span className="muted">Manage work</span><span>{flag(k.can_manage_work, k.manage_scope)}</span></div>
              <div className="kv"><span className="muted">Configure agents</span><span>{flag(k.can_configure_agents, k.configure_scope)}</span></div>
              <div className="kv"><span className="muted">Wake</span><span>{k.wake_on_timer ? "timer " : ""}{k.wake_on_demand ? "on-demand" : ""}{!k.wake_on_timer && !k.wake_on_demand ? "—" : ""}</span></div>
              <div className="kv"><span className="muted">Max concurrent runs</span><span>{k.max_concurrent_runs ?? "—"}</span></div>
              <div className="kv"><span className="muted">Monthly Allowance</span><span>{k.monthly_allowance_cents != null ? fmtCents(k.monthly_allowance_cents) : "—"}</span></div>
              <div className="kv"><span className="muted">Secret allowlist</span><span>{(k.secret_allowlist?.length ?? 0) > 0 ? `${k.secret_allowlist!.length} entr${k.secret_allowlist!.length === 1 ? "y" : "ies"}` : "none"}</span></div>
            </div>
          )}
        </div>

        {/* Capability powers — the admission-gate inputs (risk/categories/surfaces). */}
        <div className="op-group">
          <div className="op-group-title">Capability powers</div>
          {!detail ? (
            <div className="muted" style={{ fontSize: 12 }}>Capability detail unavailable for this Operative.</div>
          ) : (
            <div className="kv-grid" style={{ fontSize: 12 }}>
              <div className="kv"><span className="muted">Risk ceiling</span><span>{detail.risk_ceiling ? <span className="badge in_review" style={{ fontSize: 9 }}>{detail.risk_ceiling}</span> : "—"}</span></div>
              <div className="kv"><span className="muted">Approval timeout</span><span>{detail.approval_timeout_secs ? `${detail.approval_timeout_secs}s` : "—"}</span></div>
              <div className="kv"><span className="muted">Allowed categories</span><span>{chips(detail.allow_categories, "done")}</span></div>
              <div className="kv"><span className="muted">Denied categories</span><span>{chips(detail.deny_categories, "blocked")}</span></div>
              <div className="kv"><span className="muted">Always needs approval</span><span>{chips(detail.approval_required_categories, "in_progress")}</span></div>
              <div className="kv"><span className="muted">Surface allowlist</span><span>{chips(detail.surface_allowlist)}</span></div>
              <div className="kv"><span className="muted">Allowed sensitivity</span><span>{chips(detail.allow_sensitivity_tags, "done")}</span></div>
              <div className="kv"><span className="muted">Denied sensitivity</span><span>{chips(detail.deny_sensitivity_tags, "blocked")}</span></div>
            </div>
          )}
        </div>

        {/* Standing approvals — pre-granted clearances + their usage. */}
        <div className="op-group">
          <div className="op-group-title">Standing approvals{standing.length ? ` (${standing.length})` : ""}</div>
          {standing.length === 0 ? (
            <div className="muted" style={{ fontSize: 12 }}>No standing approvals — every gated action prompts for a fresh clearance.</div>
          ) : (
            <div className="table-scroll">
              <table className="table" style={{ fontSize: 12 }}>
                <thead><tr><th>Category</th><th>Scope</th><th>Used</th><th>Expires</th><th>Granted by</th></tr></thead>
                <tbody>
                  {standing.map((s, i) => (
                    <tr key={s.standing_id || i}>
                      <td>{s.match_category || "—"}{s.method_prefix ? <div className="mono" style={{ fontSize: 10 }}>{s.method_prefix}</div> : null}</td>
                      <td className="dim">{s.scope_kind || "—"}{s.workspace_path_glob ? <div className="muted" style={{ fontSize: 10 }}>{s.workspace_path_glob}</div> : null}</td>
                      <td>{s.calls_used ?? 0}{s.max_calls != null ? ` / ${s.max_calls}` : ""}</td>
                      <td className="muted">{fmtWhen(s.expires_at)}</td>
                      <td className="muted">{s.granted_by ? s.granted_by.slice(0, 12) : "—"}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </div>
    );
  }

  // The tabbed Operative-detail workbench — the §9 employee record / command
  // surface. A single prominent panel (not a row expansion) for the selected
  // Operative: an identity header, a tab bar, and a tab body. Every figure
  // comes from data already loaded for the page (the shared detail cache,
  // `/v1/runs`, the board columns) — no extra fetch loops, and each piece
  // degrades to an honest empty state when its source is unavailable.
  function workbench(a: Agent) {
    const id = a.agent_id ?? "";
    const d = detailCache[id];
    const detail = d?.detail ?? null;
    const keys = d?.keys ?? null;
    // Org neighbours from the already-loaded roster.
    const directReports = agents.filter((x) => x.reports_to === id);
    // This Operative's Shifts, newest first, from the page's `/v1/runs` payload.
    const agentRuns = runs
      .filter((r) => r.agent_id === id)
      .sort((x, y) => (y.started_at ?? 0) - (x.started_at ?? 0));
    // Open assigned Briefs (todo / in progress / in review) from the board fetch.
    const assigned = work.filter((c) => c.assignee_agent_id === id);
    const openCount = workload.get(id) ?? 0;
    const runningCount = running.get(id) ?? 0;
    // Allowance + ceilings: prefer the full detail, fall back to Keys.
    const allowanceCents = detail?.monthly_allowance_cents ?? keys?.monthly_allowance_cents ?? null;
    const maxConcurrent = detail?.max_concurrent_runs ?? keys?.max_concurrent_runs ?? null;
    const wakeTimer = detail?.wake_on_timer ?? keys?.wake_on_timer ?? false;
    const wakeDemand = detail?.wake_on_demand ?? keys?.wake_on_demand ?? false;
    const roleLabel = a.role ?? detail?.role ?? a.title ?? "operative";
    // The Operative's charter (instruction bundle), surfaced read-only from the
    // full-profile `agent.keys` read. Empty string when none is stored yet.
    const charter = keys?.instruction_bundle ?? "";
    const charterLines = charter ? charter.split("\n").length : 0;

    const briefLink = (c: Card) => {
      const bid = c.task_id ?? c.id ?? "";
      return (
        <Link to="/briefs" className="link" title={bid}>
          {c.title || (bid ? bid.slice(0, 10) : "Brief")}
        </Link>
      );
    };

    return (
      <div className="card op-wb" ref={selectedRef as React.RefObject<HTMLDivElement>}>
        {/* Identity header — who this Operative is, at a glance. */}
        <div className="op-wb-head">
          <div className="op-wb-id">
            <div className="row" style={{ gap: 8, flexWrap: "wrap" }}>
              <strong style={{ fontSize: 15 }}>{a.name ?? id.slice(0, 10) ?? "Operative"}</strong>
              <span className="badge in_review" style={{ fontSize: 9 }}>{roleLabel}</span>
              <Badge status={a.status ?? "active"} />
              {runningCount > 0 && <span className="badge in_progress" style={{ fontSize: 9 }}>● running</span>}
            </div>
            <div className="muted" style={{ fontSize: 11, marginTop: 4, display: "flex", gap: 12, flexWrap: "wrap" }}>
              <span className="mono">{id.slice(0, 16)}</span>
              {a.reports_to && (
                <span>
                  reports to{" "}
                  <span className="link" onClick={() => setOpen(a.reports_to ?? null)}>{nameOf(a.reports_to)}</span>
                </span>
              )}
              <span>adapter {a.rig || "(none)"}</span>
            </div>
          </div>
          <div className="row" style={{ gap: 6 }}>
            <button className="btn ghost sm" title="Copy a deep link to this Operative" onClick={() => copyLink(id)}>Copy link</button>
            <button className="btn ghost sm" onClick={() => setOpen(null)}>Close</button>
          </div>
        </div>

        {/* Tab bar. */}
        <div className="op-tabs" role="tablist">
          {TABS.map((t) => (
            <button
              key={t}
              role="tab"
              aria-selected={activeTab === t}
              className={"op-tab" + (activeTab === t ? " active" : "")}
              onClick={() => setTab(t)}
            >
              {TAB_LABEL[t]}
              {t === "runs" && agentRuns.length > 0 && <span className="op-tab-n">{agentRuns.length}</span>}
            </button>
          ))}
        </div>

        <div className="op-tab-body">
          {!d ? (
            <div className="loading" style={{ fontSize: 12 }}>Loading {a.name ?? "Operative"}…</div>
          ) : activeTab === "permissions" ? (
            operativeDetail(id)
          ) : activeTab === "overview" ? (
            <div className="op-detail">
              <div className="op-group">
                <div className="op-group-title">Summary</div>
                <div className="kv-grid" style={{ fontSize: 12 }}>
                  <div className="kv"><span className="muted">Role</span><span>{roleLabel}</span></div>
                  <div className="kv"><span className="muted">Title</span><span>{a.title || detail?.title || "—"}</span></div>
                  <div className="kv"><span className="muted">Status</span><span><Badge status={a.status ?? "active"} /></span></div>
                  <div className="kv"><span className="muted">Adapter (Rig)</span><span>{a.rig || "(none)"}</span></div>
                  <div className="kv"><span className="muted">Readiness</span><span>{rigStatusCell(a.rig)}</span></div>
                  <div className="kv"><span className="muted">Reports to</span><span>{a.reports_to ? <span className="link" onClick={() => setOpen(a.reports_to ?? null)}>{nameOf(a.reports_to)}</span> : "—"}</span></div>
                  <div className="kv"><span className="muted">Direct reports</span><span>{directReports.length}</span></div>
                  <div className="kv"><span className="muted">Pressure</span><span>{openCount} open · {runningCount} running</span></div>
                </div>
              </div>

              {directReports.length > 0 && (
                <div className="op-group">
                  <div className="op-group-title">Direct reports ({directReports.length})</div>
                  <div className="ln-reports">
                    {directReports.map((r) => (
                      <button key={r.agent_id} className="ln-report link" onClick={() => setOpen(r.agent_id ?? null)}>
                        {r.name ?? (r.agent_id ?? "").slice(0, 10)}
                        <span className="muted" style={{ fontSize: 11 }}>{r.role ?? r.title ?? ""}</span>
                      </button>
                    ))}
                  </div>
                </div>
              )}

              <div className="op-group">
                <div className="op-group-title">Assigned Briefs ({assigned.length})</div>
                {assigned.length === 0 ? (
                  <div className="muted" style={{ fontSize: 12 }}>No open Briefs assigned (todo / in progress / in review).</div>
                ) : (
                  <div style={{ fontSize: 12 }}>
                    {assigned.map((c, i) => (
                      <div key={(c.task_id ?? c.id ?? "") + i} className="op-line">
                        {c.board_status && <span className={"badge " + (c.board_status ?? "todo")} style={{ fontSize: 9 }}>{c.board_status}</span>}
                        {briefLink(c)}
                        {c.priority && <span className="muted" style={{ fontSize: 10 }}>· {c.priority}</span>}
                      </div>
                    ))}
                  </div>
                )}
              </div>

              <div className="op-group">
                <div className="op-group-title">Recent Shifts</div>
                {agentRuns.length === 0 ? (
                  <div className="muted" style={{ fontSize: 12 }}>No Shifts recorded for this Operative in the recent run ledger.</div>
                ) : (
                  <div style={{ fontSize: 12 }}>
                    {agentRuns.slice(0, 3).map((r, i) => (
                      <div key={r.run_id ?? i} className="op-line">
                        <span className={"badge " + (RUN_TONE[r.status ?? ""] ?? "todo")} style={{ fontSize: 9 }}>{r.status ?? "—"}</span>
                        <span className="badge backlog" style={{ fontSize: 9 }}>{runTrigger(r.trigger)}</span>
                        <span className="muted">{runDuration(r)}</span>
                        {r.run_id && <Link to={`/runs?run=${encodeURIComponent(r.run_id)}`} className="link">{r.run_id.slice(0, 10)} ↗</Link>}
                        <span className="muted" style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", maxWidth: 240 }}>{r.summary || ""}</span>
                      </div>
                    ))}
                    {agentRuns.length > 3 && (
                      <button className="btn ghost sm" style={{ marginTop: 6 }} onClick={() => setTab("runs")}>See all {agentRuns.length} →</button>
                    )}
                  </div>
                )}
              </div>
            </div>
          ) : activeTab === "instructions" ? (
            // Instructions — the Operative's charter / instruction bundle
            // (company-model §4.5), read-only. Sourced from the full-profile
            // `agent.keys` read; editing stays out of this slice (it flows
            // through the configure-gated PATCH and has governance implications).
            <div className="op-detail">
              <div className="op-group">
                <div className="op-group-title">Charter (instruction bundle)</div>
                {!charter ? (
                  <div className="muted" style={{ fontSize: 12 }}>
                    No charter stored for this Operative yet. A charter is operator-authored markdown
                    (its job description) that, when set, is composed into the prompt of every Shift this
                    Operative runs. Set one via <span className="mono">PATCH /v1/agents/:id</span>.
                  </div>
                ) : (
                  <>
                    <div className="muted" style={{ fontSize: 11, marginBottom: 6 }}>
                      {charter.length.toLocaleString()} char{charter.length === 1 ? "" : "s"} · {charterLines} line{charterLines === 1 ? "" : "s"} ·
                      injected into this Operative's Shifts as a trusted charter section.
                    </div>
                    {/* Rendered as plain preformatted text — never as HTML — so a
                        charter can't inject markup. Bounded + scrollable when long. */}
                    <pre className="op-charter">{charter}</pre>
                  </>
                )}
              </div>
            </div>
          ) : activeTab === "runs" ? (
            <div className="op-detail">
              <div className="op-group">
                <div className="op-group-title">Recent Shifts ({agentRuns.length})</div>
                {agentRuns.length === 0 ? (
                  <div className="muted" style={{ fontSize: 12 }}>
                    No Shifts recorded for this Operative in the recent run ledger. This list is bounded to the
                    recent <span className="mono">/v1/runs</span> window — older Shifts live on the Runs page.
                  </div>
                ) : (
                  <div className="table-scroll">
                    <table className="table" style={{ fontSize: 12 }}>
                      <thead><tr><th>Status</th><th>Trigger</th><th>Rig</th><th>Brief</th><th>Duration</th><th>Started</th><th>Result</th><th></th></tr></thead>
                      <tbody>
                        {agentRuns.map((r, i) => (
                          <tr key={r.run_id ?? i}>
                            <td><span className={"badge " + (RUN_TONE[r.status ?? ""] ?? "todo")} style={{ fontSize: 9 }}>{r.status ?? "—"}</span></td>
                            <td><span className="badge backlog" style={{ fontSize: 9 }}>{runTrigger(r.trigger)}</span></td>
                            <td className="muted">{r.rig || "—"}</td>
                            <td className="mono" style={{ fontSize: 11 }}>{(r.brief_id ?? "").slice(0, 10) || "—"}</td>
                            <td className="muted">{runDuration(r)}</td>
                            <td className="muted">{r.started_at ? new Date(r.started_at * 1000).toLocaleString() : "—"}</td>
                            <td className="muted" style={{ maxWidth: 220, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{r.summary || (r.review ? r.review : "—")}</td>
                            <td>{r.run_id && <Link to={`/runs?run=${encodeURIComponent(r.run_id)}`} className="link">open ↗</Link>}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                )}
              </div>
            </div>
          ) : activeTab === "budget" ? (
            <div className="op-detail">
              <div className="op-group">
                <div className="op-group-title">Allowance &amp; ceilings (committed)</div>
                {!keys && !detail ? (
                  <div className="muted" style={{ fontSize: 12 }}>Budget detail unavailable for this Operative.</div>
                ) : (
                  <div className="kv-grid" style={{ fontSize: 12 }}>
                    <div className="kv"><span className="muted">Monthly Allowance (committed)</span><span>{allowanceCents != null ? fmtCents(allowanceCents) : "—"}</span></div>
                    <div className="kv"><span className="muted">Risk ceiling</span><span>{detail?.risk_ceiling ? <span className="badge in_review" style={{ fontSize: 9 }}>{detail.risk_ceiling}</span> : "—"}</span></div>
                    <div className="kv"><span className="muted">Max concurrent runs</span><span>{maxConcurrent ?? "—"}</span></div>
                    <div className="kv"><span className="muted">Approval timeout</span><span>{detail?.approval_timeout_secs ? `${detail.approval_timeout_secs}s` : "—"}</span></div>
                  </div>
                )}
              </div>
              <div className="op-group">
                <div className="op-group-title">Live spend</div>
                <p className="muted" style={{ fontSize: 12, margin: 0 }}>
                  The figure above is the <strong>committed</strong> monthly cap (capacity reserved), not money spent.
                  Real month-to-date spend vs Allowance — the same ledger the dispatch gate enforces — is on the{" "}
                  <Link to="/costs" className="link">Costs page</Link>. This tab never fabricates a spend number.
                </p>
              </div>
            </div>
          ) : (
            // Configuration.
            <div className="op-detail">
              <div className="op-group">
                <div className="op-group-title">Adapter (Rig)</div>
                <div className="row" style={{ gap: 12, alignItems: "center", flexWrap: "wrap" }}>
                  {rigSelect(a)}
                  {rigStatusCell(a.rig)}
                </div>
              </div>
              <div className="op-group">
                <div className="op-group-title">Autonomy</div>
                {!detail && !keys ? (
                  <div className="muted" style={{ fontSize: 12 }}>Autonomy detail unavailable for this Operative.</div>
                ) : (
                  <div className="kv-grid" style={{ fontSize: 12 }}>
                    <div className="kv"><span className="muted">Scheduled heartbeat</span><span>{wakeTimer ? <span className="badge done" style={{ fontSize: 9 }}>on</span> : <span className="badge backlog" style={{ fontSize: 9 }}>off</span>}</span></div>
                    <div className="kv"><span className="muted">Wake on assignment</span><span>{wakeDemand ? <span className="badge done" style={{ fontSize: 9 }}>on</span> : <span className="badge backlog" style={{ fontSize: 9 }}>off</span>}</span></div>
                    <div className="kv"><span className="muted">Max concurrent runs</span><span>{maxConcurrent ?? "—"}</span></div>
                  </div>
                )}
              </div>
              <div className="op-group">
                <div className="op-group-title">Org placement &amp; identity</div>
                <div className="kv-grid" style={{ fontSize: 12 }}>
                  <div className="kv"><span className="muted">Title</span><span>{a.title || detail?.title || "—"}</span></div>
                  <div className="kv"><span className="muted">Department</span><span>{detail?.department || "—"}</span></div>
                  <div className="kv"><span className="muted">Team</span><span>{detail?.team || "—"}</span></div>
                  <div className="kv"><span className="muted">Reports to</span><span>{a.reports_to ? <span className="link" onClick={() => setOpen(a.reports_to ?? null)}>{nameOf(a.reports_to)}</span> : "—"}</span></div>
                  <div className="kv"><span className="muted">Identity (subject)</span><span className="mono" style={{ fontSize: 11 }}>{detail?.subject_id ? detail.subject_id.slice(0, 16) : "—"}</span></div>
                  <div className="kv"><span className="muted">Created</span><span>{fmtDateTime(detail?.created_at)}</span></div>
                  <div className="kv"><span className="muted">Updated</span><span>{fmtDateTime(detail?.updated_at)}</span></div>
                </div>
              </div>
              <div className="op-group">
                <div className="op-group-title">Charter &amp; model</div>
                <p className="muted" style={{ fontSize: 12, margin: 0 }}>
                  The instruction bundle (job description / charter) is now surfaced read-only on the{" "}
                  <span className="link" onClick={() => setTab("instructions")}>Instructions</span> tab —
                  the <span className="mono">agent.keys</span> read carries it. Per-Operative model config and
                  Skills are not exposed by the read API yet, and editing the charter (governed by{" "}
                  <span className="mono">PATCH /v1/agents/:id</span>, the configure gate) stays out of this surface.
                </p>
              </div>
            </div>
          )}
        </div>
      </div>
    );
  }

  function rigSelect(a: Agent) {
    const id = a.agent_id ?? "";
    return (
      <select
        className="select"
        style={{ fontSize: 12, padding: "3px 6px", minWidth: 120 }}
        value={a.rig ?? ""}
        onChange={(e) => setRig(id, e.target.value)}
      >
        <option value="">(none)</option>
        {adapters.map((ad) => {
          const av = ad.probe?.status === "available";
          return (
            <option key={ad.name} value={ad.name}>
              {ad.name}{av ? "" : " ⚠"}
            </option>
          );
        })}
      </select>
    );
  }

  // First-run: no Founder yet. Make the path forward obvious.
  if (!loading && !initialized) {
    return (
      <Section title="Crew">
        {error && <div className="banner err">{error}</div>}
        {banner && <div className={"banner " + banner.kind}>{banner.msg}</div>}
        <div className="card setup-card" style={{ maxWidth: 620 }}>
          <div className="setup-step">First-run setup</div>
          <h3 style={{ marginTop: 4 }}>Initialize your company</h3>
          <p className="muted" style={{ marginTop: -4 }}>
            Relix has no Operatives yet. Create the <strong>Founder</strong> — the first Operative who
            can own Briefs, run them through an adapter, and hire the rest of the team.
          </p>
          <label className="field">
            <span>Founder name</span>
            <input className="input" value={founderName} onChange={(e) => setFounderName(e.target.value)} placeholder="Founder" />
          </label>
          <label className="field">
            <span>Default adapter (Rig)</span>
            <select className="select" value={founderRig} onChange={(e) => setFounderRig(e.target.value)}>
              <option value="echo">echo — built-in, always available</option>
              {adapters
                .filter((a) => a.name && a.name !== "echo")
                .map((a) => {
                  const av = a.probe?.status === "available";
                  return (
                    <option key={a.name} value={a.name}>
                      {a.name}{av ? "" : " ⚠ (" + (STATUS_LABEL[a.probe?.status ?? ""] ?? "unavailable") + ")"}
                    </option>
                  );
                })}
            </select>
          </label>
          <p className="muted" style={{ fontSize: 12 }}>
            {availCount
              ? `${availCount}/${adapters.length} adapter(s) available. echo is recommended to start — switch the Founder to a coding agent once it is installed + logged in.`
              : "echo is recommended to start. Install + log in to a coding-agent CLI (Claude, Codex) on the Settings page to use a real adapter."}
          </p>
          <div className="row" style={{ marginTop: 6, gap: 8, flexWrap: "wrap" }}>
            <button className="btn" onClick={initCompany} disabled={busy}>
              {busy ? "Working…" : "Initialize Company"}
            </button>
            <button className="btn ghost" onClick={starterCrew} disabled={busy}>
              {busy ? "Working…" : "Set up starter crew (local · echo)"}
            </button>
          </div>
          <p className="muted" style={{ fontSize: 12, marginTop: 6 }}>
            <strong>Starter crew</strong> also creates a couple of safe, local <em>echo</em> Operatives
            (an Engineer + a Designer) so you can immediately Ask Prime to plan, then <em>Start the
            work</em> and watch a real Shift complete — no external coding-agent login needed. These are
            clearly-labelled local/demo workers, not Claude or Codex.
          </p>
        </div>
      </Section>
    );
  }

  return (
    <Section title="Crew">
      {error && <div className="banner err">{error}</div>}
      {banner && <div className={"banner " + banner.kind}>{banner.msg}</div>}
      {/* A deep link (`?agent=<id>`) that doesn't match any current Operative —
          a stale/shared link or a since-removed hire. Stay calm and honest, and
          give a one-click way to clear the selection. */}
      {unknownSelection && (
        <div className="banner info banner-action">
          <span>
            No Operative matches <span className="mono">{openId?.slice(0, 16)}</span> in this Guild —
            it may have been removed, or the link is stale.
          </span>
          <span className="banner-cta link" onClick={() => setOpen(null)}>Clear selection</span>
        </div>
      )}
      <div className={"banner " + (availCount ? "ok" : "info") + " banner-action"}>
        <span>
          {availCount
            ? `${availCount}/${adapters.length} agent adapter(s) available — an Operative with an available adapter can execute Briefs.`
            : "No agent adapters available. Install + log in to a coding-agent CLI (Claude, Codex). echo always works for testing."}
        </span>
        <Link to="/settings" className="banner-cta">Adapters →</Link>
      </div>

      {/* The selected Operative's detail workbench (dashboard-design §9) — a
          prominent employee-record / command surface, not a row expansion. It
          opens from any View button, a Lattice deep link, or `?agent=<id>`, and
          scrolls into view. The roster below keeps the matching row highlighted. */}
      {selectedAgent && <div className="op-wb-wrap">{workbench(selectedAgent)}</div>}

      {/* Roster at a glance — the company's shape: leadership tiers, active
          Operatives, pending hires, and how many are runnable right now. */}
      <div className="card roster-strip">
        <div className="roster-tier">
          <span className="stat">{rosterTotal}</span>
          <span className="stat-label">Crew total</span>
        </div>
        <div className="roster-tier">
          <span className="stat">{founder ? 1 : 0}{prime ? <span className="muted" style={{ fontSize: 13 }}> + 1</span> : null}</span>
          <span className="stat-label">{prime ? "Founder + Prime (leadership)" : "Founder (leadership)"}</span>
        </div>
        <div className="roster-tier">
          <span className="stat">{activeCrew.length}</span>
          <span className="stat-label">Active Operatives</span>
        </div>
        <div className="roster-tier">
          <span className="stat">{pendingHires.length}</span>
          <span className="stat-label">Pending hires</span>
        </div>
        <div className="roster-tier">
          <span className="stat">{runnableCount}<span className="muted" style={{ fontSize: 13 }}> / {activeAll.length}</span></span>
          <span className="stat-label">Runnable now</span>
        </div>
      </div>

      {/* Guild Allowance — the committed monthly budget across the Crew. */}
      <div className="card" style={{ padding: "10px 14px" }}>
        <div className="row">
          <span className="muted">Guild Allowance (committed)</span>
          <span className="spacer" style={{ flex: 1 }} />
          <strong>{fmtCents(data?.allowance)}</strong>
          <span className="muted" style={{ fontSize: 11, marginLeft: 8 }}>
            sum of per-Operative monthly caps · per-Operative limits are in each row's Permissions
          </span>
        </div>
      </div>

      {/* Founder — shown separately as the org root. */}
      {founder && (
        <div className={"card" + (openId === founder.agent_id ? " selected" : "")}>
          <h3>Founder</h3>
          <div className="row wrap" style={{ gap: 18, alignItems: "flex-start" }}>
            <div>
              <div className="row" style={{ gap: 8 }}>
                <strong>{founder.name ?? "Founder"}</strong>
                <span className="badge done">Founder</span>
                <Badge status={founder.status ?? "active"} />
              </div>
              <div className="mono" style={{ fontSize: 11, marginTop: 4 }}>{(founder.agent_id ?? "").slice(0, 16)}</div>
            </div>
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Adapter</div>
              {rigSelect(founder)}
            </div>
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Readiness</div>
              {rigStatusCell(founder.rig)}
            </div>
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Workload</div>
              <span>{workload.get(founder.agent_id ?? "") ?? 0} open · {running.get(founder.agent_id ?? "") ?? 0} running</span>
            </div>
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Workbench</div>
              <button className="btn ghost sm" title="Open this Operative's detail workbench — Overview, Permissions, Runs, Budget, Configuration" onClick={() => toggleDetail(founder.agent_id ?? "")}>
                {openId === founder.agent_id ? "Close" : "Open"}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Prime — the Founder's planning lead, shown distinctly. */}
      {prime ? (
        <div className={"card" + (openId === prime.agent_id ? " selected" : "")}>
          <h3>Prime</h3>
          <div className="row wrap" style={{ gap: 18, alignItems: "flex-start" }}>
            <div>
              <div className="row" style={{ gap: 8 }}>
                <strong>{prime.name ?? "Prime"}</strong>
                <span className="badge in_progress">Prime</span>
                <Badge status={prime.status ?? "active"} />
              </div>
              <div className="mono" style={{ fontSize: 11, marginTop: 4 }}>{(prime.agent_id ?? "").slice(0, 16)}</div>
              {prime.reports_to && (
                <div className="muted" style={{ fontSize: 11, marginTop: 2 }}>reports to {nameOf(prime.reports_to)}</div>
              )}
            </div>
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Adapter</div>
              {rigSelect(prime)}
            </div>
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Readiness</div>
              {rigStatusCell(prime.rig)}
            </div>
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Workload</div>
              <span>{workload.get(prime.agent_id ?? "") ?? 0} open · {running.get(prime.agent_id ?? "") ?? 0} running</span>
            </div>
            <div>
              <div className="muted" style={{ fontSize: 11, marginBottom: 4 }}>Workbench</div>
              <button className="btn ghost sm" title="Open this Operative's detail workbench — Overview, Permissions, Runs, Budget, Configuration" onClick={() => toggleDetail(prime.agent_id ?? "")}>
                {openId === prime.agent_id ? "Close" : "Open"}
              </button>
            </div>
          </div>
        </div>
      ) : founder ? (
        // No dedicated `prime`-role Operative exists — and that's expected, not
        // a missing hire. "Prime" is primarily the planning/orchestration
        // surface the Founder drives (company-model §12.5: propose → approve →
        // start); the starter crew is Engineer + Designer, not a Prime
        // Operative. Present it honestly as a surface, not an unfinished member.
        // A dedicated Prime Operative (lexicon role `prime`) is optional — when
        // one is hired, the rich Prime card above replaces this.
        <div className="card" style={{ padding: "12px 14px" }}>
          <div className="row" style={{ marginBottom: 4 }}>
            <h3 style={{ margin: 0 }}>Prime</h3>
            <span className="badge in_progress" style={{ marginLeft: 8 }}>planning surface</span>
            <span className="spacer" style={{ flex: 1 }} />
            <Link to="/chat" className="link" style={{ fontSize: 12 }}>Ask Prime →</Link>
          </div>
          <p className="muted" style={{ fontSize: 12, margin: 0 }}>
            Prime is the company's planning &amp; orchestration surface, driven from Chat by the
            Founder: describe a goal and Prime proposes a Mandate + Briefs, you approve to create
            them, then Start the work. A dedicated Prime Operative isn't part of the starter crew
            and isn't required to plan — hire one only if you want a standing planning lead.
          </p>
        </div>
      ) : null}

      {/* Pending hires — operatives awaiting approval / Clearance. */}
      {pendingHires.length > 0 && (
        <div className="card">
          <div className="row" style={{ marginBottom: 8 }}>
            <h3 style={{ margin: 0 }}>Pending hires</h3>
            <span className="muted" style={{ fontSize: 12, marginLeft: 8 }}>
              {pendingHires.length} awaiting approval — approve to make it runnable, or decline
            </span>
            <span className="spacer" style={{ flex: 1 }} />
            <Link to="/mandates" className="link" style={{ fontSize: 12 }}>Clearances →</Link>
          </div>
          <div className="table-scroll">
            <table className="table">
              <thead><tr><th>Operative</th><th>Role</th><th>Reports to</th><th>Status</th><th style={{ textAlign: "right" }}>Decision</th></tr></thead>
              <tbody>
                {pendingHires.map((a, i) => {
                  const id = a.agent_id ?? "";
                  return (
                    <tr key={id || i}>
                      <td><strong>{a.name ?? id.slice(0, 10)}</strong><div className="mono" style={{ fontSize: 10 }}>{id.slice(0, 12)}</div></td>
                      <td className="dim">{a.role ?? a.title ?? "—"}</td>
                      <td className="muted">{nameOf(a.reports_to) ?? "—"}</td>
                      <td><Badge status={a.status ?? "pending"} /></td>
                      <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                        <button
                          className="btn sm"
                          disabled={busy || !id}
                          title="Approve this hire and bind the safe-local echo adapter so it is immediately runnable"
                          onClick={() => approveHire(id, a.name)}
                        >
                          Approve · echo
                        </button>
                        <button
                          className="btn ghost sm"
                          style={{ marginLeft: 6 }}
                          disabled={busy || !id}
                          title="Decline this hire (the role is left unfilled)"
                          onClick={() => rejectHire(id, a.name)}
                        >
                          Reject
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {/* Operatives roster (the active crew). */}
      <div className="card">
        <div className="row" style={{ marginBottom: 10 }}>
          <h3 style={{ margin: 0 }}>Operatives</h3>
          <div className="spacer" style={{ flex: 1 }} />
          <Link to="/briefs" className="link" style={{ fontSize: 12 }}>assign work →</Link>
        </div>
        {loading ? (
          <div className="loading">Loading crew…</div>
        ) : activeCrew.length === 0 ? (
          <Empty>No other active Operatives yet — the Founder/Prime can hire more as the company grows.</Empty>
        ) : (
          <div className="table-scroll">
            <table className="table">
              <thead>
                <tr>
                  <th>Operative</th>
                  <th>Role</th>
                  <th>Reports to</th>
                  <th>Status</th>
                  <th>Adapter (Rig)</th>
                  <th>Readiness</th>
                  <th>Open</th>
                  <th>Running</th>
                  <th>Workbench</th>
                </tr>
              </thead>
              <tbody>
                {activeCrew.map((a, i) => {
                  const id = a.agent_id ?? "";
                  return (
                    <tr key={id || i} className={openId === id ? "selected" : undefined}>
                      <td>
                        <strong>{a.name ?? id.slice(0, 10) ?? "operative"}</strong>
                        <div className="mono" style={{ fontSize: 10 }}>{id.slice(0, 12)}</div>
                      </td>
                      <td className="dim">{a.role ?? a.title ?? "—"}</td>
                      <td className="muted">{nameOf(a.reports_to) ?? "—"}</td>
                      <td><Badge status={a.status ?? "active"} /></td>
                      <td>{rigSelect(a)}</td>
                      <td>{rigStatusCell(a.rig)}</td>
                      <td>{workload.get(id) ?? 0}</td>
                      <td>
                        {(running.get(id) ?? 0) > 0
                          ? <span className="badge in_progress">{running.get(id)}</span>
                          : <span className="muted">0</span>}
                      </td>
                      <td>
                        <button className="btn ghost sm" onClick={() => toggleDetail(id)} title="Open this Operative's detail workbench — Overview, Permissions, Runs, Budget, Configuration">
                          {openId === id ? "Close" : "Open"}
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </Section>
  );
}
