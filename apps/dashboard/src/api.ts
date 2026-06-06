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
