import { useState } from "react";
import { Link } from "react-router-dom";
import {
  briefCost,
  guildSpend,
  tryGet,
  tryGetReport,
  type BriefCostRollup,
  type GetReport,
  type GuildSpend,
} from "../api";
import { Empty, Section, useAsync } from "../components/common";

// The Costs surface — dashboard-design §10 / company-model §6.6. Spend by Guild
// / Operative / Brief-tree, billing-code breakdown, and over-cap incidents — all
// from REAL backend data (the durable run ledger + configured Allowances + the
// observability metrics), never fabricated. Where a figure isn't exposed by a
// route yet, the section says so honestly (route + reason) instead of showing a
// fake zero. B&W aesthetic; color is reserved for semantic status (§12).
//
// The Guild budget card reads canonical **month-to-date** spend from the
// dedicated `guild.spend` route (`GET /v1/spine/guild/spend`) — the EXACT ledger
// figure + UTC-calendar-month window the autonomous Guild hard-stop enforces, so
// the card can never disagree with the gate. The per-agent "observed spend" table
// below remains separate operational telemetry from the observability metrics
// window (24h/7d/30d), explicitly distinguished from the governance month.

interface Guild {
  tenant_id?: string;
  display_name?: string;
  monthly_allowance_cents?: number | null;
  billing_code?: string | null;
}
interface Op { agent_id?: string; name?: string; role?: string; status?: string }
interface Keys { monthly_allowance_cents?: number }
interface ActionItem {
  id?: string;
  category?: string;
  severity?: string;
  title?: string;
  reason?: string;
  target_title?: string;
  route?: string;
  action_label?: string;
}
interface CompanyActions { actions?: ActionItem[] }
interface AgentMetric {
  agent?: string;
  invocations?: number;
  total_tokens?: number;
  total_cost_micros?: number;
  window_hours?: number;
}
interface BriefCard {
  task_id?: string;
  title?: string;
  board_status?: string;
  assignee_agent_id?: string | null;
}

