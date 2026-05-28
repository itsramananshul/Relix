/**
 * Tests for ``client.memory.*``.
 */

import { RelixClient } from "../src";
import { FetchMock, jsonResponse } from "./fetchMock";

const BRIDGE = "http://relix-test.local";

function client(mock: FetchMock) {
  return new RelixClient({ bridgeUrl: BRIDGE, apiKey: "tok", fetch: mock.fetch });
}

describe("client.memory.search", () => {
  it("returns parsed MemoryResult[] from the bridge's results array", async () => {
    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/v1/memory/search`, () =>
      jsonResponse({
        results: [
          { embedding_id: "e1", score: 0.93, chunk_text: "pricing" },
          { embedding_id: "e2", score: 0.71, chunk_text: "notes" },
        ],
        count: 2,
      }),
    );
    const c = client(mock);
    const results = await c.memory.search({
      query: "pricing",
      subjectId: "u1",
      limit: 5,
    });
    expect(results).toHaveLength(2);
    expect(results[0]?.id).toBe("e1");
    expect(results[0]?.text).toBe("pricing");
    expect(results[0]?.score).toBeCloseTo(0.93);
  });

  it("sends the expected request body shape (snake_case keys)", async () => {
    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/v1/memory/search`, () =>
      jsonResponse({ results: [], count: 0 }),
    );
    const c = client(mock);
    await c.memory.search({ query: "pricing", subjectId: "u1" });
    const body = JSON.parse(mock.lastCall().body ?? "{}");
    expect(body.subject_id).toBe("u1");
    expect(body.query).toBe("pricing");
    expect(body.target).toBe("agent");
    expect(body.limit).toBe(5);
  });

  it("tolerates a bare-list response shape", async () => {
    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/v1/memory/search`, () =>
      jsonResponse([{ embedding_id: "e1", score: 0.5, chunk_text: "x" }]),
    );
    const c = client(mock);
    const results = await c.memory.search({ query: "x", subjectId: "u1" });
    expect(results).toHaveLength(1);
    expect(results[0]?.id).toBe("e1");
  });
});

describe("client.memory.ingestDocument", () => {
  it("sends the full body and parses the result", async () => {
    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/v1/memory/ingest`, () =>
      jsonResponse({
        chunks_created: 3,
        source: "notes.md",
        subject_id: "u1",
        embedded: 3,
        deferred_embeddings: 0,
        content_type: "markdown",
      }),
    );
    const c = client(mock);
    const result = await c.memory.ingestDocument({
      subjectId: "u1",
      content: "# notes",
      contentType: "markdown",
      source: "notes.md",
    });
    expect(result.chunksCreated).toBe(3);
    expect(result.contentType).toBe("markdown");
    const body = JSON.parse(mock.lastCall().body ?? "{}");
    expect(body.subject_id).toBe("u1");
    expect(body.content_type).toBe("markdown");
    expect(body.source).toBe("notes.md");
  });
});

describe("client.memory.dialectic", () => {
  it("returns a typed answer", async () => {
    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/v1/memory/dialectic`, () =>
      jsonResponse({
        answer: "User prefers concise replies.",
        confidence: 0.82,
        supporting_observations: [{ id: "o1" }],
      }),
    );
    const c = client(mock);
    const ans = await c.memory.dialectic({
      question: "what?",
      subjectId: "u1",
    });
    expect(ans.answer).toContain("concise");
    expect(ans.confidence).toBeCloseTo(0.82);
    expect(ans.supportingObservations).toHaveLength(1);
  });
});

describe("client.memory.flushContext", () => {
  it("returns flushed and kept counts", async () => {
    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/v1/memory/context_flush`, () =>
      jsonResponse({ flushed_count: 7, kept_count: 5 }),
    );
    const c = client(mock);
    const r = await c.memory.flushContext({ sessionId: "s1", keepRecent: 5 });
    expect(r.flushedCount).toBe(7);
    expect(r.keptCount).toBe(5);
  });
});
