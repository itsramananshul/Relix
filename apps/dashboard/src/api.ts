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
