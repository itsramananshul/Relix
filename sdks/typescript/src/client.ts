/**
 * Core HTTP client for the Relix bridge.
 *
 * Owns the bridge URL, bearer token, tenant id, and the `fetch`
 * implementation used by every request. The sub-APIs
 * (`client.memory`, `client.planning`, `client.skills`,
 * `client.observability`) reach back into this module for the actual
 * HTTP work via the package-internal helpers below the class.
 */

import { createParser, type EventSourceMessage } from "eventsource-parser";

import { CredentialsAPI } from "./credentials";
import { IdentityAPI } from "./identity";
import { MemoryAPI } from "./memory";
import { ObservabilityAPI } from "./observability";
import { PlanningAPI } from "./planning";
import { SkillsAPI } from "./skills";
import {
  ApiResult,
  ChatInput,
  ChatResponse,
  ChatUsage,
  RelixAuthError,
  RelixClientOptions,
  RelixConnectionError,
  RelixError,
  RelixResponseError,
  RelixTimeoutError,
  StreamChunk,
} from "./types";

const DEFAULT_BRIDGE_URL = "http://localhost:19791";
const DEFAULT_TIMEOUT_MS = 30_000;
const DEFAULT_TENANT = "default";
const SDK_USER_AGENT = "relix-typescript-sdk/0.1.0";

/**
 * Coerce an arbitrary unknown-shaped value into a `ChatResponse`.
 *
 * The bridge body is
 * `{ "reply": "...", "flow_id": "...", "trace_id": "...", "flow_log": "...",
 *    "task_id"?: "...", "workspace_lease_id"?: "...", "workspace_path"?: "..." }`;
 * we normalise to camelCase + alias `reply` → `text` for parity with
 * the OpenAI shim's `choices[0].message.content`. Unknown extras
 * pass through verbatim so a future bridge addition does not break
 * existing callers.
 */
function parseChatResponse(body: unknown): ChatResponse {
  if (!isObject(body)) {
    throw new RelixResponseError("chat response was not a JSON object", {
      statusCode: 200,
      body: JSON.stringify(body),
    });
  }
  const usageRaw = body["usage"];
  const usage: ChatUsage | undefined = isObject(usageRaw)
    ? camelKeys(usageRaw as Record<string, unknown>) as ChatUsage
    : undefined;
  const text =
    typeof body["reply"] === "string"
      ? (body["reply"] as string)
      : typeof body["text"] === "string"
        ? (body["text"] as string)
        : "";
  return {
    text,
    flowId: typeof body["flow_id"] === "string" ? (body["flow_id"] as string) : "",
    traceId: typeof body["trace_id"] === "string" ? (body["trace_id"] as string) : "",
    flowLog: typeof body["flow_log"] === "string" ? (body["flow_log"] as string) : "",
    taskId: typeof body["task_id"] === "string" ? (body["task_id"] as string) : undefined,
    workspaceLeaseId:
      typeof body["workspace_lease_id"] === "string"
        ? (body["workspace_lease_id"] as string)
        : undefined,
    workspacePath:
      typeof body["workspace_path"] === "string" ? (body["workspace_path"] as string) : undefined,
    model: typeof body["model"] === "string" ? (body["model"] as string) : undefined,
    usage,
    ...stripKeys(body, [
      "reply",
      "text",
      "flow_id",
      "trace_id",
      "flow_log",
      "task_id",
      "workspace_lease_id",
      "workspace_path",
      "model",
      "usage",
    ]),
  };
}

/**
 * Parse one SSE message payload from the bridge.
 *
 * The bridge emits `event: chunk` frames carrying
 * `{"chunk": "..."}` (or `{"text": "..."}`) and a terminal
 * `event: done` frame carrying the finalisation metadata. Be lenient
 * about which field name holds the text so a future protocol tweak
 * doesn't break the consumer.
 */
