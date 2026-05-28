"""Planning sub-API — create a plan, list / search agents.

Wraps the bridge's RELIX-7.24 planning surface:

* ``POST /v1/planning/plan`` — synthesise a workflow.
* ``GET  /v1/planning/agents`` — enumerate registered agents.
* ``POST /v1/planning/agents/search`` — find agents whose
  descriptions semantically match a task.
* ``POST /v1/planning/validate`` — parse-only validation of a spec.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from pydantic import BaseModel, ConfigDict, Field

if TYPE_CHECKING:
    from .client import RelixClient


class AgentDescriptor(BaseModel):
    """Single row from :meth:`PlanningAPI.agents` / :meth:`search_agents`."""

    model_config = ConfigDict(extra="allow")

    name: str
    description: str = ""
    capabilities: list[str] = Field(default_factory=list)
    persona: str | None = None


class PlanResult(BaseModel):
    """Return value of :meth:`PlanningAPI.plan`.

    The runtime's ``planning.create_plan`` cap returns a complex JSON
    blob with the orchestrator's decision tree, the critic's verdict,
    the rendered workflow YAML, and the selected agents. The SDK pins
    the fields a typical caller dereferences and keeps the rest under
    ``extra``.
    """

    model_config = ConfigDict(extra="allow")

    workflow_yaml: str = ""
    orchestrator_activated: bool = False
    critic_approved: bool = False
    agents_selected: list[str] = Field(default_factory=list)
    plan_id: str | None = None
    workflow_path: str | None = None


class PlanningAPI:
    """Planning sub-API. Reached via :attr:`RelixClient.planning`."""

    def __init__(self, client: "RelixClient") -> None:
        self._client = client

    def plan(
        self,
        spec: str,
        *,
        max_agents: int | None = None,
        dry_run: bool | None = None,
        peer: str | None = None,
    ) -> PlanResult:
        """Synthesise a workflow from the natural-language ``spec``.

        Args:
            spec: Free-form description of the goal. Multi-line is fine.
            max_agents: Optional ceiling on the orchestrator's agent
                selection.
            dry_run: When ``True``, the bridge returns the plan without
                writing it to disk or activating the orchestrator. Use
                for preview UIs.
            peer: Optional coordinator alias override.
        """
        body = self._build_plan_body(spec, max_agents, dry_run, peer)
        data = self._client._sync_post("/v1/planning/plan", body)
        return PlanResult.model_validate(data or {})

    async def aplan(
        self,
        spec: str,
        *,
        max_agents: int | None = None,
        dry_run: bool | None = None,
        peer: str | None = None,
    ) -> PlanResult:
        """Async mirror of :meth:`plan`."""
        body = self._build_plan_body(spec, max_agents, dry_run, peer)
        data = await self._client._async_post("/v1/planning/plan", body)
        return PlanResult.model_validate(data or {})

    @staticmethod
    def _build_plan_body(
        spec: str, max_agents: int | None, dry_run: bool | None, peer: str | None
    ) -> dict[str, Any]:
        body: dict[str, Any] = {"spec": spec}
        if max_agents is not None:
            body["max_agents"] = max_agents
        if dry_run is not None:
            body["dry_run"] = dry_run
        if peer:
            body["peer"] = peer
        return body

    def agents(self, *, peer: str | None = None) -> list[AgentDescriptor]:
        """Enumerate the agents the coordinator knows about."""
        params = {"peer": peer} if peer else None
        data = self._client._sync_get("/v1/planning/agents", params=params)
        return self._parse_agents(data)

    async def aagents(self, *, peer: str | None = None) -> list[AgentDescriptor]:
        """Async mirror of :meth:`agents`."""
        params = {"peer": peer} if peer else None
        data = await self._client._async_get("/v1/planning/agents", params=params)
        return self._parse_agents(data)

    def search_agents(
        self, task: str, *, peer: str | None = None
    ) -> list[AgentDescriptor]:
        """Find agents whose descriptions match the free-form ``task``."""
        body: dict[str, Any] = {"task": task}
        if peer:
            body["peer"] = peer
        data = self._client._sync_post("/v1/planning/agents/search", body)
        return self._parse_agents(data)

    async def asearch_agents(
        self, task: str, *, peer: str | None = None
    ) -> list[AgentDescriptor]:
        """Async mirror of :meth:`search_agents`."""
        body: dict[str, Any] = {"task": task}
        if peer:
            body["peer"] = peer
        data = await self._client._async_post("/v1/planning/agents/search", body)
        return self._parse_agents(data)

    def validate(self, spec: str, *, peer: str | None = None) -> dict[str, Any]:
        """Parse-only validation of a spec. Returns the raw bridge body."""
        body: dict[str, Any] = {"spec": spec}
        if peer:
            body["peer"] = peer
        data = self._client._sync_post("/v1/planning/validate", body)
        return data if isinstance(data, dict) else {}

    async def avalidate(
        self, spec: str, *, peer: str | None = None
    ) -> dict[str, Any]:
        """Async mirror of :meth:`validate`."""
        body: dict[str, Any] = {"spec": spec}
        if peer:
            body["peer"] = peer
        data = await self._client._async_post("/v1/planning/validate", body)
        return data if isinstance(data, dict) else {}

    @staticmethod
    def _parse_agents(data: Any) -> list[AgentDescriptor]:
        """Extract the agent rows from either a list or
        ``{"agents": [...]}`` wrapper shape."""
        if isinstance(data, list):
            rows: list[Any] = data
        elif isinstance(data, dict):
            rows = data.get("agents") or data.get("results") or []
        else:
            rows = []
        return [AgentDescriptor.model_validate(r) for r in rows]
