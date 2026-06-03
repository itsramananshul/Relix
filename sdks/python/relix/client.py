"""Core HTTP client for the Relix bridge.

This module owns three things:

1.  :class:`RelixClient` — the public entry point. Holds the bridge URL,
    bearer token, tenant id, and the httpx ``Client`` / ``AsyncClient``
    pair used by every request.
2.  The wire-typed response models (:class:`ChatResponse`,
    :class:`StreamChunk`, :class:`ChatUsage`).
3.  The shared error-translation helpers (:func:`_translate_status_error`
    et al.) used by every sub-API.

The sub-APIs (``client.memory``, ``client.planning``, ``client.skills``,
``client.observability``) live in their own modules but reach back into
this module for the actual HTTP work. Keeping the HTTP path centralised
means the tenant header, bearer auth, timeout handling, and error
mapping each live in one place.
"""

from __future__ import annotations

import json
from collections.abc import AsyncIterator, Awaitable, Iterator
from typing import TYPE_CHECKING, Any

import httpx
from pydantic import BaseModel, ConfigDict, Field

from .exceptions import (
    RelixAuthError,
    RelixConnectionError,
    RelixError,
    RelixResponseError,
    RelixTimeoutError,
)

if TYPE_CHECKING:
    from .credentials import CredentialsAPI
    from .identity import IdentityAPI
    from .memory import MemoryAPI
    from .observability import ObservabilityAPI
    from .planning import PlanningAPI
    from .skills import SkillsAPI

DEFAULT_BRIDGE_URL = "http://localhost:19791"
"""Default bridge base URL. Matches the bridge's default listen port."""

DEFAULT_TIMEOUT = 30.0
"""Default per-request timeout in seconds."""

DEFAULT_TENANT = "default"
"""Tenant id sent when the caller does not override it."""


class ChatUsage(BaseModel):
    """Token + cost accounting for a chat call when the bridge surfaces it.

    Field shapes match the OpenAI usage block so the type is recognisable
    across the wider ecosystem. Every field is optional because the
    bridge's native ``/chat`` does not currently emit usage rows; the
    OpenAI-compat shim does.
    """

    model_config = ConfigDict(extra="allow")

    prompt_tokens: int | None = None
    completion_tokens: int | None = None
    total_tokens: int | None = None
    cost_cents: int | None = None


class ChatResponse(BaseModel):
    """Typed view of the bridge's ``POST /chat`` response.

    The bridge's wire body is::

        { "reply": "...", "flow_id": "...", "trace_id": "...",
          "flow_log": "...", "task_id"?: "...",
          "workspace_lease_id"?: "...", "workspace_path"?: "..." }

    The SDK normalises ``reply`` → ``text`` for parity with the
    TypeScript SDK and the OpenAI shim. Additional fields the bridge
    might add land in the model via ``extra="allow"``.
    """

    model_config = ConfigDict(extra="allow", populate_by_name=True)

    text: str = Field(alias="reply")
    flow_id: str
    trace_id: str
    flow_log: str = ""
    task_id: str | None = None
    workspace_lease_id: str | None = None
    workspace_path: str | None = None
    model: str | None = None
    usage: ChatUsage | None = None


class StreamChunk(BaseModel):
    """One frame from ``client.chat_stream`` / ``client.achat_stream``.

    ``text`` is the slice of the assistant's reply for this frame.
    ``done`` flips to ``True`` on the terminal ``event: done`` frame —
    when set, ``flow_id`` / ``trace_id`` / ``flow_log`` carry the
    finalisation metadata. Concatenating every chunk's ``text`` yields
    the full reply.
    """

    model_config = ConfigDict(extra="allow")

    text: str = ""
    done: bool = False
    flow_id: str | None = None
    trace_id: str | None = None
    flow_log: str | None = None
    task_id: str | None = None
    workspace_lease_id: str | None = None
    workspace_path: str | None = None


