"""Tracks the pymobiledevice3 `tunneld` process lifecycle."""

from __future__ import annotations

import logging
import socket
import time
from typing import Optional

from .supervisor import ProcessSpec, Supervisor

log = logging.getLogger(__name__)


class Tunneld:
    def __init__(self, port: int = 49151):
        self.port = port
        self._supervisor: Optional[Supervisor] = None
        self._running = False

    @staticmethod
    def _port_open(port: int) -> bool:
        with socket.socket() as s:
            s.settimeout(0.5)
            return s.connect_ex(("127.0.0.1", port)) == 0

    def ensure_started(self, sup: Supervisor) -> None:
        if self._port_open(self.port):
            log.info("tunneld already listening on :%d", self.port)
            return
        sup.add(ProcessSpec(
            name="tunneld",
            argv=["sudo", "python3", "-m", "pymobiledevice3", "remote", "tunneld"],
        ))
        sup.start_all()
        self._supervisor = sup
        self._running = True

    def wait_ready(self, timeout: float = 120.0) -> None:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self._port_open(self.port):
                log.info("tunneld ready on :%d", self.port)
                return
            time.sleep(0.4)
        raise TimeoutError("tunneld never came up")
