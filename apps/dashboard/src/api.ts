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
  status: string; // open | resolved | rejected
  response?: string | null;
  created_at?: number;
  resolved_at?: number | null;
  resolved_by?: string | null;
  // Present only on `suggest_tasks` cards.
  proposal?: BriefProposal | null;
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