def _translate_transport_error(exc: Exception) -> RelixError:
    """Map an httpx-side exception into the SDK exception hierarchy.

    httpx's timeout, network, and connect errors all surface here; the
    caller never sees a raw httpx exception leak through the SDK
    boundary.
    """
    if isinstance(exc, httpx.TimeoutException):
        return RelixTimeoutError(f"request timed out: {exc}")
    if isinstance(exc, httpx.ConnectError | httpx.NetworkError):
        return RelixConnectionError(f"cannot reach bridge: {exc}")
    if isinstance(exc, RelixError):
        return exc
    return RelixConnectionError(f"transport: {exc}")


def _translate_status_error(status_code: int, body: str) -> RelixError:
    """Map a non-2xx HTTP response into the SDK exception hierarchy."""
    if status_code == 401:
        return RelixAuthError(
            "bridge rejected the bearer token (401)",
            status_code=status_code,
            body=body,
        )
    return RelixResponseError(
        f"bridge returned HTTP {status_code}",
        status_code=status_code,
        body=body,
    )


class SSEParser:
    """PART 7 — incremental SSE state machine.

    Feeds raw text into an internal buffer and returns parsed events
    whenever the buffer contains at least one complete event (events
    are separated by a blank line — ``\\n\\n``). The returned value is
    a list of ``dict`` payloads keyed by SSE field name (``event``,
    ``data``, ``id``, ``retry``, ...). Mirrors the per-spec shape from
    the user's prompt.

    The parser deals only with the framing layer — semantic
    translation into :class:`StreamChunk` happens in
    :func:`_sse_event_to_chunk` so this class is reusable for any SSE
    consumer in the SDK.
    """

    def __init__(self) -> None:
        self._buffer: str = ""

    def feed(self, chunk: str) -> list[dict[str, str]]:
        """Append ``chunk`` to the internal buffer and return any
        events that have become complete as a result.

        ``chunk`` may carry partial event text — the parser holds the
        unfinished tail until the next ``feed`` (or until ``flush``).
        Two events delivered in a single chunk produce two entries in
        the returned list; one event split across two ``feed`` calls
        produces an entry on the second call.
        """
        events: list[dict[str, str]] = []
        self._buffer += chunk
        while "\n\n" in self._buffer:
            event_text, self._buffer = self._buffer.split("\n\n", 1)
            event = self._parse_event(event_text)
            if event:
                events.append(event)
        return events

    def flush(self) -> list[dict[str, str]]:
        """Drain any remaining buffered event text.

        Some SSE producers terminate a stream by closing the socket
        without sending a trailing blank line. Calling ``flush`` once
        the upstream stream is exhausted gives the parser a chance to
        surface a final event in that case.
        """
        if not self._buffer.strip():
            self._buffer = ""
            return []
        last = self._buffer
        self._buffer = ""
        event = self._parse_event(last)
        return [event] if event else []

    def _parse_event(self, text: str) -> dict[str, str] | None:
        """Translate one raw event-text block into a field dict.

        Each line of the form ``field: value`` is folded into the
        result. The per-spec leading single space after ``:`` is
        stripped via ``.strip()`` on the value half. Blank lines, lines
        without a colon, and comment lines (starting with ``:``) are
        ignored. Returns ``None`` for an event that produced no fields.
        """
        fields: dict[str, str] = {}
        for line in text.strip().split("\n"):
            if line.startswith(":") or ":" not in line:
                continue
            field, _, value = line.partition(":")
            fields[field.strip()] = value.strip()
        return fields if fields else None


