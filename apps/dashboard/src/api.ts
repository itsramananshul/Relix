// Thin fetch wrapper for the Relix web bridge.
//
// Every request rides the HTTP-only `relix_session` cookie via
// `credentials: "include"`, so the dashboard never handles a bearer
// token directly — the bridge auth middleware admits the session.

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

// ── Session-expired signal ────────────────────────────────────────────────
// When a PROTECTED API call comes back 401/403, the operator's session cookie
// has lapsed (or was never minted). Rather than let every page render a broken
// "Could not load …" card, we fire a single signal the AuthProvider listens
// for, so the app can flip back to the login screen with a clear message.
//
// This is a CLIENT-SIDE reaction only — it never makes a protected route
// public; it just routes an honest 401 to the login path instead of a dead end.
type SessionExpiredHandler = () => void;
const sessionExpiredHandlers = new Set<SessionExpiredHandler>();

export function onSessionExpired(cb: SessionExpiredHandler): () => void {
  sessionExpiredHandlers.add(cb);
  return () => {
    sessionExpiredHandlers.delete(cb);
  };
}

function notifySessionExpired(): void {
  for (const cb of sessionExpiredHandlers) {
    try {
      cb();
    } catch {
      /* a misbehaving listener must not break the request path */
    }
  }
}

// The auth endpoints self-gate (a wrong password is a legitimate 401 on the
// login form, NOT an expired session) — never treat them as a lapsed session.
function isAuthPath(path: string): boolean {
  return path.startsWith("/v1/auth/");
}

