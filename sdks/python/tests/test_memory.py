"""Tests for ``client.memory.*``."""

from __future__ import annotations

import httpx
import pytest
import respx

from relix import (
    DialecticAnswer,
    IngestDocumentResult,
    MemoryResult,
    RelixClient,
)

BRIDGE = "http://relix-test.local"


@respx.mock
def test_memory_search_returns_parsed_memory_result_list() -> None:
    route = respx.post(f"{BRIDGE}/v1/memory/search").mock(
        return_value=httpx.Response(
            200,
            json={
                "results": [
                    {
                        "embedding_id": "e1",
                        "score": 0.93,
                        "chunk_text": "pricing discussion",
                    },
                    {
                        "embedding_id": "e2",
                        "score": 0.71,
                        "chunk_text": "follow-up notes",
                    },
                ],
                "count": 2,
            },
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        results = c.memory.search(
            query="pricing", subject_id="user-1", limit=5
        )
    assert len(results) == 2
    assert isinstance(results[0], MemoryResult)
    assert results[0].id == "e1"
    assert results[0].text == "pricing discussion"
    assert results[0].score == pytest.approx(0.93)
    body = route.calls.last.request.read().decode()
    assert '"subject_id":"user-1"' in body.replace(" ", "")
    assert '"query":"pricing"' in body.replace(" ", "")
    assert '"limit":5' in body.replace(" ", "")


@respx.mock
def test_memory_search_defaults_target_to_agent() -> None:
    route = respx.post(f"{BRIDGE}/v1/memory/search").mock(
        return_value=httpx.Response(200, json={"results": [], "count": 0})
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        c.memory.search(query="x", subject_id="u1")
    body = route.calls.last.request.read().decode()
    assert '"target":"agent"' in body.replace(" ", "")


@respx.mock
def test_memory_search_accepts_list_response_shape() -> None:
    """Bridge may also return a bare list; the SDK tolerates both."""
    respx.post(f"{BRIDGE}/v1/memory/search").mock(
        return_value=httpx.Response(
            200, json=[{"embedding_id": "e1", "score": 0.5, "chunk_text": "x"}]
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        results = c.memory.search(query="x", subject_id="u1")
    assert len(results) == 1
    assert results[0].id == "e1"


@respx.mock
def test_memory_ingest_document_sends_full_body_and_parses_result() -> None:
    route = respx.post(f"{BRIDGE}/v1/memory/ingest").mock(
        return_value=httpx.Response(
            200,
            json={
                "chunks_created": 3,
                "source": "notes.md",
                "subject_id": "u1",
                "embedded": 3,
                "deferred_embeddings": 0,
                "content_type": "markdown",
            },
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        result = c.memory.ingest_document(
            subject_id="u1",
            content="# Notes\n\nPricing",
            content_type="markdown",
            source="notes.md",
        )
    assert isinstance(result, IngestDocumentResult)
    assert result.chunks_created == 3
    assert result.content_type == "markdown"
    body = route.calls.last.request.read().decode()
    cleaned = body.replace(" ", "")
    assert '"subject_id":"u1"' in cleaned
    assert '"source":"notes.md"' in cleaned
    assert '"content_type":"markdown"' in cleaned
    assert "Pricing" in body


@respx.mock
def test_memory_dialectic_returns_typed_answer() -> None:
    respx.post(f"{BRIDGE}/v1/memory/dialectic").mock(
        return_value=httpx.Response(
            200,
            json={
                "answer": "The user prefers concise replies.",
                "confidence": 0.82,
                "supporting_observations": [{"id": "o1"}],
            },
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        ans = c.memory.dialectic(
            question="what does the user prefer?",
            subject_id="u1",
        )
    assert isinstance(ans, DialecticAnswer)
    assert ans.confidence == pytest.approx(0.82)
    assert "concise" in ans.answer
    assert len(ans.supporting_observations) == 1


@respx.mock
def test_memory_flush_context_returns_counts() -> None:
    respx.post(f"{BRIDGE}/v1/memory/context_flush").mock(
        return_value=httpx.Response(
            200, json={"flushed_count": 7, "kept_count": 5}
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        r = c.memory.flush_context(session_id="s1", keep_recent=5)
    assert r.flushed_count == 7
    assert r.kept_count == 5


@respx.mock
async def test_memory_asearch_round_trips_under_async() -> None:
    respx.post(f"{BRIDGE}/v1/memory/search").mock(
        return_value=httpx.Response(
            200,
            json={
                "results": [
                    {"embedding_id": "e1", "score": 0.5, "chunk_text": "async hit"}
                ],
                "count": 1,
            },
        )
    )
    async with RelixClient(BRIDGE, api_key="t") as c:
        results = await c.memory.asearch(query="x", subject_id="u1")
    assert results[0].text == "async hit"