def _sse_event_to_chunk(event: dict[str, str]) -> StreamChunk | None:
    """Translate one parsed SSE event into a :class:`StreamChunk`.

    Wire shape:
      * ``event: chunk`` + ``data: <raw text>`` → ``StreamChunk(text=...)``
        (the bridge sends raw text here, NOT JSON; the OpenAI shim sends
        JSON so we fall back to extracting a ``chunk`` / ``text`` field
        if the payload happens to be valid JSON).
      * ``event: done`` + ``data: {json metadata}`` →
        ``StreamChunk(done=True, flow_id=..., ...)``.
      * ``[DONE]`` literal payload → ``None`` (OpenAI-compat sentinel,
        the caller treats absence-of-frame as end-of-stream).
    """
    kind = event.get("event") or "chunk"
    payload = event.get("data", "")
    if not payload or payload == "[DONE]":
        return None
    if kind == "done":
        # Try to parse metadata; if we can't, still emit a done frame.
        try:
            parsed = json.loads(payload)
        except json.JSONDecodeError:
            return StreamChunk(text="", done=True)
        if isinstance(parsed, dict):
            return StreamChunk(
                text="",
                done=True,
                flow_id=parsed.get("flow_id"),
                trace_id=parsed.get("trace_id"),
                flow_log=parsed.get("flow_log"),
                task_id=parsed.get("task_id"),
                workspace_lease_id=parsed.get("workspace_lease_id"),
                workspace_path=parsed.get("workspace_path"),
            )
        return StreamChunk(text="", done=True)
    # `event: chunk` (or no event field) — the bridge's native shape
    # is raw text. The OpenAI shim is JSON-shaped; tolerate both.
    try:
        parsed = json.loads(payload)
    except json.JSONDecodeError:
        return StreamChunk(text=payload, done=False)
    if isinstance(parsed, dict):
        text = parsed.get("chunk") or parsed.get("text") or ""
        return StreamChunk(
            text=str(text),
            done=False,
            flow_id=parsed.get("flow_id"),
            trace_id=parsed.get("trace_id"),
            flow_log=parsed.get("flow_log"),
            task_id=parsed.get("task_id"),
            workspace_lease_id=parsed.get("workspace_lease_id"),
            workspace_path=parsed.get("workspace_path"),
        )
    if isinstance(parsed, str):
        return StreamChunk(text=parsed, done=False)
    # Unrecognised JSON scalar (number, array, bool) → treat the
    # original payload as raw text.
    return StreamChunk(text=payload, done=False)


def _iter_sse_chunks(text: str, parser: SSEParser) -> Iterator[StreamChunk]:
    """Yield :class:`StreamChunk` frames from a raw SSE byte slice.

    Normalises CRLF to LF so a non-Relix proxy that rewrites line
    endings doesn't break the parser (the bridge emits LF natively;
    the spec-compliant :class:`SSEParser` only looks for ``\\n\\n``
    separators).
    """
    normalised = text.replace("\r\n", "\n")
    for event in parser.feed(normalised):
        chunk = _sse_event_to_chunk(event)
        if chunk is not None:
            yield chunk


def _build_headers(
    *,
    api_key: str | None,
    tenant_id: str,
    extra: dict[str, str] | None = None,
) -> dict[str, str]:
    """Assemble the headers every Relix request rides on.

    ``X-Relix-Tenant`` is always sent — the tenant middleware on the
    bridge falls back to ``"default"`` if absent, but being explicit
    makes the wire trace clearer for operators.
    """
    headers: dict[str, str] = {
        "content-type": "application/json",
        "accept": "application/json",
        "x-relix-tenant": tenant_id,
        "user-agent": "relix-python-sdk/0.1.0",
    }
    if api_key:
        headers["authorization"] = f"Bearer {api_key}"
    if extra:
        headers.update({k.lower(): v for k, v in extra.items()})
    return headers


