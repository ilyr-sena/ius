"""Iproxy-replacement TCP tunnels over the mux.

Binds a local TCP listener per (local, device) port pair and splices each
incoming connection to the device. Drop-in replacement for `iproxy`.
"""

from __future__ import annotations

import logging
import socket
import threading
from typing import Optional

from .client import MuxClient

log = logging.getLogger(__name__)


class _Forwarder(threading.Thread):
    def __init__(self, upstream: socket.socket, downstream: socket.socket, name: str):
        super().__init__(daemon=True, name=name)
        self.up = upstream
        self.dn = downstream

    def run(self) -> None:
        try:
            while True:
                data = self.dn.recv(1 << 16)
                if not data:
                    break
                self.up.sendall(data)
        except OSError:
            pass
        finally:
            for s in (self.up, self.dn):
                try: s.shutdown(socket.SHUT_RDWR)
                except OSError: pass


class Tunnel:
    """Listens on a local port, spliced to a device port via the mux."""

    def __init__(self, mux: MuxClient, local_port: int, device_port: int, device_udid: Optional[str] = None):
        self.mux = mux
        self.local_port = local_port
        self.device_port = device_port
        self.device_udid = device_udid
        self._sock: Optional[socket.socket] = None
        self._accept_thread: Optional[threading.Thread] = None
        self._stop = threading.Event()
        self._children: list[threading.Thread] = []

    def start(self) -> None:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind(("127.0.0.1", self.local_port))
        s.listen(16)
        self._sock = s
        self._stop.clear()
        self._accept_thread = threading.Thread(
            target=self._accept_loop, name=f"mux-tunnel-{self.local_port}", daemon=True,
        )
        self._accept_thread.start()
        log.info("tunnel :%d → device port %d (via %s)", self.local_port, self.device_port, self.mux.endpoint)

    def stop(self) -> None:
        self._stop.set()
        if self._sock:
            try: self._sock.close()
            except OSError: pass

    def _accept_loop(self) -> None:
        while not self._stop.is_set():
            try:
                client, addr = self._sock.accept()
            except OSError:
                break
            threading.Thread(
                target=self._run_client, args=(client,),
                daemon=True, name=f"tunnel-cli-{addr}",
            ).start()

    def _resolve_device(self) -> int:
        # Resolve to a device ID on demand each time (devices can replug).
        devices = self.mux.list_devices()
        if not devices:
            raise RuntimeError("no devices attached to mux")
        if self.device_udid:
            for d in devices:
                if d.udid == self.device_udid:
                    return d.device_id
            raise RuntimeError(f"device {self.device_udid} not attached")
        return devices[0].device_id

    def _run_client(self, client: socket.socket) -> None:
        try:
            did = self._resolve_device()
            upstream = self.mux.connect(did, self.device_port)
        except Exception as e:
            log.warning("tunnel connect failed: %s", e)
            try: client.close()
            except OSError: pass
            return
        _Forwarder(upstream, client, f"tunnel-{self.local_port}").start()
