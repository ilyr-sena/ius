"""Async bridge to pymobiledevice3's RemoteServiceDiscovery layer.

The pymobiledevice3 RSD APIs are async; our runner needs synchronous calls.
This module provides the single point of conversion.
"""

from __future__ import annotations

import asyncio
import logging
from typing import Optional

log = logging.getLogger(__name__)


async def rsd_for(cfg, udid: str):
    """Open an RSD connection for the given UDID via the local tunneld."""
    from pymobiledevice3.remote.core_device.core_device_service_discovery import start_tun_service
    from pymobiledevice3.tunneld.api import get_tunneld_devices

    # If tunneld already has this device, reuse its RSD.
    try:
        devices = await get_tunneld_devices(("127.0.0.1", cfg.tunneld_port))
        for d in devices:
            if getattr(d, "udid", None) == udid:
                return d
    except Exception as e:
        log.warning("tunneld lookup failed for %s: %s — falling back to USB", udid, e)

    # Otherwise start a tun service ourselves (USB fallback).
    host, port = "127.0.0.1", cfg.tunneld_port
    # start_tun_service returns a RemoteServiceDiscoveryService context manager.
    return await start_tun_service(udid, host, port)


def lockdown_values(udid: str) -> dict:
    """Convenience: synchronous fetch-all-values for a device."""
    from pymobiledevice3.lockdown import create_using_usbmux
    ld = create_using_usbmux(serial=udid)
    try:
        return ld.all_values
    finally:
        try:
            ld.close()
        except Exception:
            pass
