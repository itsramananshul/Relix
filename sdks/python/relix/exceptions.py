"""Exception hierarchy for the Relix SDK.

All errors raised by the client inherit from :class:`RelixError` so a
caller can write `except RelixError` once and catch any failure shape.
"""

from __future__ import annotations

from typing import Any


class RelixError(Exception):
    """Base class for every error raised by the Relix SDK.

    Attributes:
        message: Human-readable error description.
        status_code: HTTP status when the error originated from a bridge
            response; ``None`` for connection / decode failures.
        body: Raw response body (or other diagnostic payload) when
            available.
    """

    def __init__(
        self,
        message: str,
        *,
        status_code: int | None = None,
        body: Any = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.status_code = status_code
        self.body = body

    def __repr__(self) -> str:
        status = self.status_code if self.status_code is not None else "?"
        return f"{type(self).__name__}(status={status}, message={self.message!r})"


class RelixConnectionError(RelixError):
    """Raised when the bridge cannot be reached at all.

    Wraps DNS failures, TCP refusals, TLS handshake errors, and any other
    network-level failure surface by httpx. Callers should treat this as
    a transient infrastructure issue rather than a client bug.
    """


class RelixTimeoutError(RelixError):
    """Raised when a request exceeds the configured timeout."""


class RelixAuthError(RelixError):
    """Raised when the bridge rejects the call's bearer token.

    Maps to a 401 from the bridge. The remediation is operator-side:
    rotate the token at ``~/.relix/bridge-token`` and update the SDK
    config with the new value.
    """


class RelixResponseError(RelixError):
    """Raised for non-2xx responses other than 401.

    Use ``status_code`` to branch on the specific code; the bridge
    follows convention (400 invalid args, 403 forbidden, 404 unknown
    method / unknown tenant, 429 rate limited, 502 responder fault,
    503 mesh not initialised).
    """