class RelixClient:
    """Synchronous + asynchronous HTTP client for the Relix bridge.

    Most users only need ``with RelixClient(...) as c:`` and then call
    ``c.chat(...)`` / ``c.memory.search(...)`` / etc. The async
    counterparts (``achat``, etc.) share the same client instance — the
    underlying httpx pools are lazily created on first use, so an app
    that only calls sync methods never pays for the async client setup
    and vice versa.

    Args:
        bridge_url: Bridge HTTP base URL. Trailing slash is stripped.
        tenant_id: Value of the ``X-Relix-Tenant`` header. Defaults to
            ``"default"``.
        api_key: Bearer token the bridge accepts on
            ``Authorization: Bearer <token>``. Generated at first bridge
            boot and stored in ``~/.relix/bridge-token``. ``None`` skips
            the header (useful for an open-network smoke test or when
            the bridge is fronted by a proxy that injects auth).
        timeout: Per-request timeout in seconds. Applies to connect +
            read + write phases.
    """

    def __init__(
        self,
        bridge_url: str = DEFAULT_BRIDGE_URL,
        *,
        tenant_id: str = DEFAULT_TENANT,
        api_key: str | None = None,
        timeout: float = DEFAULT_TIMEOUT,
    ) -> None:
        self._base_url = bridge_url.rstrip("/")
        self._tenant_id = tenant_id
        self._api_key = api_key
        self._timeout = timeout
        self._sync_client: httpx.Client | None = None
        self._async_client: httpx.AsyncClient | None = None
        # Sub-APIs are lazily constructed so an import of the SDK that
        # only uses chat does not pay for instantiating four namespaces.
        self._memory: MemoryAPI | None = None
        self._planning: PlanningAPI | None = None
        self._skills: SkillsAPI | None = None
        self._observability: ObservabilityAPI | None = None
        self._credentials: CredentialsAPI | None = None
        self._identity: IdentityAPI | None = None

    # ---- properties ----------------------------------------------------

    @property
    def bridge_url(self) -> str:
        """Bridge base URL the client was configured with."""
        return self._base_url

    @property
    def tenant_id(self) -> str:
        """Current value of the ``X-Relix-Tenant`` header."""
        return self._tenant_id

    @property
    def memory(self) -> MemoryAPI:
        """Memory sub-API (``client.memory.search`` / ``ingest_document`` / ``dialectic`` / ...)."""
        from .memory import MemoryAPI  # avoid cyclic import at module load

        if self._memory is None:
            self._memory = MemoryAPI(self)
        return self._memory

    @property
    def planning(self) -> PlanningAPI:
        """Planning sub-API (``client.planning.plan`` / ``agents``)."""
        from .planning import PlanningAPI

        if self._planning is None:
            self._planning = PlanningAPI(self)
        return self._planning

    @property
    def skills(self) -> SkillsAPI:
        """Skills sub-API (``client.skills.search`` / ``stats``)."""
        from .skills import SkillsAPI

        if self._skills is None:
            self._skills = SkillsAPI(self)
        return self._skills

    @property
    def observability(self) -> ObservabilityAPI:
        """Observability sub-API (``client.observability.health`` / ``alerts``)."""
        from .observability import ObservabilityAPI

        if self._observability is None:
            self._observability = ObservabilityAPI(self)
        return self._observability

    @property
    def credentials(self) -> CredentialsAPI:
        """Credentials sub-API (``client.credentials.store`` / ``list`` / ``rotate`` / ``revoke`` / ``audit``)."""
        from .credentials import CredentialsAPI

        if self._credentials is None:
            self._credentials = CredentialsAPI(self)
        return self._credentials

    @property
    def identity(self) -> IdentityAPI:
        """Identity sub-API (``client.identity.research``)."""
        from .identity import IdentityAPI

        if self._identity is None:
            self._identity = IdentityAPI(self)
        return self._identity

    # ---- httpx client management --------------------------------------

    def _ensure_sync(self) -> httpx.Client:
        """Lazily build the sync httpx Client and reuse it across calls."""
        if self._sync_client is None or self._sync_client.is_closed:
            self._sync_client = httpx.Client(timeout=self._timeout)
        return self._sync_client

    def _ensure_async(self) -> httpx.AsyncClient:
        """Lazily build the async httpx AsyncClient and reuse it."""
        if self._async_client is None or self._async_client.is_closed:
            self._async_client = httpx.AsyncClient(timeout=self._timeout)
        return self._async_client

    def close(self) -> None:
        """Close the underlying sync httpx Client. Idempotent."""
        if self._sync_client is not None and not self._sync_client.is_closed:
            self._sync_client.close()
        self._sync_client = None

    async def aclose(self) -> None:
        """Close the underlying async httpx Client. Idempotent."""
        if self._async_client is not None and not self._async_client.is_closed:
            await self._async_client.aclose()
        self._async_client = None

    def __enter__(self) -> "RelixClient":
        self._ensure_sync()
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    async def __aenter__(self) -> "RelixClient":
        self._ensure_async()
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.aclose()

    # ---- core HTTP -----------------------------------------------------

    def _request_sync(
        self,
        method: str,
        path: str,
        *,
        json_body: Any = None,
        params: dict[str, Any] | None = None,
    ) -> Any:
        """Synchronous JSON request. Raises an SDK error on non-2xx.

        Returns the parsed JSON body (``dict``, ``list``, or scalar) when
        the response has any. Returns ``None`` for empty 204-style
        responses.
        """
        url = self._url(path)
        client = self._ensure_sync()
        headers = _build_headers(api_key=self._api_key, tenant_id=self._tenant_id)
        try:
            resp = client.request(
                method,
                url,
                headers=headers,
                json=json_body,
                params=self._scrub_params(params),
            )
        except httpx.HTTPError as exc:
            raise _translate_transport_error(exc) from exc
        return self._parse_response(resp)

    async def _request_async(
        self,
        method: str,
        path: str,
        *,
        json_body: Any = None,
        params: dict[str, Any] | None = None,
    ) -> Any:
        """Asynchronous JSON request. Mirror of :meth:`_request_sync`."""
        url = self._url(path)
        client = self._ensure_async()
        headers = _build_headers(api_key=self._api_key, tenant_id=self._tenant_id)
        try:
            resp = await client.request(
                method,
                url,
                headers=headers,
                json=json_body,
                params=self._scrub_params(params),
            )
        except httpx.HTTPError as exc:
            raise _translate_transport_error(exc) from exc
        return self._parse_response(resp)

    def _parse_response(self, resp: httpx.Response) -> Any:
        """Translate an httpx response into JSON or raise."""
        body_text = resp.text
        if resp.status_code >= 400:
            raise _translate_status_error(resp.status_code, body_text)
        if not body_text:
            return None
        try:
            return resp.json()
        except (json.JSONDecodeError, ValueError) as exc:
            raise RelixResponseError(
                f"bridge response was not valid JSON: {exc}",
                status_code=resp.status_code,
                body=body_text,
            ) from exc

    def _url(self, path: str) -> str:
        """Resolve a relative SDK path against the configured base URL."""
        if path.startswith(("http://", "https://")):
            return path
        if not path.startswith("/"):
            path = "/" + path
        return self._base_url + path

    @staticmethod
    def _scrub_params(params: dict[str, Any] | None) -> dict[str, Any] | None:
        """Drop ``None`` / empty-string values from query params.

        httpx serialises ``None`` as the literal string ``"None"`` which
        the bridge would reject. Stripping at the SDK boundary lets sub-APIs
        pass optional args as kwargs without bookkeeping.
        """
        if not params:
            return None
        scrubbed: dict[str, Any] = {}
        for key, value in params.items():
            if value is None:
                continue
            if isinstance(value, bool):
                scrubbed[key] = "true" if value else "false"
            else:
                scrubbed[key] = value
        return scrubbed or None

    # ---- chat ----------------------------------------------------------

    def chat(
        self,
        session_id: str,
        message: str,
        *,
        agent: str | None = None,
        workspace_lease_id: str | None = None,
    ) -> ChatResponse:
        """Synchronous chat call.

        Posts to the bridge's ``POST /chat`` and decodes the response
        into :class:`ChatResponse`. ``agent`` is currently informational;
        it is forwarded as a hint when the bridge's flow template
        consumes it (the alpha bridge ignores it but accepts the field).
        ``workspace_lease_id`` binds the chat to an existing execution
        workspace lease; the response echoes the resolved lease/path when
        the bridge accepted that binding.
        """
        body = {"session_id": session_id, "message": message}
        if agent:
            body["agent"] = agent
        if workspace_lease_id:
            body["workspace_lease_id"] = workspace_lease_id
        data = self._request_sync("POST", "/chat", json_body=body)
        return ChatResponse.model_validate(data)

    async def achat(
        self,
        session_id: str,
        message: str,
        *,
        agent: str | None = None,
        workspace_lease_id: str | None = None,
    ) -> ChatResponse:
        """Asynchronous chat call. Async mirror of :meth:`chat`."""
        body = {"session_id": session_id, "message": message}
        if agent:
            body["agent"] = agent
        if workspace_lease_id:
            body["workspace_lease_id"] = workspace_lease_id
        data = await self._request_async("POST", "/chat", json_body=body)
        return ChatResponse.model_validate(data)

    def chat_stream(
        self,
        session_id: str,
        message: str,
        *,
        agent: str | None = None,
        workspace_lease_id: str | None = None,
    ) -> Iterator[StreamChunk]:
        """Synchronous streaming chat.

        Returns an iterator of :class:`StreamChunk`. Concatenate every
        chunk's ``text`` to get the full reply; the terminal frame has
        ``done=True`` and carries ``flow_id`` / ``trace_id`` / ``flow_log``.
        """
        url = self._url("/chat/stream")
        body = {"session_id": session_id, "message": message}
        if agent:
            body["agent"] = agent
        if workspace_lease_id:
            body["workspace_lease_id"] = workspace_lease_id
        headers = _build_headers(
            api_key=self._api_key,
            tenant_id=self._tenant_id,
            extra={"accept": "text/event-stream"},
        )
        client = self._ensure_sync()
        try:
            with client.stream(
                "POST", url, headers=headers, json=body
            ) as resp:
                if resp.status_code >= 400:
                    raise _translate_status_error(resp.status_code, resp.read().decode("utf-8", "replace"))
                parser = SSEParser()
                for chunk in resp.iter_text():
                    if not chunk:
                        continue
                    yield from _iter_sse_chunks(chunk, parser)
                # Flush any trailing event the upstream closed before
                # delivering its blank line.
                for tail_event in parser.flush():
                    tail_chunk = _sse_event_to_chunk(tail_event)
                    if tail_chunk is not None:
                        yield tail_chunk
        except httpx.HTTPError as exc:
            raise _translate_transport_error(exc) from exc

    async def achat_stream(
        self,
        session_id: str,
        message: str,
        *,
        agent: str | None = None,
        workspace_lease_id: str | None = None,
    ) -> AsyncIterator[StreamChunk]:
        """Asynchronous streaming chat. Async mirror of :meth:`chat_stream`."""
        url = self._url("/chat/stream")
        body = {"session_id": session_id, "message": message}
        if agent:
            body["agent"] = agent
        if workspace_lease_id:
            body["workspace_lease_id"] = workspace_lease_id
        headers = _build_headers(
            api_key=self._api_key,
            tenant_id=self._tenant_id,
            extra={"accept": "text/event-stream"},
        )
        client = self._ensure_async()
        try:
            async with client.stream(
                "POST", url, headers=headers, json=body
            ) as resp:
                if resp.status_code >= 400:
                    raw = await resp.aread()
                    raise _translate_status_error(
                        resp.status_code, raw.decode("utf-8", "replace")
                    )
                parser = SSEParser()
                async for chunk in resp.aiter_text():
                    if not chunk:
                        continue
                    for frame in _iter_sse_chunks(chunk, parser):
                        yield frame
                for tail_event in parser.flush():
                    tail_chunk = _sse_event_to_chunk(tail_event)
                    if tail_chunk is not None:
                        yield tail_chunk
        except httpx.HTTPError as exc:
            raise _translate_transport_error(exc) from exc

    # ---- info / health ------------------------------------------------

    def info(self) -> dict[str, Any]:
        """Bridge server info — ``GET /v1/info``.

        Returns the bridge's reported provider / model / version / etc.
        as a plain dict. Useful as a connectivity probe.
        """
        result = self._request_sync("GET", "/v1/info")
        return result if isinstance(result, dict) else {}

    async def ainfo(self) -> dict[str, Any]:
        """Async mirror of :meth:`info`."""
        result = await self._request_async("GET", "/v1/info")
        return result if isinstance(result, dict) else {}

    # ---- internal helpers used by sub-APIs ----------------------------

    def _sync_get(self, path: str, params: dict[str, Any] | None = None) -> Any:
        return self._request_sync("GET", path, params=params)

    def _sync_post(self, path: str, body: Any = None) -> Any:
        return self._request_sync("POST", path, json_body=body)

    def _async_get(
        self, path: str, params: dict[str, Any] | None = None
    ) -> Awaitable[Any]:
        return self._request_async("GET", path, params=params)

    def _async_post(self, path: str, body: Any = None) -> Awaitable[Any]:
        return self._request_async("POST", path, json_body=body)
