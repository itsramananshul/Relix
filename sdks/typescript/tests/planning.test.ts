/**
 * Tests for ``client.planning.*``, ``client.skills.*``, and
 * ``client.observability.*``.
 *
 * All single-response methods now return `ApiResult<T>` (PART 5);
 * each test narrows on `.ok` before reading `.data`.
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
    const res = await c.planning.plan({
      spec: "research and write",
      maxAgents: 3,
      dryRun: true,
    });
    if (!res.ok) {
      throw new Error(`expected ok=true: ${res.error.message}`);
    }
    expect(res.data.workflowYaml.startsWith("name: example")).toBe(true);
    expect(res.data.orchestratorActivated).toBe(false);
    expect(res.data.criticApproved).toBe(true);
    expect(res.data.agentsSelected).toEqual(["researcher", "writer"]);
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
    const res = await c.planning.agents();
    if (!res.ok) {
      throw new Error(`expected ok=true: ${res.error.message}`);
    }
    expect(res.data).toHaveLength(2);
    expect(res.data[0]?.name).toBe("alpha");
    expect(res.data[0]?.capabilities).toContain("ai.chat");
  });

  it("accepts a {agents: [...]} wrapped response", async () => {
    const mock = new FetchMock();
    mock.on("GET", `${BRIDGE}/v1/planning/agents`, () =>
      jsonResponse({ agents: [{ name: "alpha", description: "" }] }),
    );
    const c = client(mock);
    const res = await c.planning.agents();
    if (!res.ok) {
      throw new Error(`expected ok=true: ${res.error.message}`);
    }
    expect(res.data).toHaveLength(1);
    expect(res.data[0]?.name).toBe("alpha");
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
    const res = await c.skills.search({
      query: "research",
      minConfidence: 0.7,
      limit: 10,
    });
    if (!res.ok) {
      throw new Error(`expected ok=true: ${res.error.message}`);
    }
    expect(res.data).toHaveLength(1);
    expect(res.data[0]?.id).toBe("s1");
    expect(res.data[0]?.confidence).toBe(0.8);
    expect(res.data[0]?.usageCount).toBe(12);
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
    const res = await c.skills.stats();
    if (!res.ok) {
      throw new Error(`expected ok=true: ${res.error.message}`);
    }
    expect(res.data.totalSkills).toBe(12);
    expect(res.data.avgConfidence).toBeCloseTo(0.74);
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
    const res = await c.observability.health({ hours: 24 });
    if (!res.ok) {
      throw new Error(`expected ok=true: ${res.error.message}`);
    }
    expect(Object.keys(res.data.agents).sort()).toEqual(["alpha", "beta"]);
    expect(res.data.agents.alpha?.score).toBe(92.3);
    expect(res.data.deployment?.score).toBe(78.0);
    expect(res.data.windowHours).toBe(24);
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
    const res = await c.observability.alerts();
    if (!res.ok) {
      throw new Error(`expected ok=true: ${res.error.message}`);
    }
    expect(res.data).toHaveLength(1);
    expect(res.data[0]?.kind).toBe("cost_spike");
  });

  it("accepts a {alerts: [...]} wrapped shape", async () => {
    const mock = new FetchMock();
    mock.on("GET", /\/v1\/observability\/alerts(\?|$)/, () =>
      jsonResponse({ alerts: [{ id: "a1", kind: "low_confidence", severity: "info" }] }),
    );
    const c = client(mock);
    const res = await c.observability.alerts();
    if (!res.ok) {
      throw new Error(`expected ok=true: ${res.error.message}`);
    }
    expect(res.data).toHaveLength(1);
    expect(res.data[0]?.kind).toBe("low_confidence");
  });
});
