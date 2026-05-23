#!/usr/bin/env python3
"""hello-plugin: minimal Python implementation of the relix-plugin-v1
protocol. No third-party deps — uses only the stdlib http.server
module so a freshly-installed Python 3 can run it without pip.

What the protocol requires of a plugin:
  1. On startup, bind a random free port and write
     `RELIX_PLUGIN_PORT=<port>` to stdout on its FIRST line.
  2. Serve `GET /health`  → `{"ok": true}`.
  3. Serve `GET /ready`   → `{"ok": true}` once warm.
  4. Serve `POST /invoke` → `{"ok": true, "body": "<reply>"}` on
     success, or `{"ok": false, "error_kind": <u32>,
     "error_cause": "<msg>"}` on failure.

Capabilities exposed:
  hello.greet — args="<name>" → "Hello, <name>! From the Relix plugin system."
"""
from __future__ import annotations

import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

# Stable error kinds — mirror the subset the SDK + host care about.
INVALID_ARGS = 5
UNKNOWN_METHOD = 4
RESPONDER_INTERNAL = 11


def handle_invoke(req: dict) -> dict:
    method = req.get("method", "")
    args = req.get("args", "")
    if method == "hello.greet":
        name = args.strip()
        if not name:
            return {
                "ok": False,
                "error_kind": INVALID_ARGS,
                "error_cause": "hello.greet: name required",
            }
        return {
            "ok": True,
            "body": f"Hello, {name}! From the Relix plugin system.",
        }
    return {
        "ok": False,
        "error_kind": UNKNOWN_METHOD,
        "error_cause": f"unknown method: {method}",
    }


class Handler(BaseHTTPRequestHandler):
    # Silence the default access log — host already captures
    # stderr via tracing.
    def log_message(self, format: str, *args: object) -> None:
        return

    def _reply_json(self, status: int, body: dict) -> None:
        raw = json.dumps(body).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self) -> None:
        if self.path == "/health" or self.path == "/ready":
            self._reply_json(200, {"ok": True})
        else:
            self._reply_json(404, {"ok": False, "error_cause": "not found"})

    def do_POST(self) -> None:
        if self.path != "/invoke":
            self._reply_json(404, {"ok": False, "error_cause": "not found"})
            return
        length = int(self.headers.get("Content-Length", "0"))
        try:
            raw = self.rfile.read(length) if length > 0 else b"{}"
            req = json.loads(raw.decode("utf-8"))
        except (ValueError, UnicodeDecodeError) as e:
            self._reply_json(
                200,
                {
                    "ok": False,
                    "error_kind": INVALID_ARGS,
                    "error_cause": f"bad request body: {e}",
                },
            )
            return
        try:
            self._reply_json(200, handle_invoke(req))
        except Exception as e:  # noqa: BLE001 — catch-all is the plugin contract
            self._reply_json(
                200,
                {
                    "ok": False,
                    "error_kind": RESPONDER_INTERNAL,
                    "error_cause": f"hello-plugin: {e}",
                },
            )


def main() -> None:
    # Bind to 127.0.0.1:0 — kernel picks a free port.
    server = HTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_address[1]
    # Spec requires this be the FIRST line on stdout.
    print(f"RELIX_PLUGIN_PORT={port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(0)
