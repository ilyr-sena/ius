"""Lockdown queries via pymobiledevice3 — one connection per call.

Used for device enrichment (name, model, iOS, build) and pair checks.
"""

from __future__ import annotations

import logging
from typing import Optional

log = logging.getLogger(__name__)


class LockdownError(Exception):
    pass


class LockdownClient:
    def __init__(self, udid: str):
        self.udid = udid

    def fetch_all(self) -> dict:
        """Return the full properties dict from lockdown."""
        import asyncio
        from pymobiledevice3.lockdown import create_using_usbmux

        async def _go():
            dev = await create_using_usbmux(serial=self.udid)
            return dev.all_values

        return asyncio.run(_go())

    def get(self, key: str, default: Optional[str] = None):
        vals = self.fetch_all()
        return vals.get(key, default)

    def summary(self) -> dict:
        """Compact identity record for the registry."""
        from ..models import device_model_name
        vals = self.fetch_all()
        hw_model = vals.get("ProductType") or vals.get("HardwareModel") or ""
        return {
            "udid": self.udid,
            "name": vals.get("DeviceName") or "",
            "model": device_model_name(hw_model) or hw_model,
            "ios_version": vals.get("ProductVersion") or "",
            "build_version": vals.get("BuildVersion") or "",
            "hardware_model": hw_model,
        }
