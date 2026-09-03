"""Hotplug watcher — drives the registry by listening to Attach/Detach events."""

from __future__ import annotations

import logging
import threading
import time
from typing import Callable, Optional

from ..mux import MuxClient

log = logging.getLogger(__name__)

EventCallback = Callable[[str, dict], None]   # (kind, payload)


class Watcher:
    """Long-lived listener on the mux. Delivers attach/detach events."""

    def __init__(self, mux: MuxClient):
        self.mux = mux
        self._cb: list[EventCallback] = []
        self._thread: Optional[threading.Thread] = None
        self._stop = threading.Event()
        self._running = False

    def add_listener(self, cb: EventCallback) -> None:
        self._cb.append(cb)

    def start(self) -> None:
        if self._running:
            return
        self._stop.clear()
        self._running = True
        self._thread = threading.Thread(target=self._run, name="meridian-watcher", daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=2)
        self._running = False

    def _emit(self, kind: str, payload: dict) -> None:
        for cb in self._cb:
            try:
                cb(kind, payload)
            except Exception as e:
                log.warning("watcher: listener raised: %s", e)

    def _run(self) -> None:
        backoff = 0.1
        while not self._stop.is_set():
            try:
                sock = self.mux.listen()
                log.info("watcher: listening for attach/detach events")
                backoff = 0.1
                while not self._stop.is_set():
                    try:
                        msg = self.mux._recv(sock)
                    except (ConnectionError, OSError) as e:
                        log.debug("watcher socket ended: %s", e)
                        break
                    self._handle_plist(msg)
                try:
                    sock.close()
                except OSError:
                    pass
            except Exception as e:
                log.warning("watcher connect failed: %s (retrying in %.1fs)", e, backoff)
                time.sleep(backoff)
                backoff = min(backoff * 2, 5.0)
        log.info("watcher: stopped")

    def _handle_plist(self, msg: dict) -> None:
        kind = msg.get("MessageType")
        if kind == "Attached":
            props = msg.get("Properties", {})
            payload = {
                "udid": props.get("SerialNumber"),
                "device_id": props.get("DeviceID", 0),
                "product_id": props.get("ProductID", 0),
                "connection_type": props.get("ConnectionType", "USB"),
            }
            self._emit("attach", payload)
        elif kind == "Detached":
            payload = {
                "udid": msg.get("SerialNumber") or "",
                "device_id": msg.get("DeviceID", 0),
            }
            self._emit("detach", payload)