// Committed Allowance arrives as a bare integer (cents) or an object; pull the
// first cents-like number defensively.
function committedCents(v: unknown): number | null {
  if (typeof v === "number") return v;
  if (typeof v === "string" && v.trim() && !Number.isNaN(Number(v))) return Number(v);
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
// Ledger / metrics figures are micro-USD (1,000,000 micros = $1).
function fmtMicros(m?: number | null): string {
  if (m == null) return "—";
  return "$" + (m / 1_000_000).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
}
function fmtDate(secs?: number): string {
  if (!secs) return "—";
  const d = new Date(secs * 1000);
  return Number.isNaN(d.getTime()) ? "—" : d.toLocaleDateString();
}
// Unix MS → a local date (the guild.spend window/reset fields are unix-ms).
function fmtDateMs(ms?: number | null): string {
  if (ms == null) return "—";
  const d = new Date(ms);
  return Number.isNaN(d.getTime()) ? "—" : d.toLocaleDateString();
}
// A YYYY-MM-DD date input → unix SECONDS at 00:00 UTC (matches the ledger's
// inclusive window math). Empty → undefined (server defaults to the month).
function dateToSecs(v: string): number | undefined {
  if (!v) return undefined;
  const ms = Date.parse(v + "T00:00:00Z");
  return Number.isNaN(ms) ? undefined : Math.floor(ms / 1000);
}

const SEV_TONE: Record<string, string> = { high: "blocked", medium: "in_progress", low: "backlog" };
const WINDOWS: { h: number; label: string }[] = [
  { h: 24, label: "24h" },
  { h: 168, label: "7d" },
  { h: 720, label: "30d" },
];

export function Costs() {
  // Spend-window selector for the observability metrics (distinct from the
  // governance calendar-month Allowance window — labelled as such below).
  const [hours, setHours] = useState(720);

  // Base reads: Guild budget, committed Allowance, the Crew + per-Operative
  // Keys (for allowance), and the Action Center (for budget/over-cap signals).
  const base = useAsync(async () => {
    const [guild, allowance, opsRaw, actions] = await Promise.all([
      tryGet<Guild>("/v1/spine/guild/detail", {}),
      tryGet<unknown>("/v1/spine/allowance/committed", {}),
      tryGet<Op[]>("/v1/spine/operatives", []),
      tryGet<CompanyActions | null>("/v1/spine/company/actions", null),
    ]);
    const ops = (Array.isArray(opsRaw) ? opsRaw : []).slice(0, 100);
    // Per-Operative monthly Allowance from each one's Keys (bounded fan-out).
    const keyPairs = await Promise.all(
      ops.map(async (o) => {
        if (!o.agent_id) return [undefined, null] as const;
        const k = await tryGet<Keys | null>(`/v1/spine/keys/${encodeURIComponent(o.agent_id)}`, null);
        return [o.agent_id, k] as const;
      }),
    );
    const allowanceByAgent = new Map<string, number | null>();
    for (const [id, k] of keyPairs) {
      if (id) allowanceByAgent.set(id, k?.monthly_allowance_cents ?? null);
    }
    return {
      guild: guild ?? {},
      committed: committedCents(allowance),
      ops,
      allowanceByAgent,
      actions: actions ?? null,
    };
  }, []);

  // Canonical month-to-date Guild spend (the gate's own ledger figure + UTC
  // calendar-month window). Reported so an unavailable route shows an honest
  // state — NEVER an approximation in its place.
  const spend = useAsync<GetReport<GuildSpend | null>>(async () => guildSpend.get(), []);

  // Observability metrics (per-agent observed spend over the selected window).
  // Reported so an unavailable/disabled metrics layer shows an honest state.
  const metrics = useAsync<GetReport<unknown>>(
    async () => tryGetReport<unknown>(`/v1/metrics/agents?hours=${hours}`, []),
    [hours],
  );

  const guild = base.data?.guild ?? {};
  const committed = base.data?.committed ?? null;
  const ops = base.data?.ops ?? [];
  const allowanceByAgent = base.data?.allowanceByAgent ?? new Map<string, number | null>();
  const actions = base.data?.actions;

  // Budget / over-cap signals — the authoritative live-spend governance items.
  const budgetItems = (actions?.actions ?? []).filter((a) => a.category === "budget");

  // Metrics rows + the spend join (metrics key on the agent's name/handle).
  const metricsReport = metrics.data;
  const metricRows: AgentMetric[] = Array.isArray(metricsReport?.data)
    ? (metricsReport!.data as AgentMetric[])
    : [];
  const metricsAvailable = Array.isArray(metricsReport?.data) && !metricsReport?.error;
  const spendByKey = new Map<string, number>();
  for (const r of metricRows) {
    if (r.agent) spendByKey.set(r.agent, (spendByKey.get(r.agent) ?? 0) + (r.total_cost_micros ?? 0));
  }
  const totalObservedMicros = metricRows.reduce((n, r) => n + (r.total_cost_micros ?? 0), 0);
  // Best-effort per-Operative spend: metrics key on the agent's name or id.
  const opSpend = (o: Op): number | null => {
    if (o.name && spendByKey.has(o.name)) return spendByKey.get(o.name) ?? null;
    if (o.agent_id && spendByKey.has(o.agent_id)) return spendByKey.get(o.agent_id) ?? null;
    return null;
  };

  const cap = guild.monthly_allowance_cents ?? null;
  const committedPct = cap && committed != null && cap > 0 ? Math.round((committed / cap) * 100) : null;

  // ── Canonical Guild month-to-date spend (guild.spend) ──────────────────────
  const gs = spend.data?.data ?? null;
  const spendErr = spend.data?.error ?? null;
  // Prefer the budget the canonical route reports; fall back to guild/detail's
  // cap so the budget figure still renders if only one route answers.
  const canonBudget = gs?.budget_cents ?? cap;
  const spentCents = gs?.spent_cents ?? null;
  const remainingCents = gs?.remaining_cents ?? null;
  const overBudget = gs?.over_budget ?? null;
  const spentPct =
    canonBudget && spentCents != null && canonBudget > 0
      ? Math.round((spentCents / canonBudget) * 100)
      : null;

  return (
    <Section title="Costs">
      {base.error && <div className="banner err">{base.error}</div>}

      {/* ── Guild budget — canonical month-to-date spend (guild.spend) ─── */}
      <div className="card">
        <div className="row" style={{ marginBottom: 10 }}>
          <h3 style={{ margin: 0 }}>Guild budget</h3>
          {(gs?.display_name || guild.display_name) && (
            <span className="muted" style={{ fontSize: 12, marginLeft: 8 }}>· {gs?.display_name || guild.display_name}</span>
          )}
          <div className="spacer" style={{ flex: 1 }} />
          {overBudget === true && <span className="badge blocked" style={{ fontSize: 9 }}>over budget</span>}
          {guild.billing_code && <span className="badge" style={{ fontSize: 9, marginLeft: 6 }}>billing: {guild.billing_code}</span>}
        </div>
        {base.loading || spend.loading ? (
          <div className="loading">Loading budget…</div>
        ) : (
          <>
            {/* Primary trio: budget vs ACTUAL month-to-date spend (canonical). */}
            <div className="grid cols-3">
              <div>
                <div className="stat">{canonBudget != null ? fmtCents(canonBudget) : "—"}</div>
                <div className="stat-label">Monthly Guild budget</div>
              </div>
              <div>
                <div className="stat">
                  {spentCents != null ? fmtCents(spentCents) : <span className="muted">—</span>}
                </div>
                <div className="stat-label">Spent this month{spentPct != null ? ` · ${spentPct}%` : ""}</div>
              </div>
              <div>
                <div className="stat" style={remainingCents != null && remainingCents < 0 ? { color: "var(--err)" } : undefined}>
                  {remainingCents != null ? fmtCents(remainingCents) : <span className="muted">—</span>}
                </div>
                <div className="stat-label">Remaining</div>
              </div>
            </div>

            {/* Spent-vs-budget progress (real spend; over-cap is a red bar). */}
            {canonBudget != null && spentCents != null && canonBudget > 0 ? (
              <div className="progress-bar" style={{ marginTop: 12 }}>
                <div
                  className="progress-fill"
                  style={{
                    width: `${Math.min(100, spentPct ?? 0)}%`,
                    background: overBudget ? "var(--err)" : (spentPct ?? 0) >= 90 ? "var(--warn)" : "var(--ok)",
                  }}
                />
              </div>
            ) : null}

            {/* Honest unavailable / no-budget / no-ledger states. */}
            {spendErr ? (
              <div className="muted" style={{ fontSize: 12, marginTop: 10 }}>
                Canonical Guild spend unavailable — {spendErr} (GET /v1/spine/guild/spend).
              </div>
            ) : spentCents == null ? (
              <div className="muted" style={{ fontSize: 12, marginTop: 10 }}>
                Month-to-date spend can't be computed — the metrics ledger is unavailable.
              </div>
            ) : canonBudget == null ? (
              <div className="muted" style={{ fontSize: 12, marginTop: 10 }}>
                No Guild monthly budget configured — set one to gate autonomous spend against a ceiling.
                {" "}({fmtCents(spentCents)} spent this month.)
              </div>
            ) : null}

            {/* Reset bookkeeping + the committed-Allowance planning figure
                (capacity reserved — DISTINCT from money already spent). */}
            <div className="muted" style={{ fontSize: 11, marginTop: 8 }}>
              {gs?.resets_at_ms != null && `Resets ${fmtDateMs(gs.resets_at_ms)} (UTC calendar month). `}
              Spend is canonical month-to-date from the run ledger — the same figure + window the autonomous Guild hard-stop enforces.
            </div>
            <div className="row" style={{ marginTop: 8, alignItems: "baseline", gap: 6 }}>
              <span className="muted" style={{ fontSize: 11 }}>Committed Allowance (sum of per-Operative caps, capacity reserved):</span>
              <strong style={{ fontSize: 12 }}>{fmtCents(committed)}</strong>
              {committedPct != null && <span className="muted" style={{ fontSize: 11 }}>· {committedPct}% of budget</span>}
            </div>
          </>
        )}
      </div>

      {/* ── Budget & over-cap signals (authoritative live figures) ── */}
      <div className="card">
        <div className="row" style={{ marginBottom: 10 }}>
          <h3 style={{ margin: 0 }}>Budget &amp; over-cap signals</h3>
          <div className="spacer" style={{ flex: 1 }} />
          <span className="muted" style={{ fontSize: 12 }}>computed from live spend</span>
        </div>
        {base.loading ? (
          <div className="loading">Loading…</div>
        ) : actions === null ? (
          <Empty>Action Center unavailable — over-cap signals can't be read right now (GET /v1/spine/company/actions).</Empty>
        ) : budgetItems.length === 0 ? (
          <Empty>No budget or over-cap signals — committed Allowance is within budget and no Operative is hard-stopped.</Empty>
        ) : (
          <div className="table-scroll">
            <table className="table compact">
              <tbody>
                {budgetItems.map((a, i) => (
                  <tr key={a.id ?? i}>
                    <td style={{ width: 60 }}>
                      <span className={"badge " + (SEV_TONE[a.severity ?? ""] ?? "todo")} style={{ fontSize: 9 }}>
                        {a.severity ?? "—"}
                      </span>
                    </td>
                    <td>
                      <div style={{ fontSize: 13, fontWeight: 600 }}>{a.title ?? "(budget signal)"}</div>
                      {a.reason && <div className="muted" style={{ fontSize: 11 }}>{a.reason}</div>}
                    </td>
                    <td style={{ textAlign: "right" }}>
                      {a.route && <Link to={a.route} className="btn sm ghost">{a.action_label ?? "Open"} →</Link>}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* ── Operative allowance & observed spend ───────────────────── */}
      <div className="card">
        <div className="row" style={{ marginBottom: 10 }}>
          <h3 style={{ margin: 0 }}>Operative allowance &amp; spend</h3>
          <div className="spacer" style={{ flex: 1 }} />
          <Link to="/agents" className="link" style={{ fontSize: 12 }}>Crew →</Link>
        </div>
        {base.loading ? (
          <div className="loading">Loading…</div>
        ) : ops.length === 0 ? (
          <Empty>No Operatives yet.</Empty>
        ) : (
          <div className="table-scroll">
            <table className="table compact">
              <thead>
                <tr>
                  <th>Operative</th>
                  <th>Role</th>
                  <th style={{ textAlign: "right" }}>Monthly Allowance</th>
                  <th style={{ textAlign: "right" }}>Observed spend ({WINDOWS.find((w) => w.h === hours)?.label})</th>
                </tr>
              </thead>
              <tbody>
                {ops.map((o, i) => {
                  const id = o.agent_id ?? "";
                  const allow = allowanceByAgent.get(id);
                  const spent = opSpend(o);
                  return (
                    <tr key={id || i}>
                      <td><strong>{o.name ?? id.slice(0, 10)}</strong></td>
                      <td className="dim">{o.role ?? "—"}</td>
                      <td style={{ textAlign: "right" }}>{allow != null ? fmtCents(allow) : <span className="muted">—</span>}</td>
                      <td style={{ textAlign: "right" }}>
                        {!metricsAvailable ? <span className="muted">—</span> : spent != null ? fmtMicros(spent) : <span className="muted">no data</span>}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        <div className="muted" style={{ fontSize: 11, marginTop: 8 }}>
          Allowance is the configured monthly cap (Keys). Observed spend is from observability metrics over the
          selected window below — not the governance calendar-month; an unmatched agent shows “no data”.
        </div>
      </div>

      {/* ── Observed spend by agent (metrics window) ───────────────── */}
      <div className="card">
        <div className="row" style={{ marginBottom: 10 }}>
          <h3 style={{ margin: 0 }}>Observed spend by agent</h3>
          <div className="spacer" style={{ flex: 1 }} />
          <span className="btn-group">
            {WINDOWS.map((w) => (
              <button
                key={w.h}
                className={"btn sm " + (hours === w.h ? "" : "ghost")}
                onClick={() => setHours(w.h)}
              >
                {w.label}
              </button>
            ))}
          </span>
        </div>
        {metrics.loading ? (
          <div className="loading">Loading metrics…</div>
        ) : !metricsAvailable ? (
          <Empty>
            Observed-spend metrics unavailable
            {metricsReport?.error ? ` — ${metricsReport.error}` : " (GET /v1/metrics/agents)"}.
          </Empty>
        ) : metricRows.length === 0 ? (
          <Empty>No agent activity recorded in the last {WINDOWS.find((w) => w.h === hours)?.label}.</Empty>
        ) : (
          <div className="table-scroll">
            <table className="table compact">
              <thead>
                <tr>
                  <th>Agent</th>
                  <th style={{ textAlign: "right" }}>Invocations</th>
                  <th style={{ textAlign: "right" }}>Tokens</th>
                  <th style={{ textAlign: "right" }}>Cost</th>
                </tr>
              </thead>
              <tbody>
                {metricRows
                  .slice()
                  .sort((a, b) => (b.total_cost_micros ?? 0) - (a.total_cost_micros ?? 0))
                  .map((r, i) => (
                    <tr key={r.agent ?? i}>
                      <td><strong>{r.agent ?? "—"}</strong></td>
                      <td style={{ textAlign: "right" }}>{(r.invocations ?? 0).toLocaleString()}</td>
                      <td style={{ textAlign: "right" }}>{(r.total_tokens ?? 0).toLocaleString()}</td>
                      <td style={{ textAlign: "right" }}>{fmtMicros(r.total_cost_micros)}</td>
                    </tr>
                  ))}
                <tr>
                  <td><strong>Total</strong></td>
                  <td />
                  <td />
                  <td style={{ textAlign: "right" }}><strong>{fmtMicros(totalObservedMicros)}</strong></td>
                </tr>
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* ── Brief-tree cost rollup ─────────────────────────────────── */}
      <BriefRollup />
    </Section>
  );
}

// The §6.6 Brief-tree cost rollup: pick a Brief → sum the durable run ledger
// over it AND its same-Guild Sub-brief tree, with own/descendant split and a
// per-billing-code breakdown. Default window = current calendar month (omit
// since/until); simple date inputs override it.
function BriefRollup() {
  const [q, setQ] = useState("");
  const [results, setResults] = useState<BriefCard[] | null>(null);
  const [searching, setSearching] = useState(false);
  const [selected, setSelected] = useState<BriefCard | null>(null);
  const [since, setSince] = useState("");
  const [until, setUntil] = useState("");
  const [report, setReport] = useState<GetReport<BriefCostRollup | null> | null>(null);
  const [loadingCost, setLoadingCost] = useState(false);

  async function search() {
    const term = q.trim();
    if (!term) return;
    setSearching(true);
    try {
      const r = await tryGet<BriefCard[]>(
        `/v1/spine/briefs/search?q=${encodeURIComponent(term)}&limit=20`,
        [],
      );
      setResults(Array.isArray(r) ? r : []);
    } finally {
      setSearching(false);
    }
  }

  async function loadCost(brief: BriefCard) {
    if (!brief.task_id) return;
    setSelected(brief);
    setLoadingCost(true);
    try {
      const r = await briefCost.rollup(brief.task_id, dateToSecs(since), dateToSecs(until));
      setReport(r);
    } finally {
      setLoadingCost(false);
    }
  }

  const c = report?.data ?? null;

  return (
    <div className="card">
      <div className="row" style={{ marginBottom: 10 }}>
        <h3 style={{ margin: 0 }}>Brief-tree cost rollup</h3>
        <div className="spacer" style={{ flex: 1 }} />
        <Link to="/briefs" className="link" style={{ fontSize: 12 }}>Briefs →</Link>
      </div>

      <div className="filter-bar" style={{ marginBottom: 10 }}>
        <input
          className="input"
          style={{ flex: "1 1 220px", minWidth: 0 }}
          placeholder="Find a Brief by title…"
          value={q}
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => { if (e.key === "Enter") void search(); }}
        />
        <button className="btn sm" onClick={() => void search()} disabled={searching || !q.trim()}>
          {searching ? "…" : "Search"}
        </button>
      </div>

      {results && results.length === 0 && (
        <Empty>No Briefs match “{q}”.</Empty>
      )}
      {results && results.length > 0 && (
        <div className="table-scroll" style={{ marginBottom: 12 }}>
          <table className="table compact">
            <tbody>
              {results.map((b, i) => (
                <tr
                  key={b.task_id ?? i}
                  className={selected?.task_id === b.task_id ? "mandate-row" : undefined}
                  style={{ cursor: "pointer" }}
                  onClick={() => void loadCost(b)}
                >
                  <td><strong style={{ fontSize: 13 }}>{b.title ?? "(untitled)"}</strong></td>
                  <td><span className={"badge " + (b.board_status ?? "todo")} style={{ fontSize: 9 }}>{b.board_status ?? "—"}</span></td>
                  <td className="mono" style={{ fontSize: 10 }}>{(b.task_id ?? "").slice(0, 10)}</td>
                  <td style={{ textAlign: "right" }}><span className="link" style={{ fontSize: 12 }}>cost →</span></td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {/* Optional window override — blank = current calendar month (the gate's window). */}
      <div className="filter-bar" style={{ marginBottom: 10 }}>
        <label className="muted" style={{ fontSize: 12 }}>Since
          <input type="date" className="input" style={{ marginLeft: 6, width: "auto", display: "inline-block" }} value={since} onChange={(e) => setSince(e.target.value)} />
        </label>
        <label className="muted" style={{ fontSize: 12 }}>Until
          <input type="date" className="input" style={{ marginLeft: 6, width: "auto", display: "inline-block" }} value={until} onChange={(e) => setUntil(e.target.value)} />
        </label>
        {selected && (
          <button className="btn sm ghost" onClick={() => void loadCost(selected)} disabled={loadingCost}>
            {loadingCost ? "…" : "Apply window"}
          </button>
        )}
        <span className="muted" style={{ fontSize: 11 }}>blank = current calendar month</span>
      </div>

      {!selected ? (
        <Empty>Search and pick a Brief to roll up its whole same-Guild Sub-brief tree's cost.</Empty>
      ) : loadingCost ? (
        <div className="loading">Computing rollup…</div>
      ) : report?.error ? (
        <Empty>
          Cost rollup unavailable for “{selected.title}” — {report.error}{" "}
          (GET /v1/spine/briefs/{(selected.task_id ?? "").slice(0, 10)}…/cost).
        </Empty>
      ) : !c ? (
        <Empty>No cost data for this Brief in the selected window.</Empty>
      ) : (
        <div>
          <div className="row" style={{ marginBottom: 8 }}>
            <strong>{selected.title}</strong>
            <span className="muted" style={{ fontSize: 11, marginLeft: 8 }}>
              window {fmtDate(c.since_secs)} → {fmtDate(c.until_secs)}
            </span>
          </div>
          <div className="grid cols-3">
            <div>
              <div className="stat">{fmtMicros(c.cost_micros)}</div>
              <div className="stat-label">Tree total · {c.brief_count} Brief{c.brief_count === 1 ? "" : "s"} · {c.run_count} run{c.run_count === 1 ? "" : "s"}</div>
            </div>
            <div>
              <div className="stat">{fmtMicros(c.own_cost_micros)}</div>
              <div className="stat-label">This Brief · {c.own_run_count} run{c.own_run_count === 1 ? "" : "s"}</div>
            </div>
            <div>
              <div className="stat">{fmtMicros(c.descendant_cost_micros)}</div>
              <div className="stat-label">Sub-briefs · {c.descendant_run_count} run{c.descendant_run_count === 1 ? "" : "s"}</div>
            </div>
          </div>

          <div className="op-group" style={{ marginTop: 14 }}>
            <div className="op-group-title">By billing code</div>
            {c.by_billing_code.length === 0 ? (
              <div className="muted" style={{ fontSize: 12 }}>No runs in this window.</div>
            ) : (
              <div className="table-scroll">
                <table className="table compact">
                  <thead>
                    <tr>
                      <th>Billing code</th>
                      <th style={{ textAlign: "right" }}>Runs</th>
                      <th style={{ textAlign: "right" }}>Cost</th>
                    </tr>
                  </thead>
                  <tbody>
                    {c.by_billing_code.map((row, i) => (
                      <tr key={row.billing_code || i}>
                        <td>{row.billing_code ? <span className="mono">{row.billing_code}</span> : <span className="muted">unattributed</span>}</td>
                        <td style={{ textAlign: "right" }}>{row.run_count}</td>
                        <td style={{ textAlign: "right" }}>{fmtMicros(row.cost_micros)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
