"""Long-lived subprocess supervisor.

Replaces the ad-hoc Popen tracking in ius.py. Each child process is a
ProcessSpec with a name and a restart policy; the supervisor owns them all
and shuts them down cleanly on teardown.
"""

from __future__ import annotations

import logging
import subprocess
import threading
import time
from dataclasses import dataclass, field
from typing import Optional

log = logging.getLogger(__name__)


@dataclass
class ProcessSpec:
    name: str
    argv: list[str]
    autostart: bool = True
    stop_timeout: float = 4.0
    env: Optional[dict] = None


@dataclass
class _ProcessState:
    spec: ProcessSpec
    proc: Optional[subprocess.Popen] = None
    stopping: bool = False
    fail_count: int = 0


class Supervisor:
    """Owns a set of processes. Restarts on unexpected exit with backoff."""

    def __init__(self, max_restart_backoff: float = 30.0):
        self._max_restart_backoff = max_restart_backoff
        self._lock = threading.Lock()
        self._procs: dict[str, _ProcessState] = {}
        self._shutdown = threading.Event()
        self._watcher_thread: Optional[threading.Thread] = None

    # ------------------------------------------------------------------
    # lifecycle

    def add(self, spec: ProcessSpec) -> None:
        with self._lock:
            self._procs[spec.name] = _ProcessState(spec=spec)

    def start_all(self) -> None:
        with self._lock:
            for st in self._procs.values():
                if st.spec.autostart:
                    self._spawn(st)
            if not self._watcher_thread or not self._watcher_thread.is_alive():
                self._shutdown.clear()
                self._watcher_thread = threading.Thread(
                    target=self._watch_loop, name="meridian-supervisor", daemon=True,
                )
                self._watcher_thread.start()

    def stop_all(self) -> None:
        self._shutdown.set()
        with self._lock:
            for st in self._procs.values():
                st.stopping = True
                if st.proc and st.proc.poll() is None:
                    try:
                        st.proc.terminate()
                    except Exception:
                        pass
        # Extra grace, then hard kill.
        deadline = time.time() + 5
        while time.time() < deadline:
            all_dead = True
            with self._lock:
                for st in self._procs.values():
                    if st.proc and st.proc.poll() is None:
                        all_dead = False
            if all_dead:
                break
            time.sleep(0.1)
        with self._lock:
            for st in self._procs.values():
                if st.proc and st.proc.poll() is None:
                    try:
                        st.proc.kill()
                    except Exception:
                        pass

    # ------------------------------------------------------------------
    # internals

    def _spawn(self, st: _ProcessState) -> None:
        kws: dict = {}
        if st.spec.env is not None:
            kws["env"] = st.spec.env
        try:
            st.proc = subprocess.Popen(
                st.spec.argv,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                **kws,
            )
            log.info("started %s (pid=%s)", st.spec.name, st.proc.pid)
        except FileNotFoundError as e:
            log.error("could not start %s: %s", st.spec.name, e)
            st.proc = None

    def _watch_loop(self) -> None:
        while not self._shutdown.is_set():
            with self._lock:
                procs: list[_ProcessState] = list(self._procs.values())
            for st in procs:
                if st.stopping:
                    continue
                if st.proc is None:
                    continue
                if st.proc.poll() is not None:
                    st.fail_count += 1
                    delay = min(self._max_restart_backoff, 2 ** st.fail_count) / 10
                    log.warning(
                        "%s exited (code=%s) — restarting in %.1fs",
                        st.spec.name, st.proc.returncode, delay,
                    )
                    time.sleep(delay)
                    self._spawn(st)
            time.sleep(0.5)
