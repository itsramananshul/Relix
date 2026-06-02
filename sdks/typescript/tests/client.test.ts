/**
 * End-to-end tests for ``RelixClient`` (chat, streaming, info, auth,
 * header propagation, error mapping).
 *
 * PART 5: single-response methods return `ApiResult<T>`. Each test
 * narrows on the discriminated union (`if (!res.ok) ...`) before
 * reading the success payload. Streaming methods (`chatStream`) keep
 * their `AsyncIterable<StreamChunk>` shape — the ApiResult wrapping
 * does not apply to multi-value protocols.
 */

import {
  RelixAuthError,
  RelixClient,
  RelixConnectionError,
  RelixResponseError,
  type StreamChunk,
} from "../src";
import { FetchMock, jsonResponse, sseResponse, textResponse } from "./fetchMock";

const BRIDGE = "http://relix-test.local";

function makeClient(mock: FetchMock, opts: Partial<ConstructorParameters<typeof RelixClient>[0]> = {}) {
  return new RelixClient({
    bridgeUrl: BRIDGE,
    apiKey: "tok",
    fetch: mock.fetch,
    ...opts,
  });
}

describe("RelixClient.chat", () => {
  it("returns a ChatResponse with text aliased from reply", async () => {
    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/chat`, () =>
      jsonResponse({
        reply: "hi there",
        flow_id: "f1",
        trace_id: "t1",
        flow_log: "/tmp/log",
        task_id: "task-1",
        workspace_lease_id: "lease-1",
        workspace_path: "/work/acme",
      }),
    );
    const client = makeClient(mock);
    const res = await client.chat({
      sessionId: "u1",
      message: "hello",
      workspaceLeaseId: "lease-1",
    });
    if (!res.ok) {
      throw new Error(`expected ok=true: ${res.error.message}`);
    }
    expect(res.data.text).toBe("hi there");
    expect(res.data.flowId).toBe("f1");
    expect(res.data.traceId).toBe("t1");
    expect(res.data.taskId).toBe("task-1");
    expect(res.data.workspaceLeaseId).toBe("lease-1");
    expect(res.data.workspacePath).toBe("/work/acme");
    expect(JSON.parse(mock.lastCall().body as string).workspace_lease_id).toBe("lease-1");
  });

  it("sends X-Relix-Tenant and Authorization headers", async () => {
    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/chat`, () =>
      jsonResponse({ reply: "ok", flow_id: "f", trace_id: "t", flow_log: "" }),
    );
    const client = makeClient(mock, { tenantId: "acme", apiKey: "my-tok" });
    await client.chat({ sessionId: "u1", message: "hi" });
    const last = mock.lastCall();
    expect(last.headers["x-relix-tenant"]).toBe("acme");
    expect(last.headers.authorization).toBe("Bearer my-tok");
  });

  it("omits Authorization when no api key is configured", async () => {
    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/chat`, () =>
      jsonResponse({ reply: "ok", flow_id: "f", trace_id: "t", flow_log: "" }),
    );
    const client = new RelixClient({ bridgeUrl: BRIDGE, fetch: mock.fetch });
    await client.chat({ sessionId: "u1", message: "hi" });
    const last = mock.lastCall();
    expect(last.headers.authorization).toBeUndefined();
    expect(last.headers["x-relix-tenant"]).toBe("default");
  });

  it("returns ApiResult ok=false with RelixAuthError on 401", async () => {
    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/chat`, () => textResponse("bad token", 401));
    const client = makeClient(mock);
    const res = await client.chat({ sessionId: "u1", message: "hi" });
    expect(res.ok).toBe(false);
    if (res.ok) {
      throw new Error("expected ok=false");
    }
    expect(res.error).toBeInstanceOf(RelixAuthError);
  });

  it("returns ApiResult ok=false with RelixResponseError + status on 500", async () => {
    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/chat`, () => textResponse("boom", 500));
    const client = makeClient(mock);
    const res = await client.chat({ sessionId: "u1", message: "hi" });
    expect(res.ok).toBe(false);
    if (res.ok) {
      throw new Error("expected ok=false");
    }
    expect(res.error).toBeInstanceOf(RelixResponseError);
    expect((res.error as RelixResponseError).statusCode).toBe(500);
    expect((res.error as RelixResponseError).body).toBe("boom");
  });

  it("returns ApiResult ok=false with RelixConnectionError on network failure", async () => {
    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/chat`, () => {
      throw new Error("ECONNREFUSED");
    });
    const client = makeClient(mock);
    const res = await client.chat({ sessionId: "u1", message: "hi" });
    expect(res.ok).toBe(false);
    if (res.ok) {
      throw new Error("expected ok=false");
    }
    expect(res.error).toBeInstanceOf(RelixConnectionError);
  });
});

