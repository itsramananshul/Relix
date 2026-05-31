/**
 * Public surface of `@relix/sdk`.
 *
 * Everything a consumer is likely to import re-exports from here so
 * the package barrel is a single import.
 */

export { RelixClient } from "./client";
export { CredentialsAPI } from "./credentials";
export { IdentityAPI } from "./identity";
export { MemoryAPI } from "./memory";
export { ObservabilityAPI } from "./observability";
export { PlanningAPI } from "./planning";
export { SkillsAPI } from "./skills";

export {
  RelixAuthError,
  RelixConnectionError,
  RelixError,
  RelixResponseError,
  RelixTimeoutError,
} from "./types";

export type {
  AgentDescriptor,
  AgentHealth,
  Alert,
  ApiResult,
  ChatInput,
  ChatResponse,
  ChatUsage,
  CredentialAuditEntry,
  CredentialMetadata,
  CredentialsListInput,
  CredentialsRotateInput,
  CredentialsStoreInput,
  DialecticAnswer,
  FlushContextResult,
  HealthSummary,
  IdentityProfile,
  IdentityResearchInput,
  IngestDocumentResult,
  MemoryDialecticInput,
  MemoryFlushContextInput,
  MemoryIngestDocumentInput,
  MemoryResult,
  MemorySearchInput,
  ObservabilityAlertHistoryInput,
  ObservabilityHealthInput,
  PlanResult,
  PlanningPlanInput,
  RelixClientOptions,
  ResearchResult,
  Skill,
  SkillStats,
  SkillsSearchInput,
  StreamChunk,
} from "./types";