async function parse(res: Response): Promise<unknown> {
  const text = await res.text();
  if (!text) return null;
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

async function request(method: string, path: string, body?: unknown): Promise<unknown> {
  const res = await fetch(path, {
    method,
    credentials: "include",
    headers: body !== undefined ? { "content-type": "application/json" } : undefined,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const data = await parse(res);
  if (!res.ok) {
    // A 401/403 on any non-auth route means the session lapsed — signal the
    // app to reauthenticate instead of leaving the page on a broken card.
    if ((res.status === 401 || res.status === 403) && !isAuthPath(path)) {
      notifySessionExpired();
    }
    const msg =
      (data && typeof data === "object" && "error" in data
        ? String((data as Record<string, unknown>).error)
        : typeof data === "string" && data
          ? data
          : `HTTP ${res.status}`) || `HTTP ${res.status}`;
    throw new ApiError(res.status, msg);
  }
  return data;
}

export const api = {
  get: <T = unknown>(path: string) => request("GET", path) as Promise<T>,
  post: <T = unknown>(path: string, body?: unknown) => request("POST", path, body) as Promise<T>,
  put: <T = unknown>(path: string, body?: unknown) => request("PUT", path, body) as Promise<T>,
  patch: <T = unknown>(path: string, body?: unknown) => request("PATCH", path, body) as Promise<T>,
  del: <T = unknown>(path: string) => request("DELETE", path) as Promise<T>,
};

// Best-effort GET that resolves to a fallback instead of throwing, so a
// single unavailable surface degrades to an empty/placeholder state
// rather than blanking the whole page. Use this ONLY for genuinely-optional
// surfaces — for core data prefer `tryGetReport` so a failure is surfaced.
export async function tryGet<T>(path: string, fallback: T): Promise<T> {
  try {
    return (await api.get<T>(path)) ?? fallback;
  } catch {
    return fallback;
  }
}

// Like `tryGet`, but ALSO reports the failure so the page can show an
// explicit error state (a banner + retry) instead of a silent empty panel.
// `status` distinguishes 401/403 (session) from 502/503 (bridge can't reach
// the coordinator) so callers can route the user to the right fix.
export interface GetReport<T> {
  data: T;
  error: string | null;
  status: number | null;
}
export async function tryGetReport<T>(path: string, fallback: T): Promise<GetReport<T>> {
  try {
    const data = (await api.get<T>(path)) ?? fallback;
    return { data, error: null, status: 200 };
  } catch (e) {
    if (e instanceof ApiError) return { data: fallback, error: e.message, status: e.status };
    return { data: fallback, error: e instanceof Error ? e.message : String(e), status: null };
  }
}

// ── Run (Shift) control helpers ───────────────────────────────────────────
// One wiring for the Shift lifecycle (review / apply / cancel + the safe-apply
// plan), shared by the Runs page and the Brief workroom so the same operator
// actions aren't parsed two different ways. All hit the existing `/v1/runs/:id`
// routes the bridge already serves.

// One file in a safe-apply plan (`/v1/runs/:id/diff` → plan.items).
export interface ApplyPlanItem {
  rel_path?: string;
  kind?: string;
  action?: string; // create / overwrite / delete / noop / refuse
  can_apply?: boolean;
  conflict?: boolean;
  reason?: string;
}
export interface ApplyPlan {
  project_root?: string;
  items?: ApplyPlanItem[];
  applicable?: boolean;
  changes?: number;
  conflicts?: number;
  blocked?: number;
  note?: string;
}
// Safe-apply preview (`/v1/runs/:id/diff`).
export interface RunDiff {
  run_id?: string;
  status?: string;
  review?: string;
  apply_status?: string;
  eligible?: boolean;
  reason?: string;
  plan?: ApplyPlan;
}
export interface ApplyResult {
  apply_status?: string;
  applied_files?: number;
  failed_files?: number;
  brief_status?: string;
}

// One transcript event from the durable, capped, redacted `run_events`
// table (`/v1/runs/:id/events`). `kind`/`source` classify the line; `message`
// is the redacted, length-bounded text; `payload_json` is the optional bounded
// detail (e.g. a tool-call's input). Shared by the Runs page and the Brief
// workroom so the same transcript renders identically in both places.
export interface RunEvent {
  event_id?: number;
  ts?: number;
  kind?: string;
  source?: string;
  message?: string;
  payload_json?: string;
}

export const runControls = {
  // The chronological transcript for a run (oldest first). Optional surface →
  // degrades to [] so an unavailable transcript never blanks the embedding view.
  events: (runId: string) =>
    tryGet<RunEvent[]>(`/v1/runs/${encodeURIComponent(runId)}/events`, []),
  // Record an operator accept/reject of a done run.
  review: (runId: string, decision: "accepted" | "rejected", note = "") =>
    api.post(`/v1/runs/${encodeURIComponent(runId)}/review`, { decision, note }),
  // Copy an accepted run's changed files into the project root.
  apply: (runId: string) =>
    api.post<ApplyResult>(`/v1/runs/${encodeURIComponent(runId)}/apply`, {}),
  // Request cancellation of an in-flight run.
  cancel: (runId: string) =>
    api.post<{ active?: boolean; note?: string }>(
      `/v1/runs/${encodeURIComponent(runId)}/cancel`,
      {},
    ),
  // The safe-apply PLAN for a run (per-file actions + applicability). Optional
  // surface → resolves to null on failure so the panel degrades, not blanks.
  diff: (runId: string) =>
    tryGet<RunDiff | null>(`/v1/runs/${encodeURIComponent(runId)}/diff`, null),
};

// ── Brief thread interactions (answerable cards) ──────────────────────────
// The ask/confirm cards an Operative/companion raises on a Brief
// (relix-execution-and-issue-design §1.9; relix-dashboard-design §7). The
// operator answers them inline; the answer writes a Chronicle event and
// flips the card's status. All hit `/v1/spine/briefs/:id/interactions`.

// One proposed child Brief inside a `suggest_tasks` card.
export interface SuggestChild {
  title: string;
  priority?: string | null;
  // Optional intra-proposal dependency: the 0-based index of an earlier
  // sibling this child depends on (§1.6). On accept it becomes a Snag
  // (blocked_on) — the referenced sibling must reach `done` first.
  after?: number | null;
  // Optional explicit assignee hint (§1.9). Mutually exclusive: a child
  // names an Operative by id (precise) OR by role (resolved to the oldest
  // active same-role Operative), never both. On accept the hint is
  // validated through the existing assign-Key gate (same-Guild, active)
  // and the child is assigned; absent ⇒ the child opens unassigned.
  assignee_agent_id?: string | null;
  assignee_role?: string | null;
}

// The bounded proposal a `suggest_tasks` card carries.
export interface BriefProposal {
  summary: string;
  children: SuggestChild[];
}

export interface BriefInteraction {
  interaction_id: string;
  task_id: string;
  kind: string; // ask | confirm | suggest_tasks
  prompt: string;
  choices: string[];
  author: string;
  status: string; // open | resolved | rejected | expired
  response?: string | null;
  created_at?: number;
  resolved_at?: number | null;
  resolved_by?: string | null;
  // Present only on `suggest_tasks` cards.
  proposal?: BriefProposal | null;
  // Approval-bound plan confirm (§1.8): when this `confirm` was opened against
  // a specific `plan` Dossier revision, the bound Dossier id (which IS the
  // revision — Dossiers are immutable) and its kind (`plan`). Present only on a
  // bound confirm; an accept after the plan changed (newer revision or a
  // superseding comment) is refused server-side and the card flips to
  // `expired`.
  bound_doc_id?: string | null;
  bound_doc_kind?: string | null;
  // Plan package (§1.7/§1.8/§3.1): when this `confirm` was opened as part of a
  // plan package, the exact linked `suggest_tasks` interaction id it gates.
  // Present only on a plan-package confirm; accepting such a confirm must go
  // through the safe `briefPlanConfirms.respond` path (not the generic
  // interaction respond) so approval materializes the linked proposal exactly
  // once through the decomposition ledger.
  bound_interaction_id?: string | null;
}

export const briefInteractions = {
  // List a Brief's cards (oldest first). Optional surface → degrades to []
  // so a Brief with no interactions (or a bridge hiccup) never blanks.
  list: (briefId: string) =>
    tryGet<BriefInteraction[]>(
      `/v1/spine/briefs/${encodeURIComponent(briefId)}/interactions`,
      [],
    ),
  // Raise a new card (used by agents/companion; exposed for completeness).
  open: (
    briefId: string,
    body: { kind: "ask" | "confirm"; prompt: string; choices?: string[]; author: string },
  ) =>
    api.post<{ interaction_id: string }>(
      `/v1/spine/briefs/${encodeURIComponent(briefId)}/interactions`,
      body,
    ),
  // Answer a card. `status` is the terminal verdict; a duplicate answer
  // surfaces as a typed 400 (ApiError).
  respond: (
    briefId: string,
    interactionId: string,
    body: { responder: string; status: "resolved" | "rejected"; response?: string },
  ) =>
    api.post(
      `/v1/spine/briefs/${encodeURIComponent(briefId)}/interactions/${encodeURIComponent(
        interactionId,
      )}/respond`,
      body,
    ),
  // Open an approval-bound plan confirm (§1.8): a `confirm` card bound to the
  // Brief's latest `plan` Dossier revision. The route refuses (4xx) when the
  // Brief has no `plan` Dossier — the caller surfaces that honestly. `author`
  // defaults to the bridge identity when omitted. The card lists/answers
  // through the same interaction routes; an accept after the plan changed is
  // refused as stale and the card flips to `expired`.
  openPlanConfirm: (
    briefId: string,
    body: { author?: string; prompt?: string },
  ) =>
    api.post<{ interaction_id: string }>(
      `/v1/spine/briefs/${encodeURIComponent(briefId)}/plan-confirm`,
      body,
    ),
};

// ── Brief suggest_tasks cards (proposed child-Brief trees) ────────────────
// An Operative proposes a bounded list of child Briefs on a Brief
// (relix-execution-and-issue-design §1.9). The operator accepts — which
// materializes them as real Sub-briefs — or rejects. The cards list through
// the same `briefInteractions.list` (kind `suggest_tasks`, with a `proposal`).
export const briefSuggestions = {
  // Raise a new suggestion (used by agents/companion; exposed for completeness).
  open: (
    briefId: string,
    body: { author: string; summary?: string; children: SuggestChild[] },
  ) =>
    api.post<{ interaction_id: string }>(
      `/v1/spine/briefs/${encodeURIComponent(briefId)}/suggestions`,
      body,
    ),
  // Accept (materialize the child Briefs) or reject a suggestion. Accept
  // returns the created child ids; a duplicate answer surfaces as a typed 400.
  respond: (
    briefId: string,
    interactionId: string,
    body: { responder: string; accept: boolean },
  ) =>
    api.post<{ created: string[] }>(
      `/v1/spine/briefs/${encodeURIComponent(briefId)}/suggestions/${encodeURIComponent(
        interactionId,
      )}/respond`,
      body,
    ),
};

// ── Plan-package confirms (approval-bound, linked to a proposal) ───────────
// A plan package (relix-execution-and-issue-design §1.7/§1.8/§3.1) opens a
// `plan` Dossier + a `suggest_tasks` proposal + an approval-bound `confirm`
// linked to both (the confirm carries `bound_interaction_id`). Accepting the
// confirm must use this safe route, not the generic interaction respond, so the
// linked proposal materializes exactly once through the resumable decomposition
// ledger; rejecting closes the confirm and its still-open proposal.
export const briefPlanConfirms = {
  // Open a plan package. Returns the three artifact ids. (Used by the
  // companion and by the workroom's minimal manual plan-package composer.)
  open: (
    briefId: string,
    body: {
      author: string;
      plan_title?: string;
      plan_body: string;
      summary?: string;
      children: SuggestChild[];
      prompt?: string;
    },
  ) =>
    api.post<{ plan_doc_id: string; suggestion_id: string; confirm_id: string }>(
      `/v1/spine/briefs/${encodeURIComponent(briefId)}/plan-package`,
      body,
    ),
  // Accept (re-check the plan is latest, then materialize the linked proposal)
  // or reject a plan-package confirm. Returns the typed outcome + created child
  // ids. A duplicate accept is idempotent and returns the SAME ids.
  respond: (
    briefId: string,
    confirmId: string,
    body: { responder: string; accept: boolean },
  ) =>
    api.post<{ outcome: string; suggestion_id: string; created: string[] }>(
      `/v1/spine/briefs/${encodeURIComponent(briefId)}/plan-confirms/${encodeURIComponent(
        confirmId,
      )}/respond`,
      body,
    ),
};

// ── Brief Dossiers — issue documents (author / revision-lock / fork) ──────
// A Dossier is a durable, append-only artifact on a Brief (plan/design/notes;
// relix-execution-and-issue-design §1.8). v1 authoring: revise under an
// optimistic lock, or explicitly fork a new line from a stale/base revision.
// `revision_number` is derived (1-based within Brief+kind, oldest first).

// A Dossier listing row (metadata only, no body) as carried on the Brief
// detail's `dossiers` array.
export interface DossierMeta {
  doc_id: string;
  kind: string;
  title: string;
  created_at?: number;
  updated_at?: number;
  author?: string | null;
  revision_of_doc_id?: string | null;
  forked_from_doc_id?: string | null;
  revision_number?: number;
}

// A full Dossier (with body) — returned by the latest-load route.
export interface Dossier extends DossierMeta {
  task_id: string;
  body: string;
}

// The successful-author result (mirrors the coordinator's DossierAuthored).
export interface DossierAuthored {
  doc_id: string;
  task_id: string;
  kind: string;
  title: string;
  author?: string | null;
  mode: "create" | "revise" | "fork";
  revision_number: number;
  revision_of_doc_id?: string | null;
  forked_from_doc_id?: string | null;
}

export const briefDossiers = {
  // Load the latest revision of a kind (full body + metadata), or `null` when
  // the Brief has no Dossier of that kind. The editor keeps the returned
  // `doc_id` as the optimistic-lock base for the next save.
  latest: (briefId: string, kind: string) =>
    api.get<Dossier | null>(
      `/v1/spine/briefs/${encodeURIComponent(briefId)}/dossiers/latest?kind=${encodeURIComponent(
        kind,
      )}`,
    ),
  // Author a Dossier revision. `mode` defaults to `revise`; pass
  // `expected_latest_doc_id` to enforce the optimistic lock (a stale base — a
  // newer revision landed first — rejects with **HTTP 409**, nothing written:
  // the caller reloads or forks, and must NOT retry the 409 blindly). `mode:
  // "fork"` branches a new line from `base_doc_id` even if the latest moved.
  author: (
    briefId: string,
    body: {
      kind: string;
      title: string;
      body: string;
      author: string;
      mode?: "revise" | "fork";
      expected_latest_doc_id?: string;
      base_doc_id?: string;
    },
  ) =>
    api.post<DossierAuthored>(
      `/v1/spine/briefs/${encodeURIComponent(briefId)}/dossiers/author`,
      body,
    ),
};

// ── Brief-tree cost rollup (brief.cost_rollup) ────────────────────────────
// The §6.6 issue-tree cost rollup: sum the durable `brief_runs` ledger over a
// Brief AND its same-Guild Sub-brief tree, with own-vs-descendant totals, tree
// counts, and a per-billing-code breakdown (dashboard-design §10;
// company-model §6.6). All figures are REAL run cost — micro-USD from the
// ledger, never UI data. Windowed on the canonical Allowance month unless
// since/until (unix SECONDS) are supplied. Hits `GET /v1/spine/briefs/:id/cost`.

// One billing-code's slice of a Brief-tree's cost. `billing_code:""` = unattributed.
export interface BillingCodeCost {
  billing_code: string;
  run_count: number;
  cost_micros: number;
}

export interface BriefCostRollup {
  brief_id: string;
  tenant_id: string;
  // Resolved window the rollup billed against (unix SECONDS).
  since_secs: number;
  until_secs: number;
  // Whole same-Guild tree (root Brief + descendants).
  brief_count: number;
  run_count: number;
  cost_micros: number;
  // Just the root Brief.
  own_run_count: number;
  own_cost_micros: number;
  // Descendant Sub-briefs (= tree − own).
  descendant_run_count: number;
  descendant_cost_micros: number;
  by_billing_code: BillingCodeCost[];
}

// ── Canonical Guild month-to-date spend (guild.spend) ─────────────────────
// THE numeric Guild spend the Costs page reads (company-model §6.6;
// dashboard-design §10). NOT a dashboard-only approximation: it is the EXACT
// ledger figure + UTC-calendar-month window the autonomous Guild hard-stop
// enforces (`heartbeat::guild_spend_micros` over `heartbeat::allowance_window`),
// so the card can never disagree with the gate. Hits `GET /v1/spine/guild/spend`.
//
// `spent_*` are null when no metrics ledger is wired (spend can't be computed
// honestly — never a fabricated 0). The `budget_cents`/`remaining_cents`/
// `over_budget` triplet is null when no positive Guild budget is configured.
export interface GuildSpend {
  tenant_id: string;
  guild_id: string;
  display_name: string | null;
  // Exact integer micro-USD (1,000,000 micros = $1) + a rounded cents view.
  spent_micros: number | null;
  spent_cents: number | null;
  // Configured Guild budget + remaining (cents); null when no budget is set.
  budget_cents: number | null;
  remaining_cents: number | null;
  over_budget: boolean | null;
  // Canonical Allowance window (UTC calendar month) + reset bookkeeping (unix-ms).
  window_start_ms: number;
  resets_at_ms: number;
  now_ms: number;
  source: string;
  computed_from: string;
}

export const guildSpend = {
  // Canonical month-to-date Guild spend. Reports the failure (via
  // `tryGetReport`) so the Costs card shows an honest unavailable state with the
  // route/reason instead of falling back to a fabricated/approximated figure.
  get: () => tryGetReport<GuildSpend | null>("/v1/spine/guild/spend", null),
};

export const briefCost = {
  // The Brief-tree rollup. `since`/`until` are unix SECONDS — omit both for the
  // canonical current-calendar-month window the dispatch gate uses. Reports the
  // failure (via `tryGetReport`) so the Costs page shows an honest unavailable
  // state with the route/reason instead of fabricated zeroes.
  rollup: (briefId: string, since?: number, until?: number) => {
    const qs = new URLSearchParams();
    if (since != null) qs.set("since", String(since));
    if (until != null) qs.set("until", String(until));
    const q = qs.toString();
    return tryGetReport<BriefCostRollup | null>(
      `/v1/spine/briefs/${encodeURIComponent(briefId)}/cost${q ? `?${q}` : ""}`,
      null,
    );
  },
};

// ── Live run-event stream (SSE) ───────────────────────────────────────────
// Subscribe to the bridge's `/v1/runs/events/stream` execution feed so the
// Runs page + Brief detail can refresh the moment a Shift starts, finishes,
// is refused, recovered, moved, reviewed, or applied — instead of only at
// fetch time. Cookie auth rides the same-origin EventSource automatically.

export type RunEventConn = "connecting" | "live" | "reconnecting" | "unavailable";

export interface RunStreamEvent {
  // Normalized SSE event name: run_started | run_finished |
  // run_cancel_requested | brief_moved | review_changed | apply_changed.
  name: string;
  // The Brief (task) id carried by the event, when present.
  taskId: string | null;
}

const RUN_EVENT_NAMES = [
  "run_started",
  "run_finished",
  "run_cancel_requested",
  "brief_moved",
  "review_changed",
  "apply_changed",
];

// Open the stream and call `onEvent` per execution transition + `onConn` on
// connection-state changes. Manages reconnect with capped backoff and reports
// honest state (live / reconnecting / unavailable). Returns an unsubscribe fn.
export function subscribeRunEvents(
  onEvent: (ev: RunStreamEvent) => void,
  onConn: (state: RunEventConn) => void,
): () => void {
  let es: EventSource | null = null;
  let closed = false;
  let attempts = 0;
  let backoff = 1000;
  let timer: ReturnType<typeof setTimeout> | null = null;

  const handler = (name: string) => (e: MessageEvent) => {
    let taskId: string | null = null;
    try {
      const j = JSON.parse(e.data);
      if (j && typeof j === "object" && "task_id" in j && j.task_id != null) {
        taskId = String((j as Record<string, unknown>).task_id);
      }
    } catch {
      /* non-JSON frame — forward with no taskId */
    }
    onEvent({ name, taskId });
  };

  const connect = () => {
    if (closed) return;
    onConn(attempts === 0 ? "connecting" : "reconnecting");
    es = new EventSource("/v1/runs/events/stream", { withCredentials: true });
    es.onopen = () => {
      attempts = 0;
      backoff = 1000;
      onConn("live");
    };
    for (const n of RUN_EVENT_NAMES) {
      es.addEventListener(n, handler(n) as EventListener);
    }
    es.onerror = () => {
      // The browser would auto-reconnect, but we manage it so we can surface
      // honest state + cap reconnect storms. Persistent failure → unavailable
      // (still retrying, so it can recover to live).
      es?.close();
      es = null;
      if (closed) return;
      attempts += 1;
      onConn(attempts >= 3 ? "unavailable" : "reconnecting");
      timer = setTimeout(connect, backoff);
      backoff = Math.min(backoff * 2, 15000);
    };
  };

  connect();
  return () => {
    closed = true;
    if (timer) clearTimeout(timer);
    es?.close();
    es = null;
  };
}

// ── Dedicated Active Runs snapshot stream (SSE) ────────────────────────────
// Subscribe to the bridge's `/v1/runs/stream` feed so the Active Runs table
// refreshes the moment the recent-run ledger changes — a run started, finished,
// refused, reviewed, applied, or retried — instead of re-fetching on each
// run-event. Tenant-scoped server-side (it proxies the SAME `brief.runs` read
// the `/v1/runs` list route serves); polling-backed (~2.5s) + fingerprint-gated,
// so an unchanged ledger pushes nothing. Honest polling-backed SSE — NOT a true
// event bus/websocket. Cookie auth rides the same-origin EventSource. The page
// falls back to its mount-load + manual Refresh whenever this never reaches
// `live` (see Runs.tsx).

export type RunsStreamConn = "connecting" | "live" | "reconnecting" | "unavailable";

// Open the runs snapshot stream. `onRuns` receives the full recent-run array on
// the initial snapshot + every change (same shape as `GET /v1/runs`, truncated
// to `limit`); `onConn` reports honest connection state. Manages reconnect with
// capped backoff. Returns an unsubscribe fn.
export function subscribeRuns(
  onRuns: (runs: unknown[]) => void,
  onConn: (state: RunsStreamConn) => void,
  limit = 100,
): () => void {
  let es: EventSource | null = null;
  let closed = false;
  let attempts = 0;
  let backoff = 1000;
  let timer: ReturnType<typeof setTimeout> | null = null;

  const connect = () => {
    if (closed) return;
    onConn(attempts === 0 ? "connecting" : "reconnecting");
    es = new EventSource(`/v1/runs/stream?limit=${limit}`, { withCredentials: true });
    es.onopen = () => {
      attempts = 0;
      backoff = 1000;
      onConn("live");
    };
    es.addEventListener("runs", (e: MessageEvent) => {
      try {
        const arr = JSON.parse(e.data);
        if (Array.isArray(arr)) onRuns(arr as unknown[]);
      } catch {
        /* malformed frame — ignore, the next snapshot corrects it */
      }
    });
    // NB: the server's transient `event: error` frames just precede the next
    // snapshot; EventSource's own connection `error` is handled by `onerror`.
    es.onerror = () => {
      es?.close();
      es = null;
      if (closed) return;
      attempts += 1;
      onConn(attempts >= 3 ? "unavailable" : "reconnecting");
      timer = setTimeout(connect, backoff);
      backoff = Math.min(backoff * 2, 15000);
    };
  };

  connect();
  return () => {
    closed = true;
    if (timer) clearTimeout(timer);
    es?.close();
    es = null;
  };
}

// ── Prime guided driver v1 (next governed step + one-step advance) ──────────
// The READ-ONLY next step for a Prime work session, plus the bounded one-step
// advance. The advance runs AT MOST ONE safe governed step through the existing
// gated route; a stale request returns HTTP 409 (re-read and try again). It
// never auto-approves a strategy / hire / spawn / budget gate.

export interface PrimeNextStep {
  phase: string;
  label: string;
  reason: string;
  route: string;
  action_api: string;
  can_advance: boolean;
  advance_action: string | null;
  proposal_id: string | null;
  mandate_id: string | null;
  plan_id: string | null;
  strategy_status: string | null;
  missing_roles: string[];
  pending_hires: unknown[];
  pending_clearances: unknown[];
  counts: Record<string, number>;
}

export interface PrimeAdvanceResult {
  advanced: boolean;
  refused?: string;
  requested_action?: string;
  action?: string;
  reason?: string;
  mandate_id?: string;
  result?: unknown;
  next_step: PrimeNextStep | null;
}

export const primeDriver = {
  nextStep: (proposalId: string) =>
    api.get<PrimeNextStep>(
      `/v1/spine/prime/proposals/${encodeURIComponent(proposalId)}/next-step`,
    ),
  advance: (proposalId: string, action: string) =>
    api.post<PrimeAdvanceResult>(
      `/v1/spine/prime/proposals/${encodeURIComponent(proposalId)}/advance`,
      { action },
    ),
  // Mandate-level twins of the two above — the SAME guided-driver routes, keyed
  // by a Mandate id instead of a proposal id (company-model §5.4/§8.2 + §12.5;
  // bridge `mandate_next_step` / `mandate_advance`). Same shapes, same
  // guarantees: `nextStep` is READ-ONLY; a stale one-step `advance` returns
  // HTTP 409 (re-read and try again, never retry the 409 blindly); the driver
  // never auto-approves a strategy / hire / spawn / budget gate.
  mandateNextStep: (mandateId: string) =>
    api.get<PrimeNextStep>(
      `/v1/spine/mandates/${encodeURIComponent(mandateId)}/next-step`,
    ),
  mandateAdvance: (mandateId: string, action: string) =>
    api.post<PrimeAdvanceResult>(
      `/v1/spine/mandates/${encodeURIComponent(mandateId)}/advance`,
      { action },
    ),
};

// ── Dedicated Prime Shift-Room status stream (SSE) ─────────────────────────
// Subscribe to the bridge's dedicated `/v1/spine/prime/proposals/:id/status/
// stream` feed so the Shift Room renders the live session status pushed by the
// server (initial snapshot + on every change), instead of polling. Cookie auth
// rides the same-origin EventSource. Falls back to polling at the call site
// whenever this never reaches `live`.

export type StatusStreamConn = "connecting" | "live" | "reconnecting" | "unavailable";

// Open the dedicated status stream for one proposal. `onStatus` receives the
// full session-status JSON on the initial snapshot + every change; `onConn`
// reports honest connection state; `onGone` fires once when the server emits a
// terminal `not_found` (the proposal is unknown / cross-Guild). Manages
// reconnect with capped backoff. Returns an unsubscribe fn.
export function subscribePrimeStatus(
  proposalId: string,
  onStatus: (status: unknown) => void,
  onConn: (state: StatusStreamConn) => void,
  onGone?: () => void,
): () => void {
  let es: EventSource | null = null;
  let closed = false;
  let attempts = 0;
  let backoff = 1000;
  let timer: ReturnType<typeof setTimeout> | null = null;

  const connect = () => {
    if (closed) return;
    onConn(attempts === 0 ? "connecting" : "reconnecting");
    es = new EventSource(`/v1/spine/prime/proposals/${proposalId}/status/stream`, {
      withCredentials: true,
    });
    es.onopen = () => {
      attempts = 0;
      backoff = 1000;
      onConn("live");
    };
    es.addEventListener("status", (e: MessageEvent) => {
      try {
        onStatus(JSON.parse(e.data));
      } catch {
        /* malformed frame — ignore, the next snapshot corrects it */
      }
    });
    // Terminal: the proposal is gone / cross-Guild. Stop cleanly — no reconnect.
    es.addEventListener("not_found", () => {
      closed = true;
      es?.close();
      es = null;
      onConn("unavailable");
      onGone?.();
    });
    // NB: we intentionally do NOT listen for a custom `error` event — the
    // server's transient `event: error` frames just precede the next snapshot,
    // and EventSource's own connection `error` is handled by `onerror` below.
    es.onerror = () => {
      es?.close();
      es = null;
      if (closed) return;
      attempts += 1;
      onConn(attempts >= 3 ? "unavailable" : "reconnecting");
      timer = setTimeout(connect, backoff);
      backoff = Math.min(backoff * 2, 15000);
    };
  };

  connect();
  return () => {
    closed = true;
    if (timer) clearTimeout(timer);
    es?.close();
    es = null;
  };
}

// ── Dedicated Brief interaction-card stream (SSE) ──────────────────────────
// Subscribe to the bridge's dedicated `/v1/spine/briefs/:id/interactions/
// stream` feed so the Brief workroom's ask/confirm/suggest/plan-package cards
// refresh the moment the card list changes — a card raised, answered, or
// superseded — even when NO run event fires (the run-event stream above misses
// those). Tenant-scoped server-side (it proxies the same `brief.interactions`
// read the list route serves); polling-backed (~2.5s) + fingerprint-gated, so
// an unchanged list pushes nothing. Cookie auth rides the same-origin
// EventSource. Complements `subscribeRunEvents` — it does not replace it.

export type InteractionStreamConn = "connecting" | "live" | "reconnecting" | "unavailable";

// Open the interaction stream for one Brief. `onInteractions` receives the full
// card array on the initial snapshot + every change (same shape as
// `briefInteractions.list`); `onConn` reports honest connection state; `onGone`
// fires once when the server emits a terminal `not_found` (the Brief is unknown
// / cross-Guild). Manages reconnect with capped backoff. Returns an unsubscribe.
export function subscribeBriefInteractions(
  briefId: string,
  onInteractions: (interactions: BriefInteraction[]) => void,
  onConn: (state: InteractionStreamConn) => void,
  onGone?: () => void,
): () => void {
  let es: EventSource | null = null;
  let closed = false;
  let attempts = 0;
  let backoff = 1000;
  let timer: ReturnType<typeof setTimeout> | null = null;

  const connect = () => {
    if (closed) return;
    onConn(attempts === 0 ? "connecting" : "reconnecting");
    es = new EventSource(
      `/v1/spine/briefs/${encodeURIComponent(briefId)}/interactions/stream`,
      { withCredentials: true },
    );
    es.onopen = () => {
      attempts = 0;
      backoff = 1000;
      onConn("live");
    };
    es.addEventListener("interactions", (e: MessageEvent) => {
      try {
        const arr = JSON.parse(e.data);
        if (Array.isArray(arr)) onInteractions(arr as BriefInteraction[]);
      } catch {
        /* malformed frame — ignore, the next snapshot corrects it */
      }
    });
    // Terminal: the Brief is gone / cross-Guild. Stop cleanly — no reconnect.
    es.addEventListener("not_found", () => {
      closed = true;
      es?.close();
      es = null;
      onConn("unavailable");
      onGone?.();
    });
    // NB: the server's transient `event: error` frames just precede the next
    // snapshot; EventSource's own connection `error` is handled by `onerror`.
    es.onerror = () => {
      es?.close();
      es = null;
      if (closed) return;
      attempts += 1;
      onConn(attempts >= 3 ? "unavailable" : "reconnecting");
      timer = setTimeout(connect, backoff);
      backoff = Math.min(backoff * 2, 15000);
    };
  };

  connect();
  return () => {
    closed = true;
    if (timer) clearTimeout(timer);
    es?.close();
    es = null;
  };
}

// ── Approvals hub (Clearances + the governance action feed) ───────────────
// The operator-decision surface (dashboard-design §10). Two REAL backends:
//
//  1. Pending Clearances — `GET /v1/spine/clearances` (the unified
//     `coord.approval.pending` queue: spawn-hire Clearances, strategy gates,
//     budget overrides, high-risk approvals — distinguished by `method`).
//     Decided inline via `POST /v1/spine/clearances/:id/decide`, which forwards
//     to `coord.approval.decide` under the bridge's verified identity (the
//     runtime cap enforces the real authorisation — never fabricated here).
//
//  2. Direct pending hires + budget alerts — the `hire`/`budget` items in the
//     `GET /v1/spine/company/actions` feed (a `route=direct` hire carries no
//     Clearance, so it is activated via `POST /v1/agents/:id/approve-hire`).
//
// No other approval type has a decide route today, so the hub surfaces those as
// honest "decide on <route>" pointers rather than a fake inline action.

// One pending Clearance row (the bridge parses `coord.approval.pending`'s TSV).
// `requested_at`/`expires_at` arrive as string columns (unix seconds); coerce at
// render. The typed fields (`subject_id`, `capability_category`, `expires_at`,
// `task_id`) are surfaced verbatim from the runtime approval row — an empty
// string means the runtime did not record that field for this Clearance (treat
// as absent). Nothing is fabricated; there is no free-form resource/scope/
// payload editor field because the runtime does not store one.
export interface Clearance {
  approval_id: string;
  agent_id: string;
  method: string;
  reason: string;
  requested_at: string;
  // Typed payload fields (optional / possibly empty — see above).
  subject_id?: string;
  capability_category?: string;
  expires_at?: string;
  task_id?: string;
}

export const clearances = {
  // Pending Clearances (best-effort report so the hub shows an honest
  // unavailable state with the route/reason instead of a blank panel).
  list: (limit = 50) =>
    tryGetReport<Clearance[]>(`/v1/spine/clearances?limit=${limit}`, []),
  // Greenlight / refuse a Clearance. `decision` is `approve`|`reject`; the
  // runtime refuses to re-decide an already-terminal approval, so side effects
  // (e.g. activating a spawn hire) apply exactly once.
  decide: (approvalId: string, decision: "approve" | "reject", note = "") =>
    api.post<{ ok: boolean; approval_id: string; decision: string; approval_token?: string }>(
      `/v1/spine/clearances/${encodeURIComponent(approvalId)}/decide`,
      { decision, note },
    ),
};

// ── Dedicated pending-Clearance stream (SSE) ──────────────────────────────
// Subscribe to the bridge's `/v1/spine/clearances/stream` feed so the Approvals
// hub refreshes the moment the pending queue changes — a Clearance raised,
// decided, or expired — instead of only on manual Refresh. Tenant-scoped
// server-side (it proxies the SAME `coord.approval.pending` read the list route
// serves); polling-backed (~2.5s) + fingerprint-gated, so an unchanged queue
// pushes nothing. Honest polling-backed SSE — NOT a true event bus/websocket.
// Cookie auth rides the same-origin EventSource. The page falls back to bounded
// polling whenever this never reaches `live` (see Approvals.tsx).

export type ClearanceStreamConn = "connecting" | "live" | "reconnecting" | "unavailable";

// Open the Clearance stream. `onClearances` receives the full pending array on
// the initial snapshot + every change (same shape as `clearances.list`'s data);
// `onConn` reports honest connection state. Manages reconnect with capped
// backoff. Returns an unsubscribe fn.
export function subscribeClearances(
  onClearances: (clearances: Clearance[]) => void,
  onConn: (state: ClearanceStreamConn) => void,
): () => void {
  let es: EventSource | null = null;
  let closed = false;
  let attempts = 0;
  let backoff = 1000;
  let timer: ReturnType<typeof setTimeout> | null = null;

  const connect = () => {
    if (closed) return;
    onConn(attempts === 0 ? "connecting" : "reconnecting");
    es = new EventSource("/v1/spine/clearances/stream?limit=50", { withCredentials: true });
    es.onopen = () => {
      attempts = 0;
      backoff = 1000;
      onConn("live");
    };
    es.addEventListener("clearances", (e: MessageEvent) => {
      try {
        const arr = JSON.parse(e.data);
        if (Array.isArray(arr)) onClearances(arr as Clearance[]);
      } catch {
        /* malformed frame — ignore, the next snapshot corrects it */
      }
    });
    // NB: the server's transient `event: error` frames just precede the next
    // snapshot; EventSource's own connection `error` is handled by `onerror`.
    es.onerror = () => {
      es?.close();
      es = null;
      if (closed) return;
      attempts += 1;
      onConn(attempts >= 3 ? "unavailable" : "reconnecting");
      timer = setTimeout(connect, backoff);
      backoff = Math.min(backoff * 2, 15000);
    };
  };

  connect();
  return () => {
    closed = true;
    if (timer) clearTimeout(timer);
    es?.close();
    es = null;
  };
}

// One row of the server-computed `company.actions` feed (company-model §5.4 /
// §8.2). Read-only; the Approvals hub consumes only the `hire`/`budget` items
// (the `approval` items duplicate the Clearance list, which has the real
// decide route + the true approval_id).
export interface CompanyActionItem {
  id?: string;
  category?: string;
  severity?: string;
  title?: string;
  reason?: string;
  target_type?: string;
  target_id?: string;
  target_title?: string;
  action_label?: string;
  route?: string;
  action_api?: string;
  suggested_rig?: string;
  // Recovery diagnosis (execution-and-issue §3.3b) — set on `failed_or_refused`
  // cards built from a run's durable diagnosis.
  failure_class?: string;
  retryable?: boolean;
  retry_budget_remaining?: number;
  // Guarded retry target — set on a `failed_or_refused` card ONLY when the
  // source run is retry-eligible (retryable + budget + no existing child). Pairs
  // with `action_api` = `POST /v1/runs/<run_id>/retry` so the Action Center can
  // open one guarded retry directly. Absent when not safely retryable from here.
  run_id?: string;
}
export interface CompanyActionsFeed {
  actions?: CompanyActionItem[];
  counts?: { total?: number; by_category?: Record<string, number>; by_severity?: Record<string, number> };
  truncated?: boolean;
}

export const companyActions = {
  list: () => tryGetReport<CompanyActionsFeed | null>("/v1/spine/company/actions", null),
};

// ── Dedicated Action Center snapshot stream (SSE) ──────────────────────────
// Subscribe to the bridge's `/v1/spine/company/actions/stream` feed so the
// Command Center action feed refreshes the moment it changes — an approval,
// hire, blocker, needs-review, or recovery card appears or clears — instead of
// only on the run-event trigger / 20s poll. Tenant-scoped server-side (it
// proxies the SAME `company.actions` read the list route serves); polling-backed
// (~2.5s) + fingerprint-gated, so an unchanged feed pushes nothing. Honest
// polling-backed SSE — NOT a true event bus/websocket. Cookie auth rides the
// same-origin EventSource. The page keeps its bounded poll + invalidation-bus
// fallback so the feed still converges when this never reaches `live`.

export type CompanyActionsConn = "connecting" | "live" | "reconnecting" | "unavailable";

// Open the actions snapshot stream. `onActions` receives the full feed on the
// initial snapshot + every change (same shape as `GET /v1/spine/company/actions`);
// `onConn` reports honest connection state. Manages reconnect with capped
// backoff. Returns an unsubscribe fn.
export function subscribeCompanyActions(
  onActions: (feed: CompanyActionsFeed) => void,
  onConn: (state: CompanyActionsConn) => void,
): () => void {
  let es: EventSource | null = null;
  let closed = false;
  let attempts = 0;
  let backoff = 1000;
  let timer: ReturnType<typeof setTimeout> | null = null;

  const connect = () => {
    if (closed) return;
    onConn(attempts === 0 ? "connecting" : "reconnecting");
    es = new EventSource("/v1/spine/company/actions/stream", { withCredentials: true });
    es.onopen = () => {
      attempts = 0;
      backoff = 1000;
      onConn("live");
    };
    es.addEventListener("actions", (e: MessageEvent) => {
      try {
        const feed = JSON.parse(e.data);
        if (feed && typeof feed === "object") onActions(feed as CompanyActionsFeed);
      } catch {
        /* malformed frame — ignore, the next snapshot corrects it */
      }
    });
    es.onerror = () => {
      es?.close();
      es = null;
      if (closed) return;
      attempts += 1;
      onConn(attempts >= 3 ? "unavailable" : "reconnecting");
      timer = setTimeout(connect, backoff);
      backoff = Math.min(backoff * 2, 15000);
    };
  };

  connect();
  return () => {
    closed = true;
    if (timer) clearTimeout(timer);
    es?.close();
    es = null;
  };
}

// ── Adapter runtime state (admin / session recovery) ──────────────────────
// The persisted adapter runtime rows for ONE Operative (resumable session id,
// accumulated usage/cost, last run status) — the recovery pointer the Settings
// hub exposes (dashboard-design §10). Tenant-scoped, newest first. Reset forgets
// the rows (optionally scoped to one Brief) so a wedged resumable session can be
// cleared without touching the durable run ledger.
export interface RuntimeStateRow {
  agent_id?: string;
  rig?: string;
  brief_key?: string;
  session_id?: string;
  provider?: string;
  model?: string;
  input_tokens?: number;
  output_tokens?: number;
  cost_micros?: number;
  last_run_id?: string;
  last_status?: string;
  last_error?: string;
  updated_at?: number;
  [k: string]: unknown;
}

export const runtimeState = {
  // Global recovery list: every persisted adapter session in the Guild across
  // ALL Operatives, newest first (`GET /v1/runs/runtime-state/list`). Reports
  // failure so the panel shows the bridge's own error, not a blank.
  list: (limit?: number) =>
    tryGetReport<{ rows?: RuntimeStateRow[] } | RuntimeStateRow[] | null>(
      `/v1/runs/runtime-state/list${limit ? `?limit=${limit}` : ""}`,
      null,
    ),
  // Per-agent lookup (the route requires an agent_id). Reports failure so the
  // panel shows the bridge's own error, not a blank.
  get: (agentId: string) =>
    tryGetReport<RuntimeStateRow[] | { rows?: RuntimeStateRow[] } | null>(
      `/v1/runs/runtime-state?agent_id=${encodeURIComponent(agentId)}`,
      null,
    ),
  // Forget persisted runtime state for one agent (optionally one Brief).
  reset: (agentId: string, briefKey?: string) =>
    api.post<{ removed?: number }>("/v1/runs/runtime-state/reset", {
      agent_id: agentId,
      ...(briefKey ? { brief_key: briefKey } : {}),
    }),
};

// ── Operative Skills — procedural memory (read-only) ──────────────────────
// The `memory.skill_*` catalogue, surfaced read-only on the Operative
// workbench (relix-dashboard-design §9 — the agent detail includes Skills).
// The bridge proxies `GET /v1/skills` → `memory.skill_search` on the AI node;
// when the `[skills]` capability is DISABLED the bridge replies with the calm
// shell `{ available:false, reason }` (HTTP 200) instead of a 502, so the panel
// renders an honest unavailable state. Every field is optional — the search
// SUMMARY carries id/name/description/source_agent/confidence/usage_count/
// version/tags/status (no timestamps), but the helper stays defensive so a
// thinner OR fuller row still renders.
export interface SkillSummary {
  id?: string;
  name?: string;
  title?: string;
  description?: string;
  summary?: string;
  body?: string;
  source_agent?: string;
  agent?: string;
  source?: string;
  confidence?: number;
  usage_count?: number;
  version?: number;
  status?: string;
  tags?: string[];
  // Timestamps are not on the search summary today, but accepted defensively
  // (seconds or `_ms`) so a future/fuller shape renders without a change here.
  updated_at?: number;
  updated_at_ms?: number;
  created_at?: number;
  created_at_ms?: number;
  [k: string]: unknown;
}

// The parsed skills-search outcome: either the bridge's `{available:false}`
// shell, or a list pulled defensively from `results`/`items`/`skills`/`rows`/
// a raw array.
export interface SkillSearchResult {
  available: boolean;
  reason: string | null;
  items: SkillSummary[];
}

// Normalize whatever the skills route returned into a `SkillSearchResult`.
// Recognizes the unavailable shell, the documented `{results}` list, the other
// common list keys, and a bare array; anything else degrades to empty.
function parseSkillResult(raw: unknown): SkillSearchResult {
  if (Array.isArray(raw)) {
    return { available: true, reason: null, items: raw as SkillSummary[] };
  }
  if (raw && typeof raw === "object") {
    const o = raw as Record<string, unknown>;
    if (o.available === false) {
      return {
        available: false,
        reason: typeof o.reason === "string" ? o.reason : null,
        items: [],
      };
    }
    for (const k of ["results", "items", "skills", "rows", "list"]) {
      if (Array.isArray(o[k])) {
        return { available: true, reason: null, items: o[k] as SkillSummary[] };
      }
    }
  }
  return { available: true, reason: null, items: [] };
}

export const skills = {
  // Search the catalogue for skills relevant to one Operative. `agent` scopes
  // to the Operative's id; a trimmed `q` adds a substring match. Never throws —
  // a bridge/route failure degrades to an empty (available) result so the tab
  // shows a calm empty state, while the backend's explicit `{available:false}`
  // is surfaced verbatim.
  search: async (opts: {
    agent?: string;
    q?: string;
    limit?: number;
  }): Promise<SkillSearchResult> => {
    const qs = new URLSearchParams();
    const q = opts.q?.trim();
    if (q) qs.set("q", q);
    if (opts.agent) qs.set("agent", opts.agent);
    qs.set("limit", String(opts.limit ?? 20));
    const raw = await tryGet<unknown>(`/v1/skills?${qs.toString()}`, null);
    return parseSkillResult(raw);
  },
  // Aggregate catalogue counts (optional context — degrades to null).
  stats: () => tryGet<unknown>("/v1/skills/stats", null),
};

// Outcome of probing one health dimension. `status` is the HTTP code (null
// when the request never reached the bridge — a network/DNS/TLS failure).
export interface Probe {
  ok: boolean;
  status: number | null;
  detail: string;
  tenant?: string | null;
}

// Low-level health probe used by the diagnostics panel. Never throws: a
// down bridge resolves to `{ ok:false, status:null }` so the panel itself
// can never blank. Reads the `x-relix-tenant` response header when present
// so the panel can show the current Guild/tenant.
export async function probe(path: string): Promise<Probe> {
  try {
    const res = await fetch(path, { method: "GET", credentials: "include" });
    const tenant = res.headers.get("x-relix-tenant");
    if (res.ok) return { ok: true, status: res.status, detail: "ok", tenant };
    const text = await res.text().catch(() => "");
    let detail = `HTTP ${res.status}`;
    if (text) {
      try {
        const j = JSON.parse(text);
        detail = (j && typeof j === "object" && "error" in j ? String(j.error) : text) || detail;
      } catch {
        detail = text.slice(0, 200);
      }
    }
    return { ok: false, status: res.status, detail, tenant };
  } catch (e) {
    return {
      ok: false,
      status: null,
      detail: e instanceof Error ? e.message : "bridge unreachable",
    };
  }
}
