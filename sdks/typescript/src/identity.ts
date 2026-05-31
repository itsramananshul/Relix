/**
 * Identity sub-API. Reached via `client.identity`.
 *
 * Wraps the bridge's RELIX-7.18 / GAP-17 Part 2 surface:
 * * `POST /v1/identity/research` — research-backed identity synthesis.
 *
 * Matches the Python `client.identity` surface field-for-field.
 */

import { doJsonRequest, isObject, runRequest, type RelixClient } from "./client";
import type {
  ApiResult,
  IdentityProfile,
  IdentityResearchInput,
  ResearchResult,
} from "./types";

export class IdentityAPI {
  constructor(private readonly client: RelixClient) {}

  /**
   * Synthesise an identity profile for `subjectName`.
   *
   * The bridge proxies onto the identity peer's `identity.research`
   * cap. When the production approval gate is enabled the bridge
   * blocks until the operator votes (up to five minutes); the SDK
   * respects the bridge's deadline.
   */
  async research(input: IdentityResearchInput): Promise<ApiResult<ResearchResult>> {
    return runRequest(async () => {
      const body: Record<string, unknown> = { subject_name: input.subjectName };
      if (input.context !== undefined) {
        body.context = input.context;
      }
      if (input.peer !== undefined) {
        body.peer = input.peer;
      }
      const data = await doJsonRequest(this.client, "POST", "/v1/identity/research", body);
      return parseResearch(data);
    });
  }
}

function parseResearch(data: unknown): ResearchResult {
  const r = isObject(data) ? data : {};
  return {
    subjectName: typeof r["subject_name"] === "string" ? (r["subject_name"] as string) : "",
    profile: parseProfile(r["profile"]),
    queriesGenerated: Array.isArray(r["queries_generated"])
      ? (r["queries_generated"] as unknown[]).filter((x): x is string => typeof x === "string")
      : [],
    resultsConsulted:
      typeof r["results_consulted"] === "number" ? (r["results_consulted"] as number) : 0,
    providerUsed: typeof r["provider_used"] === "string" ? (r["provider_used"] as string) : "",
    approvalId: typeof r["approval_id"] === "string" ? (r["approval_id"] as string) : undefined,
    approvalVerdict:
      typeof r["approval_verdict"] === "string" ? (r["approval_verdict"] as string) : undefined,
    memoryRecordId:
      typeof r["memory_record_id"] === "string" ? (r["memory_record_id"] as string) : undefined,
    approved: typeof r["approved"] === "boolean" ? (r["approved"] as boolean) : false,
  };
}

function parseProfile(raw: unknown): IdentityProfile {
  const r = isObject(raw) ? raw : {};
  const expertiseAreas = Array.isArray(r["expertise_areas"])
    ? (r["expertise_areas"] as unknown[]).filter((x): x is string => typeof x === "string")
    : [];
  const publicProfiles = Array.isArray(r["public_profiles"])
    ? (r["public_profiles"] as unknown[]).filter((x): x is Record<string, unknown> =>
        isObject(x),
      )
    : [];
  const notableWork = Array.isArray(r["notable_work"])
    ? (r["notable_work"] as unknown[]).filter((x): x is string => typeof x === "string")
    : [];
  const sourcesUsed = Array.isArray(r["sources_used"])
    ? (r["sources_used"] as unknown[]).filter((x): x is string => typeof x === "string")
    : [];
  return {
    displayName: typeof r["display_name"] === "string" ? (r["display_name"] as string) : undefined,
    professionalRole:
      typeof r["professional_role"] === "string" ? (r["professional_role"] as string) : undefined,
    organization:
      typeof r["organization"] === "string" ? (r["organization"] as string) : undefined,
    location: typeof r["location"] === "string" ? (r["location"] as string) : undefined,
    expertiseAreas,
    publicProfiles,
    notableWork,
    confidence: typeof r["confidence"] === "number" ? (r["confidence"] as number) : 0,
    sourcesUsed,
    synthesisNotes:
      typeof r["synthesis_notes"] === "string" ? (r["synthesis_notes"] as string) : "",
  };
}
