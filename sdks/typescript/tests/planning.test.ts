/**
 * Tests for ``client.planning.*``, ``client.skills.*``, and
 * ``client.observability.*``.
 */

import { RelixClient } from "../src";
import { FetchMock, jsonResponse } from "./fetchMock";

const BRIDGE = "http://relix-test.local";

function client(mock: FetchMock) {
  return new RelixClient({ bridgeUrl: BRIDGE, apiKey: "tok", fetch: mock.fetch });
}

describe("client.planning.plan", () => {
  it("sends dryRun and parses orchestrator fields", async () => {
    const mock = new FetchMock();
    mock.on("POST", `${BRIDGE}/v1/planning/plan`, () =>
      jsonResponse({
        workflow_yaml: "name: example\nsteps: []\n",
        orchestrator_activated: false,
        critic_approved: true,
        agents_selected: ["researcher", "writer"],
        plan_id: "plan-1",
      }),
    );
    const c = client(mock);
    const plan = await c.planning.plan({
      spec: "research and write",
      maxAgents: 3,
      dryRun: true,
    });
    expect(plan.workflowYaml.startsWith("name: example")).toBe(true);
    expect(plan.orchestratorActivated).toBe(false);
    expect(plan.criticApproved).toBe(true);
    expect(plan.agentsSelected).toEqual(["researcher", "writer"]);
    const body = JSON.parse(mock.lastCall().body ?? "{}");
    expect(body.dry_run).toBe(true);
    expect(body.max_agents).toBe(3);
  });
});

describe("client.planning.agents", () => {
  it("accepts a bare-list response", async () => {
    const mock = new FetchMock();
    mock.on("GET", `${BRIDGE}/v1/planning/agents`, () =>
      jsonResponse([
        { name: "alpha", description: "Researcher", capabilities: ["ai.chat"] },
        { name: "beta", description: "Writer" },
      ]),
    );
    const c = client(mock);
    const agents = await c.planning.agents();
    expect(agents).toHaveLength(2);
    expect(agents[0]?.name).toBe("alpha");
    expect(agents[0]?.capabilities).toContain("ai.chat");
  });

  it("accepts a {agents: [...]} wrapped response", async () => {
    const mock = new FetchMock();
    mock.on("GET", `${BRIDGE}/v1/planning/agents`, () =>
      jsonResponse({ agents: [{ name: "alpha", description: "" }] }),
    );
    const c = client(mock);
    const agents = await c.planning.agents();
    expect(agents).toHaveLength(1);
    expect(agents[0]?.name).toBe("alpha");
  });
});

describe("client.skills.search", () => {
  it("passes min_confidence as a query param and parses the response", async () => {
    const mock = new FetchMock();
    mock.on("GET", /\/v1\/skills(\?|$)/, () =>
      jsonResponse({
        skills: [
          {
            id: "s1",
            name: "web_research",
            description: "Research",
            confidence: 0.8,
            usage_count: 12,
            status: "active",
            version: 2,
          },
        ],
      }),
    );
    const c = client(mock);
    const skills = await c.skills.search({
      query: "research",
      minConfidence: 0.7,
      limit: 10,
    });
    expect(skills).toHaveLength(1);
    expect(skills[0]?.id).toBe("s1");
    expect(skills[0]?.confidence).toBe(0.8);
    expect(skills[0]?.usageCount).toBe(12);
    const url = mock.lastCall().url;
    expect(url).toMatch(/min_confidence=0\.7/);
    expect(url).toMatch(/q=research/);
    expect(url).toMatch(/limit=10/);
  });

  it("omits unset params from the URL", async () => {
    const mock = new FetchMock();
    mock.on("GET", /\/v1\/skills(\?|$)/, () => jsonResponse({ skills: [] }));
    const c = client(mock);
    await c.skills.search();
    const url = mock.lastCall().url;
    expect(url).not.toMatch(/q=/);
    expect(url).not.toMatch(/min_confidence/);
  });
});

describe("client.skills.stats", () => {
  it("returns typed counts", async () => {
    const mock = new FetchMock();
    mock.on("GET", `${BRIDGE}/v1/skills/stats`, () =>
      jsonResponse({
        total_skills: 12,
        active_skills: 10,
        deprecated_skills: 2,
        avg_confidence: 0.74,
        total_usage: 305,
      }),
    );
    const c = client(mock);
    const stats = await c.skills.stats();
    expect(stats.totalSkills).toBe(12);
    expect(stats.avgConfidence).toBeCloseTo(0.74);
  });
});

describe("client.observability.health", () => {
  it("parses agents + deployment roll-up", async () => {
    const mock = new FetchMock();
    mock.on("GET", /\/v1\/observability\/health(\?|$)/, () =>
      jsonResponse({
        agents: {
          alpha: { score: 92.3, color: "green", signals: { errors: 0 } },
          beta: { score: 64.0, color: "yellow" },
        },
        _deployment: { score: 78.0, color: "yellow" },
        hours: 24,
      }),
    );
    const c = client(mock);
    const h = await c.observability.health({ hours: 24 });
    expect(Object.keys(h.agents).sort()).toEqual(["alpha", "beta"]);
    expect(h.agents.alpha?.score).toBe(92.3);
    expect(h.deployment?.score).toBe(78.0);
    expect(h.windowHours).toBe(24);
  });
});

describe("client.observability.alerts", () => {
  it("accepts a bare-list shape", async () => {
    const mock = new FetchMock();
    mock.on("GET", /\/v1\/observability\/alerts(\?|$)/, () =>
      jsonResponse([
        {
          id: "a1",
          kind: "cost_spike",
          agent: "alpha",
          severity: "warn",
          message: "cost > $1",
        },
      ]),
    );
    const c = client(mock);
    const alerts = await c.observability.alerts();
    expect(alerts).toHaveLength(1);
    expect(alerts[0]?.kind).toBe("cost_spike");
  });

  it("accepts a {alerts: [...]} wrapped shape", async () => {
    const mock = new FetchMock();
    mock.on("GET", /\/v1\/observability\/alerts(\?|$)/, () =>
      jsonResponse({ alerts: [{ id: "a1", kind: "low_confidence", severity: "info" }] }),
    );
    const c = client(mock);
    const alerts = await c.observability.alerts();
    expect(alerts).toHaveLength(1);
    expect(alerts[0]?.kind).toBe("low_confidence");
  });
});
