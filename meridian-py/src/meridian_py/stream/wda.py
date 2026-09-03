"""WDA client — session management with auto-recovery."""

from __future__ import annotations

import logging
import threading
from typing import Optional

log = logging.getLogger(__name__)

try:
    import requests
    HAS_REQUESTS = True
except ImportError:
    HAS_REQUESTS = False


class WdaError(Exception):
    pass


class WdaClient:
    def __init__(self, base: str = "http://127.0.0.1:8100"):
        if not HAS_REQUESTS:
            raise WdaError("requests not installed")
        self.base = base.rstrip("/")
        self._http = requests.Session()
        self._lock = threading.Lock()
        self._session_id: Optional[str] = None

    def alive(self) -> bool:
        try:
            r = self._http.get(f"{self.base}/status", timeout=2)
            return r.status_code < 500
        except Exception:
            return False

    def session_id(self) -> str:
        with self._lock:
            if self._session_id:
                return self._session_id
            r = self._http.post(
                f"{self.base}/session",
                json={"capabilities": {}},
                timeout=8,
            )
            r.raise_for_status()
            sid = r.json()["value"]["sessionId"]
            self._session_id = sid
            # WDA: do not wait for app idle before resuming from gestures.
            try:
                self._http.post(
                    f"{self.base}/session/{sid}/appium/settings",
                    json={"settings": {"waitForIdleTimeout": 0}},
                    timeout=6,
                )
            except Exception:
                pass
            log.info("WDA session: %s", sid)
            return sid

    def invalidate(self) -> None:
        with self._lock:
            self._session_id = None

    def press_key(self, key: str) -> None:
        sid = self.session_id()
        try:
            r = self._http.post(
                f"{self.base}/session/{sid}/wda/pressKey",
                json={"key": key},
                timeout=8,
            )
            r.raise_for_status()
        except requests.exceptions.HTTPError:
            self.invalidate()
            r = self._http.post(
                f"{self.base}/session/{self.session_id()}/wda/pressKey",
                json={"key": key},
                timeout=8,
            )
            r.raise_for_status()

    def type_text(self, text: str) -> None:
        sid = self.session_id()
        r = self._post(f"/session/{sid}/wda/keys", {"value": [text]})
        r.raise_for_status()

    def send_keys(self, value: list) -> None:
        sid = self.session_id()
        r = self._post(f"/session/{sid}/wda/keys", {"value": value})
        r.raise_for_status()

    def _post(self, path: str, payload: dict):
        try:
            return self._http.post(f"{self.base}{path}", json=payload, timeout=8)
        except requests.exceptions.ConnectionError:
            # Try recovering once.
            self.invalidate()
            self.session_id()  # refresh
            return self._http.post(f"{self.base}{path}", json=payload, timeout=8)
