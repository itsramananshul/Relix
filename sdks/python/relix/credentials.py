"""Credentials sub-API — store / list / get / rotate / revoke / audit.

Wraps the bridge's RELIX-7.30 Part 2 credential-vault surface:

* ``POST /v1/credentials`` — store a new credential.
* ``GET  /v1/credentials?owner_agent=...`` — list credentials owned
  by an agent.
* ``GET  /v1/credentials/:name`` — fetch one credential's metadata.
* ``POST /v1/credentials/:name/rotate`` — rotate a credential's
  value, archiving the prior one.
* ``POST /v1/credentials/:name/revoke`` — soft-delete.
* ``GET  /v1/credentials/:name/audit`` — recent audit-log rows.

The SDK kwarg names (``owner``, ``expires_at``) map onto the bridge's
underscore-suffixed wire shape (``owner_agent``, ``expires_at_ms``)
inside the helpers below. The full bridge schema accepts additional
fields the SDK does not surface today (custom metadata, group
sharing); those flow through via ``extra`` dict-passthrough on the
result models.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from pydantic import BaseModel, ConfigDict, Field

if TYPE_CHECKING:
    from .client import RelixClient


class CredentialMetadata(BaseModel):
    """One row from :meth:`CredentialsAPI.list` or :meth:`get`.

    The bridge never returns the credential value via these read
    endpoints — only metadata. Operators who need to fetch the
    decrypted value go through the runtime's just-in-time secret
    store, not the dashboard surface.
    """

    model_config = ConfigDict(extra="allow", populate_by_name=True)

    name: str
    kind: str | None = None
    owner: str | None = Field(default=None, alias="owner_agent")
    created_at_ms: int | None = None
    expires_at_ms: int | None = None
    rotation_interval_secs: int | None = None
    last_rotated_at_ms: int | None = None
    revoked: bool = False
    status: str | None = None


class CredentialAuditEntry(BaseModel):
    """One row from :meth:`CredentialsAPI.audit`.

    Mirrors the runtime's ``CredentialAuditRow`` shape: who touched
    the credential, when, what action (store / rotate / revoke /
    fetch), and an optional reason string.
    """

    model_config = ConfigDict(extra="allow")

    name: str = ""
    action: str = ""
    actor: str | None = None
    timestamp_ms: int | None = None
    reason: str | None = None


class CredentialsAPI:
    """Credentials sub-API. Reached via :attr:`RelixClient.credentials`."""

    def __init__(self, client: "RelixClient") -> None:
        self._client = client

    # ---- store ---------------------------------------------------------

    def store(
        self,
        name: str,
        value: str,
        *,
        kind: str | None = None,
        owner: str | None = None,
        expires_at: int | None = None,
        rotation_interval_secs: int | None = None,
    ) -> dict[str, Any]:
        """Persist a new credential. Returns the bridge's response dict.

        Args:
            name: Stable credential identifier (operators reference
                this from `secret://` URIs in tool / agent configs).
            value: Plaintext value. The bridge encrypts it via the
                vault's master key before writing to disk.
            kind: Free-form taxonomy hint (`"oauth_token"`,
                `"api_key"`, ...).
            owner: Subject id that owns this credential. Defaults to
                the calling identity on the bridge side.
            expires_at: Absolute expiry, unix ms. The runtime treats
                expired credentials as revoked.
            rotation_interval_secs: Soft reminder interval; the
                bridge surfaces stale credentials past this age in
                its audit + alert surfaces.
        """
        body = self._build_store_body(name, value, kind, owner, expires_at, rotation_interval_secs)
        data = self._client._sync_post("/v1/credentials", body)
        return data if isinstance(data, dict) else {}

    async def astore(
        self,
        name: str,
        value: str,
        *,
        kind: str | None = None,
        owner: str | None = None,
        expires_at: int | None = None,
        rotation_interval_secs: int | None = None,
    ) -> dict[str, Any]:
        """Async mirror of :meth:`store`."""
        body = self._build_store_body(name, value, kind, owner, expires_at, rotation_interval_secs)
        data = await self._client._async_post("/v1/credentials", body)
        return data if isinstance(data, dict) else {}

    @staticmethod
    def _build_store_body(
        name: str,
        value: str,
        kind: str | None,
        owner: str | None,
        expires_at: int | None,
        rotation_interval_secs: int | None,
    ) -> dict[str, Any]:
        # SDK kwarg → bridge wire-field translation: `owner` →
        # `owner_agent` and `expires_at` → `expires_at_ms`. Keeps the
        # caller-facing API tight while still hitting the documented
        # bridge schema.
        body: dict[str, Any] = {"name": name, "value": value}
        if kind is not None:
            body["kind"] = kind
        if owner is not None:
            body["owner_agent"] = owner
        if expires_at is not None:
            body["expires_at_ms"] = expires_at
        if rotation_interval_secs is not None:
            body["rotation_interval_secs"] = rotation_interval_secs
        return body

    # ---- list ----------------------------------------------------------

    def list(self, *, owner: str | None = None) -> list[CredentialMetadata]:
        """List credentials. ``owner`` filters by owning subject id."""
        params = {"owner_agent": owner} if owner else None
        data = self._client._sync_get("/v1/credentials", params=params)
        return self._parse_list(data)

    async def alist(self, *, owner: str | None = None) -> list[CredentialMetadata]:
        """Async mirror of :meth:`list`."""
        params = {"owner_agent": owner} if owner else None
        data = await self._client._async_get("/v1/credentials", params=params)
        return self._parse_list(data)

    @staticmethod
    def _parse_list(data: Any) -> list[CredentialMetadata]:
        if isinstance(data, list):
            rows: list[Any] = data
        elif isinstance(data, dict):
            rows = data.get("credentials") or data.get("results") or []
        else:
            rows = []
        return [CredentialMetadata.model_validate(r) for r in rows]

    # ---- get -----------------------------------------------------------

    def get(self, name: str) -> CredentialMetadata:
        """Fetch one credential's metadata (never the value)."""
        data = self._client._sync_get(f"/v1/credentials/{name}")
        return CredentialMetadata.model_validate(data or {})

    async def aget(self, name: str) -> CredentialMetadata:
        """Async mirror of :meth:`get`."""
        data = await self._client._async_get(f"/v1/credentials/{name}")
        return CredentialMetadata.model_validate(data or {})

    # ---- rotate --------------------------------------------------------

    def rotate(self, name: str, new_value: str) -> dict[str, Any]:
        """Rotate the credential to ``new_value``. Archives the old."""
        body = {"new_value": new_value}
        data = self._client._sync_post(f"/v1/credentials/{name}/rotate", body)
        return data if isinstance(data, dict) else {}

    async def arotate(self, name: str, new_value: str) -> dict[str, Any]:
        """Async mirror of :meth:`rotate`."""
        body = {"new_value": new_value}
        data = await self._client._async_post(f"/v1/credentials/{name}/rotate", body)
        return data if isinstance(data, dict) else {}

    # ---- revoke --------------------------------------------------------

    def revoke(self, name: str) -> dict[str, Any]:
        """Soft-delete the credential. The audit log keeps the row."""
        data = self._client._sync_post(f"/v1/credentials/{name}/revoke", {})
        return data if isinstance(data, dict) else {}

    async def arevoke(self, name: str) -> dict[str, Any]:
        """Async mirror of :meth:`revoke`."""
        data = await self._client._async_post(f"/v1/credentials/{name}/revoke", {})
        return data if isinstance(data, dict) else {}

    # ---- audit ---------------------------------------------------------

    def audit(self, name: str) -> list[CredentialAuditEntry]:
        """Recent audit-log entries for one credential."""
        data = self._client._sync_get(f"/v1/credentials/{name}/audit")
        return self._parse_audit(data)

    async def aaudit(self, name: str) -> list[CredentialAuditEntry]:
        """Async mirror of :meth:`audit`."""
        data = await self._client._async_get(f"/v1/credentials/{name}/audit")
        return self._parse_audit(data)

    @staticmethod
    def _parse_audit(data: Any) -> list[CredentialAuditEntry]:
        if isinstance(data, list):
            rows: list[Any] = data
        elif isinstance(data, dict):
            rows = data.get("audit") or data.get("results") or []
        else:
            rows = []
        return [CredentialAuditEntry.model_validate(r) for r in rows]
