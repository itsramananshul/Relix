/**
 * Shared types + error hierarchy for the Relix TypeScript SDK.
 *
 * Every public field is exported from this module so consumers can
 * import them via `import type { ChatResponse } from "@relix/sdk"`.
 */

// ---------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------

/**
 * Base class for every error raised by the SDK. Use
 * `instanceof RelixError` to catch any failure shape; the more
 * specific subclasses below are useful for branching.
 */
export class RelixError extends Error {
  public readonly statusCode: number | undefined;
  public readonly body: string | undefined;

  constructor(
    message: string,
    options: { statusCode?: number; body?: string } = {},
  ) {
    super(message);
    this.name = "RelixError";
    this.statusCode = options.statusCode;
    this.body = options.body;
    // Restore the prototype chain when the consumer transpiles to a
    // pre-ES6 target — see TypeScript Handbook "Extending Built-ins".
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

/**
 * Raised when the bridge cannot be reached at all (DNS failure, TCP
 * refusal, TLS handshake error, etc.).
 */
export class RelixConnectionError extends RelixError {
  constructor(message: string, options: { body?: string } = {}) {
    super(message, options);
    this.name = "RelixConnectionError";
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

/** Raised when a request exceeds the configured timeout. */
export class RelixTimeoutError extends RelixError {
  constructor(message: string) {
    super(message);
    this.name = "RelixTimeoutError";
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

/**
 * Raised when the bridge rejects the call's bearer token (HTTP 401).
 *
 * Remediation: rotate the token at `~/.relix/bridge-token` and
 * re-configure the client.
 */
export class RelixAuthError extends RelixError {
  constructor(message: string, options: { body?: string } = {}) {
    super(message, { ...options, statusCode: 401 });
    this.name = "RelixAuthError";
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

/** Raised for any non-2xx response other than 401. */
export class RelixResponseError extends RelixError {
  constructor(
    message: string,
    options: { statusCode: number; body?: string },
  ) {
    super(message, options);
    this.name = "RelixResponseError";
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

// ---------------------------------------------------------------------
// Client configuration
// ---------------------------------------------------------------------

/** Options accepted by `new RelixClient({...})`. */
export interface RelixClientOptions {
  /**
   * Bridge HTTP base URL. Trailing slash is stripped. Defaults to
   * `"http://localhost:19791"`.
   */
  bridgeUrl?: string;
  /**
   * Bridge bearer token. Sent as `Authorization: Bearer <token>`.
   * Stored at `~/.relix/bridge-token` on first bridge boot.
   */
  apiKey?: string | undefined;
  /**
   * Value of the `X-Relix-Tenant` header. Defaults to `"default"`.
   */
  tenantId?: string;
  /**
   * Per-request timeout in milliseconds. Defaults to 30 000.
   */
  timeout?: number;
  /**
   * Optional override of the `fetch` implementation. Defaults to the
   * global `fetch` (Node 18+ / modern browsers).
   *
   * Tests inject a mock via this hook so the SDK never reaches for
   * `globalThis.fetch` directly. Production callers can leave it
   * unset.
   */
  fetch?: typeof fetch;
}

// ---------------------------------------------------------------------
// Chat
// ---------------------------------------------------------------------

/** Request body for `client.chat` / `client.chatStream`. */
export interface ChatInput {
  sessionId: string;
  message: string;
  /** Optional agent alias forwarded as a hint to the bridge flow. */
  agent?: string;
  /** Existing execution workspace lease to bind this chat/run to. */
  workspaceLeaseId?: string;
}

/** Token + cost accounting returned by the bridge when available. */
export interface ChatUsage {
  promptTokens?: number;
  completionTokens?: number;
  totalTokens?: number;
  costCents?: number;
  [key: string]: unknown;
}

/** Typed view of the bridge's `POST /chat` response. */
export interface ChatResponse {
  /** Final assistant reply. Maps from the bridge's `reply` field. */
  text: string;
  flowId: string;
  traceId: string;
  flowLog: string;
  taskId?: string;
  workspaceLeaseId?: string;
  workspacePath?: string;
  model?: string;
  usage?: ChatUsage;
  /** Forward-compat: extra keys returned by the bridge land here. */
  [key: string]: unknown;
}

/** One frame from `client.chatStream`. */
export interface StreamChunk {
  /** Slice of the assistant's reply for this frame. */
  text: string;
  /** Terminal `event: done` frame has `done = true`. */
  done: boolean;
  flowId?: string;
  traceId?: string;
  flowLog?: string;
  taskId?: string;
  workspaceLeaseId?: string;
  workspacePath?: string;
}

// ---------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------

export interface MemorySearchInput {
  query: string;
  subjectId: string;
  /** `"agent"` (default) or `"user"` — memory shelf the bridge searches. */
  target?: "agent" | "user";
  /** Maximum hits returned. Bridge clamps to 1–20. */
  limit?: number;
  /** Memory-node alias override (defaults to `"memory"`). */
  peer?: string;
}

export interface MemoryResult {
  id: string;
  text: string;
  score: number;
  layer?: string;
  confidence?: number;
  tags: string[];
  [key: string]: unknown;
}

export interface MemoryIngestDocumentInput {
  subjectId: string;
  content: string;
  contentType?: string;
  source?: string;
  observerId?: string;
  chunkSizeChars?: number;
  peer?: string;
}

export interface IngestDocumentResult {
  chunksCreated: number;
  embedded: number;
  deferredEmbeddings: number;
  source: string;
  subjectId: string;
  contentType: string;
  [key: string]: unknown;
}

export interface MemoryDialecticInput {
  question: string;
  subjectId: string;
  observerId?: string;
  peer?: string;
}

export interface DialecticAnswer {
  answer: string;
  confidence: number;
  supportingObservations: unknown[];
  [key: string]: unknown;
}

export interface MemoryFlushContextInput {
  sessionId: string;
  keepRecent?: number;
  peer?: string;
}

export interface FlushContextResult {
  flushedCount: number;
  keptCount: number;
  [key: string]: unknown;
}

// ---------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------

export interface PlanningPlanInput {
  spec: string;
  maxAgents?: number;
  dryRun?: boolean;
  peer?: string;
}

export interface PlanResult {
  workflowYaml: string;
  orchestratorActivated: boolean;
  criticApproved: boolean;
  agentsSelected: string[];
  planId?: string;
  workflowPath?: string;
  [key: string]: unknown;
}

export interface AgentDescriptor {
  name: string;
  description: string;
  capabilities: string[];
  persona?: string;
  [key: string]: unknown;
}

// ---------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------

export interface SkillsSearchInput {
  query?: string;
  agent?: string;
  minConfidence?: number;
  limit?: number;
  peer?: string;
}

export interface Skill {
  id: string;
  name: string;
  description: string;
  agentId?: string;
  confidence: number;
  usageCount: number;
  status: string;
  version: number;
  tags: string[];
  steps?: string[];
  [key: string]: unknown;
}

export interface SkillStats {
  totalSkills: number;
  activeSkills: number;
  deprecatedSkills: number;
  avgConfidence: number;
  totalUsage: number;
  [key: string]: unknown;
}

// ---------------------------------------------------------------------
// Observability
// ---------------------------------------------------------------------

export interface ObservabilityHealthInput {
  hours?: number;
  peer?: string;
}

export interface AgentHealth {
  score: number;
  color: string;
  signals: Record<string, unknown>;
  [key: string]: unknown;
}

export interface HealthSummary {
  agents: Record<string, AgentHealth>;
  deployment?: AgentHealth;
  windowHours?: number;
  [key: string]: unknown;
}

export interface Alert {
  id?: string;
  kind: string;
  agent?: string;
  severity: string;
  message: string;
  startedAt?: number;
  endedAt?: number;
  [key: string]: unknown;
}

export interface ObservabilityAlertHistoryInput {
  limit?: number;
  agent?: string;
  peer?: string;
}

// ---------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------

export interface CredentialsStoreInput {
  name: string;
  value: string;
  kind?: string;
  owner?: string;
  /** Absolute expiry, unix ms. */
  expiresAt?: number;
  rotationIntervalSecs?: number;
}

export interface CredentialsListInput {
  owner?: string;
}

export interface CredentialsRotateInput {
  name: string;
  newValue: string;
}

export interface CredentialMetadata {
  name: string;
  kind?: string;
  owner?: string;
  createdAtMs?: number;
  expiresAtMs?: number;
  rotationIntervalSecs?: number;
  lastRotatedAtMs?: number;
  revoked: boolean;
  status?: string;
  [key: string]: unknown;
}

export interface CredentialAuditEntry {
  name: string;
  action: string;
  actor?: string;
  timestampMs?: number;
  reason?: string;
  [key: string]: unknown;
}

// ---------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------

export interface IdentityResearchInput {
  subjectName: string;
  context?: string;
  peer?: string;
}

export interface IdentityProfile {
  displayName?: string;
  professionalRole?: string;
  organization?: string;
  location?: string;
  expertiseAreas: string[];
  publicProfiles: Record<string, unknown>[];
  notableWork: string[];
  confidence: number;
  sourcesUsed: string[];
  synthesisNotes: string;
  [key: string]: unknown;
}

export interface ResearchResult {
  subjectName: string;
  profile: IdentityProfile;
  queriesGenerated: string[];
  resultsConsulted: number;
  providerUsed: string;
  approvalId?: string;
  approvalVerdict?: string;
  memoryRecordId?: string;
  approved: boolean;
  [key: string]: unknown;
}

// ---------------------------------------------------------------------
// ApiResult — PART 5 discriminated union
// ---------------------------------------------------------------------

/**
 * Discriminated union returned by every single-response SDK method.
 *
 * Consumers narrow on the `ok` boolean:
 * ```ts
 * const r = await client.chat({ sessionId: "u", message: "hi" });
 * if (r.ok) {
 *   console.log(r.data.text);
 * } else {
 *   console.error(r.error.message);
 * }
 * ```
 *
 * Streaming methods (`chatStream`) keep returning
 * `AsyncIterable<StreamChunk>` directly — the discriminated union is
 * orthogonal to multi-value protocols.
 */
export type ApiResult<T> =
  | { ok: true; data: T }
  | { ok: false; error: RelixError };
