/**
 * Memory sub-API. Reached via `client.memory`.
 *
 * Wraps the bridge's memory-layer endpoints:
 * * `POST /v1/memory/search`
 * * `POST /v1/memory/ingest`
 * * `POST /v1/memory/dialectic`
 * * `POST /v1/memory/context_flush`
 */

import { doJsonRequest, isObject, runRequest, type RelixClient } from "./client";
import type {
  ApiResult,
  DialecticAnswer,
  FlushContextResult,
  IngestDocumentResult,
  MemoryDialecticInput,
  MemoryFlushContextInput,
  MemoryIngestDocumentInput,
  MemoryResult,
  MemorySearchInput,
} from "./types";

export class MemoryAPI {
  constructor(private readonly client: RelixClient) {}

  /** Semantic search over the subject's persistent memory. */
  async search(input: MemorySearchInput): Promise<ApiResult<MemoryResult[]>> {
    return runRequest(async () => {
      const body: Record<string, unknown> = {
        subject_id: input.subjectId,
        target: input.target ?? "agent",
        query: input.query,
        limit: input.limit ?? 5,
      };
      if (input.peer) {
        body.peer = input.peer;
      }
      const data = await doJsonRequest(this.client, "POST", "/v1/memory/search", body);
      return parseSearchResults(data);
    });
  }

  /** Ingest a text / markdown / code / pdf document. */
  async ingestDocument(
    input: MemoryIngestDocumentInput,
  ): Promise<ApiResult<IngestDocumentResult>> {
    return runRequest(async () => {
      const body: Record<string, unknown> = {
        subject_id: input.subjectId,
        observer_id: input.observerId ?? "sdk-typescript",
        source: input.source ?? "sdk",
        content: input.content,
        content_type: input.contentType ?? "markdown",
      };
      if (input.chunkSizeChars !== undefined) {
        body.chunk_size_chars = input.chunkSizeChars;
      }
      if (input.peer) {
        body.peer = input.peer;
      }
      const data = await doJsonRequest(this.client, "POST", "/v1/memory/ingest", body);
      return parseIngestResult(data);
    });
  }

  /** Ask the memory store to synthesise an answer to a question. */
  async dialectic(input: MemoryDialecticInput): Promise<ApiResult<DialecticAnswer>> {
    return runRequest(async () => {
      const body: Record<string, unknown> = {
        observer_id: input.observerId ?? "sdk-typescript",
        subject_id: input.subjectId,
        question: input.question,
      };
      if (input.peer) {
        body.peer = input.peer;
      }
      const data = await doJsonRequest(this.client, "POST", "/v1/memory/dialectic", body);
      return parseDialectic(data);
    });
  }

  /** Explicit context-window flush for `sessionId`. */
  async flushContext(input: MemoryFlushContextInput): Promise<ApiResult<FlushContextResult>> {
    return runRequest(async () => {
      const body: Record<string, unknown> = {
        session_id: input.sessionId,
        keep_recent_n: input.keepRecent ?? 5,
      };
      if (input.peer) {
        body.peer = input.peer;
      }
      const data = await doJsonRequest(this.client, "POST", "/v1/memory/context_flush", body);
      return parseFlushResult(data);
    });
  }
}

function parseSearchResults(data: unknown): MemoryResult[] {
  let rows: unknown[];
  if (Array.isArray(data)) {
    rows = data;
  } else if (isObject(data)) {
    const r = data["results"] ?? data["hits"];
    rows = Array.isArray(r) ? r : [];
  } else {
    rows = [];
  }
  return rows.map(parseSearchRow);
}

function parseSearchRow(row: unknown): MemoryResult {
  const r = isObject(row) ? row : {};
  const id =
    typeof r["id"] === "string"
      ? (r["id"] as string)
      : typeof r["embedding_id"] === "string"
        ? (r["embedding_id"] as string)
        : "";
  const text =
    typeof r["text"] === "string"
      ? (r["text"] as string)
      : typeof r["chunk_text"] === "string"
        ? (r["chunk_text"] as string)
        : typeof r["content"] === "string"
          ? (r["content"] as string)
          : "";
  const score = typeof r["score"] === "number" ? (r["score"] as number) : 0;
  const tags =
    Array.isArray(r["tags"])
      ? (r["tags"] as unknown[]).filter((t): t is string => typeof t === "string")
      : [];
  return {
    id,
    text,
    score,
    tags,
    layer: typeof r["layer"] === "string" ? (r["layer"] as string) : undefined,
    confidence: typeof r["confidence"] === "number" ? (r["confidence"] as number) : undefined,
  };
}

function parseIngestResult(data: unknown): IngestDocumentResult {
  const r = isObject(data) ? data : {};
  return {
    chunksCreated: numField(r, "chunks_created"),
    embedded: numField(r, "embedded"),
    deferredEmbeddings: numField(r, "deferred_embeddings"),
    source: strField(r, "source"),
    subjectId: strField(r, "subject_id"),
    contentType: strField(r, "content_type"),
  };
}

function parseDialectic(data: unknown): DialecticAnswer {
  const r = isObject(data) ? data : {};
  const supporting = Array.isArray(r["supporting_observations"])
    ? (r["supporting_observations"] as unknown[])
    : [];
  return {
    answer: strField(r, "answer"),
    confidence: typeof r["confidence"] === "number" ? (r["confidence"] as number) : 0,
    supportingObservations: supporting,
  };
}

function parseFlushResult(data: unknown): FlushContextResult {
  const r = isObject(data) ? data : {};
  return {
    flushedCount: numField(r, "flushed_count"),
    keptCount: numField(r, "kept_count"),
  };
}

function numField(r: Record<string, unknown>, key: string): number {
  return typeof r[key] === "number" ? (r[key] as number) : 0;
}

function strField(r: Record<string, unknown>, key: string): string {
  return typeof r[key] === "string" ? (r[key] as string) : "";
}
