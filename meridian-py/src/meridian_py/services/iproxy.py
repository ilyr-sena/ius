"""iproxy subprocess management.

One tunnel per (host, device) port pair, supervised. (When the daemon is
meridian-relay, the same topology works through its socket; iproxy protocol
is usbmuxd-compatible.)
"""

from __future__ import annotations

import logging
from typing import Iterable

from .supervisor import ProcessSpec, Supervisor

log = logging.getLogger(__name__)


class IProxy:
    def __init__(self, ports: Iterable[tuple[int, int]]):
        self.ports = list(ports)

    def start_all(self, sup: Supervisor) -> None:
        for lp, dp in self.ports:
            sup.add(ProcessSpec(
                name=f"iproxy {lp}→{dp}",
                argv=["iproxy", str(lp), str(dp)],
            ))

    def stop(self, sup: Supervisor) -> None:
        sup.stop_all()
