"""Tests for ``client.planning.*`` and ``client.skills.*`` and
``client.observability.*``. All three sub-APIs sit on the bridge's
coordinator-proxy endpoints and share enough plumbing that bundling
their tests here keeps the test file count small."""

from __future__ import annotations

import httpx
import respx

from relix import RelixClient

BRIDGE = "http://relix-test.local"


# ---- planning ----------------------------------------------------------


@respx.mock
def test_planning_plan_sends_dry_run_and_parses_orchestrator_fields() -> None:
    route = respx.post(f"{BRIDGE}/v1/planning/plan").mock(
        return_value=httpx.Response(
            200,
            json={
                "workflow_yaml": "name: example\nsteps: []\n",
                "orchestrator_activated": False,
                "critic_approved": True,
                "agents_selected": ["researcher", "writer"],
                "plan_id": "plan-1",
            },
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        plan = c.planning.plan(
            spec="research and summarise", max_agents=3, dry_run=True
        )
    assert plan.workflow_yaml.startswith("name: example")
    assert plan.orchestrator_activated is False
    assert plan.critic_approved is True
    assert plan.agents_selected == ["researcher", "writer"]
    assert plan.plan_id == "plan-1"
    body = route.calls.last.request.read().decode()
    assert '"dry_run":true' in body.replace(" ", "")
    assert '"max_agents":3' in body.replace(" ", "")


@respx.mock
def test_planning_agents_handles_list_response() -> None:
    respx.get(f"{BRIDGE}/v1/planning/agents").mock(
        return_value=httpx.Response(
            200,
            json=[
                {"name": "alpha", "description": "Researcher", "capabilities": ["ai.chat"]},
                {"name": "beta", "description": "Writer"},
            ],
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        agents = c.planning.agents()
    assert len(agents) == 2
    assert agents[0].name == "alpha"
    assert "ai.chat" in agents[0].capabilities


@respx.mock
def test_planning_agents_handles_dict_wrapped_response() -> None:
    respx.get(f"{BRIDGE}/v1/planning/agents").mock(
        return_value=httpx.Response(
            200, json={"agents": [{"name": "alpha", "description": ""}]}
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        agents = c.planning.agents()
    assert len(agents) == 1
    assert agents[0].name == "alpha"


# ---- skills ------------------------------------------------------------


@respx.mock
def test_skills_search_passes_min_confidence_as_query_param() -> None:
    route = respx.get(f"{BRIDGE}/v1/skills").mock(
        return_value=httpx.Response(
            200,
            json={
                "skills": [
                    {
                        "id": "s1",
                        "name": "web_research",
                        "description": "Research",
                        "confidence": 0.8,
                        "usage_count": 12,
                    }
                ]
            },
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        skills = c.skills.search(query="research", min_confidence=0.7, limit=10)
    assert len(skills) == 1
    assert skills[0].id == "s1"
    assert skills[0].confidence == 0.8
    # Query parameters land on the URL.
    sent = str(route.calls.last.request.url)
    assert "min_confidence=0.7" in sent
    assert "q=research" in sent
    assert "limit=10" in sent


@respx.mock
def test_skills_search_omits_unset_params() -> None:
    route = respx.get(f"{BRIDGE}/v1/skills").mock(
        return_value=httpx.Response(200, json={"skills": []})
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        c.skills.search()
    sent = str(route.calls.last.request.url)
    assert "q=" not in sent
    assert "min_confidence" not in sent
    assert "agent" not in sent


@respx.mock
def test_skills_stats_returns_typed_stats() -> None:
    respx.get(f"{BRIDGE}/v1/skills/stats").mock(
        return_value=httpx.Response(
            200,
            json={
                "total_skills": 12,
                "active_skills": 10,
                "deprecated_skills": 2,
                "avg_confidence": 0.74,
                "total_usage": 305,
            },
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        stats = c.skills.stats()
    assert stats.total_skills == 12
    assert stats.avg_confidence == 0.74


# ---- observability -----------------------------------------------------


@respx.mock
def test_observability_health_parses_agent_roll_up() -> None:
    respx.get(f"{BRIDGE}/v1/observability/health").mock(
        return_value=httpx.Response(
            200,
            json={
                "agents": {
                    "alpha": {"score": 92.3, "color": "green", "signals": {"errors": 0}},
                    "beta": {"score": 64.0, "color": "yellow"},
                },
                "_deployment": {"score": 78.0, "color": "yellow"},
                "hours": 24,
            },
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        h = c.observability.health(hours=24)
    assert set(h.agents) == {"alpha", "beta"}
    assert h.agents["alpha"].score == 92.3
    assert h.agents["alpha"].color == "green"
    assert h.deployment is not None
    assert h.deployment.score == 78.0
    assert h.window_hours == 24


@respx.mock
def test_observability_alerts_parses_list_shape() -> None:
    respx.get(f"{BRIDGE}/v1/observability/alerts").mock(
        return_value=httpx.Response(
            200,
            json=[
                {
                    "id": "a1",
                    "kind": "cost_spike",
                    "agent": "alpha",
                    "severity": "warn",
                    "message": "cost > $1",
                }
            ],
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        alerts = c.observability.alerts()
    assert len(alerts) == 1
    assert alerts[0].kind == "cost_spike"
    assert alerts[0].severity == "warn"


@respx.mock
def test_observability_alerts_parses_dict_wrapped_shape() -> None:
    respx.get(f"{BRIDGE}/v1/observability/alerts").mock(
        return_value=httpx.Response(
            200,
            json={"alerts": [{"id": "a1", "kind": "low_confidence", "severity": "info"}]},
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        alerts = c.observability.alerts()
    assert len(alerts) == 1
    assert alerts[0].kind == "low_confidence"
