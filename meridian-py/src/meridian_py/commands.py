"""Leaf-mode commands: list, info, watch, tunnel.

These are One-shot paths: they run, produce output, and exit. No state kept
beyond the current invocation.
"""

from __future__ import annotations

import logging
import os
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


# ---------------------------------------------------------------------------
# stream: launch the probe app and expose the :9100 endpoint only.

def stream_cmd(cfg: Config, args) -> int:
    """Stream-focused mode: tunnels + probe launch, no WDA/HID/HTTP pad."""
    import signal

    mux = _mux(cfg)

    # Check device presence.
    devices = mux.list_devices()
    if not devices:
        log.error("no devices attached")
        return 1
    udid = args.udid or devices[0].udid
    if not any(d.udid == udid for d in devices):
        log.error("device %s not attached", udid)
        return 1
    log.info("stream mode on %s", udid[:16])

    # Optional app launch — but only *after* the tunnels are up, so connect
    # failures mean something. Launch happens in the background; you can also
    # just open the probe app on the phone by hand.
    if not args.no_launch:
        def _launch_in_bg():
            try:
                import asyncio
                from pymobiledevice3.remote.core_device.app_service import AppServiceService
                from pymobiledevice3.tunneld.api import get_tunneld_devices

                async def _app_launch():
                    rsds = await get_tunneld_devices(("127.0.0.1", cfg.tunneld_port))
                    rsd = next((r for r in rsds if getattr(r, 'udid', None) == udid), None)
                    if rsd is None:
                        raise RuntimeError("tunneld can't reach this device")
                    async with AppServiceService(rsd) as svc:
                        await svc.launch_application(cfg.probe_bundle)

                asyncio.run(_app_launch())
                log.info("probe app launched")
            except Exception as e:
                log.warning(
                    "could not launch the probe app itself — install it once, then it will just work: %s",
                    e,
                )

        import threading as _th
        _th.Thread(target=_launch_in_bg, daemon=True).start()

    # Tunnels are what actually move the stream bytes to the local box.
    pairs = [(p, p) for (p, _) in cfg.iproxy_ports]
    if not pairs:
        pairs = [(9100, 9100)]
    tunnels = [Tunnel(mux, lp, dp, udid) for lp, dp in pairs]
    for t in tunnels:
        t.start()
        log.info("tunnel :%d → device :%d", t.local_port, t.device_port)

    def _sig(*_a):
        print()
        for t in tunnels:
            t.stop()
        log.info("stopping")
        sys.exit(0)

    signal.signal(signal.SIGINT, _sig)
    signal.signal(signal.SIGTERM, _sig)

    if getattr(args, "open", False) or not os.environ.get("SSH_TTY"):
        try:
            import webbrowser
            webbrowser.open(f"http://127.0.0.1:{pairs[0][0]}/stream.html")
        except Exception:
            pass

    print(f"stream: http://127.0.0.1:{pairs[0][0]}/stream.html  (or /stream for MJPEG)")
    signal.pause()
    return 0


# ---------------------------------------------------------------------------
# sideload: provision, sign, and install an IPA over USB directly from Linux

def sideload_cmd(cfg: Config, args) -> int:
    """Sign and install an IPA package directly over USB."""
    from pathlib import Path
    from .sideload import sideload_app

    ipa_path = Path(args.ipa)
    if not ipa_path.exists():
        log.error("file not found: %s", args.ipa)
        return 1

    custom_key = Path(args.key) if getattr(args, "key", None) else None
    custom_cert = Path(args.cert) if getattr(args, "cert", None) else None

    try:
        sideload_app(
            ipa_path=ipa_path,
            udid=args.udid or cfg.udid,
            bundle_id=getattr(args, "bundle_id", None),
            force_renew=getattr(args, "renew", False),
            force_login=getattr(args, "login", False),
            custom_key=custom_key,
            custom_cert=custom_cert,
        )
        return 0
    except Exception as e:
        log.error("sideload failed: %s", e)
        return 1

