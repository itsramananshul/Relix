"""PART 4 — automatic sync↔async parity audit.

Walks every public method on :class:`RelixClient` and on each sub-API
class (`MemoryAPI`, `PlanningAPI`, `SkillsAPI`, `ObservabilityAPI`,
`CredentialsAPI`, `IdentityAPI`) and asserts that:

* every sync endpoint method has an `a`-prefixed async twin, AND
* every async method has the corresponding sync sibling
  (twin name = leading `a` stripped).

The source of truth for "is this method async" is
``inspect.iscoroutinefunction`` / ``inspect.isasyncgenfunction`` — not
the leading character of the name (some sync methods like ``agents``,
``audit``, ``alerts`` happen to start with `a`; their async twins
double the leading `a` to ``aagents`` / ``aaudit`` / ``aalerts``).
"""

from __future__ import annotations

import inspect

import pytest

from relix import RelixClient
from relix.credentials import CredentialsAPI
from relix.identity import IdentityAPI
from relix.memory import MemoryAPI
from relix.observability import ObservabilityAPI
from relix.planning import PlanningAPI
from relix.skills import SkillsAPI

# Public methods on the client itself that are NOT API endpoints and
# therefore have no async counterpart by design. `close` / `aclose`
# is the only paired non-endpoint method; the rest are pure
# property accessors or lifecycle hooks.
CLIENT_NON_ENDPOINT_METHODS: frozenset[str] = frozenset(
    {
        "close",
        "aclose",
        "__enter__",
        "__exit__",
        "__aenter__",
        "__aexit__",
    }
)


def _public_endpoint_methods(cls: type) -> list[tuple[str, object]]:
    """`(name, fn)` for every endpoint-style method defined on ``cls``.

    Excludes: private names, dunder names, property descriptors,
    inherited members. Sub-API property accessors on ``RelixClient``
    (memory / planning / ...) are descriptors, not methods, so they
    drop out via the ``isinstance(raw, property)`` check.
    """
    out: list[tuple[str, object]] = []
    for name in sorted(cls.__dict__):
        if name.startswith("_") and name not in {
            "__enter__",
            "__exit__",
            "__aenter__",
            "__aexit__",
        }:
            continue
        raw = cls.__dict__[name]
        if isinstance(raw, property):
            continue
        if not callable(raw):
            continue
        out.append((name, raw))
    return out


def _is_async(member: object) -> bool:
    return inspect.iscoroutinefunction(member) or inspect.isasyncgenfunction(member)


SUB_APIS = [
    RelixClient,
    MemoryAPI,
    PlanningAPI,
    SkillsAPI,
    ObservabilityAPI,
    CredentialsAPI,
    IdentityAPI,
]


@pytest.mark.parametrize("cls", SUB_APIS)
def test_every_sync_endpoint_has_an_async_twin(cls: type) -> None:
    """Every sync endpoint method must have an `a`-prefixed async
    twin. Lifecycle helpers on RelixClient are exempt via
    :data:`CLIENT_NON_ENDPOINT_METHODS`."""
    methods = dict(_public_endpoint_methods(cls))
    missing: list[str] = []
    for name, member in methods.items():
        if _is_async(member):
            continue
        if cls is RelixClient and name in CLIENT_NON_ENDPOINT_METHODS:
            continue
        async_twin = f"a{name}"
        if async_twin not in methods:
            missing.append(f"{cls.__name__}.{name} -> expected {async_twin}")
            continue
        twin = methods[async_twin]
        if not _is_async(twin):
            missing.append(
                f"{cls.__name__}.{async_twin} exists but is not async"
            )
    assert not missing, "sync endpoints without async twins:\n  " + "\n  ".join(missing)


@pytest.mark.parametrize("cls", SUB_APIS)
def test_every_async_endpoint_has_a_sync_sibling(cls: type) -> None:
    """Every async endpoint method must have the corresponding sync
    sibling (twin name = leading `a` stripped). Catches async
    additions that forget their sync counterpart."""
    methods = dict(_public_endpoint_methods(cls))
    missing: list[str] = []
    for name, member in methods.items():
        if not _is_async(member):
            continue
        if cls is RelixClient and name in CLIENT_NON_ENDPOINT_METHODS:
            continue
        # By convention, async methods are spelled `a<sync>`.
        if not name.startswith("a"):
            missing.append(
                f"{cls.__name__}.{name} is async but lacks the conventional "
                f"a-prefix"
            )
            continue
        sync_name = name[1:]
        if not sync_name:
            continue
        if sync_name not in methods:
            missing.append(f"{cls.__name__}.{name} -> expected {sync_name}")
    assert not missing, "async endpoints without sync siblings:\n  " + "\n  ".join(missing)
