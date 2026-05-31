/**
 * Credentials sub-API. Reached via `client.credentials`.
 *
 * Wraps the bridge's RELIX-7.30 Part 2 credential-vault surface:
 * * `POST /v1/credentials`                  — store
 * * `GET  /v1/credentials?owner_agent=...`  — list
 * * `GET  /v1/credentials/:name`            — get
 * * `POST /v1/credentials/:name/rotate`     — rotate (body: `{ new_value }`)
 * * `POST /v1/credentials/:name/revoke`     — revoke
 * * `GET  /v1/credentials/:name/audit`      — audit
 *
 * Matches the Python `client.credentials` surface field-for-field;
 * Python `snake_case` ↔ TypeScript `camelCase` translations are
 * handled at the wire boundary so the two SDKs feel identical to a
 * polyglot consumer.
 */

import { doJsonRequest, isObject, runRequest, type RelixClient } from "./client";
import type {
  ApiResult,
  CredentialAuditEntry,
  CredentialMetadata,
  CredentialsListInput,
  CredentialsRotateInput,
  CredentialsStoreInput,
} from "./types";

export class CredentialsAPI {
  constructor(private readonly client: RelixClient) {}

  /** Store a new credential. Returns the bridge's response dict. */
  async store(
    input: CredentialsStoreInput,
  ): Promise<ApiResult<Record<string, unknown>>> {
    return runRequest(async () => {
      // SDK kwarg → bridge wire-field translation: `owner` →
      // `owner_agent`, `expiresAt` → `expires_at_ms`.
      const body: Record<string, unknown> = {
        name: input.name,
        value: input.value,
      };
      if (input.kind !== undefined) {
        body.kind = input.kind;
      }
      if (input.owner !== undefined) {
        body.owner_agent = input.owner;
      }
      if (input.expiresAt !== undefined) {
        body.expires_at_ms = input.expiresAt;
      }
      if (input.rotationIntervalSecs !== undefined) {
        body.rotation_interval_secs = input.rotationIntervalSecs;
      }
      const data = await doJsonRequest(this.client, "POST", "/v1/credentials", body);
      return isObject(data) ? data : {};
    });
  }

  /** List credentials. `owner` filters by owning subject id. */
  async list(input: CredentialsListInput = {}): Promise<ApiResult<CredentialMetadata[]>> {
    return runRequest(async () => {
      const params = input.owner ? { owner_agent: input.owner } : undefined;
      const data = await doJsonRequest(this.client, "GET", "/v1/credentials", undefined, params);
      return parseList(data);
    });
  }

  /** Fetch one credential's metadata (never the value). */
  async get(name: string): Promise<ApiResult<CredentialMetadata>> {
    return runRequest(async () => {
      const data = await doJsonRequest(
        this.client,
        "GET",
        `/v1/credentials/${encodeURIComponent(name)}`,
      );
      return parseMetadata(data);
    });
  }

  /** Rotate the credential to `newValue`. Archives the old value. */
  async rotate(input: CredentialsRotateInput): Promise<ApiResult<Record<string, unknown>>> {
    return runRequest(async () => {
      const body = { new_value: input.newValue };
      const data = await doJsonRequest(
        this.client,
        "POST",
        `/v1/credentials/${encodeURIComponent(input.name)}/rotate`,
        body,
      );
      return isObject(data) ? data : {};
    });
  }

  /** Soft-delete the credential. The audit log keeps the row. */
  async revoke(name: string): Promise<ApiResult<Record<string, unknown>>> {
    return runRequest(async () => {
      const data = await doJsonRequest(
        this.client,
        "POST",
        `/v1/credentials/${encodeURIComponent(name)}/revoke`,
        {},
      );
      return isObject(data) ? data : {};
    });
  }

  /** Recent audit-log entries for one credential. */
  async audit(name: string): Promise<ApiResult<CredentialAuditEntry[]>> {
    return runRequest(async () => {
      const data = await doJsonRequest(
        this.client,
        "GET",
        `/v1/credentials/${encodeURIComponent(name)}/audit`,
      );
      return parseAudit(data);
    });
  }
}

function parseList(data: unknown): CredentialMetadata[] {
  let rows: unknown[];
  if (Array.isArray(data)) {
    rows = data;
  } else if (isObject(data)) {
    const r = data["credentials"] ?? data["results"];
    rows = Array.isArray(r) ? r : [];
  } else {
    rows = [];
  }
  return rows.map(parseMetadata);
}

function parseMetadata(row: unknown): CredentialMetadata {
  const r = isObject(row) ? row : {};
  return {
    name: typeof r["name"] === "string" ? (r["name"] as string) : "",
    kind: typeof r["kind"] === "string" ? (r["kind"] as string) : undefined,
    owner: typeof r["owner_agent"] === "string" ? (r["owner_agent"] as string) : undefined,
    createdAtMs: typeof r["created_at_ms"] === "number" ? (r["created_at_ms"] as number) : undefined,
    expiresAtMs: typeof r["expires_at_ms"] === "number" ? (r["expires_at_ms"] as number) : undefined,
    rotationIntervalSecs:
      typeof r["rotation_interval_secs"] === "number"
        ? (r["rotation_interval_secs"] as number)
        : undefined,
    lastRotatedAtMs:
      typeof r["last_rotated_at_ms"] === "number"
        ? (r["last_rotated_at_ms"] as number)
        : undefined,
    revoked: typeof r["revoked"] === "boolean" ? (r["revoked"] as boolean) : false,
    status: typeof r["status"] === "string" ? (r["status"] as string) : undefined,
  };
}

function parseAudit(data: unknown): CredentialAuditEntry[] {
  let rows: unknown[];
  if (Array.isArray(data)) {
    rows = data;
  } else if (isObject(data)) {
    const r = data["audit"] ?? data["results"];
    rows = Array.isArray(r) ? r : [];
  } else {
    rows = [];
  }
  return rows.map(parseAuditRow);
}

function parseAuditRow(row: unknown): CredentialAuditEntry {
  const r = isObject(row) ? row : {};
  return {
    name: typeof r["name"] === "string" ? (r["name"] as string) : "",
    action: typeof r["action"] === "string" ? (r["action"] as string) : "",
    actor: typeof r["actor"] === "string" ? (r["actor"] as string) : undefined,
    timestampMs:
      typeof r["timestamp_ms"] === "number" ? (r["timestamp_ms"] as number) : undefined,
    reason: typeof r["reason"] === "string" ? (r["reason"] as string) : undefined,
  };
}
