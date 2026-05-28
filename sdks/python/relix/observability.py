"""Observability sub-API — health summary, active alerts, alert history.

Wraps the bridge's RELIX-7.28 Part 2 observability surface:

* ``GET /v1/observability/health`` — per-agent + deployment health
  roll-up. The response shape is a dict keyed by agent id where each
  entry carries a 0–100 score, a colour tag, and the underlying signal
  counters.
* ``GET /v1/observability/alerts`` — every currently-firing alert.
* ``GET /v1/observability/alerts/history`` — recent alert chronicle.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from pydantic import BaseModel, ConfigDict, Field

if TYPE_CHECKING:
    from .client import RelixClient


class AgentHealth(BaseModel):
    """One agent's roll-up score in :attr:`HealthSummary.agents`."""

    model_config = ConfigDict(extra="allow")

    score: float = 0.0
    color: str = "unknown"
    signals: dict[str, Any] = Field(default_factory=dict)


class HealthSummary(BaseModel):
    """Return value of :meth:`ObservabilityAPI.health`.

    ``agents`` is keyed by agent id; the special ``"_deployment"`` key
    (when present) carries the deployment-wide roll-up. Extra top-level
    fields the bridge may add land in ``extra``.
    """

    model_config = ConfigDict(extra="allow")

    agents: dict[str, AgentHealth] = Field(default_factory=dict)
    deployment: AgentHealth | None = None
    window_hours: int | None = None


class Alert(BaseModel):
    """One row from :meth:`ObservabilityAPI.alerts` /
    :meth:`alert_history`."""

    model_config = ConfigDict(extra="allow")

    id: str | None = None
    kind: str = ""
    agent: str | None = None
    severity: str = ""
    message: str = ""
    started_at: int | None = None
    ended_at: int | None = None


class ObservabilityAPI:
    """Observability sub-API. Reached via :attr:`RelixClient.observability`."""

    def __init__(self, client: "RelixClient") -> None:
        self._client = client

    def health(
        self,
        *,
        hours: int | None = None,
        peer: str | None = None,
    ) -> HealthSummary:
        """Per-agent + deployment health roll-up over the last ``hours`` hours."""
        params = {"hours": hours, "peer": peer}
        data = self._client._sync_get("/v1/observability/health", params=params)
        return self._parse_health(data)

    async def ahealth(
        self,
        *,
        hours: int | None = None,
        peer: str | None = None,
    ) -> HealthSummary:
        """Async mirror of :meth:`health`."""
        params = {"hours": hours, "peer": peer}
        data = await self._client._async_get("/v1/observability/health", params=params)
        return self._parse_health(data)

    def alerts(self, *, peer: str | None = None) -> list[Alert]:
        """Every currently-firing alert across all agents."""
        params = {"peer": peer}
        data = self._client._sync_get("/v1/observability/alerts", params=params)
        return self._parse_alerts(data)

    async def aalerts(self, *, peer: str | None = None) -> list[Alert]:
        """Async mirror of :meth:`alerts`."""
        params = {"peer": peer}
        data = await self._client._async_get("/v1/observability/alerts", params=params)
        return self._parse_alerts(data)

    def alert_history(
        self,
        *,
        limit: int | None = None,
        agent: str | None = None,
        peer: str | None = None,
    ) -> list[Alert]:
        """Recent rows from the alert chronicle."""
        params = {"limit": limit, "agent": agent, "peer": peer}
        data = self._client._sync_get(
            "/v1/observability/alerts/history", params=params
        )
        return self._parse_alerts(data)

    async def aalert_history(
        self,
        *,
        limit: int | None = None,
        agent: str | None = None,
        peer: str | None = None,
    ) -> list[Alert]:
        """Async mirror of :meth:`alert_history`."""
        params = {"limit": limit, "agent": agent, "peer": peer}
        data = await self._client._async_get(
            "/v1/observability/alerts/history", params=params
        )
        return self._parse_alerts(data)

    @staticmethod
    def _parse_health(data: Any) -> HealthSummary:
        """Translate the bridge's health body into a :class:`HealthSummary`.

        The bridge currently returns
        ``{"agents": {"name": {...}}, "_deployment"?: {...}, "hours"?: N}``;
        we tolerate either the explicit ``"_deployment"`` key or a
        top-level ``deployment`` mirror, and we let unknown extras
        land under ``model_config = extra="allow"``.
        """
        if not isinstance(data, dict):
            return HealthSummary()
        agents_raw = data.get("agents", {})
        agents: dict[str, AgentHealth] = {}
        if isinstance(agents_raw, dict):
            for k, v in agents_raw.items():
                if isinstance(v, dict):
                    agents[str(k)] = AgentHealth.model_validate(v)
        deployment = None
        for key in ("deployment", "_deployment"):
            raw = data.get(key)
            if isinstance(raw, dict):
                deployment = AgentHealth.model_validate(raw)
                break
        hours = data.get("window_hours") or data.get("hours")
        return HealthSummary(
            agents=agents,
            deployment=deployment,
            window_hours=int(hours) if isinstance(hours, int) else None,
        )

    @staticmethod
    def _parse_alerts(data: Any) -> list[Alert]:
        if isinstance(data, list):
            rows: list[Any] = data
        elif isinstance(data, dict):
            rows = data.get("alerts") or data.get("results") or []
        else:
            rows = []
        return [Alert.model_validate(r) for r in rows]
