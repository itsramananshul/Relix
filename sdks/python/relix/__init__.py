"""Python SDK for the Relix AI agent mesh platform.

Wraps the Relix web bridge's HTTP surface in a typed, ergonomic client.
The bridge is the wire contract — this package never reaches into the
mesh directly, so a Relix deployment of any size (single embedded node or
a multi-machine mesh) presents the same API to a Python caller.

Basic usage::

    from relix import RelixClient

    with RelixClient(bridge_url="http://localhost:19791", api_key="...") as c:
        reply = c.chat(session_id="user-123", message="hello")
        print(reply.text)

See the README for the full surface; every method has both a sync
(`chat`) and async (`achat`) form.
"""

from .client import (
    ChatResponse,
    ChatUsage,
    RelixClient,
    SSEParser,
    StreamChunk,
)
from .credentials import (
    CredentialAuditEntry,
    CredentialMetadata,
    CredentialsAPI,
)
from .exceptions import (
    RelixAuthError,
    RelixConnectionError,
    RelixError,
    RelixResponseError,
    RelixTimeoutError,
)
from .identity import (
    IdentityAPI,
    IdentityProfile,
    ResearchResult,
)
from .memory import (
    DialecticAnswer,
    FlushContextResult,
    IngestDocumentResult,
    MemoryResult,
)
from .observability import (
    AgentHealth,
    Alert,
    HealthSummary,
)
from .planning import (
    AgentDescriptor,
    PlanResult,
)
from .skills import Skill, SkillStats

__version__ = "0.1.0"

__all__ = [
    "AgentDescriptor",
    "AgentHealth",
    "Alert",
    "ChatResponse",
    "ChatUsage",
    "CredentialAuditEntry",
    "CredentialMetadata",
    "CredentialsAPI",
    "DialecticAnswer",
    "FlushContextResult",
    "HealthSummary",
    "IdentityAPI",
    "IdentityProfile",
    "IngestDocumentResult",
    "MemoryResult",
    "PlanResult",
    "RelixAuthError",
    "RelixClient",
    "RelixConnectionError",
    "RelixError",
    "RelixResponseError",
    "RelixTimeoutError",
    "ResearchResult",
    "SSEParser",
    "Skill",
    "SkillStats",
    "StreamChunk",
    "__version__",
]
