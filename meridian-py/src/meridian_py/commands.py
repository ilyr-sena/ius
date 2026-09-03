"""Leaf-mode commands: list, info, watch, tunnel.

These are One-shot paths: they run, produce output, and exit. No state kept
beyond the current invocation.
"""

from __future__ import annotations

import logging
import signal
import sys
import time

from .config import Config
from .mux import MuxClient, LockdownClient
from .mux.tunnel import Tunnel

log = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# helpers

def _mux(cfg: Config) -> MuxClient:
    return MuxClient(cfg.mux_endpoint)


def _print_device_line(d) -> None:
    model = d.model or "?"
    name = d.name or d.udid
    ios = d.ios_version or "?"
    print(f"  {d.device_id:>6}  {d.udid:<26}  {model:<14}  {ios:<10}  {name}")


def _enrich(uds: list) -> list:
    out = []
    for u in uds:
        try:
            info = LockdownClient(u.udid).summary()
            u.name = info.get("name") or None
            u.model = info.get("model") or None
            u.ios_version = info.get("ios_version") or None
            u.build_version = info.get("build_version") or None
        except Exception as e:
            log.warning("enrich failed for %s: %s", u.udid, e)
        out.append(u)
    return out


# ---------------------------------------------------------------------------
# list

def list_devices_cmd(cfg: Config) -> int:
    """Print everything we know — currently attached on top, previously seen below."""
    from .devices.registry import DeviceRegistry
    mux = _mux(cfg)
    registry = DeviceRegistry(cfg.state_dir / "devices.json")

    try:
        devices = _enrich(mux.list_devices())
    except Exception as e:
        log.error("could not reach mux: %s", e)
        devices = []

    attached_udids = set()
    for d in devices:
        attached_udids.add(d.udid)
        registry.see({
            "udid": d.udid,
            "device_id": d.device_id,
            "product_id": d.product_id,
            "name": d.name,
            "model": d.model,
            "ios_version": d.ios_version,
            "build_version": d.build_version,
        })
    registry.flush(force=True)

    known = registry.list_known()
    if not devices and not known:
        print("no devices found")
        return 0

    if devices:
        print("── attached ───────────────────────────────────────────────")
        print(f"{'ID':>4}  {'UDID':<26}  {'Model':<14}  {'iOS':<10}  Name")
        for d in devices:
            _print_device_line(d)
    else:
        print("no devices currently attached")

    detached = [d for d in known if d.udid not in attached_udids]
    if detached:
        print("── previously seen ───────────────────────────────────────")
        for d in sorted(detached, key=lambda x: x.last_seen, reverse=True):
            seen = time.strftime("%Y-%m-%d %H:%M", time.localtime(d.last_seen))
            print(f"  last={seen}  {d.udid:<26}  {d.model or '?':<14}  {d.ios_version or '?':<10}  {d.name or d.udid}")
    return 0


# ---------------------------------------------------------------------------
# info

def info_cmd(cfg: Config, udid: str | None) -> int:
    mux = _mux(cfg)
    try:
        devices = mux.list_devices()
    except Exception as e:
        log.error("could not reach mux: %s", e)
        return 1
    if not devices:
        print("no devices found", file=sys.stderr)
        return 1
    if udid is None:
        d = devices[0]
    else:
        d = next((x for x in devices if x.udid == udid), None)
        if d is None:
            print(f"device not found: {udid}", file=sys.stderr)
            return 1
    info = LockdownClient(d.udid).summary()
    print(f"  Name:         {info.get('name') or 'Unknown'}")
    print(f"  UDID:         {d.udid}")
    print(f"  Model:        {info.get('model') or 'Unknown'}")
    print(f"  iOS Version:  {info.get('ios_version') or 'Unknown'}")
    print(f"  Build:        {info.get('build_version') or 'Unknown'}")
    print(f"  Product ID:   0x{d.product_id:04X}")
    return 0


# ---------------------------------------------------------------------------
# watch

def watch_cmd(cfg: Config) -> int:
    mux = _mux(cfg)
    log.info("listening for attach/detach events (Ctrl+C to stop)")
    try:
        s = mux.listen()
    except Exception as e:
        log.error("listen failed: %s", e)
        return 1

    print("watching for device events...")
    while True:
        try:
            msg = MuxClient._recv(s)
        except Exception as e:
            log.warning("listen dropped (%s) — reconnecting", e)
            time.sleep(2)
            s = mux.listen()
            continue
        mt = msg.get("MessageType")
        if mt == "Attached":
            props = msg.get("Properties", {})
            # Enrich on attach so the first print carries identity
            try:
                info = LockdownClient(props.get("SerialNumber", "")).summary()
                print(f"+ attached   {props.get('SerialNumber')}  {info.get('name','?')}  {info.get('model','?')}  iOS {info.get('ios_version','?')}")
            except Exception:
                print(f"+ attached   {props.get('SerialNumber')}")
        elif mt == "Detached":
            sn = msg.get("SerialNumber") or msg.get("Properties", {}).get("SerialNumber") or msg.get("DeviceID") or "?"
            print(f"- detached   {sn}")
        else:
            log.debug("raw mux event: %s", msg)


# ---------------------------------------------------------------------------
# tunnel

def tunnel_cmd(cfg: Config, pairs: list[str], udid: str | None) -> int:
    mux = _mux(cfg)
    parsed = []
    for pt in pairs:
        lp, sep, dp = pt.partition(":")
        if not sep:
            print(f"bad tunnel pair '{pt}' — expected local:device", file=sys.stderr)
            return 2
        parsed.append((int(lp), int(dp)))

    tunnels = [Tunnel(mux, lp, dp, udid) for lp, dp in parsed]
    for t in tunnels:
        t.start()

    def _sig(*_a):
        print("\r")
        for t in tunnels:
            t.stop()
        sys.exit(0)

    signal.signal(signal.SIGINT, _sig)
    signal.signal(signal.SIGTERM, _sig)

    print("tunnels up — Ctrl+C to stop")
    signal.pause()
    return 0
