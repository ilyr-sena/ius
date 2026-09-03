"""Data types shared across mux and registry."""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from typing import Optional


# ---------------------------------------------------------------------------
# Device model name tables
# ---------------------------------------------------------------------------

_IPHONE: dict[str, str] = {
    "iPhone6,1": "iPhone 5s", "iPhone6,2": "iPhone 5s",
    "iPhone7,1": "iPhone 6 Plus", "iPhone7,2": "iPhone 6",
    "iPhone8,1": "iPhone 6s", "iPhone8,2": "iPhone 6s Plus",
    "iPhone8,4": "iPhone SE (1st gen)",
    "iPhone9,1": "iPhone 7", "iPhone9,3": "iPhone 7",
    "iPhone9,2": "iPhone 7 Plus", "iPhone9,4": "iPhone 7 Plus",
    "iPhone10,1": "iPhone 8", "iPhone10,4": "iPhone 8",
    "iPhone10,2": "iPhone 8 Plus", "iPhone10,5": "iPhone 8 Plus",
    "iPhone10,3": "iPhone X", "iPhone10,6": "iPhone X",
    "iPhone11,2": "iPhone XS",
    "iPhone11,4": "iPhone XS Max", "iPhone11,6": "iPhone XS Max",
    "iPhone11,8": "iPhone XR",
    "iPhone12,1": "iPhone 11",
    "iPhone12,3": "iPhone 11 Pro",
    "iPhone12,5": "iPhone 11 Pro Max",
    "iPhone12,8": "iPhone SE (2nd gen)",
    "iPhone13,1": "iPhone 12 mini",
    "iPhone13,2": "iPhone 12",
    "iPhone13,3": "iPhone 12 Pro",
    "iPhone13,4": "iPhone 12 Pro Max",
    "iPhone14,2": "iPhone 13 Pro",
    "iPhone14,3": "iPhone 13 Pro Max",
    "iPhone14,4": "iPhone 13 mini",
    "iPhone14,5": "iPhone 13",
    "iPhone14,6": "iPhone SE (3rd gen)",
    "iPhone14,7": "iPhone 14",
    "iPhone14,8": "iPhone 14 Plus",
    "iPhone15,2": "iPhone 14 Pro",
    "iPhone15,3": "iPhone 14 Pro Max",
    "iPhone15,4": "iPhone 15",
    "iPhone15,5": "iPhone 15 Plus",
    "iPhone16,1": "iPhone 15 Pro",
    "iPhone16,2": "iPhone 15 Pro Max",
    "iPhone17,1": "iPhone 16 Pro",
    "iPhone17,2": "iPhone 16 Pro Max",
    "iPhone17,3": "iPhone 16",
    "iPhone17,4": "iPhone 16 Plus",
    "iPhone17,5": "iPhone 16e",
}

_IPAD: dict[str, str] = {
    "iPad13,1": "iPad Air (4th gen)", "iPad13,2": "iPad Air (4th gen)",
    "iPad13,4": "iPad Pro 11\" (3rd gen)", "iPad13,5": "iPad Pro 11\" (3rd gen)",
    "iPad13,6": "iPad Pro 11\" (3rd gen)", "iPad13,7": "iPad Pro 11\" (3rd gen)",
    "iPad13,8": "iPad Pro 12.9\" (5th gen)", "iPad13,9": "iPad Pro 12.9\" (5th gen)",
    "iPad13,10": "iPad Pro 12.9\" (5th gen)", "iPad13,11": "iPad Pro 12.9\" (5th gen)",
    "iPad13,16": "iPad Air (5th gen)", "iPad13,17": "iPad Air (5th gen)",
    "iPad13,18": "iPad (10th gen)", "iPad13,19": "iPad (10th gen)",
    "iPad14,3": "iPad Pro 11\" (4th gen)", "iPad14,4": "iPad Pro 11\" (4th gen)",
    "iPad14,5": "iPad Pro 12.9\" (6th gen)", "iPad14,6": "iPad Pro 12.9\" (6th gen)",
    "iPad14,8": "iPad Air 11\" (M2)", "iPad14,9": "iPad Air 11\" (M2)",
    "iPad14,10": "iPad Air 13\" (M2)", "iPad14,11": "iPad Air 13\" (M2)",
    "iPad15,3": "iPad Air 11\" (M3)", "iPad15,4": "iPad Air 11\" (M3)",
    "iPad15,5": "iPad Air 13\" (M3)", "iPad15,6": "iPad Air 13\" (M3)",
    "iPad15,7": "iPad mini (A17 Pro)", "iPad15,8": "iPad mini (A17 Pro)",
    "iPad16,3": "iPad Pro 11\" (M4)", "iPad16,4": "iPad Pro 11\" (M4)",
    "iPad16,5": "iPad Pro 13\" (M4)", "iPad16,6": "iPad Pro 13\" (M4)",
}

_IPOD: dict[str, str] = {
    "iPod9,1": "iPod touch (7th gen)",
}

# ---------------------------------------------------------------------------
# Device + event types used by mux + registry
# ---------------------------------------------------------------------------

@dataclass
class Device:
    """A device reported by the mux."""

    udid: str
    device_id: int
    connection_type: str = "USB"
    product_id: int = 0

    # Enriched fields (lockdown / pair record / models table)
    name: Optional[str] = None
    model: Optional[str] = None         # friendly model name
    ios_version: Optional[str] = None
    build_version: Optional[str] = None

    def summary(self) -> str:
        bits = [f"UDID={self.udid}", f"ProductID=0x{self.product_id:04X}"]
        if self.name:
            bits.append(f"Name={self.name}")
        if self.model:
            bits.append(f"Model={self.model}")
        if self.ios_version:
            bits.append(f"iOS={self.ios_version}")
        return ", ".join(bits)


@dataclass
class DeviceInfo:
    """Full metadata record — persists between runs."""

    udid: str
    name: str = ""
    model: str = ""
    ios_version: str = ""
    build_version: str = ""
    product_id: int = 0
    first_seen: float = 0.0
    last_seen: float = 0.0
    is_attached: bool = False

    def to_dict(self) -> dict:
        return {
            "udid": self.udid,
            "name": self.name,
            "model": self.model,
            "ios_version": self.ios_version,
            "build_version": self.build_version,
            "product_id": self.product_id,
            "first_seen": self.first_seen,
            "last_seen": self.last_seen,
            "is_attached": self.is_attached,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "DeviceInfo":
        return cls(**{k: d[k] for k in cls.__dataclass_fields__ if k in d})

    def summary(self) -> str:
        status = "attached" if self.is_attached else "detached"
        name = self.name or self.udid
        model = self.model or "?"
        ios = self.ios_version or "?"
        return f"{self.udid} — {name} ({model}, iOS {ios}) [{status}]"


@dataclass
class DeviceEvent:
    kind: str                 # "attach" | "detach"
    device: Optional[Device] = None   # attach only
    udid: str = ""
    device_id: int = 0


def device_model_name(hardware_id: Optional[str]) -> Optional[str]:
    """Resolve a ProductType string to its friendly name."""
    if not hardware_id:
        return None
    hid = hardware_id.strip()
    if hid in _IPHONE:
        return _IPHONE[hid]
    if hid in _IPAD:
        return _IPAD[hid]
    if hid in _IPOD:
        return _IPOD[hid]
    return None
