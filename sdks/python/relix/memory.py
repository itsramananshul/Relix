"""Memory sub-API — search, ingest, dialectic, context flush.

The bridge fronts four kinds of memory-layer calls:

* **Search** — ``POST /v1/memory/search`` — semantic search over a
  subject's persistent memory.
* **Ingest document** — ``POST /v1/memory/ingest`` — chunk and embed a
  text / markdown / pdf / code document.
* **Dialectic** — ``POST /v1/memory/dialectic`` — synthesise an answer
  to a question from one subject's observations.
* **Context flush** — ``POST /v1/memory/context_flush`` — explicit
  context window reset.

Each method has a sync and async form. The sub-API instance is reached
via ``client.memory`` and shares the parent client's httpx pools.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from pydantic import BaseModel, ConfigDict, Field

if TYPE_CHECKING:
    from .client import RelixClient


class MemoryResult(BaseModel):
    """One row from :meth:`MemoryAPI.search`.

    The bridge's ``POST /v1/memory/search`` returns
    ``{"results": [{"embedding_id": ..., "score": ..., "chunk_text": ...}], "count": N}``;
    the SDK normalises ``chunk_text`` → ``text``, ``embedding_id`` → ``id``
    for parity with the TypeScript SDK. The original keys are
    preserved as aliases so a caller switching from a raw bridge JSON
    consumer to the SDK doesn't rewrite their field accessors.
    """

    model_config = ConfigDict(extra="allow", populate_by_name=True)

    id: str = Field(alias="embedding_id")
    text: str = Field(alias="chunk_text")
    score: float = 0.0
    layer: str | None = None
    confidence: float | None = None
    tags: list[str] = Field(default_factory=list)


class IngestDocumentResult(BaseModel):
    """Return value of :meth:`MemoryAPI.ingest_document`.

    Field shapes mirror the runtime's ``IngestDocumentResponse``: how
    many chunks landed, how many were embedded synchronously, how many
    were deferred to the background embedding pipeline, and the
    canonical source label the caller passed in.
    """

    model_config = ConfigDict(extra="allow")

    chunks_created: int = 0
    embedded: int = 0
    deferred_embeddings: int = 0
    source: str = ""
    subject_id: str = ""
    content_type: str = ""


class DialecticAnswer(BaseModel):
    """Return value of :meth:`MemoryAPI.dialectic`.

    The cap returns a free-form JSON blob; the SDK pins the three
    fields every caller depends on (``answer``, ``confidence``,
    ``supporting_observations``) and keeps everything else under
    ``extra``.
    """

    model_config = ConfigDict(extra="allow")

    answer: str = ""
    confidence: float = 0.0
    supporting_observations: list[Any] = Field(default_factory=list)


class FlushContextResult(BaseModel):
    """Return value of :meth:`MemoryAPI.flush_context`."""

    model_config = ConfigDict(extra="allow")

    flushed_count: int = 0
    kept_count: int = 0


class MemoryAPI:
    """Memory sub-API. Reached via :attr:`RelixClient.memory`."""

    def __init__(self, client: "RelixClient") -> None:
        self._client = client

    # ---- search --------------------------------------------------------

    def search(
        self,
        query: str,
        *,
        subject_id: str,
        target: str = "agent",
        limit: int = 5,
        peer: str | None = None,
    ) -> list[MemoryResult]:
        """Semantic search over ``subject_id``'s persistent memory.

        Args:
            query: Free-form natural-language search string.
            subject_id: The user / agent the memory belongs to.
            target: ``"agent"`` (default) or ``"user"`` — selects the
                memory shelf the bridge searches.
            limit: Maximum hits returned. Bridge clamps to 1–20.
            peer: Optional memory-node alias override (defaults to
                ``"memory"``).
        """
        body = self._build_search_body(query, subject_id, target, limit, peer)
        data = self._client._sync_post("/v1/memory/search", body)
        return self._parse_search(data)

    async def asearch(
        self,
        query: str,
        *,
        subject_id: str,
        target: str = "agent",
        limit: int = 5,
        peer: str | None = None,
    ) -> list[MemoryResult]:
        """Async mirror of :meth:`search`."""
        body = self._build_search_body(query, subject_id, target, limit, peer)
        data = await self._client._async_post("/v1/memory/search", body)
        return self._parse_search(data)

    @staticmethod
    def _build_search_body(
        query: str, subject_id: str, target: str, limit: int, peer: str | None
    ) -> dict[str, Any]:
        body: dict[str, Any] = {
            "subject_id": subject_id,
            "target": target,
            "query": query,
            "limit": limit,
        }
        if peer:
            body["peer"] = peer
        return body

    @staticmethod
    def _parse_search(data: Any) -> list[MemoryResult]:
        if isinstance(data, dict):
            rows = data.get("results") or data.get("hits") or []
        elif isinstance(data, list):
            rows = data
        else:
            rows = []
        return [MemoryResult.model_validate(r) for r in rows]

    # ---- ingest --------------------------------------------------------

    def ingest_document(
        self,
        *,
        subject_id: str,
        content: str,
        content_type: str = "markdown",
        source: str = "sdk",
        observer_id: str = "sdk-python",
        chunk_size_chars: int | None = None,
        peer: str | None = None,
    ) -> IngestDocumentResult:
        """Ingest a text document into the memory store.

        Args:
            subject_id: User / agent the document is about.
            content: Verbatim text to ingest (markdown / txt / code).
                For PDFs, pass the base64-encoded bytes via the lower
                level dict API directly.
            content_type: ``"markdown"`` | ``"txt"`` | ``"code"`` |
                ``"pdf"`` | ``"image"``.
            source: Operator-visible source label that appears on every
                resulting record.
            observer_id: Who is ingesting on the subject's behalf;
                defaults to a stable per-SDK marker.
            chunk_size_chars: Optional override for the chunker.
            peer: Optional memory-node alias.
        """
        body = self._build_ingest_body(
            subject_id, content, content_type, source, observer_id, chunk_size_chars, peer
        )
        data = self._client._sync_post("/v1/memory/ingest", body)
        return IngestDocumentResult.model_validate(data or {})

    async def aingest_document(
        self,
        *,
        subject_id: str,
        content: str,
        content_type: str = "markdown",
        source: str = "sdk",
        observer_id: str = "sdk-python",
        chunk_size_chars: int | None = None,
        peer: str | None = None,
    ) -> IngestDocumentResult:
        """Async mirror of :meth:`ingest_document`."""
        body = self._build_ingest_body(
            subject_id, content, content_type, source, observer_id, chunk_size_chars, peer
        )
        data = await self._client._async_post("/v1/memory/ingest", body)
        return IngestDocumentResult.model_validate(data or {})

    @staticmethod
    def _build_ingest_body(
        subject_id: str,
        content: str,
        content_type: str,
        source: str,
        observer_id: str,
        chunk_size_chars: int | None,
        peer: str | None,
    ) -> dict[str, Any]:
        body: dict[str, Any] = {
            "subject_id": subject_id,
            "source": source,
            "observer_id": observer_id,
            "content": content,
            "content_type": content_type,
        }
        if chunk_size_chars is not None:
            body["chunk_size_chars"] = chunk_size_chars
        if peer:
            body["peer"] = peer
        return body

    # ---- dialectic -----------------------------------------------------

    def dialectic(
        self,
        question: str,
        *,
        subject_id: str,
        observer_id: str = "sdk-python",
        peer: str | None = None,
    ) -> DialecticAnswer:
        """Ask the memory store to synthesise an answer to ``question``.

        The cap walks the subject's observations and asks the configured
        dialectic model to fuse them into a single response.
        """
        body: dict[str, Any] = {
            "observer_id": observer_id,
            "subject_id": subject_id,
            "question": question,
        }
        if peer:
            body["peer"] = peer
        data = self._client._sync_post("/v1/memory/dialectic", body)
        return DialecticAnswer.model_validate(data or {})

    async def adialectic(
        self,
        question: str,
        *,
        subject_id: str,
        observer_id: str = "sdk-python",
        peer: str | None = None,
    ) -> DialecticAnswer:
        """Async mirror of :meth:`dialectic`."""
        body: dict[str, Any] = {
            "observer_id": observer_id,
            "subject_id": subject_id,
            "question": question,
        }
        if peer:
            body["peer"] = peer
        data = await self._client._async_post("/v1/memory/dialectic", body)
        return DialecticAnswer.model_validate(data or {})

    # ---- context flush -------------------------------------------------

    def flush_context(
        self,
        *,
        session_id: str,
        keep_recent: int = 5,
        peer: str | None = None,
    ) -> FlushContextResult:
        """Mark the raw-turns table's pre-``keep_recent`` rows flushed.

        Returns the row counts the bridge reports. The action is
        idempotent: re-flushing with the same ``keep_recent`` is a
        no-op.
        """
        body: dict[str, Any] = {
            "session_id": session_id,
            "keep_recent_n": keep_recent,
        }
        if peer:
            body["peer"] = peer
        data = self._client._sync_post("/v1/memory/context_flush", body)
        return FlushContextResult.model_validate(data or {})

    async def aflush_context(
        self,
        *,
        session_id: str,
        keep_recent: int = 5,
        peer: str | None = None,
    ) -> FlushContextResult:
        """Async mirror of :meth:`flush_context`."""
        body: dict[str, Any] = {
            "session_id": session_id,
            "keep_recent_n": keep_recent,
        }
        if peer:
            body["peer"] = peer
        data = await self._client._async_post("/v1/memory/context_flush", body)
        return FlushContextResult.model_validate(data or {})
