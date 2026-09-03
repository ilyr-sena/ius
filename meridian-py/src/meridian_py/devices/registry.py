"""Persistent registry of known devices.

- Tracks every device it has ever seen, keyed by UDID.
- Updates attach/detach timestamps as events arrive.
- Persists to disk as JSON (debounced writes — not per-event).
- Queryable from HTTP/CLI layers.
"""

from __future__ import annotations

import json
import logging
import threading
import time
from pathlib import Path
from typing import Iterable, List, Optional

from ..models import DeviceInfo

log = logging.getLogger(__name__)


class DeviceRegistry:
    def __init__(self, state_file: Path):
        self.path = state_file
        self._lock = threading.Lock()
        self._devices: dict[str, DeviceInfo] = {}
        self._dirty = False
        self._last_flush = 0.0
        self._load()

    # ------------------------------------------------------------------
    # persistence

    def _load(self) -> None:
        if not self.path.exists():
            log.debug("registry: no state file yet at %s", self.path)
            return
        try:
            with self.path.open() as f:
                raw = json.load(f)
            for ud, rec in raw.items():
                self._devices[ud] = DeviceInfo.from_dict(rec)
                # On load everything is presumed detached.
                self._devices[ud].is_attached = False
            log.info("registry: loaded %d device(s) from disk", len(self._devices))
        except Exception as e:
            log.warning("registry: could not load state file: %s", e)

    def flush(self, force: bool = False) -> None:
        with self._lock:
            if not self._dirty:
                return
            now = time.time()
            if not force and now - self._last_flush < 2.0:
                return
            self.path.parent.mkdir(parents=True, exist_ok=True)
            tmp = self.path.with_suffix(".json.tmp")
            with tmp.open("w") as f:
                json.dump(
                    {udid: d.to_dict() for udid, d in self._devices.items()},
                    f, indent=2, sort_keys=True,
                )
            tmp.replace(self.path)
            self._dirty = False
            self._last_flush = now
            log.debug("registry: wrote %d device(s) to %s", len(self._devices), self.path)

    # ------------------------------------------------------------------
    # reconciling

    def see(self, dev: dict) -> DeviceInfo:
        """Mark a device present (attaches) or update metadata.

        `dev` is a minimal dict with at least: udid, product_id.
        Extra keys (name, model, ios_version, build_version) enrich the record.
        """
        udid = dev.get("udid") or ""
        if not udid:
            raise ValueError("device has no udid")

        with self._lock:
            now = time.time()
            existing = self._devices.get(udid)
            if existing is None:
                rec = DeviceInfo(udid=udid, first_seen=now)
                self._devices[udid] = rec
                log.info("registry: first contact with %s%s",
                         udid[:12],
                         f" ({dev.get('name','')})" if dev.get('name') else "")
                existing = rec
            existing.is_attached = True
            existing.last_seen = now

            # Merge enriched fields when the caller provides fresh ones.
            for meta in ("name", "model", "ios_version", "build_version"):
                v = dev.get(meta)
                if v and not getattr(existing, meta):
                    setattr(existing, meta, v)
            if dev.get("product_id"):
                existing.product_id = int(dev["product_id"])
            self._dirty = True
            return existing

    def detach(self, udid: str) -> None:
        with self._lock:
            if rec := self._devices.get(udid):
                rec.is_attached = False
                rec.last_seen = time.time()
                self._dirty = True
                log.debug("registry: detached %s", udid)

    # ------------------------------------------------------------------
    # queries

    def list_known(self) -> List[DeviceInfo]:
        with self._lock:
            return [DeviceInfo.from_dict(rec.to_dict()) for rec in self._devices.values()]

    def attached(self) -> List[DeviceInfo]:
        with self._lock:
            return [DeviceInfo.from_dict(rec.to_dict())
                    for rec in self._devices.values() if rec.is_attached]

    def get(self, udid: str) -> Optional[DeviceInfo]:
        with self._lock:
            rec = self._devices.get(udid)
            if rec is None:
                return None
            return DeviceInfo.from_dict(rec.to_dict())
