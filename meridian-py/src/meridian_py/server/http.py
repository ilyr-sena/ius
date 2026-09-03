"""HTTP API server: device registry, WDA state, app launches, icons.

Replaces the commands-server part of ius.py's main: same routes, cleaned up.
"""

from __future__ import annotations

import json
import logging
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Callable, Optional

log = logging.getLogger(__name__)


class SafeHandler(BaseHTTPRequestHandler):
    """Standard JSON request handler with saner error reporting."""

    server_version = "MeridianPy/1.0"

    def _json(self, code: int, payload: dict) -> None:
        body = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_a, **_kw):
        # Quiet the default request logging.
        pass

    def do_GET(self):
        self._route("GET")

    def do_POST(self):
        self._route("POST")

    def _route(self, method: str) -> None:
        server: "HTTPServer" = self.server  # type: ignore
        path = self.path.split("?")[0]

        try:
            if method == "GET" and path == "/devices":
                self._json(200, {"devices": server.devices_snapshot()})
            elif method == "GET" and path == "/devices/attached":
                self._json(200, {"devices": server.attached_snapshot()})
            elif method == "GET" and path == "/status":
                self._json(200, {"ok": True, "pid": None})
            elif method == "GET" and path == "/apps.json":
                body = server.apps_jso()
                self._json(200, body)
            elif method == "POST" and path.startswith("/app/launch/"):
                bid = path[len("/app/launch/"):]
                try:
                    server.launch_app(bid)
                    self._json(200, {"ok": True, "bundleId": bid})
                except Exception as e:
                    self._json(502, {"ok": False, "error": str(e)})
            else:
                self._json(404, {"error": f"unknown route {method} {path}"})
        except Exception as e:
            log.exception("route %s %s failed", method, path)
            self._json(500, {"error": str(e)})


class HTTPServer:
    """A thin wrapper. Required business-logic handlers are injected."""

    def __init__(
        self,
        port: int,
        devices_snapshot_fn: Optional[Callable[[], list]] = None,
        attached_snapshot_fn: Optional[Callable[[], list]] = None,
        apps_json_fn: Optional[Callable[[], dict]] = None,
        launch_app_fn: Optional[Callable[[str], Any]] = None,
    ):
        self.port = port
        self._devices_snapshot_fn = devices_snapshot_fn or (lambda: [])
        self._attached_snapshot_fn = attached_snapshot_fn or (lambda: [])
        self._apps_json_fn = apps_json_fn or (lambda: {})
        self._launch_app_fn = launch_app_fn or (lambda bid: None)

        self._server: Optional[ThreadingHTTPServer] = None
        self._thread: Optional[threading.Thread] = None

    def devices_snapshot(self) -> list:
        return self._devices_snapshot_fn()

    def attached_snapshot(self) -> list:
        return self._attached_snapshot_fn()

    def apps_jso(self) -> dict:
        return self._apps_json_fn()

    def launch_app(self, bid: str) -> None:
        return self._launch_app_fn(bid)

    def start(self) -> None:
        self._server = ThreadingHTTPServer(("127.0.0.1", self.port), SafeHandler)
        self._thread = threading.Thread(
            target=self._server.serve_forever,
            name="meridian-http",
            daemon=True,
        )
        self._thread.start()
        log.info("HTTP :%d (registry API)", self.port)

    def stop(self) -> None:
        if self._server:
            self._server.shutdown()
            self._server.server_close()
            self._server = None