describe("RelixClient.chatStream", () => {
  it("yields decoded chunks and a terminal done frame", async () => {
    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/chat/stream`, () =>
      sseResponse(["Hello ", "world", "!"]),
    );
    const client = makeClient(mock);
    const frames: StreamChunk[] = [];
    for await (const chunk of client.chatStream({ sessionId: "u1", message: "hi" })) {
      frames.push(chunk);
    }
    const text = frames
      .filter((c) => !c.done)
      .map((c) => c.text)
      .join("");
    expect(text).toBe("Hello world!");
    const done = frames.filter((c) => c.done);
    expect(done).toHaveLength(1);
    expect(done[0]?.flowId).toBe("f1");
    expect(done[0]?.traceId).toBe("t1");
    expect(done[0]?.taskId).toBe("task-1");
    expect(done[0]?.workspaceLeaseId).toBe("lease-1");
    expect(done[0]?.workspacePath).toBe("/work/acme");
  });

  it("throws RelixAuthError when the stream endpoint returns 401", async () => {
    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/chat/stream`, () => textResponse("nope", 401));
    const client = makeClient(mock);
    const iter = client.chatStream({ sessionId: "u1", message: "hi" })[Symbol.asyncIterator]();
    await expect(iter.next()).rejects.toBeInstanceOf(RelixAuthError);
  });

  it("tolerates a payload split across multiple stream chunks", async () => {
    // Hand-build a Response whose body is two byte ranges so the
    // eventsource-parser carry-over path is exercised.
    const frames = ["foo", "bar"];
    const body = `event: chunk\ndata: ${JSON.stringify({ chunk: frames[0] })}\n\n` +
      `event: chunk\ndata: ${JSON.stringify({ chunk: frames[1] })}\n\n`;
    const bytes = new TextEncoder().encode(body);
    const half = Math.floor(bytes.length / 2);
    const part1 = bytes.slice(0, half);
    const part2 = bytes.slice(half);
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(part1);
        controller.enqueue(part2);
        controller.close();
      },
    });
    const resp = new Response(stream, {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    });

    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/chat/stream`, () => resp);
    const client = makeClient(mock);

    const text: string[] = [];
    for await (const chunk of client.chatStream({ sessionId: "u1", message: "hi" })) {
      if (!chunk.done) {
        text.push(chunk.text);
      }
    }
    expect(text.join("")).toBe("foobar");
  });
});

describe("RelixClient.info", () => {
  it("returns the bridge info dict wrapped in ApiResult", async () => {
    const mock = new FetchMock();
    mock.on("GET", `${BRIDGE}/v1/info`, () =>
      jsonResponse({ system: "relix", version: "0.1.0", model: "mock" }),
    );
    const client = makeClient(mock);
    const res = await client.info();
    if (!res.ok) {
      throw new Error(`expected ok=true: ${res.error.message}`);
    }
    expect(res.data.system).toBe("relix");
    expect(res.data.version).toBe("0.1.0");
  });
});

describe("RelixClient construction", () => {
  it("strips a trailing slash from bridgeUrl", () => {
    const mock = new FetchMock();
    const client = new RelixClient({ bridgeUrl: "http://x/", fetch: mock.fetch });
    expect(client.bridgeUrl).toBe("http://x");
  });

  it("defaults tenantId to 'default'", () => {
    const mock = new FetchMock();
    const client = new RelixClient({ bridgeUrl: BRIDGE, fetch: mock.fetch });
    expect(client.tenantId).toBe("default");
  });

  it("throws when no fetch is available and globalThis.fetch is missing", () => {
    // Save and clear the global, then restore.
    const originalFetch = globalThis.fetch as unknown;
    // @ts-expect-error force-clear for test
    delete globalThis.fetch;
    try {
      expect(() => new RelixClient({ bridgeUrl: BRIDGE })).toThrow(/no fetch/);
    } finally {
      // @ts-expect-error restore
      globalThis.fetch = originalFetch;
    }
  });
});
