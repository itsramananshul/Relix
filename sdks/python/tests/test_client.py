"""End-to-end tests for ``relix.RelixClient``.

Uses respx (httpx mock adapter) so every test runs in-process — no
actual bridge required. Each test exercises the wire contract the
bridge enforces and asserts the SDK decodes it correctly.
"""

from __future__ import annotations

import asyncio
import json

import httpx
import pytest
import respx

from relix import (
    ChatResponse,
    RelixAuthError,
    RelixClient,
    RelixConnectionError,
    RelixResponseError,
    StreamChunk,
)

BRIDGE = "http://relix-test.local"


# ---- sync chat ---------------------------------------------------------


@respx.mock
def test_chat_returns_typed_response_with_aliased_reply_field() -> None:
    route = respx.post(f"{BRIDGE}/chat").mock(
        return_value=httpx.Response(
            200,
            json={
                "reply": "hi there",
                "flow_id": "flow-1",
                "trace_id": "trace-1",
                "flow_log": "/tmp/log.txt",
                "task_id": "task-1",
                "workspace_lease_id": "lease-1",
                "workspace_path": "/work/acme",
            },
        )
    )
    with RelixClient(BRIDGE, api_key="tok") as c:
        resp = c.chat(
            session_id="u1",
            message="hello",
            workspace_lease_id="lease-1",
        )
    assert isinstance(resp, ChatResponse)
    assert resp.text == "hi there"
    assert resp.flow_id == "flow-1"
    assert resp.trace_id == "trace-1"
    assert resp.task_id == "task-1"
    assert resp.workspace_lease_id == "lease-1"
    assert resp.workspace_path == "/work/acme"
    sent = json.loads(route.calls.last.request.content)
    assert sent["workspace_lease_id"] == "lease-1"
    assert route.called


@respx.mock
def test_chat_sends_x_relix_tenant_and_bearer_headers() -> None:
    route = respx.post(f"{BRIDGE}/chat").mock(
        return_value=httpx.Response(
            200,
            json={"reply": "ok", "flow_id": "f", "trace_id": "t", "flow_log": ""},
        )
    )
    with RelixClient(BRIDGE, tenant_id="acme", api_key="my-token") as c:
        c.chat(session_id="u1", message="hi")
    req = route.calls.last.request
    assert req.headers["x-relix-tenant"] == "acme"
    assert req.headers["authorization"] == "Bearer my-token"


@respx.mock
def test_chat_omits_authorization_header_when_no_api_key() -> None:
    route = respx.post(f"{BRIDGE}/chat").mock(
        return_value=httpx.Response(
            200,
            json={"reply": "ok", "flow_id": "f", "trace_id": "t", "flow_log": ""},
        )
    )
    with RelixClient(BRIDGE) as c:
        c.chat(session_id="u1", message="hi")
    req = route.calls.last.request
    assert "authorization" not in {k.lower() for k in req.headers}
    assert req.headers["x-relix-tenant"] == "default"


@respx.mock
def test_chat_raises_auth_error_on_401() -> None:
    respx.post(f"{BRIDGE}/chat").mock(
        return_value=httpx.Response(401, text="bad token")
    )
    with RelixClient(BRIDGE, api_key="bad") as c, pytest.raises(RelixAuthError) as exc:
        c.chat(session_id="u1", message="hi")
    assert exc.value.status_code == 401
    assert exc.value.body == "bad token"


@respx.mock
def test_chat_raises_response_error_on_500_with_body() -> None:
    respx.post(f"{BRIDGE}/chat").mock(
        return_value=httpx.Response(500, text="boom")
    )
    with RelixClient(BRIDGE, api_key="t") as c, pytest.raises(RelixResponseError) as exc:
        c.chat(session_id="u1", message="hi")
    assert exc.value.status_code == 500
    assert "500" in str(exc.value)


@respx.mock
def test_chat_raises_connection_error_on_transport_failure() -> None:
    respx.post(f"{BRIDGE}/chat").mock(side_effect=httpx.ConnectError("refused"))
    with RelixClient(BRIDGE, api_key="t") as c, pytest.raises(RelixConnectionError):
        c.chat(session_id="u1", message="hi")


# ---- async chat --------------------------------------------------------


@respx.mock
async def test_achat_returns_typed_response() -> None:
    respx.post(f"{BRIDGE}/chat").mock(
        return_value=httpx.Response(
            200,
            json={"reply": "async hi", "flow_id": "f", "trace_id": "t", "flow_log": ""},
        )
    )
    async with RelixClient(BRIDGE, api_key="t") as c:
        resp = await c.achat(session_id="u1", message="hello")
    assert resp.text == "async hi"
    assert resp.flow_id == "f"