function parseStreamMessage(msg: EventSourceMessage): StreamChunk | null {
  const raw = msg.data?.trim();
  if (!raw || raw === "[DONE]") {
    return null;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return { text: raw, done: msg.event === "done" };
  }
  if (typeof parsed === "string") {
    return { text: parsed, done: msg.event === "done" };
  }
  if (!isObject(parsed)) {
    return null;
  }
  const o = parsed as Record<string, unknown>;
  const text =
    (typeof o["chunk"] === "string" ? (o["chunk"] as string) : undefined) ??
    (typeof o["text"] === "string" ? (o["text"] as string) : undefined) ??
    "";
  return {
    text,
    done: msg.event === "done",
    flowId: typeof o["flow_id"] === "string" ? (o["flow_id"] as string) : undefined,
    traceId: typeof o["trace_id"] === "string" ? (o["trace_id"] as string) : undefined,
    flowLog: typeof o["flow_log"] === "string" ? (o["flow_log"] as string) : undefined,
    taskId: typeof o["task_id"] === "string" ? (o["task_id"] as string) : undefined,
    workspaceLeaseId:
      typeof o["workspace_lease_id"] === "string" ? (o["workspace_lease_id"] as string) : undefined,
    workspacePath: typeof o["workspace_path"] === "string" ? (o["workspace_path"] as string) : undefined,
  };
}

/** Type guard for plain objects. */
export function isObject(x: unknown): x is Record<string, unknown> {
  return typeof x === "object" && x !== null && !Array.isArray(x);
}

/**
 * Translate one snake_case `Record<string, unknown>` into a
 * camelCase clone.
 *
 * Shallow — the bridge response bodies are flat enough that a deep
 * recursion would only add complexity without buying anything.
 */
export function camelKeys(input: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(input)) {
    out[snakeToCamel(key)] = value;
  }
  return out;
}

/** Strip a fixed set of keys from a record clone. Used for forward-compat. */
function stripKeys(
  input: Record<string, unknown>,
  drop: readonly string[],
): Record<string, unknown> {
  const dropSet = new Set(drop);
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(input)) {
    if (!dropSet.has(k)) {
      out[k] = v;
    }
  }
  return out;
}

function snakeToCamel(s: string): string {
  if (!s.includes("_")) {
    return s;
  }
  return s.replace(/_([a-z])/g, (_, c) => (c as string).toUpperCase());
}

/**
 * Translate a `fetch` failure (network error, abort) into the SDK
 * exception hierarchy.
 */
function translateTransportError(err: unknown): RelixConnectionError | RelixTimeoutError {
  if (err instanceof Error) {
    if (err.name === "AbortError") {
      return new RelixTimeoutError(`request timed out: ${err.message}`);
    }
    return new RelixConnectionError(`cannot reach bridge: ${err.message}`);
  }
  return new RelixConnectionError(`transport: ${String(err)}`);
}

/**
 * Translate a non-2xx response into the SDK exception hierarchy.
 *
 * 401 → `RelixAuthError`. Every other 4xx / 5xx → `RelixResponseError`
 * with the status code attached so callers can branch.
 */
async function translateStatusError(resp: Response): Promise<Error> {
  let body = "";
  try {
    body = await resp.text();
  } catch {
    // ignore — keep body empty
  }
  if (resp.status === 401) {
    return new RelixAuthError("bridge rejected the bearer token (401)", { body });
  }
  return new RelixResponseError(`bridge returned HTTP ${resp.status}`, {
    statusCode: resp.status,
    body,
  });
}

/** Build the headers ride on every request. */
function buildHeaders(
  apiKey: string | undefined,
  tenantId: string,
  extra: Record<string, string> = {},
): Record<string, string> {
  const headers: Record<string, string> = {
    "content-type": "application/json",
    accept: "application/json",
    "x-relix-tenant": tenantId,
    "user-agent": SDK_USER_AGENT,
  };
  if (apiKey) {
    headers.authorization = `Bearer ${apiKey}`;
  }
  for (const [k, v] of Object.entries(extra)) {
    headers[k.toLowerCase()] = v;
  }
  return headers;
}

/**
 * Synchronous-looking async wrapper around `fetch` with timeout +
 * error translation. Centralised so the four sub-APIs all share the
 * same auth / tenant / timeout behaviour.
 *
 * Exported package-internally so the sub-API files can call it
 * without going through `RelixClient` instance state — keeps the
 * sub-APIs structurally independent (they own one `RelixClient`
 * reference each).
 */
