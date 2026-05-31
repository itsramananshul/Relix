/**
 * Observability sub-API. Reached via `client.observability`.
 *
 * Wraps the bridge's RELIX-7.28 Part 2 observability surface:
 * * `GET /v1/observability/health?hours=N&peer=...`
 * * `GET /v1/observability/alerts?peer=...`
 * * `GET /v1/observability/alerts/history?limit=N&agent=...`
 */

import { doJsonRequest, isObject, runRequest, type RelixClient } from "./client";
import type {
  AgentHealth,
  Alert,
  ApiResult,
  HealthSummary,
  ObservabilityAlertHistoryInput,
  ObservabilityHealthInput,
} from "./types";

export class ObservabilityAPI {
  constructor(private readonly client: RelixClient) {}

  async health(input: ObservabilityHealthInput = {}): Promise<ApiResult<HealthSummary>> {
    return runRequest(async () => {
      const params: Record<string, unknown> = {
        hours: input.hours,
        peer: input.peer,
      };
      const data = await doJsonRequest(
        this.client,
        "GET",
        "/v1/observability/health",
        undefined,
        params,
      );
      return parseHealth(data);
    });
  }

  async alerts(peer?: string): Promise<ApiResult<Alert[]>> {
    return runRequest(async () => {
      const data = await doJsonRequest(
        this.client,
        "GET",
        "/v1/observability/alerts",
        undefined,
        peer ? { peer } : undefined,
      );
      return parseAlerts(data);
    });
  }

  async alertHistory(
    input: ObservabilityAlertHistoryInput = {},
  ): Promise<ApiResult<Alert[]>> {
    return runRequest(async () => {
      const params: Record<string, unknown> = {
        limit: input.limit,
        agent: input.agent,
        peer: input.peer,
      };
      const data = await doJsonRequest(
        this.client,
        "GET",
        "/v1/observability/alerts/history",
        undefined,
        params,
      );
      return parseAlerts(data);
    });
  }
}

function parseHealth(data: unknown): HealthSummary {
  if (!isObject(data)) {
    return { agents: {} };
  }
  const agentsRaw = data["agents"];
  const agents: Record<string, AgentHealth> = {};
  if (isObject(agentsRaw)) {
    for (const [name, raw] of Object.entries(agentsRaw)) {
      if (isObject(raw)) {
        agents[name] = parseAgentHealthRow(raw);
      }
    }
  }
  let deployment: AgentHealth | undefined;
  for (const key of ["deployment", "_deployment"]) {
    const raw = data[key];
    if (isObject(raw)) {
      deployment = parseAgentHealthRow(raw);
      break;
    }
  }
  const hours = data["window_hours"] ?? data["hours"];
  return {
    agents,
    deployment,
    windowHours: typeof hours === "number" ? hours : undefined,
  };
}

function parseAgentHealthRow(r: Record<string, unknown>): AgentHealth {
  const signals = isObject(r["signals"]) ? (r["signals"] as Record<string, unknown>) : {};
  return {
    score: typeof r["score"] === "number" ? (r["score"] as number) : 0,
    color: typeof r["color"] === "string" ? (r["color"] as string) : "unknown",
    signals,
  };
}

function parseAlerts(data: unknown): Alert[] {
  let rows: unknown[];
  if (Array.isArray(data)) {
    rows = data;
  } else if (isObject(data)) {
    const r = data["alerts"] ?? data["results"];
    rows = Array.isArray(r) ? r : [];
  } else {
    rows = [];
  }
  return rows.map(parseAlertRow);
}

function parseAlertRow(row: unknown): Alert {
  const r = isObject(row) ? row : {};
  return {
    id: typeof r["id"] === "string" ? (r["id"] as string) : undefined,
    kind: typeof r["kind"] === "string" ? (r["kind"] as string) : "",
    agent: typeof r["agent"] === "string" ? (r["agent"] as string) : undefined,
    severity: typeof r["severity"] === "string" ? (r["severity"] as string) : "",
    message: typeof r["message"] === "string" ? (r["message"] as string) : "",
    startedAt: typeof r["started_at"] === "number" ? (r["started_at"] as number) : undefined,
    endedAt: typeof r["ended_at"] === "number" ? (r["ended_at"] as number) : undefined,
  };
}
