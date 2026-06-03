/**
 * Tiny in-process fetch mock for the SDK test suite.
 *
 * Pluggable via the `RelixClientOptions.fetch` hook on the client, so
 * we never have to monkey-patch the global `fetch`. Each handler runs
 * exactly once unless explicitly registered as multi-shot.
 */

export type MockHandler = (
  input: string,
  init: RequestInit,
) => Response | Promise<Response>;

export interface RecordedCall {
  method: string;
  url: string;
  headers: Record<string, string>;
  body: string | null;
}

export class FetchMock {
  private readonly handlers: Map<string, MockHandler[]> = new Map();
  public readonly calls: RecordedCall[] = [];

  /** Register a handler for one full URL string. Last-registered wins. */
  on(method: string, urlPredicate: string | RegExp, handler: MockHandler): void {
    const key = keyFor(method, urlPredicate);
    const arr = this.handlers.get(key) ?? [];
    arr.push(handler);
    this.handlers.set(key, arr);
  }

  /** The fetch impl to hand to RelixClient. */
  get fetch(): typeof fetch {
    return async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
      const url = input instanceof URL ? input.toString() : String(input);
      const method = (init?.method ?? "GET").toUpperCase();
      const headers = headersToRecord(init?.headers);
      const body = init?.body === undefined || init?.body === null ? null : String(init.body);
      this.calls.push({ method, url, headers, body });
      for (const [key, handlers] of this.handlers) {
        const [m, target] = decodeKey(key);
        if (m !== method) {
          continue;
        }
        if (target instanceof RegExp ? target.test(url) : urlMatches(target, url)) {
          const next = handlers.shift();
          if (next !== undefined) {
            return next(url, init ?? {});
          }
        }
      }
      throw new Error(`FetchMock: no handler for ${method} ${url}`);
    };
  }

  /** Most recently recorded call (throws if none). */
  lastCall(): RecordedCall {
    if (this.calls.length === 0) {
      throw new Error("FetchMock.lastCall(): no calls recorded");
    }
    return this.calls[this.calls.length - 1] as RecordedCall;
  }
}

function urlMatches(target: string, actual: string): boolean {
  // Tests can match either a full URL or a base path; the latter is
  // useful when query params are dynamic.
  if (target === actual) {
    return true;
  }
  if (actual.startsWith(target + "?")) {
    return true;
  }
  return false;
}

function keyFor(method: string, target: string | RegExp): string {
  if (target instanceof RegExp) {
    return `${method.toUpperCase()}|R:${target.source}|${target.flags}`;
  }
  return `${method.toUpperCase()}|S:${target}`;
}

function decodeKey(key: string): [string, string | RegExp] {
  const idx = key.indexOf("|");
  const method = key.slice(0, idx);
  const tail = key.slice(idx + 1);
  if (tail.startsWith("R:")) {
    const sep = tail.lastIndexOf("|");
    const source = tail.slice(2, sep);
    const flags = tail.slice(sep + 1);
    return [method, new RegExp(source, flags)];
  }
  return [method, tail.slice(2)];
}

function headersToRecord(h: HeadersInit | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  if (h === undefined) {
    return out;
  }
  if (h instanceof Headers) {
    h.forEach((v, k) => (out[k.toLowerCase()] = v));
    return out;
  }
  if (Array.isArray(h)) {
    for (const [k, v] of h) {
      out[String(k).toLowerCase()] = String(v);
    }
    return out;
  }
  for (const [k, v] of Object.entries(h)) {
    out[String(k).toLowerCase()] = String(v);
  }
  return out;
}

/** Build a JSON `Response` for an SDK test. */
export function jsonResponse(body: unknown, status: number = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/** Build a text `Response` (used for non-2xx body fixtures). */
export function textResponse(body: string, status: number, contentType = "text/plain"): Response {
  return new Response(body, {
    status,
    headers: { "content-type": contentType },
  });
}

/** Build an SSE `Response` from a series of bridge-shape chunk strings. */
export function sseResponse(chunks: string[], withDone: boolean = true): Response {
  const pieces: string[] = chunks.map(
    (c) => `event: chunk\ndata: ${JSON.stringify({ chunk: c })}\n\n`,
  );
  if (withDone) {
    pieces.push(
      `event: done\ndata: ${JSON.stringify({
        flow_id: "f1",
        trace_id: "t1",
        flow_log: "/tmp/x",
        task_id: "task-1",
        workspace_lease_id: "lease-1",
        workspace_path: "/work/acme",
      })}\n\n`,
    );
  }
  return new Response(pieces.join(""), {
    status: 200,
    headers: { "content-type": "text/event-stream" },
  });
}
