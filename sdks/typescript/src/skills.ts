/**
 * Skills sub-API. Reached via `client.skills`.
 *
 * Wraps the bridge's GAP-4 skill surface:
 * * `GET  /v1/skills?q=...&min_confidence=...&agent=...&limit=...`
 * * `GET  /v1/skills/stats`
 * * `GET  /v1/skills/:id`
 */

import { doJsonRequest, isObject, runRequest, type RelixClient } from "./client";
import type { ApiResult, Skill, SkillStats, SkillsSearchInput } from "./types";

export class SkillsAPI {
  constructor(private readonly client: RelixClient) {}

  async search(input: SkillsSearchInput = {}): Promise<ApiResult<Skill[]>> {
    return runRequest(async () => {
      const params: Record<string, unknown> = {
        q: input.query,
        agent: input.agent,
        min_confidence: input.minConfidence,
        limit: input.limit,
        peer: input.peer,
      };
      const data = await doJsonRequest(this.client, "GET", "/v1/skills", undefined, params);
      return parseSkills(data);
    });
  }

  async stats(): Promise<ApiResult<SkillStats>> {
    return runRequest(async () => {
      const data = await doJsonRequest(this.client, "GET", "/v1/skills/stats");
      return parseStats(data);
    });
  }

  async get(skillId: string): Promise<ApiResult<Skill>> {
    return runRequest(async () => {
      const data = await doJsonRequest(
        this.client,
        "GET",
        `/v1/skills/${encodeURIComponent(skillId)}`,
      );
      return parseSkillRow(data);
    });
  }
}

function parseSkills(data: unknown): Skill[] {
  let rows: unknown[];
  if (Array.isArray(data)) {
    rows = data;
  } else if (isObject(data)) {
    const r = data["skills"] ?? data["results"];
    rows = Array.isArray(r) ? r : [];
  } else {
    rows = [];
  }
  return rows.map(parseSkillRow);
}

function parseSkillRow(row: unknown): Skill {
  const r = isObject(row) ? row : {};
  const tags = Array.isArray(r["tags"])
    ? (r["tags"] as unknown[]).filter((x): x is string => typeof x === "string")
    : [];
  const steps = Array.isArray(r["steps"])
    ? (r["steps"] as unknown[]).filter((x): x is string => typeof x === "string")
    : undefined;
  return {
    id: typeof r["id"] === "string" ? (r["id"] as string) : "",
    name: typeof r["name"] === "string" ? (r["name"] as string) : "",
    description: typeof r["description"] === "string" ? (r["description"] as string) : "",
    agentId: typeof r["agent_id"] === "string" ? (r["agent_id"] as string) : undefined,
    confidence: typeof r["confidence"] === "number" ? (r["confidence"] as number) : 0,
    usageCount: typeof r["usage_count"] === "number" ? (r["usage_count"] as number) : 0,
    status: typeof r["status"] === "string" ? (r["status"] as string) : "active",
    version: typeof r["version"] === "number" ? (r["version"] as number) : 1,
    tags,
    steps,
  };
}

function parseStats(data: unknown): SkillStats {
  const r = isObject(data) ? data : {};
  return {
    totalSkills: numField(r, "total_skills"),
    activeSkills: numField(r, "active_skills"),
    deprecatedSkills: numField(r, "deprecated_skills"),
    avgConfidence: typeof r["avg_confidence"] === "number" ? (r["avg_confidence"] as number) : 0,
    totalUsage: numField(r, "total_usage"),
  };
}

function numField(r: Record<string, unknown>, key: string): number {
  return typeof r[key] === "number" ? (r[key] as number) : 0;
}
