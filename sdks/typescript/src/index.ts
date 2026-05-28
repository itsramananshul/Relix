/**
 * Public surface of `@relix/sdk`.
 *
 * Everything a consumer is likely to import re-exports from here so
 * the package barrel is a single import.
 */

export { RelixClient } from "./client";
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
  ChatInput,
  ChatResponse,
  ChatUsage,
  DialecticAnswer,
  FlushContextResult,
  HealthSummary,
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
  Skill,
  SkillStats,
  SkillsSearchInput,
  StreamChunk,
} from "./types";
