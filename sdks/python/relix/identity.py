"""Identity sub-API — research-backed identity synthesis.

Wraps the bridge's RELIX-7.18 / GAP-17 Part 2 identity surface:

* ``POST /v1/identity/research`` — kick off the five-stage research
  pipeline (query generation → parallel web search → LLM synthesis →
  human approval gate → memory write). The bridge proxies onto the
  identity peer; the SDK returns the synthesised profile + approval
  verdict + memory record id.

The runtime's `identity.research` cap can wait up to five minutes on
the approval gate; the SDK leaves the bridge's 600s deadline in place
and lets the caller block on the result. Applications that need a
non-blocking surface should run the call inside a task and poll the
result, or build a tiny wrapper that returns after the dispatch
returns 202 (the bridge surfaces success synchronously today).
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from pydantic import BaseModel, ConfigDict, Field

if TYPE_CHECKING:
    from .client import RelixClient


class IdentityProfile(BaseModel):
    """Synthesised identity profile returned inside :class:`ResearchResult`.

    Mirrors the runtime's ``identity::research::IdentityProfile``
    fields. Every field is optional because the LLM may surface a
    sparse profile when the public web has thin coverage for the
    subject.
    """

    model_config = ConfigDict(extra="allow")

    display_name: str | None = None
    professional_role: str | None = None
    organization: str | None = None
    location: str | None = None
    expertise_areas: list[str] = Field(default_factory=list)
    public_profiles: list[dict[str, Any]] = Field(default_factory=list)
    notable_work: list[str] = Field(default_factory=list)
    confidence: float = 0.0
    sources_used: list[str] = Field(default_factory=list)
    synthesis_notes: str = ""


class ResearchResult(BaseModel):
    """Return value of :meth:`IdentityAPI.research`.

    Mirrors the runtime's ``ResearchResult`` shape — the three fields
    pinned by the PART 3 spec (``profile``, ``approved``,
    ``memory_record_id``) plus the rest the runtime emits so callers
    that care about the full envelope (queries generated, provider
    used, approval id / verdict) can read them without round-tripping
    through ``.extra``.
    """

    model_config = ConfigDict(extra="allow")

    subject_name: str = ""
    profile: IdentityProfile = Field(default_factory=IdentityProfile)
    queries_generated: list[str] = Field(default_factory=list)
    results_consulted: int = 0
    provider_used: str = ""
    approval_id: str | None = None
    approval_verdict: str | None = None
    memory_record_id: str | None = None
    approved: bool = False


class IdentityAPI:
    """Identity sub-API. Reached via :attr:`RelixClient.identity`."""

    def __init__(self, client: "RelixClient") -> None:
        self._client = client

    def research(
        self,
        subject_name: str,
        *,
        context: str | None = None,
        peer: str | None = None,
    ) -> ResearchResult:
        """Synthesise an identity profile for ``subject_name``.

        The bridge proxies onto the identity peer's `identity.research`
        cap. When `[identity.research] require_approval = true` (the
        production default) the bridge blocks until the operator votes
        on the approval gate (up to 5 minutes); the SDK respects the
        bridge's deadline and returns when the call completes.
        """
        body = self._build_research_body(subject_name, context, peer)
        data = self._client._sync_post("/v1/identity/research", body)
        return ResearchResult.model_validate(data or {})

    async def aresearch(
        self,
        subject_name: str,
        *,
        context: str | None = None,
        peer: str | None = None,
    ) -> ResearchResult:
        """Async mirror of :meth:`research`."""
        body = self._build_research_body(subject_name, context, peer)
        data = await self._client._async_post("/v1/identity/research", body)
        return ResearchResult.model_validate(data or {})

    @staticmethod
    def _build_research_body(
        subject_name: str, context: str | None, peer: str | None
    ) -> dict[str, Any]:
        body: dict[str, Any] = {"subject_name": subject_name}
        if context is not None:
            body["context"] = context
        if peer is not None:
            body["peer"] = peer
        return body
