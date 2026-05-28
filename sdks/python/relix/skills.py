"""Skills sub-API — search the skill catalogue, read aggregate stats.

Wraps the bridge's GAP-4 skill surface:

* ``GET /v1/skills`` — semantic search across the catalogue
  (filtered by ``q`` / ``min_confidence`` / ``agent`` / ``limit``).
* ``GET /v1/skills/stats`` — aggregate counts.
* ``GET /v1/skills/:id`` — full skill detail.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from pydantic import BaseModel, ConfigDict, Field

if TYPE_CHECKING:
    from .client import RelixClient


class Skill(BaseModel):
    """One row from :meth:`SkillsAPI.search` or :meth:`get`.

    Schema reflects ``crates/relix-runtime/src/nodes/ai/skill_store.rs``'s
    ``Skill`` table. Optional fields are ones that may be ``NULL`` on
    older rows or absent from search responses (vs the full-detail
    ``get``).
    """

    model_config = ConfigDict(extra="allow")

    id: str
    name: str
    description: str = ""
    agent_id: str | None = None
    confidence: float = 0.0
    usage_count: int = 0
    status: str = "active"
    version: int = 1
    tags: list[str] = Field(default_factory=list)
    steps: list[str] | None = None


class SkillStats(BaseModel):
    """Return value of :meth:`SkillsAPI.stats`."""

    model_config = ConfigDict(extra="allow")

    total_skills: int = 0
    active_skills: int = 0
    deprecated_skills: int = 0
    avg_confidence: float = 0.0
    total_usage: int = 0


class SkillsAPI:
    """Skills sub-API. Reached via :attr:`RelixClient.skills`."""

    def __init__(self, client: "RelixClient") -> None:
        self._client = client

    def search(
        self,
        *,
        query: str | None = None,
        agent: str | None = None,
        min_confidence: float | None = None,
        limit: int | None = None,
        peer: str | None = None,
    ) -> list[Skill]:
        """Search the skill catalogue.

        Args:
            query: Free-form natural-language search string.
            agent: Filter to a single agent id.
            min_confidence: Drop skills below this confidence (0.0–1.0).
            limit: Maximum rows.
            peer: Coordinator alias override.
        """
        params = self._build_search_params(query, agent, min_confidence, limit, peer)
        data = self._client._sync_get("/v1/skills", params=params)
        return self._parse_skills(data)

    async def asearch(
        self,
        *,
        query: str | None = None,
        agent: str | None = None,
        min_confidence: float | None = None,
        limit: int | None = None,
        peer: str | None = None,
    ) -> list[Skill]:
        """Async mirror of :meth:`search`."""
        params = self._build_search_params(query, agent, min_confidence, limit, peer)
        data = await self._client._async_get("/v1/skills", params=params)
        return self._parse_skills(data)

    @staticmethod
    def _build_search_params(
        query: str | None,
        agent: str | None,
        min_confidence: float | None,
        limit: int | None,
        peer: str | None,
    ) -> dict[str, Any]:
        return {
            "q": query,
            "agent": agent,
            "min_confidence": min_confidence,
            "limit": limit,
            "peer": peer,
        }

    @staticmethod
    def _parse_skills(data: Any) -> list[Skill]:
        if isinstance(data, list):
            rows: list[Any] = data
        elif isinstance(data, dict):
            rows = data.get("skills") or data.get("results") or []
        else:
            rows = []
        return [Skill.model_validate(r) for r in rows]

    def stats(self) -> SkillStats:
        """Aggregate counts across the skill catalogue."""
        data = self._client._sync_get("/v1/skills/stats")
        return SkillStats.model_validate(data or {})

    async def astats(self) -> SkillStats:
        """Async mirror of :meth:`stats`."""
        data = await self._client._async_get("/v1/skills/stats")
        return SkillStats.model_validate(data or {})

    def get(self, skill_id: str) -> Skill:
        """Full detail for one skill, including step list + version history."""
        data = self._client._sync_get(f"/v1/skills/{skill_id}")
        return Skill.model_validate(data or {})

    async def aget(self, skill_id: str) -> Skill:
        """Async mirror of :meth:`get`."""
        data = await self._client._async_get(f"/v1/skills/{skill_id}")
        return Skill.model_validate(data or {})
