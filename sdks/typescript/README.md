# @relix/sdk — TypeScript SDK for Relix

> Version 0.4.1

Typed TypeScript client for the [Relix](https://github.com/itsramananshul/Relix) AI agent mesh platform. Wraps the Relix web bridge's HTTP surface — chat (one-shot + streaming), memory (search, ingest, dialectic, context flush), planning, skills, and observability.

## Installation

```bash
npm install @relix/sdk
# or
pnpm add @relix/sdk
# or
yarn add @relix/sdk
```

Requires Node 18+. Uses the native global `fetch`; no axios, no node-fetch.

## Quick start

Start a local Relix bridge (or point at any reachable one), grab its bearer token from `~/.relix/bridge-token`, then:

```ts
import { RelixClient } from "@relix/sdk";

const client = new RelixClient({
  bridgeUrl: "http://localhost:19791",
  apiKey: process.env.RELIX_TOKEN,
  tenantId: "my-tenant",
});

const reply = await client.chat({
  sessionId: "user-123",
  message: "What do you know about me?",
});
if (reply.ok) {
  console.log(reply.data.text, reply.data.taskId);
}
```

## Chat

```ts
// One-shot
const resp = await client.chat({
  sessionId: "user-123",
  message: "hi",
  agent: "coordinator",
  workspaceLeaseId: "wsl_123", // optional execution workspace binding
});
if (resp.ok) {
  console.log(resp.data.text, resp.data.flowId, resp.data.traceId);
  console.log(resp.data.taskId, resp.data.workspaceLeaseId, resp.data.workspacePath);
}

// Streaming
for await (const chunk of client.chatStream({
  sessionId: "user-123",
  message: "tell me a story",
})) {
  if (!chunk.done) {
    process.stdout.write(chunk.text);
  }
}
```

The terminal frame has `chunk.done === true` and carries `flowId` / `traceId` / `flowLog` plus `taskId`, `workspaceLeaseId`, and `workspacePath` when the bridge created or resolved those bindings.

## Memory

```ts
// Semantic search.
const hits = await client.memory.search({
  query: "pricing discussion",
  subjectId: "user-123",
  limit: 5,
});

// Ingest a document.
const result = await client.memory.ingestDocument({
  subjectId: "user-123",
  content: "# Notes\n\nPricing discussion",
  contentType: "markdown",
  source: "notes.md",
});
console.log(`Created ${result.chunksCreated} chunks`);

// Dialectic synthesis.
const answer = await client.memory.dialectic({
  question: "What are this user's top concerns?",
  subjectId: "user-123",
});
console.log(answer.answer, answer.confidence);

// Context flush.
const flushed = await client.memory.flushContext({
  sessionId: "sess-123",
  keepRecent: 5,
});
```

## Planning

```ts
const plan = await client.planning.plan({
  spec: "Research Rust async and summarize in under 300 words",
  maxAgents: 3,
  dryRun: false,
});
console.log(plan.workflowYaml);
console.log(plan.orchestratorActivated, plan.criticApproved);

// List agents.
const agents = await client.planning.agents();

// Find agents matching a task.
const matches = await client.planning.searchAgents("research");
```

## Skills

```ts
const skills = await client.skills.search({
  query: "web research",
  minConfidence: 0.7,
  limit: 10,
});

const stats = await client.skills.stats();
console.log(stats.totalSkills, stats.avgConfidence);

const skill = await client.skills.get("skill_abc123");
```

## Observability

```ts
const health = await client.observability.health({ hours: 24 });
for (const [name, agent] of Object.entries(health.agents)) {
  console.log(`${name}: ${agent.score}/100 (${agent.color})`);
}

const alerts = await client.observability.alerts();
const history = await client.observability.alertHistory({ limit: 50 });
```

## Error handling

```ts
import {
  RelixAuthError,
  RelixConnectionError,
  RelixError,
  RelixResponseError,
  RelixTimeoutError,
} from "@relix/sdk";

try {
  await client.chat({ sessionId: "u1", message: "hi" });
} catch (err) {
  if (err instanceof RelixAuthError) {
    // rotate the bridge token
  } else if (err instanceof RelixConnectionError) {
    console.log("bridge unreachable:", err.message);
  } else if (err instanceof RelixError) {
    console.log(`status=${err.statusCode} body=${err.body}`);
  }
}
```

## Tenant scoping

Every request rides on the `X-Relix-Tenant` header:

```ts
const client = new RelixClient({ apiKey: "...", tenantId: "acme" });
```

The bridge stamps the value onto every audit row and routes memory / policy / audit through the per-tenant pipeline when the operator has wired the GAP-23 multi-tenant configuration.

## Configuration

| Option       | Default                    | Description                                                              |
|--------------|----------------------------|--------------------------------------------------------------------------|
| `bridgeUrl`  | `http://localhost:19791`   | Bridge HTTP base URL.                                                    |
| `apiKey`     | `undefined`                | Bridge bearer token; sent as `Authorization: Bearer <token>`.            |
| `tenantId`   | `"default"`                | Value of the `X-Relix-Tenant` header.                                    |
| `timeout`    | `30_000`                   | Per-request timeout (ms).                                                |
| `fetch`      | `globalThis.fetch`          | Override the fetch impl (tests, edge runtimes that need a polyfill).     |

## Comparison with the Rust `relix-sdk` crate

The Rust crate (`crates/relix-sdk`) is a leaner client that covers only the
core bridge surface: `GET /v1/info`, `POST /chat`, `POST /chat/stream` (SSE),
`POST /v1/memory/embed`, and `POST /v1/memory/search`. It does not expose
planning, skills, observability, or the dialectic / context-flush memory
endpoints.

One notable difference: the Rust SDK's `search()` method hard-codes
`top_k: 10` with no configurable override. The TypeScript SDK exposes a
`limit` parameter that is forwarded to the bridge. When calling the Rust SDK
from a context where a different result count is needed, use `relix-embedded`
directly or the Python/TypeScript SDKs instead.

## Documentation

Full Relix docs at <https://github.com/itsramananshul/Relix/tree/main/docs>. Bridge wire contract: `crates/relix-web-bridge/src/`.

## License

MIT