export async function doJsonRequest(
  client: RelixClient,
  method: string,
  path: string,
  body?: unknown,
  queryParams?: Record<string, unknown>,
): Promise<unknown> {
  const url = buildUrl(client.bridgeUrl, path, queryParams);
  const headers = buildHeaders(client.apiKey, client.tenantId);
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), client.timeoutMs);
  let resp: Response;
  try {
    resp = await client.fetchImpl(url, {
      method,
      headers,
      body: body !== undefined ? JSON.stringify(body) : undefined,
      signal: controller.signal,
    });
  } catch (err) {
    throw translateTransportError(err);
  } finally {
    clearTimeout(timer);
  }
  if (!resp.ok) {
    throw await translateStatusError(resp);
  }
  const text = await resp.text();
  if (text === "") {
    return null;
  }
  try {
    return JSON.parse(text);
  } catch (err) {
    throw new RelixResponseError(`bridge response was not valid JSON: ${(err as Error).message}`, {
      statusCode: resp.status,
      body: text,
    });
  }
}

/**
 * PART 5 — wrap an async producer in the `ApiResult<T>` discriminated
 * union. Any `RelixError` thrown by the producer maps to
 * `{ ok: false, error }`; any other throw is converted into a
 * `RelixError` so the API surface never leaks an untyped exception.
 *
 * Used by every single-response SDK method on `RelixClient` and the
 * sub-APIs. Streaming methods (`chatStream`) keep their
 * `AsyncIterable<StreamChunk>` shape — the discriminated union is
 * orthogonal to multi-value protocols.
 */
export async function runRequest<T>(
  producer: () => Promise<T>,
): Promise<ApiResult<T>> {
  try {
    const data = await producer();
    return { ok: true, data };
  } catch (err) {
    if (err instanceof RelixError) {
      return { ok: false, error: err };
    }
    if (err instanceof Error) {
      return {
        ok: false,
        error: new RelixConnectionError(`unexpected SDK error: ${err.message}`),
      };
    }
    return {
      ok: false,
      error: new RelixConnectionError(`unexpected SDK error: ${String(err)}`),
    };
  }
}

/** Assemble a final URL with optional query string. */
function buildUrl(
  base: string,
  path: string,
  queryParams: Record<string, unknown> | undefined,
): string {
  if (/^https?:\/\//.test(path)) {
    return path;
  }
  const cleanPath = path.startsWith("/") ? path : `/${path}`;
  const url = `${base}${cleanPath}`;
  if (!queryParams) {
    return url;
  }
  const params: string[] = [];
  for (const [k, v] of Object.entries(queryParams)) {
    if (v === undefined || v === null || v === "") {
      continue;
    }
    if (typeof v === "boolean") {
      params.push(`${encodeURIComponent(k)}=${v ? "true" : "false"}`);
    } else {
      params.push(`${encodeURIComponent(k)}=${encodeURIComponent(String(v))}`);
    }
  }
  return params.length > 0 ? `${url}?${params.join("&")}` : url;
}

/**
 * Public Relix HTTP client. One instance per bridge / tenant /
 * api-key triple is the intended usage; the `fetch` underlying it
 * is the global one (Node 18+) unless the constructor receives a
 * `fetch` override.
 *
 * @example
 * ```ts
 * import { RelixClient } from "@relix/sdk";
 *
 * const client = new RelixClient({
 *   bridgeUrl: "http://localhost:19791",
 *   apiKey: process.env.RELIX_TOKEN,
 *   tenantId: "acme",
 * });
 * const reply = await client.chat({ sessionId: "user-1", message: "hi" });
 * console.log(reply.text);
 * ```
 */
export class RelixClient {
  public readonly bridgeUrl: string;
  public readonly tenantId: string;
  public readonly apiKey: string | undefined;
  public readonly timeoutMs: number;
  public readonly fetchImpl: typeof fetch;

  public readonly memory: MemoryAPI;
  public readonly planning: PlanningAPI;
  public readonly skills: SkillsAPI;
  public readonly observability: ObservabilityAPI;
  public readonly credentials: CredentialsAPI;
  public readonly identity: IdentityAPI;

