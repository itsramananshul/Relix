# relix — Python SDK for Relix

> Version 0.4.1

Typed Python client for the [Relix](https://github.com/itsramananshul/Relix) AI agent mesh platform. Wraps the Relix web bridge's HTTP surface — chat (sync, async, streaming), memory (search, ingest, dialectic, context flush), planning, skills, and observability.

## Installation

```bash
pip install relix
```

Requires Python 3.10+.

## Quick start

Start a local Relix bridge (or point at any reachable one), grab its bearer token from `~/.relix/bridge-token`, then:

```python
from relix import RelixClient

with RelixClient(
    bridge_url="http://localhost:19791",
    api_key="<bridge token>",
    tenant_id="my-tenant",
) as relix:
    reply = relix.chat(session_id="user-123", message="What do you know about me?")
    print(reply.text)
```

Async users:

```python
import asyncio
from relix import RelixClient

async def main():
    async with RelixClient(api_key="<bridge token>") as relix:
        reply = await relix.achat(session_id="user-123", message="hello")
        print(reply.text)

asyncio.run(main())
```

## Chat

Sync, async, and streaming variants share the same `session_id` / `message` shape:

```python
# One-shot
resp = client.chat(
    session_id="user-123",
    message="hi",
    agent="coordinator",
    workspace_lease_id="wsl_123",  # optional execution workspace binding
)
print(resp.text, resp.flow_id, resp.trace_id, resp.task_id)
print(resp.workspace_lease_id, resp.workspace_path)

# Async
resp = await client.achat(session_id="user-123", message="hi")

# Streaming
for chunk in client.chat_stream(session_id="user-123", message="tell me a story"):
    if not chunk.done:
        print(chunk.text, end="", flush=True)

# Async streaming
async for chunk in client.achat_stream(session_id="user-123", message="hello"):
    if not chunk.done:
        print(chunk.text, end="", flush=True)
```

The terminal frame on every stream has `chunk.done == True` and carries `flow_id` / `trace_id` / `flow_log` plus the final `task_id`, `workspace_lease_id`, and `workspace_path` when the bridge created or resolved those bindings.

## Memory

```python
# Semantic search over the subject's persistent memory.
results = client.memory.search(
    query="pricing discussion",
    subject_id="user-123",
    limit=5,
)
for r in results:
    print(r.text, r.score)

# Ingest a document (markdown / txt / code / pdf).
result = client.memory.ingest_document(
    subject_id="user-123",
    content="# Meeting Notes\n\nWe discussed pricing...",
    content_type="markdown",
    source="meeting_notes.md",
)
print(f"Created {result.chunks_created} chunks")

# Dialectic synthesis — ask the memory store a question.
answer = client.memory.dialectic(
    question="What are this user's top concerns about pricing?",
    subject_id="user-123",
    observer_id="coordinator",
)
print(answer.answer, answer.confidence)

# Explicit context-window flush.
flushed = client.memory.flush_context(session_id="sess-123", keep_recent=5)
print(f"Flushed {flushed.flushed_count} turns, kept {flushed.kept_count}")
```

## Planning

```python
# Create + activate a plan.
plan = client.planning.plan(
    spec="Research the latest Rust async developments and write a summary under 300 words",
    max_agents=3,
    dry_run=False,
)
print(plan.workflow_yaml)
print(plan.orchestrator_activated, plan.critic_approved)

# List agents the coordinator knows about.
agents = client.planning.agents()
for a in agents:
    print(a.name, a.description)

# Find agents whose descriptions match a free-form task.
matches = client.planning.search_agents(task="research")
```

## Skills

```python
# Search the skill catalogue.
skills = client.skills.search(query="web research", min_confidence=0.7, limit=10)

# Aggregate stats.
stats = client.skills.stats()
print(stats.total_skills, stats.avg_confidence)

# Full detail for one skill (steps + version history).
skill = client.skills.get("skill_abc123")
```

## Observability

```python
# Per-agent + deployment health roll-up.
health = client.observability.health(hours=24)
for name, agent in health.agents.items():
    print(f"{name}: {agent.score}/100 ({agent.color})")

# Currently-firing alerts.
for alert in client.observability.alerts():
    print(alert.kind, alert.agent, alert.severity, alert.message)

# Recent alert history.
history = client.observability.alert_history(limit=50)
```

## Error handling

Every method raises a subclass of `RelixError`:

```python
from relix import (
    RelixAuthError,        # 401
    RelixConnectionError,  # bridge unreachable
    RelixError,            # base class
    RelixResponseError,    # any non-2xx other than 401
    RelixTimeoutError,     # request timed out
)

try:
    response = client.chat(session_id="u1", message="hello")
except RelixAuthError:
    # rotate the bridge token at ~/.relix/bridge-token
    ...
except RelixConnectionError as e:
    print("bridge unreachable:", e)
except RelixError as e:
    print(f"relix error: status={e.status_code} body={e.body}")
```

## Tenant scoping

Every request rides on the `X-Relix-Tenant` header. Configure once on the client:

```python
client = RelixClient(api_key="...", tenant_id="acme")
```

The bridge stamps the value onto every audit row and routes memory / policy / audit through the per-tenant pipeline when the operator has wired the GAP-23 multi-tenant configuration.

## Configuration knobs

| Argument      | Default                   | Description                                                   |
|---------------|---------------------------|---------------------------------------------------------------|
| `bridge_url`  | `http://localhost:19791`  | Bridge HTTP base URL. Trailing slash is stripped.            |
| `tenant_id`   | `"default"`               | Value of the `X-Relix-Tenant` header.                         |
| `api_key`     | `None`                    | Bridge bearer token; sent as `Authorization: Bearer <token>`. |
| `timeout`     | `30.0`                    | Per-request timeout in seconds.                               |

## Comparison with the Rust `relix-sdk` crate

The Rust crate (`crates/relix-sdk`) is a leaner client that covers only the
core bridge surface: `GET /v1/info`, `POST /chat`, `POST /chat/stream` (SSE),
`POST /v1/memory/embed`, and `POST /v1/memory/search`. It does not expose
planning, skills, observability, or the dialectic / context-flush memory
endpoints.

One notable difference: the Rust SDK's `search()` method hard-codes
`top_k: 10` with no configurable override. The Python SDK exposes a `limit`
parameter that is forwarded to the bridge. When calling the Rust SDK from a
context where a different result count is needed, use `relix-embedded`
directly or the Python/TypeScript SDKs instead.

## Documentation

The full Relix documentation lives at <https://github.com/itsramananshul/Relix/tree/main/docs>. The wire contract this SDK depends on is documented in `docs/bridge-invariants.md` and the bridge handler modules at `crates/relix-web-bridge/src/`.

## License

MIT
