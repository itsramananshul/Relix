# Browser tool (CW4 / PH-BROWSER-FEATURES)

`tool.browser.*` ships the **capability surface** for browser
automation behind a pluggable `BrowserBackend` trait. As of
PH-BROWSER-FEATURES the runtime is structured for three live
backends — `headless_chrome`, `playwright`, and `webdriver` —
each gated on a Cargo feature. Their implementations land in
follow-up milestones (PH-BROWSER-HC / -PW / -WD). Until the
respective live driver lands, each feature-stub returns a
labeled scaffold that surfaces the operator's chosen backend
name in `list_sessions` / dashboard but refuses every
navigate / get_text / screenshot call with a
`BackendNotConnected` error whose reason explicitly names the
upcoming milestone tag. The wire format, session model,
capability descriptors, manifest advertisement, and dispatch
path are stable across the scaffold→live transition.

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

The operator CANNOT today (with `backend = "none"`):

- Navigate (`tool.browser.navigate`) — returns
  `BackendNotConnected`.
- Read page text (`tool.browser.get_text`) — same.
- Screenshot (`tool.browser.screenshot`) — same.
- Click (`tool.browser.click`) — same (W2-002a).
- Type text (`tool.browser.type_text`) — same (W2-002a).
- Wait for selector (`tool.browser.wait_for_selector`) — same (W2-002a).
- Read failure screenshots (`tool.browser.capture_read`) — requires
  `screenshot_on_failure_dir` to be configured and a live backend that
  produces failure PNGs (W2-002f).

## Config

```toml
[tool.browser]
# One of: "none" | "headless_chrome" | "playwright" | "webdriver"
# - "none" (default) wires the surface but returns
#   BackendNotConnected on every navigate / get_text /
#   screenshot / click / type_text / wait_for_selector.
# - "headless_chrome" requires --features browser-headless-chrome.
#   PH-BROWSER-FEATURES ships a scaffold; PH-BROWSER-HC will
#   land the live Chrome DevTools Protocol driver.
# - "playwright" requires --features browser-playwright.
#   PH-BROWSER-PW will land the live Playwright sidecar driver.
# - "webdriver" requires --features browser-webdriver.
#   PH-BROWSER-WD will land the live fantoccini / WebDriver
#   driver.
backend = "none"
# Per-node cap on live sessions. Enforced by every backend
# (including scaffolds) — protects future real backends from
# runaway allocation.
max_sessions = 16
# Per-call deadline (seconds). Returned in error envelopes
# even when the scaffold has nothing to time out yet.
call_timeout_secs = 30
# W2-002: URL of the operator-supplied WebDriver daemon
# (chromedriver / geckodriver). Only used when backend = "webdriver".
# Default: chromedriver's standard port.
webdriver_url = "http://127.0.0.1:9515"
# W2-002c: optional directory where the backend persists a PNG
# screenshot every time navigate / click / type_text fails on a
# live tab. Required for tool.browser.capture_read to work.
# The directory must already exist; it is NOT created automatically.
# screenshot_on_failure_dir = "/tmp/relix-screenshots"
```

Selecting a backend whose feature flag isn't compiled into
this Relix build is a **loud startup error** —
`ToolBackend::new` returns `ToolError::Build` and the tool node
fails to construct. There is **no silent fallback** to
`NoneBackend`: the operator's intent ("I chose
`headless_chrome`") is not quietly overridden.

When the `[tool.browser]` section is absent the capability
family is NOT registered (operators see no `tool.browser.*`
methods in `relix-cli capability ls`).

## Building with backends enabled

```bash
# Default — only the "none" backend is available.
cargo build -p relix-controller

# Compile a single backend in.
cargo build -p relix-controller --features relix-runtime/browser-headless-chrome
cargo build -p relix-controller --features relix-runtime/browser-playwright
cargo build -p relix-controller --features relix-runtime/browser-webdriver

# All three at once (handy for the dev test matrix).
cargo build -p relix-controller --features relix-runtime/browser-all
```

The flags are additive — multiple backends can be compiled in
the same binary, with the active one selected at runtime via
`[tool.browser] backend = "..."`. Operators on machines with
only one runtime installed should compile only the matching
feature to keep the binary small.

## Recommended default (D-008)

`headless_chrome` is the recommended default for operators
who don't want to install extra runtimes — no Node, no
sidecar driver process, no npm package. Just a `chrome` /
`chromium` binary that's already on most operator machines.
Operators who need multi-engine coverage (Firefox / WebKit)
should pick `playwright`; operators who want W3C-standards
alignment with their existing automation should pick
`webdriver`.

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

- **PH-BROWSER-HC**: live Chrome DevTools Protocol driver
  behind `--features browser-headless-chrome`. Replaces the
  scaffold in `browser/headless_chrome.rs` with a real
  driver against the operator's `chrome` / `chromium`
  binary. Recommended default per D-008.
- **PH-BROWSER-PW**: live Playwright sidecar driver behind
  `--features browser-playwright`. Best multi-engine
  coverage; heaviest install.
- **PH-BROWSER-WD**: live fantoccini / WebDriver driver
  behind `--features browser-webdriver`. Most W3C-y;
  requires operator-supplied driver binary.
- **PH-BROWSER-D008-RESOLVE**: flip D-008 from "open" to
  "shipped all three behind features" once the live drivers
  land.
- **PH-BROWSER-DASH**: dashboard browser-session inspector —
  live page title, current URL, last screenshot thumbnail
  (post-real-backend).
- **PH-BROWSER-CHRONICLE**: chronicle event for every
  navigate (post-real-backend).
- **PH-BROWSER-CANCEL**: cooperative cancel via the existing
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

### `tool.browser.capture_read` security

Reads a failure-screenshot PNG from `screenshot_on_failure_dir` by
filename. The handler enforces:

- Filename must end with `.png`.
- Filename must not contain `/`, `\`, `..`, `\0`, or `:`.
- Maximum filename length: 256 characters.
- After joining with `screenshot_on_failure_dir`, the resulting path is
  canonicalized and checked to ensure it remains inside the configured
  directory (escape check).

### `tool.browser.type_text` — credential-safe tracing

`type_text` logs the **character count** of the typed text, not the text
content itself. This ensures that credentials, passwords, or other
sensitive strings typed into browser form fields are not captured in
tracing output or the audit log.
