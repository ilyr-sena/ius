import json
import tempfile
from pathlib import Path

from meridian_py.devices import DeviceRegistry


def test_attach_persist_detach():
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "devices.json"
        r = DeviceRegistry(p)
        r.see({"udid": "A" * 20, "product_id": 0x12A8})
        r.flush(force=True)

        # Reload proves persistence.
        r2 = DeviceRegistry(p)
        rec = r2.get("A" * 20)
        assert rec is not None
        assert not rec.is_attached  # Reload assumes detached


def test_enrich_merges():
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "devices.json"
        r = DeviceRegistry(p)
        r.see({"udid": "B" * 20})
        r.see({"udid": "B" * 20, "name": "Steve's iPhone", "ios_version": "27.0"})
        rec = r.get("B" * 20)
        assert rec.name == "Steve's iPhone"
        assert rec.ios_version == "27.0"


def test_detach_only_marks_attached_false():
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "devices.json"
        r = DeviceRegistry(p)
        r.see({"udid": "C" * 20})
        assert r.get("C" * 20).is_attached
        r.detach("C" * 20)
        assert not r.get("C" * 20).is_attached
