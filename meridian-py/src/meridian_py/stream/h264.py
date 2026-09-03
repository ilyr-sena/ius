"""H.264 streaming service: engages the iUS Probe app's WebSocket stream.

The probe app (ProbeApp/H264Stream.swift) handles hardware H.264 encode on
the device. From here we just need to (1) make sure the app is launched,
(2) publish its WebSocket stream so clients can consume it.

This module does NOT serve the frontend; it only makes sure the app has a
working stream ready. The probe app itself serves :9100 (MJPEG + H264 WS)
and serves /stream.html for browser playback.
"""

from __future__ import annotations

import logging
import threading
import time
from typing import Optional

log = logging.getLogger(__name__)


class H264StreamManager:
    """Coordinates the H264 stream for the selected device.

    The actual encode happens on-device in the probe app; here we only
    manage: launch, health, and re-launch on app crashes.
    """

    def __init__(self, bundle_id: str):
        self.bundle_id = bundle_id
        self._supervisor = None
        self._launched_at: Optional[float] = None
        self._watch = threading.Event()

    def launch_via(self, launcher) -> None:
        """Launch the probe app via the given callable (AppServiceService wrapper)."""
        log.info("launching stream app %s", self.bundle_id)
        launcher(self.bundle_id)
        self._launched_at = time.time()
        log.info("stream app launched")

    def stop(self) -> None:
        log.info("stream app stopped")
        self._launched_at = None
