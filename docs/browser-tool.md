# Browser tool (CW4)

`tool.browser.*` ships the **capability surface** for browser
automation. As of this milestone there is **no live browser
backend** wired — every navigate / get_text / screenshot call
returns a typed `BackendNotConnected` error. The wire format,
session model, capability descriptors, manifest advertisement,
and dispatch path are all real. A future milestone wires a
real Playwright (or CDP) backend behind the same trait.

## Honesty contract

> If no actual browser backend exists yet, do NOT fake browser
> execution. Create real contracts and explicit backend-missing
> errors. No mock success.

Concrete: the operator can:

- Open a session (`tool.browser.open_session`) — returns a
  16-hex session id. Session is tracked in-memory.
- List sessions (`tool.browser.list_sessions`) — each row's
  `status` field reads `unconnected`.
- Close a session (`tool.browser.close_session`).

The operator CANNOT today:

- Navigate (`tool.browser.navigate`) — returns
  `BackendNotConnected` with the reason "operator selected
  backend=\"none\" — capability surface is wired but no real
  browser backend ships in this Relix build yet".
- Read page text (`tool.browser.get_text`) — same.
- Screenshot (`tool.browser.screenshot`) — same.

## Config

```toml
[tool.browser]
# "none" (default) wires the surface but returns
# BackendNotConnected on every navigate/get_text/screenshot.
# "playwright" is reserved for a future milestone — selecting
# it today returns BackendNotConnected with a pointed reason
# explaining the integration is pending.
backend = "none"
# Per-node cap on live sessions. Protects future real backends
# from runaway allocation.
max_sessions = 16
# Per-call deadline (seconds). Returned in error envelopes
# even though no real call ever times out today.
call_timeout_secs = 30
```

When the `[tool.browser]` section is absent the capability
family is NOT registered (operators see no `tool.browser.*`
methods in `relix-cli capability ls`).

## Why ship the surface before the backend?

1. **Visibility**: operators reading the dashboard or `capability ls`
   see what's *intended* to ship, not just what's *live*. The
   `BackendNotConnected` reason explains the gap precisely.
2. **Stable contract**: the wire format + descriptors are
   pinned now. Future Playwright work slots into the
   `BrowserBackend` trait without touching the dispatch path
   or operator-facing UX.
3. **Honesty over fake-success**: a mock backend that returned
   "navigated to https://example.com" would mislead operators
   reading the chronicle. Returning `BackendNotConnected` makes
   the gap impossible to miss.

## Future milestones

- **CW4-A**: Playwright backend. Pure backend swap behind the
  existing `BrowserBackend` trait. Adds tab management,
  click/type/scroll primitives, screenshot persistence.
- **CW4-B**: dashboard browser-session inspector — live page
  title, current URL, last screenshot thumbnail.
- **CW4-C**: chronicle event for every navigate (post-real-
  backend).
- **CW4-D**: cooperative cancel via the existing
  `task.pause` / `task.freeze` semantics.

## Security model (forward-looking)

When a real backend lands the existing capability sensitivity
tags will gate access:

- `browser:session` — any browser surface use.
- `external:network` + `egress:http` — navigate (mirrors
  `tool.web_fetch`'s SSRF posture).
- `binary:image` — screenshot output.
- `requires_groups: ["operators"]` — by default not exposed
  to `chat-users`.

The SSRF guard from `tool.web_fetch` (in `security.rs`) is the
right pattern to reuse for navigate: validate the URL up-front
+ refuse private-network targets unless the operator
explicitly opts in via a future `[tool.browser] allow_private`
toggle.