# ---- streaming ---------------------------------------------------------


def _sse_body(*chunks: str, with_done: bool = True) -> bytes:
    """Build a bridge-shape SSE body from a sequence of chunk strings.

    The bridge's `chat::chat_stream` emits `data: <raw text>` for
    `event: chunk` frames (NOT `data: {"chunk": "..."}`). This fixture
    matches the literal wire shape so tests exercise the real parser
    path instead of a JSON-wrapped fixture that masks the bug.
    """
    pieces: list[str] = []
    for c in chunks:
        pieces.append(f"event: chunk\ndata: {c}\n\n")
    if with_done:
        pieces.append(
            'event: done\ndata: {"flow_id": "f1", "trace_id": "t1", '
            '"flow_log": "/tmp/x", "task_id": "task-1", '
            '"workspace_lease_id": "lease-1", '
            '"workspace_path": "/work/acme"}\n\n'
        )
    return "".join(pieces).encode()


@respx.mock
def test_chat_stream_yields_chunks_and_terminal_done_frame() -> None:
    # Use chunks WITHOUT trailing/leading whitespace because the
    # PART 7 SSEParser uses `value.strip()` per the spec — leading or
    # trailing space INSIDE the data payload is normalised away. The
    # bridge's `split_utf8_into_chunks` never deliberately strands a
    # bare space at a chunk boundary, so this matches the real wire.
    respx.post(f"{BRIDGE}/chat/stream").mock(
        return_value=httpx.Response(
            200,
            headers={"content-type": "text/event-stream"},
            content=_sse_body("Hello", "world", "!"),
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        chunks: list[StreamChunk] = list(c.chat_stream(session_id="u1", message="hi"))
    text = "".join(ch.text for ch in chunks if not ch.done)
    assert text == "Helloworld!"
    done = [ch for ch in chunks if ch.done]
    assert len(done) == 1
    assert done[0].flow_id == "f1"
    assert done[0].task_id == "task-1"
    assert done[0].workspace_lease_id == "lease-1"
    assert done[0].workspace_path == "/work/acme"


@respx.mock
def test_chat_stream_handles_partial_byte_boundary_split_frames() -> None:
    """Two frames split mid-payload across two iter_text chunks must
    still parse correctly. respx returns the whole body in one chunk by
    default, so we test the SSEParser's buffer logic via a concatenated
    body and let httpx deliver it as one chunk to the SDK reader.
    """
    body = _sse_body("ab", "cd")
    respx.post(f"{BRIDGE}/chat/stream").mock(
        return_value=httpx.Response(
            200,
            headers={"content-type": "text/event-stream"},
            content=body,
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        text = "".join(
            ch.text
            for ch in c.chat_stream(session_id="u1", message="hi")
            if not ch.done
        )
    assert text == "abcd"


@respx.mock
def test_chat_stream_raises_on_non_2xx() -> None:
    respx.post(f"{BRIDGE}/chat/stream").mock(
        return_value=httpx.Response(401, text="nope")
    )
    with RelixClient(BRIDGE, api_key="t") as c, pytest.raises(RelixAuthError):
        list(c.chat_stream(session_id="u1", message="hi"))


@respx.mock
async def test_achat_stream_yields_chunks() -> None:
    respx.post(f"{BRIDGE}/chat/stream").mock(
        return_value=httpx.Response(
            200,
            headers={"content-type": "text/event-stream"},
            content=_sse_body("foo", "bar"),
        )
    )
    async with RelixClient(BRIDGE, api_key="t") as c:
        chunks: list[StreamChunk] = []
        async for ch in c.achat_stream(session_id="u1", message="hi"):
            chunks.append(ch)
    assert "".join(c.text for c in chunks if not c.done) == "foobar"


# ---- info --------------------------------------------------------------


@respx.mock
def test_info_returns_bridge_info_dict() -> None:
    respx.get(f"{BRIDGE}/v1/info").mock(
        return_value=httpx.Response(
            200,
            json={"system": "relix", "version": "0.1.0", "model": "mock"},
        )
    )
    with RelixClient(BRIDGE, api_key="t") as c:
        info = c.info()
    assert info["system"] == "relix"
    assert info["version"] == "0.1.0"


# ---- close idempotence -------------------------------------------------


def test_close_is_idempotent_when_called_before_use() -> None:
    c = RelixClient(BRIDGE)
    c.close()
    c.close()


def test_aclose_is_idempotent_when_called_before_use() -> None:
    c = RelixClient(BRIDGE)
    asyncio.run(c.aclose())
    asyncio.run(c.aclose())
