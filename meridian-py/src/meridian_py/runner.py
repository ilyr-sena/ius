"""The orchestrated session runner — `meridian-py run`.

Replaces the ad-hoc amain in ius.py with a self-contained lifecycle:
supervisor + watcher + registry + stream + WDA + HTTP server.
"""

from __future__ import annotations

import logging
import signal
import sys
import threading
import time
from pathlib import Path
from typing import Any, Optional

from .config import Config
from .devices import DeviceRegistry, Watcher
from .mux import MuxClient
from .mux.tunnel import Tunnel
from .mux.lockdown import LockdownClient
from .services import Supervisor, ProcessSpec
from .services.tunneld import Tunneld
from .services.iproxy import IProxy
from .stream import WdaClient
from .server import HTTPServer

log = logging.getLogger(__name__)


class Session:
    """Everything the full run needs, held together. Has a clean stop()."""

    def __init__(self, cfg: Config, udid: str):
        self.cfg = cfg
        self.udid = udid

        self.mux = MuxClient(cfg.mux_endpoint)
        self.registry = DeviceRegistry(cfg.state_dir / "devices.json")
        self.supervisor = Supervisor()
        self._started = threading.Event()
        self._stopping = threading.Event()

        self._stop_hooks: list = []
        self._wda: Optional[WdaClient] = None
        self._tunnel_specs: list[tuple[int, int]] = []

    # ------------------------------------------------------------------
    # helpers used by start()

    def _resolve_and_log_device(self) -> None:
        devices = self.mux.list_devices()
        if not devices:
            raise RuntimeError("no devices attached")
        if self.udid:
            if all(d.udid != self.udid for d in devices):
                raise RuntimeError(f"device {self.udid} not attached")
        else:
            self.udid = devices[0].udid
        log.info("using device %s", self.udid[:16])

    def _enrich_all_attached(self) -> None:
        devices = self.mux.list_devices()
        for d in devices:
            self.registry.see({"udid": d.udid, "device_id": d.device_id, "product_id": d.product_id})
            try:
                info = LockdownClient(d.udid).summary()
                self.registry.see({
                    "udid": d.udid,
                    "name": info.get("name"),
                    "model": info.get("model"),
                    "ios_version": info.get("ios_version"),
                    "build_version": info.get("build_version"),
                })
            except Exception as e:
                log.debug("lockdown enrich failed for %s: %s", d.udid, e)

    def _attach_hooks(self) -> None:
        watcher = Watcher(self.mux)
        watcher.add_listener(self._on_device_change)
        watcher.start()
        self._stop_hooks.append(watcher.stop)

    def _on_device_change(self, kind: str, payload: dict) -> None:
        try:
            if kind == "attach":
                udid = payload["udid"]
                rec = self.registry.see({
                    "udid": udid,
                    "device_id": payload.get("device_id"),
                    "product_id": payload.get("product_id"),
                })
                log.info("attach: %s", udid)
                # Enrich asynchronously so the watcher never blocks.
                threading.Thread(target=self._enrich_later, args=(udid,), daemon=True).start()
                if self._wda and not self._wda.alive():
                    log.info("device returned — WDA marked dead, will relaunch on demand")
            elif kind == "detach":
                self.registry.detach(payload["udid"])
                log.info("detach: %s", payload["udid"])
        except Exception as e:
            log.warning("device change handler error: %s", e)

    def _enrich_later(self, udid: str) -> None:
        try:
            info = LockdownClient(udid).summary()
            self.registry.see({
                "udid": udid,
                "name": info.get("name"),
                "model": info.get("model"),
                "ios_version": info.get("ios_version"),
                "build_version": info.get("build_version"),
            })
        except Exception as e:
            log.debug("async enrich skipped for %s: %s", udid, e)

    # ------------------------------------------------------------------
    # stages

    def _start_maintained_services(self, args) -> None:
        """iproxy/tunneld are supervised. WDA/probe are logical services too."""
        if not args.no_tunnels:
            for lp, dp in self.cfg.iproxy_ports:
                spec = ProcessSpec(
                    name=f"iproxy {lp}",
                    argv=["iproxy", str(lp), str(dp)],
                )
                self.supervisor.add(spec)
            self._tunnel_specs = list(self.cfg.iproxy_ports)

        # superversion for tunneld lives in own supervisor so restarts isolated
        tum = Tunneld(port=self.cfg.tunneld_port)
        if not tum._port_open(tum.port):
            tum_sup = Supervisor()
            tum.ensure_started(tum_sup)
            tum_sup.start_all()
            self._stop_hooks.append(tum_sup.stop_all)
        else:
            log.info("tunneld already listening on :%d (yours, not ours)", tum.port)

        self.supervisor.start_all()
        self._stop_hooks.append(self.supervisor.stop_all)

    def _start_http(self, args) -> None:
        if args.no_http:
            log.info("HTTP API disabled (--no-http)")
            return

        def _devices() -> list:
            return [d.to_dict() for d in self.registry.list_known()]

        def _attached() -> list:
            return [d.to_dict() for d in self.registry.attached()]

        def _apps_json() -> dict:
            return {}

        srv = HTTPServer(
            port=self.cfg.http_port,
            devices_snapshot_fn=_devices,
            attached_snapshot_fn=_attached,
            apps_json_fn=_apps_json,
        )
        srv.start()
        self._stop_hooks.append(srv.stop)

    def _start_wda(self, args) -> None:
        if args.no_wda:
            log.info("WDA disabled (--no-wda)")
            return
        self._wda = WdaClient()
        try:
            if self._wda.alive():
                log.info("WDA already alive")
            else:
                log.info("WDA will launch on demand")
        except Exception:
            log.info("WDA will launch on demand")

    def _start_probe(self, args) -> None:
        if args.no_stream:
            log.info("stream-app disabled (--no-stream)")
            return
        launcher = lambda bundle_id: asyncio_free_launch(self.cfg, self.udid, bundle_id)  # noqa: F405
        threading.Thread(
            target=launcher,
            args=(self.cfg.probe_bundle,),
            daemon=True,
        ).start()

    # ------------------------------------------------------------------
    # entrypoint

    def start(self, args) -> int:
        log.info("── session starting ───────────────────────────────")
        self._resolve_and_log_device()

        # Enrich immediately so list output is complete from first tick
        self._enrich_all_attached()

        self._attach_hooks()
        self._start_maintained_services(args)
        self._start_http(args)
        self._start_wda(args)
        self._start_probe(args)

        log.info("── ready ─────────────────────────────────────────")
        return self._idle()

    def _idle(self) -> int:
        try:
            while not self._stopping.is_set():
                time.sleep(0.5)
        except KeyboardInterrupt:
            pass
        self.stop()
        return 0

    def stop(self) -> None:
        if self._stopping.is_set():
            return
        self._stopping.set()
        log.info("shutting down — registry persists to disk")
        self.registry.flush(force=True)
        for h in reversed(self._stop_hooks):
            try:
                h()
            except Exception as e:
                log.debug("stop hook failed: %s", e)


# ---------------------------------------------------------------------------
# Cross-boundary helper: launch a bundle-id with AppServiceService
# (needs the asyncio loop for pymobiledevice3's async APIs).
def asyncio_free_launch(cfg: Config, udid: str, bundle: str) -> None:
    import asyncio
    from pymobiledevice3.remote.core_device.app_service import AppServiceService
    from .device_bridge import rsd_for  # typing only path

    async def _go():
        rsd = await rsd_for(cfg, udid)
        async with AppServiceService(rsd) as svc:
            await svc.launch_application(bundle)

    try:
        asyncio.run(_go())
    except Exception as e:
        log.warning("launch of %s failed: %s", bundle, e)


# ---------------------------------------------------------------------------
# Entry point for `run`

def run_session(cfg: Config, args) -> int:
    udid = args.udid
    sess = Session(cfg, udid or "")
    try:
        return sess.start(args)
    except Exception as e:
        log.error("session failed: %s", e)
        sess.stop()
        return 2