  constructor(options: RelixClientOptions = {}) {
    this.bridgeUrl = (options.bridgeUrl ?? DEFAULT_BRIDGE_URL).replace(/\/+$/, "");
    this.tenantId = options.tenantId ?? DEFAULT_TENANT;
    this.apiKey = options.apiKey;
    this.timeoutMs = options.timeout ?? DEFAULT_TIMEOUT_MS;
    const fetchImpl = options.fetch ?? globalThis.fetch;
    if (typeof fetchImpl !== "function") {
      throw new Error(
        "no fetch implementation available — pass `fetch` in RelixClientOptions or run on Node 18+",
      );
    }
    this.fetchImpl = fetchImpl;

    this.memory = new MemoryAPI(this);
    this.planning = new PlanningAPI(this);
    this.skills = new SkillsAPI(this);
    this.observability = new ObservabilityAPI(this);
    this.credentials = new CredentialsAPI(this);
    this.identity = new IdentityAPI(this);
  }

  /**
   * `POST /chat`. Returns the bridge's chat response wrapped in an
   * `ApiResult<ChatResponse>` discriminated union — callers branch on
   * `result.ok` instead of try/catch.
   */
  async chat(input: ChatInput): Promise<ApiResult<ChatResponse>> {
    return runRequest(async () => {
      const body: Record<string, unknown> = {
        session_id: input.sessionId,
        message: input.message,
      };
      if (input.agent) {
        body.agent = input.agent;
      }
      if (input.workspaceLeaseId) {
        body.workspace_lease_id = input.workspaceLeaseId;
      }
      const data = await doJsonRequest(this, "POST", "/chat", body);
      return parseChatResponse(data);
    });
  }

  /**
   * Streaming `POST /chat/stream`. Returns an async iterator of
   * :class:`StreamChunk` frames. Concatenate every `chunk.text` to
   * get the full reply; the terminal frame has `done == true` and
   * carries `flowId` / `traceId` / `flowLog`.
   */
  async *chatStream(input: ChatInput): AsyncIterable<StreamChunk> {
    const url = `${this.bridgeUrl}/chat/stream`;
    const headers = buildHeaders(this.apiKey, this.tenantId, {
      accept: "text/event-stream",
    });
    const reqBody: Record<string, unknown> = {
      session_id: input.sessionId,
      message: input.message,
    };
    if (input.agent) {
      reqBody.agent = input.agent;
    }
    if (input.workspaceLeaseId) {
      reqBody.workspace_lease_id = input.workspaceLeaseId;
    }
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    let resp: Response;
    try {
      resp = await this.fetchImpl(url, {
        method: "POST",
        headers,
        body: JSON.stringify(reqBody),
        signal: controller.signal,
      });
    } catch (err) {
      clearTimeout(timer);
      throw translateTransportError(err);
    }
    if (!resp.ok) {
      clearTimeout(timer);
      throw await translateStatusError(resp);
    }
    if (resp.body === null) {
      clearTimeout(timer);
      return;
    }

    // eventsource-parser handles every framing edge case the bridge
    // can emit: split-across-chunks data lines, multi-line `data:`
    // payloads, comment lines starting with `:`, blank-line frame
    // separators in both LF and CRLF forms.
    const pending: StreamChunk[] = [];
    let parserDone = false;
    const parser = createParser({
      onEvent(msg: EventSourceMessage) {
        const chunk = parseStreamMessage(msg);
        if (chunk !== null) {
          pending.push(chunk);
        }
      },
    });

    const reader = resp.body.getReader();
    const decoder = new TextDecoder("utf-8");
    try {
      while (!parserDone) {
        const { value, done } = await reader.read();
        if (done) {
          parserDone = true;
          break;
        }
        parser.feed(decoder.decode(value, { stream: true }));
        while (pending.length > 0) {
          // Yield each chunk independently so a consumer awaiting
          // backpressure sees one frame per iteration.
          const next = pending.shift();
          if (next !== undefined) {
            yield next;
          }
        }
      }
      // Flush any trailing partial decode.
      const tail = decoder.decode();
      if (tail.length > 0) {
        parser.feed(tail);
      }
      while (pending.length > 0) {
        const next = pending.shift();
        if (next !== undefined) {
          yield next;
        }
      }
    } finally {
      clearTimeout(timer);
      try {
        reader.releaseLock();
      } catch {
        // ignore: the stream may already be closed by the runtime.
      }
    }
  }

  /** `GET /v1/info`. ApiResult-wrapped. */
  async info(): Promise<ApiResult<Record<string, unknown>>> {
    return runRequest(async () => {
      const data = await doJsonRequest(this, "GET", "/v1/info");
      return isObject(data) ? data : {};
    });
  }
}
