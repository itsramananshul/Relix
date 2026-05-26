// PH-BROWSER-PW — Playwright sidecar (Node.js).
//
// Relix spawns this script via `node -` (script piped on stdin)
// or `node <path>`; the Rust side prefers `node -` with the
// script body fed in. The Rust driver communicates over stdio
// using newline-delimited JSON-RPC:
//
//   request:  {"id": <u64>, "method": "<name>", "params": {...}}
//   response: {"id": <u64>, "result": <json>}
//        or:  {"id": <u64>, "error": {"code": <i32>, "message": "..."}}
//
// Methods:
//   ping                  -> { "pong": true }
//   browser.launch        -> { } (idempotent; reuses existing browser)
//   context.newPage       -> { "guid": "<page-id>" }
//   page.goto             -> { "url": "<final-url>" }
//   page.innerText        -> { "text": "<body innerText>" }
//   page.screenshot       -> { "pngBase64": "<base64 png>" }
//   page.close            -> { }
//   browser.close         -> { }
//
// Honesty contract: on ANY uncaught failure (playwright-core not
// installed, page closed unexpectedly, navigation timeout, etc.)
// the sidecar emits {error: {code, message}} and keeps running.
// If require('playwright-core') itself throws, the sidecar exits
// with code 2 so the parent's spawn-time read returns EOF and
// the Rust driver maps it to BackendNotConnected.
//
// The script exits cleanly when stdin closes (parent dropped us).

'use strict';

const readline = require('readline');

// Resolve playwright-core. Errors here are fatal — emit a single
// startup error line then exit. The Rust side reads the first
// line; if it's a startup-error envelope it surfaces as
// BackendNotConnected with the message verbatim.
let chromium;
try {
  ({ chromium } = require('playwright-core'));
} catch (e) {
  // Emit a structured startup error and bail. Use id=0 — the
  // Rust driver treats id=0 + error as a startup failure.
  process.stdout.write(
    JSON.stringify({
      id: 0,
      error: {
        code: -32001,
        message: 'playwright-core require() failed: ' + (e && e.message ? e.message : String(e)),
      },
    }) + '\n',
  );
  process.exit(2);
}

// Lazy-launched browser; reused across context.newPage calls.
let browser = null;
// guid -> Page handle. Guids are stringified counters, scoped
// to this sidecar process — they're returned to Rust as opaque.
const pages = new Map();
let nextGuid = 1;

function send(id, result, error) {
  const msg = error ? { id, error } : { id, result };
  process.stdout.write(JSON.stringify(msg) + '\n');
}

function sendError(id, message, code) {
  send(id, undefined, { code: code || -32603, message: String(message) });
}

