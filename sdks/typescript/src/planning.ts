/**
 * Planning sub-API. Reached via `client.planning`.
 *
 * Wraps the bridge's RELIX-7.24 planning surface:
 * * `POST /v1/planning/plan`
 * * `GET  /v1/planning/agents`
 * * `POST /v1/planning/agents/search`
 * * `POST /v1/planning/validate`
 */

import { doJsonRequest, isObject, runRequest, type RelixClient } from "./client";
import type {
  AgentDescriptor,
  ApiResult,
  PlanResult,
  PlanningPlanInput,
} from "./types";

export class PlanningAPI {
  constructor(private readonly client: RelixClient) {}

  async plan(input: PlanningPlanInput): Promise<ApiResult<PlanResult>> {
    return runRequest(async () => {
      const body: Record<string, unknown> = { spec: input.spec };
      if (input.maxAgents !== undefined) {
        body.max_agents = input.maxAgents;
      }
      if (input.dryRun !== undefined) {
        body.dry_run = input.dryRun;
      }
      if (input.peer) {
        body.peer = input.peer;
      }
      const data = await doJsonRequest(this.client, "POST", "/v1/planning/plan", body);
      return parsePlanResult(data);
    });
  }

  async agents(peer?: string): Promise<ApiResult<AgentDescriptor[]>> {
    return runRequest(async () => {
      const params = peer ? { peer } : undefined;
      const data = await doJsonRequest(
        this.client,
        "GET",
        "/v1/planning/agents",
        undefined,
        params,
      );
      return parseAgents(data);
    });
  }

  async searchAgents(task: string, peer?: string): Promise<ApiResult<AgentDescriptor[]>> {
    return runRequest(async () => {
      const body: Record<string, unknown> = { task };
      if (peer) {
        body.peer = peer;
      }
      const data = await doJsonRequest(this.client, "POST", "/v1/planning/agents/search", body);
      return parseAgents(data);
    });
  }

  async validate(spec: string, peer?: string): Promise<ApiResult<Record<string, unknown>>> {
    return runRequest(async () => {
      const body: Record<string, unknown> = { spec };
      if (peer) {
        body.peer = peer;
      }
      const data = await doJsonRequest(this.client, "POST", "/v1/planning/validate", body);
      return isObject(data) ? data : {};
    });
  }
}

function parsePlanResult(data: unknown): PlanResult {
  const r = isObject(data) ? data : {};
  const agentsSelected = Array.isArray(r["agents_selected"])
    ? (r["agents_selected"] as unknown[]).filter((x): x is string => typeof x === "string")
    : [];
  return {
    workflowYaml: typeof r["workflow_yaml"] === "string" ? (r["workflow_yaml"] as string) : "",
    orchestratorActivated:
      typeof r["orchestrator_activated"] === "boolean"
        ? (r["orchestrator_activated"] as boolean)
        : false,
    criticApproved:
      typeof r["critic_approved"] === "boolean" ? (r["critic_approved"] as boolean) : false,
    agentsSelected,
    planId: typeof r["plan_id"] === "string" ? (r["plan_id"] as string) : undefined,
    workflowPath:
      typeof r["workflow_path"] === "string" ? (r["workflow_path"] as string) : undefined,
  };
}

function parseAgents(data: unknown): AgentDescriptor[] {
  let rows: unknown[];
  if (Array.isArray(data)) {
    rows = data;
  } else if (isObject(data)) {
    const r = data["agents"] ?? data["results"];
    rows = Array.isArray(r) ? r : [];
  } else {
    rows = [];
  }
  return rows.map(parseAgentRow);
}

function parseAgentRow(row: unknown): AgentDescriptor {
  const r = isObject(row) ? row : {};
  const capabilities = Array.isArray(r["capabilities"])
    ? (r["capabilities"] as unknown[]).filter((x): x is string => typeof x === "string")
    : [];
  return {
    name: typeof r["name"] === "string" ? (r["name"] as string) : "",
    description: typeof r["description"] === "string" ? (r["description"] as string) : "",
    capabilities,
    persona: typeof r["persona"] === "string" ? (r["persona"] as string) : undefined,
  };
}