async function handle(req) {
  const { id, method, params } = req;
  switch (method) {
    case 'ping':
      return send(id, { pong: true });

    case 'browser.launch': {
      if (browser) return send(id, {});
      // Headless, no extra args. Operators that want Firefox /
      // WebKit will get a follow-up milestone; for PH-BROWSER-PW
      // we ship Chromium-only to match the smallest install.
      browser = await chromium.launch({ headless: true });
      return send(id, {});
    }

    case 'context.newPage': {
      if (!browser) {
        return sendError(id, 'browser not launched; call browser.launch first');
      }
      const ctx = await browser.newContext();
      const page = await ctx.newPage();
      const guid = String(nextGuid++);
      pages.set(guid, page);
      return send(id, { guid });
    }

    case 'page.goto': {
      const page = pages.get(params && params.guid);
      if (!page) return sendError(id, 'page guid not found: ' + (params && params.guid));
      const url = String(params.url || '');
      const timeout = Number(params.timeout) || 30000;
      const resp = await page.goto(url, { timeout, waitUntil: 'load' });
      return send(id, { url: resp ? resp.url() : url });
    }

    case 'page.innerText': {
      const page = pages.get(params && params.guid);
      if (!page) return sendError(id, 'page guid not found: ' + (params && params.guid));
      const selector = (params && params.selector) || 'body';
      const text = await page.innerText(selector);
      return send(id, { text });
    }

    case 'page.screenshot': {
      const page = pages.get(params && params.guid);
      if (!page) return sendError(id, 'page guid not found: ' + (params && params.guid));
      const fullPage = !!(params && params.fullPage);
      const buf = await page.screenshot({ fullPage, type: 'png' });
      return send(id, { pngBase64: buf.toString('base64') });
    }

    // W2-002a / F12: page.click — dispatches a click on the
    // first element matching the CSS selector. Playwright's
    // locator API auto-waits for the element to be visible
    // and actionable before clicking; the `timeout` param
    // bounds how long it'll wait.
    case 'page.click': {
      const page = pages.get(params && params.guid);
      if (!page) return sendError(id, 'page guid not found: ' + (params && params.guid));
      const selector = String(params && params.selector || '');
      if (!selector) return sendError(id, 'page.click: selector required');
      const timeout = Number(params && params.timeout) || 30000;
      await page.click(selector, { timeout });
      return send(id, {});
    }

    // W2-002a / F12: page.type_text — focuses the matched
    // element (Playwright's `fill` clears + sets the value)
    // and writes the text. `fill` is the right primitive for
    // form inputs; `type` exists but dispatches per-key
    // events which is slower and surprises operators who
    // expect setting a value.
    case 'page.type_text': {
      const page = pages.get(params && params.guid);
      if (!page) return sendError(id, 'page guid not found: ' + (params && params.guid));
      const selector = String(params && params.selector || '');
      if (!selector) return sendError(id, 'page.type_text: selector required');
      const text = String(params && params.text || '');
      const timeout = Number(params && params.timeout) || 30000;
      await page.fill(selector, text, { timeout });
      return send(id, {});
    }

    // W2-002a / F12: page.wait_for_selector — block until
    // the selector appears in the DOM or the timeout elapses.
    // The `state: 'visible'` default makes "wait for the
    // user-visible element" the same as the trait's
    // operator-facing semantics.
    case 'page.wait_for_selector': {
      const page = pages.get(params && params.guid);
      if (!page) return sendError(id, 'page guid not found: ' + (params && params.guid));
      const selector = String(params && params.selector || '');
      if (!selector) return sendError(id, 'page.wait_for_selector: selector required');
      const timeout = Number(params && params.timeout) || 30000;
      await page.waitForSelector(selector, { state: 'visible', timeout });
      return send(id, {});
    }

    case 'page.close': {
      const page = pages.get(params && params.guid);
      if (!page) return sendError(id, 'page guid not found: ' + (params && params.guid));
      // Close the page AND its owning context — each newPage above
      // builds a fresh context, so closing it here releases the
      // associated cookies / storage. Browser process stays up.
      try {
        const ctx = page.context();
        await page.close();
        await ctx.close();
      } catch (e) {
        // Already closed — ok.
      }
      pages.delete(params.guid);
      return send(id, {});
    }

    case 'browser.close': {
      if (browser) {
        try { await browser.close(); } catch (e) {}
        browser = null;
        pages.clear();
      }
      return send(id, {});
    }

    default:
      return sendError(id, 'unknown method: ' + method, -32601);
  }
}

const rl = readline.createInterface({ input: process.stdin });

rl.on('line', (line) => {
  if (!line) return;
  let req;
  try {
    req = JSON.parse(line);
  } catch (e) {
    return sendError(0, 'json parse: ' + e.message, -32700);
  }
  // Each request runs to completion before the next line is
  // processed by handle(); readline buffers further lines.
  handle(req).catch((e) => sendError(req && req.id, e && e.stack ? e.stack : String(e)));
});

rl.on('close', async () => {
  // Parent dropped stdin — tidy up and exit.
  try { if (browser) await browser.close(); } catch (e) {}
  process.exit(0);
});

// Unhandled-rejection / uncaught-exception belt-and-braces. Emit
// to stderr so the Rust side can correlate if it's draining; do
// NOT exit — the parent decides lifecycle.
process.on('unhandledRejection', (r) => {
  process.stderr.write('[sidecar] unhandledRejection: ' + (r && r.stack ? r.stack : String(r)) + '\n');
});
process.on('uncaughtException', (e) => {
  process.stderr.write('[sidecar] uncaughtException: ' + (e && e.stack ? e.stack : String(e)) + '\n');
});
