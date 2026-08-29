#!/usr/bin/env python3
"""IUS – Integrated USB Server.

Merges stream_run.py (iproxy tunnels), hid_test.py (HID/gesture controller),
and device detection into a single entry point.

Usage:
    python3 tools/ius.py                 # auto-detect device, start everything
    python3 tools/ius.py --udid <UDID>   # explicit device selection
    python3 tools/ius.py --no-wda        # skip WDA launch

Endpoints (once running):
    HID test pad   : http://127.0.0.1:9001/
    MJPEG stream   : http://127.0.0.1:9100/stream?fps=15&scale=0.5&q=0.4
    H.264 stream   : http://127.0.0.1:9100/stream.html
    WDA            : http://127.0.0.1:8100/status
    Probe API      : http://127.0.0.1:9100/capture/stats
    Apps JSON      : http://127.0.0.1:9001/apps.json
    App icons      : http://127.0.0.1:9001/icon/<bundleId>.png
"""
import asyncio
import base64
import hashlib
import http.server
import json
import os
import pathlib
import shlex
import site
import socket
import subprocess
import sys
import threading
import time
import urllib.parse

# ── root-drop if launched via sudo by mistake ────────────────────────────────
if os.name == "posix" and os.geteuid() == 0 and os.environ.get("SUDO_USER"):
    import pwd
    _pw = pwd.getpwnam(os.environ["SUDO_USER"])
    os.environ["HOME"] = _pw.pw_dir
    os.initgroups(_pw.pw_name, _pw.pw_gid)
    os.setuid(_pw.pw_uid)
    os.execv(sys.executable, [sys.executable] + sys.argv)

import requests

from pymobiledevice3.remote.core_device.app_service import AppServiceService
from pymobiledevice3.remote.core_device.hid_service import (
    ASCII_TO_HID,
    HID_BUTTON_STATE_DOWN,
    HID_BUTTON_STATE_UP,
    IndigoHIDService,
    KEY_BACKSPACE,
    KEYBOARD_SURFACE_DEFAULT_SERVICE_ID,
    KEY_ENTER,
    KEY_LEFT_SHIFT,
    TOUCHSCREEN_STATE_CONTACT,
    TOUCHSCREEN_STATE_RELEASE,
    touch_session,
)
from pymobiledevice3.remote.core_device.icon_service import IconService
from pymobiledevice3.remote.remote_service_discovery import (
    RemoteServiceDiscoveryService,
)
from pymobiledevice3.tunneld.api import get_tunneld_devices

# ── constants ────────────────────────────────────────────────────────────────

WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
STREAM_HZ = 60
TUNNELD_PORT = 49151
IPROXY_PORTS = [(9100, 9100), (8100, 8100)]

WDA_BASE = "http://127.0.0.1:8100"
WDA_BUNDLE_ID = os.environ.get(
    "IUS_WDA_BUNDLE", "com.facebook.WebDriverAgentRunner.xctrunner.SRTHYBYH35"
)

ICON_CACHE_DIR = pathlib.Path.home() / ".cache" / "ius" / "icons"

STOCK_BUNDLES = {
    "com.apple.Preferences", "com.apple.camera", "com.apple.AppStore",
    "com.apple.mobilenotes", "com.apple.calculator", "com.apple.mobilecal",
    "com.apple.mobiletimer", "com.apple.DocumentsApp", "com.apple.MobileSMS",
    "com.apple.mail", "com.apple.Maps", "com.apple.mobilesafari",
    "com.apple.mobileslideshow", "com.apple.Music", "com.apple.weather",
    "com.apple.reminders", "com.apple.Health", "com.apple.Wallet",
    "com.apple.facetime", "com.apple.MobileAddressBook",
    "com.apple.podcasts", "com.apple.iBooks", "com.apple.news",
    "com.apple.tv", "com.apple.Home", "com.apple.stocks",
    "com.apple.shortcuts", "com.apple.freeform", "com.apple.Journal",
    "com.apple.VoiceMemos", "com.apple.compass", "com.apple.measure",
    "com.apple.findmy", "com.apple.Fitness", "com.apple.Translate",
    "com.apple.Bridge",
}

NAMED_BUTTONS = {
    "home": (0x0C, 0x40, 0.05),
    "lock": (0x0C, 0x30, 0.5),
    "volume-up": (0x0C, 0xE9, 0.05),
    "volume-down": (0x0C, 0xEA, 0.05),
    "mute": (0x0C, 0xE2, 0.05),
    "siri": (0x0C, 0xCF, 1.0),
}

_HID_CODE_MAP = {"KeyA": 0x04}
_HID_CODE_MAP.update(
    {f"Key{c}": u for u, c in enumerate("BCDEFGHIJKLMNOPQRSTUVWXYZ", start=5)}
)
_HID_CODE_MAP.update({f"Digit{i}": 0x1E + n for n, i in enumerate("123456789")})
_HID_CODE_MAP.update({
    "Digit0": 0x27, "Enter": 0x28, "Escape": 0x29, "Backspace": 0x2A,
    "Tab": 0x2B, "Space": 0x2C, "Minus": 0x2D, "Equal": 0x2E,
    "BracketLeft": 0x2F, "BracketRight": 0x30, "Backslash": 0x31,
    "IntlBackslash": 0x64, "Semicolon": 0x33, "Quote": 0x34,
    "Backquote": 0x35, "Comma": 0x36, "Period": 0x37, "Slash": 0x38,
    "CapsLock": 0x39, "NumpadEnter": 0x58, "NumpadAdd": 0x57,
    "NumpadSubtract": 0x56, "NumpadDecimal": 0x63,
})
_HID_CODE_MAP.update({f"F{n}": 0x3A + n - 1 for n in range(1, 13)})
_HID_CODE_MAP.update({
    "ArrowUp": 0x52, "ArrowDown": 0x51, "ArrowLeft": 0x50, "ArrowRight": 0x4F,
})

_KEY_MAP = {"Backspace": KEY_BACKSPACE, "Enter": KEY_ENTER}
_MOD_USAGES = {0xE0, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7}

# ── shared state ─────────────────────────────────────────────────────────────

RS = {"loop": None, "queue": None}
RSD_CACHE = {"rsd": None}
ACTION_CTX = {
    "kb_attempt": 0,
    "indigo": None,
    "hid": None,
    "kbd_id": None,
    "send_lock": None,
    "want_kb": False,
    "recycle": False,
}
APPS_LOCK = threading.Lock()
APPS_STATE = {"list": None}
ICON_MEM = {}
WDA_LOCK = threading.Lock()
WDA_STATE = {"session": None}
WDA_HTTP = requests.Session()
WDA_QUEUE = asyncio.Queue()
UDID = None  # set at startup


# ── device detection ─────────────────────────────────────────────────────────

def detect_udid():
    """Run `idevice_id -l` and return the first UDID found."""
    try:
        result = subprocess.run(
            ["idevice_id", "-l"],
            capture_output=True, text=True, timeout=10,
        )
        udid = result.stdout.strip().split("\n")[0].strip()
        if udid:
            return udid
    except (subprocess.TimeoutExpired, FileNotFoundError, IndexError):
        pass
    return None


# ── iproxy tunnel management ────────────────────────────────────────────────

iproxy_procs = []


def start_iproxy_tunnels():
    """Start iproxy for each port pair."""
    for hp, dp in IPROXY_PORTS:
        print(f"[*] iproxy {hp} -> {dp}")
        iproxy_procs.append(subprocess.Popen(["iproxy", str(hp), str(dp)]))


def stop_iproxy_tunnels():
    for p in iproxy_procs:
        if p.poll() is None:
            p.terminate()
    for p in iproxy_procs:
        try:
            p.wait(timeout=3)
        except subprocess.TimeoutExpired:
            p.kill()
    iproxy_procs.clear()


# ── tunneld management ──────────────────────────────────────────────────────

tunneld_proc = None


def _port_open(port):
    with socket.socket() as s:
        s.settimeout(0.5)
        return s.connect_ex(("127.0.0.1", port)) == 0


def _ensure_tunneld():
    global tunneld_proc
    if _port_open(TUNNELD_PORT):
        print("[*] tunneld already running")
        return
    import site as _site
    user_site = _site.getusersitepackages()
    inner = (
        f"PYTHONPATH={shlex.quote(user_site)} "
        f"exec {shlex.quote(sys.executable)} "
        f"-m pymobiledevice3 remote tunneld"
    )
    print(f"[*] starting tunneld via sudo (PYTHONPATH={user_site})...")
    tunneld_proc = subprocess.Popen(["sudo", "sh", "-c", inner])


async def _wait_tunneld(timeout_s=120):
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout_s
    while loop.time() < deadline:
        if tunneld_proc is not None and tunneld_proc.poll() is not None:
            raise SystemExit(
                f"[!] tunneld exited early ({tunneld_proc.returncode}) - "
                f"start it manually: sudo python3 -m pymobiledevice3 remote tunneld"
            )
        if _port_open(TUNNELD_PORT):
            print("[+] tunneld ready")
            return
        await asyncio.sleep(0.5)
    raise SystemExit("[!] tunneld never came up on :49151")


def stop_tunneld():
    if tunneld_proc is not None and tunneld_proc.poll() is None:
        print("[*] stopping tunneld")
        tunneld_proc.terminate()
        try:
            tunneld_proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            tunneld_proc.kill()


# ── WDA management ──────────────────────────────────────────────────────────

def _wda_alive():
    try:
        r = WDA_HTTP.get(f"{WDA_BASE}/status", timeout=2)
        return r.status_code < 500
    except Exception:
        return False


def _wda_launch_runner():
    """Launch WDA runner app on device via pymobiledevice3 CLI."""
    if not UDID:
        print("[!] no UDID - cannot launch WDA")
        return False
    print(f"[*] launching WDA runner on {UDID[:16]}...")
    try:
        subprocess.run(
            [
                "pymobiledevice3", "developer", "dvt", "xcuitest",
                "--tunnel", UDID,
                WDA_BUNDLE_ID,
            ],
            capture_output=True, text=True, timeout=30,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError) as e:
        print(f"[!] WDA launch command failed: {e}")
        return False
    # Wait for WDA HTTP server
    deadline = time.time() + 25
    while time.time() < deadline:
        if _wda_alive():
            print("[+] WDA server up")
            return True
        time.sleep(0.7)
    print("[!] WDA server never came up")
    return False


def _wda_session_id():
    with WDA_LOCK:
        if WDA_STATE["session"]:
            return WDA_STATE["session"]
        r = WDA_HTTP.post(f"{WDA_BASE}/session",
                          json={"capabilities": {}}, timeout=8)
        r.raise_for_status()
        sid = r.json()["value"]["sessionId"]
        try:
            WDA_HTTP.post(
                f"{WDA_BASE}/session/{sid}/appium/settings",
                json={"settings": {"waitForIdleTimeout": 0}}, timeout=8)
        except Exception as e:
            print(f"[!] wda settings tweak failed (non-fatal): {e}")
        WDA_STATE["session"] = sid
        print(f"[+] wda session {sid}")
        return sid


def _wda_presskey(name):
    _wda_post_inner("/wda/presskey", {"key": name})


def _wda_post_inner(path, payload):
    sid = _wda_session_id()
    r = WDA_HTTP.post(f"{WDA_BASE}/session/{sid}{path}",
                      json=payload, timeout=8)
    if r.status_code >= 400:
        with WDA_LOCK:
            WDA_STATE["session"] = None
        sid = _wda_session_id()
        r = WDA_HTTP.post(f"{WDA_BASE}/session/{sid}{path}",
                          json=payload, timeout=8)
    r.raise_for_status()
    return r


def _wda_post(path, payload):
    try:
        return _wda_post_inner(path, payload)
    except requests.exceptions.ConnectionError:
        if not _wda_launch_runner():
            raise
        return _wda_post_inner(path, payload)


def wda_dispatch(action, job):
    if action == "type":
        text = str(job.get("text") or "")
        if not text:
            return
        _wda_post("/wda/keys", {"value": [text]})
        print(f"[ius] wda type: {text!r}")
    elif action == "key":
        key = str(job.get("key") or "")
        if key == "Backspace":
            _wda_post("/wda/keys", {"value": ["\b"]})
            print("[ius] wda key: backspace")
        elif key == "Enter":
            try:
                _wda_post("/wda/keys", {"value": ["\n"]})
            except Exception:
                _wda_presskey("return")
            print("[ius] wda key: return")
        else:
            print(f"[!] unsupported wda key '{key}'")


# ── HID helpers ──────────────────────────────────────────────────────────────

def norm(v):
    return max(0, min(65535, round(v * 65535)))


async def _hid_send_usages(codes):
    hid = ACTION_CTX["hid"]
    kbd = ACTION_CTX["kbd_id"]
    slock = ACTION_CTX["send_lock"]
    if hid is None or kbd is None or slock is None:
        return False
    mods = codes & _MOD_USAGES
    keys = codes - _MOD_USAGES

    async def send(frame):
        async with slock:
            await hid.send_keyboard(kbd, frame)

    if mods:
        await send(mods)
        await asyncio.sleep(0.008)
    if keys:
        await send(codes)
        await asyncio.sleep(0.008)
        await send(mods)
    await asyncio.sleep(0.008)
    await send(set())
    return True


async def _hid_send_char(ch):
    entry = ASCII_TO_HID.get(ch)
    if entry is None:
        return False
    usage, shift = entry
    codes = {usage} | ({KEY_LEFT_SHIFT} if shift else set())
    return await _hid_send_usages(codes)


async def _input_dispatch(job):
    if job.get("kind") == "hid":
        usage = _HID_CODE_MAP.get(str(job.get("code") or ""))
        if usage is None:
            print(f"[!] unmapped key code '{job.get('code')}'")
            return
        codes = {usage}
        if job.get("shift"):
            codes.add(KEY_LEFT_SHIFT)
        if job.get("ctrl"):
            codes.add(0xE0)
        if job.get("alt"):
            codes.add(0xE2)
        if await _hid_send_usages(codes):
            return
        await _exec_wda("type", {"text": str(job.get("text") or "")})
        return
    action = str(job.get("action") or "")
    if action == "type":
        ch = str(job.get("text") or "")
        if len(ch) == 1 and await _hid_send_char(ch):
            return
        await _exec_wda("type", job)
        return
    if action == "key":
        usage = _KEY_MAP.get(str(job.get("key") or ""))
        if usage is not None and await _hid_send_usages({usage}):
            return
        await _exec_wda("key", job)
        return
    await _exec_wda(action, job)


async def _exec_wda(action, job):
    await asyncio.get_event_loop().run_in_executor(None, wda_dispatch, action, job)


async def wda_writer():
    while True:
        job = await WDA_QUEUE.get()
        await _input_dispatch(job)


# ── RSD / app helpers ───────────────────────────────────────────────────────

async def _rsd_for_device():
    if RSD_CACHE["rsd"] is not None:
        return RSD_CACHE["rsd"]
    rsds = await get_tunneld_devices(("127.0.0.1", TUNNELD_PORT))
    rsd = next((r for r in rsds if r.udid == UDID), None)
    if rsd is None:
        raise RuntimeError("tunneld has no matching device")
    RSD_CACHE["rsd"] = rsd
    return rsd


async def _fetch_apps_async():
    rsd = await _rsd_for_device()
    async with AppServiceService(rsd) as svc:
        raw = await svc.list_apps(
            include_app_clips=False,
            include_removable_apps=True,
            include_hidden_apps=False,
            include_internal_apps=False,
            include_default_apps=True,
        )
    out, seen = [], set()
    n_user = 0
    for a in raw:
        bid = a.get("bundleIdentifier") or a.get("CFBundleIdentifier")
        if not bid or bid in seen:
            continue
        bpath = str(a.get("applicationBundlePath") or a.get("path") or "")
        plow = bpath.lower()
        kind = str(a.get("applicationType") or "").lower()
        is_user = (
            kind == "user"
            or bool(a.get("isRemovable") or a.get("removable"))
            or ("/containers/bundle/application/" in plow
                and ".staged" not in plow)
        )
        if not is_user and bid not in STOCK_BUNDLES:
            continue
        name = str(a.get("displayName") or a.get("name") or "").strip()
        if not name:
            continue
        exe = bpath.split(".app/")[-1].strip("/") if ".app/" in bpath else ""
        out.append({"bundleId": bid, "name": name, "exe": exe, "user": is_user})
        seen.add(bid)
        if is_user:
            n_user += 1
    out.sort(key=lambda e: (0 if e.pop("user") else 1, e["name"].lower()))
    print(f"[+] app list: {len(out)} apps ({n_user} downloaded, {len(out) - n_user} stock)")
    return out


def _token_name(tok):
    for k in ("name", "executablePath", "executable"):
        v = tok.get(k)
        if isinstance(v, dict):
            v = next(iter(v.values()), None)
        if isinstance(v, str) and v.strip():
            return v.strip().rsplit("/", 1)[-1]
    return None


def _token_pid(tok):
    p = tok.get("pid")
    try:
        return int(p)
    except (TypeError, ValueError):
        return None


async def _running_apps_async():
    with APPS_LOCK:
        lst = APPS_STATE["list"] or []
    exe_map = {e["exe"]: e["bundleId"] for e in lst if e.get("exe")}
    rsd = await _rsd_for_device()
    async with AppServiceService(rsd) as svc:
        toks = await svc.list_processes()
    running = []
    for tok in toks:
        nm, pid = _token_name(tok), _token_pid(tok)
        if not nm or pid is None:
            continue
        bid = exe_map.get(nm)
        if bid:
            running.append({"bundleId": bid, "pid": pid})
    return running


async def _launch_app_async(bundle_id):
    rsd = await _rsd_for_device()
    async with AppServiceService(rsd) as svc:
        await svc.launch_application(bundle_id)


async def _kill_pid_async(pid):
    rsd = await _rsd_for_device()
    async with AppServiceService(rsd) as svc:
        await svc.send_signal_to_process(pid, 9)


async def _fetch_icon_async(bundle_id):
    rsd = await _rsd_for_device()
    async with IconService(rsd) as svc:
        icon = await svc.fetch_icon(
            bundle_identifier=bundle_id, width=90.0, height=90.0, scale=2.0
        )
    return icon.png_data


def run_on_loop(coro, timeout=90):
    return asyncio.run_coroutine_threadsafe(coro, RS["loop"]).result(timeout)


# ── gesture worker ───────────────────────────────────────────────────────────

async def gesture_worker(queue):
    while True:
        try:
            rsds = await get_tunneld_devices(("127.0.0.1", TUNNELD_PORT))
            rsd = next((r for r in rsds if r.udid == UDID), None)
            if rsd is None:
                print("[!] tunneld has no matching device")
                await asyncio.sleep(3)
                continue
            print("[+] device via tunneld")

            indigo = IndigoHIDService(rsd)
            async with indigo as ibtn:
                print("[+] button channel armed")
                ACTION_CTX["indigo"] = ibtn
                try:
                    async with touch_session(rsd) as hid:
                        print("[+] GESTURES LIVE")
                        if ACTION_CTX["want_kb"]:
                            try:
                                ACTION_CTX["kb_attempt"] += 1
                                req = KEYBOARD_SURFACE_DEFAULT_SERVICE_ID + (
                                    ACTION_CTX["kb_attempt"] % 8
                                )
                                got = await hid.create_keyboard_service(service_id=req)
                                ACTION_CTX["kbd_id"] = got
                                ACTION_CTX["hid"] = hid
                                print(f"[+] virtual keyboard mounted (req {req:#x}, got {got:#x})")
                            except Exception as e:
                                print(f"[!] keyboard surface unavailable ({e}) - typing falls back to WDA")
                        else:
                            ACTION_CTX["hid"] = hid
                        await consume_and_stream(queue, hid)
                finally:
                    ACTION_CTX["indigo"] = None
                    ACTION_CTX["hid"] = None
                    ACTION_CTX["kbd_id"] = None
        except Exception as e:
            RSD_CACHE["rsd"] = None
            msg = str(e)
            if msg == "recycle requested":
                print("[*] HID session recycled")
                await asyncio.sleep(0.25)
            else:
                print(f"[!] dropped ({msg}) - retrying in 3s")
                await asyncio.sleep(3)


async def consume_and_stream(queue, hid):
    import random

    send_lock = ACTION_CTX["send_lock"]
    state = {"down": False, "x": norm(0.5), "y": norm(0.5)}
    lnx = lny = None
    snx = sny = 0.0
    last_move_wall = 0.0
    settling = False
    settled_done = False

    async def send_contact(x, y):
        async with send_lock:
            await hid.send_touchscreen(TOUCHSCREEN_STATE_CONTACT, x, y)

    async def send_release(x, y):
        async with send_lock:
            await hid.send_touchscreen(TOUCHSCREEN_STATE_RELEASE, x, y)

    async def perform_action(name):
        entry = NAMED_BUTTONS.get(name)
        if entry is None:
            print(f"[!] unknown action '{name}'")
            return
        indigo = ACTION_CTX["indigo"]
        if indigo is None:
            print("[!] no button channel - action dropped")
            return
        usage_page, usage_code, hold = entry
        print(f"[ius] action: {name}")
        await indigo.send_button(usage_page, usage_code, HID_BUTTON_STATE_DOWN)
        await asyncio.sleep(hold)
        await indigo.send_button(usage_page, usage_code, HID_BUTTON_STATE_UP)

    async def consumer():
        nonlocal lnx, lny, snx, sny, last_move_wall, settling, settled_done
        while True:
            job = await queue.get()
            kind = job.get("kind")
            if kind == "down":
                state["x"] = norm(job["fx"])
                state["y"] = norm(job["fy"])
                state["down"] = True
                lnx = lny = None
                snx = sny = 0.0
                settling = False
                settled_done = False
                last_move_wall = time.monotonic()
            elif kind == "move":
                state["x"] = norm(job["fx"])
                state["y"] = norm(job["fy"])
                nx = norm(job["fx"]); ny = norm(job["fy"])
                if lnx is not None:
                    ddx, ddy = nx - lnx, ny - lny
                    if abs(ddx) + abs(ddy) > 25:
                        snx, sny = ddx, ddy
                lnx, lny = nx, ny
                settling = False
                last_move_wall = time.monotonic()
            elif kind == "release":
                state["down"] = False
                state["x"] = norm(job["fx"])
                state["y"] = norm(job["fy"])
                await send_release(state["x"], state["y"])
                print("[ius] finger up")
            elif kind == "action":
                await perform_action(str(job.get("name") or ""))
            elif kind == "keyboard":
                want = bool(job.get("on"))
                if want != ACTION_CTX["want_kb"] or (want and ACTION_CTX["kbd_id"] is None):
                    ACTION_CTX["want_kb"] = want
                    if ACTION_CTX["hid"] is not None:
                        ACTION_CTX["recycle"] = True
                    elif want:
                        print("[!] arm requested but no HID session yet - will mount on connect")
                print(f"[ius] iphone keyboard {'hidden (virtual kb mounted)' if want else 'visible (virtual kb unmounted)'}")
            elif kind == "hid" or kind == "wda":
                WDA_QUEUE.put_nowait(job)

    task = asyncio.create_task(consumer())
    try:
        period = 1.0 / STREAM_HZ
        while True:
            if ACTION_CTX.get("recycle"):
                ACTION_CTX["recycle"] = False
                print("[*] recycling HID session (keyboard unmount)")
                raise RuntimeError("recycle requested")
            await asyncio.sleep(period)
            if not state["down"]:
                continue
            now = time.monotonic()
            active = (now - last_move_wall) < 0.12

            if active:
                settling = False
                jx = max(0, min(65535, state["x"] + random.randint(-2, 2)))
                jy = max(0, min(65535, state["y"] + random.randint(-2, 2)))
                await send_contact(jx, jy)
            elif not settled_done and (abs(snx) + abs(sny)) > 60:
                settling = True
                print("[ius] cursor froze - easing out...")
                gx = float(state["x"]); gy = float(state["y"])
                sxg = snx * 0.30; syg = sny * 0.30
                for _i in range(14):
                    await asyncio.sleep(0.016)
                    gx += sxg; gy += syg
                    sxg *= 0.70; syg *= 0.70
                    gx = min(65535, max(0, gx)); gy = min(65535, max(0, gy))
                    await send_contact(round(gx), round(gy))
                state["x"] = int(min(65535, max(0, gx)))
                state["y"] = int(min(65535, max(0, gy)))
                settled_done = True
                settling = False
                print("[ius] eased out - holding still")
            else:
                jx = max(0, min(65535, state["x"] + random.randint(-1, 1)))
                jy = max(0, min(65535, state["y"] + random.randint(-1, 1)))
                await send_contact(jx, jy)
    finally:
        task.cancel()
        print("[s] streamer stopped")


# ── HTTP + WebSocket server ─────────────────────────────────────────────────

PAGE = """<!doctype html>
<html><head><meta charset="utf-8"><title>HID test pad</title>
<style>
 body{background:#111;color:#ddd;font-family:ui-monospace,monospace;margin:0;padding:14px;text-align:center}
 #wrap{display:flex;justify-content:center}
 #pad{position:relative;height:min(84vh,860px);aspect-ratio:390/844;border:2px solid #4af;
      background:#16161d;cursor:crosshair;overflow:hidden;touch-action:none;user-select:none}
 .dot{position:absolute;width:9px;height:9px;margin:-4.5px;border-radius:50%;
      background:#4af;opacity:.55;pointer-events:none}
 .dot.rel{background:#f55}
 #ro{margin-top:10px;font-size:15px;white-space:pre}
 #log{margin-top:6px;font-size:12px;opacity:.75;max-height:140px;overflow:auto;text-align:left}
</style></head>
<body>
<h3>HID test pad &mdash; iPhone 390&times;844 pt</h3>
<div id="wrap"><div id="pad"></div></div>
<div id="ro">move mouse to see coords</div>
<div id="log"></div>
<script>
const pad=document.getElementById('pad'), ro=document.getElementById('ro'),
      lg=document.getElementById('log');
let down=false;

window.onerror=(m)=>log('JS ERROR: '+m);
function log(t){ const li=document.createElement('div'); li.textContent=t; lg.prepend(li); }
function frac(ev){
  const r=pad.getBoundingClientRect();
  const fx=Math.min(Math.max((ev.clientX-r.left)/r.width,0),1);
  const fy=Math.min(Math.max((ev.clientY-r.top)/r.height,0),1);
  return [fx,fy];
}
function dot(fx,fy,rel){
  const d=document.createElement('div'); d.className='dot'+(rel?' rel':'');
  d.style.left=(fx*100)+'%'; d.style.top=(fy*100)+'%';
  pad.appendChild(d); setTimeout(()=>d.remove(),1200);
}
const pending=[];
let wsReady=false;

async function send(o){
  const msg=JSON.stringify(o);
  if(wsReady){
    try{ ws.send(msg); }catch(e){ log('[send failed] '+e); }
  } else if(pending.length<240) pending.push(msg);
}

const ws=new WebSocket('ws://'+location.host+'/ws');
ws.binaryType='arraybuffer';
ws.onopen=()=>{ wsReady=true; setStatus('ws open');
  while(pending.length && ws.readyState===1){
    try{ ws.send(pending.shift()); }catch(e){}
  }
};
ws.onclose=()=>{ wsReady=false; setStatus('disconnected - reloading'); setTimeout(()=>location.reload(),1200); };
ws.onerror=()=>setStatus('ws error');

function track(fx,fy){
  const nx=Math.round(fx*65535), ny=Math.round(fy*65535);
  const px=(fx*390).toFixed(0), py=(fy*844).toFixed(0);
  ro.textContent=`fx=${fx.toFixed(3)} fy=${fy.toFixed(3)} | nx=${nx} ny=${ny} | ${px},${py} pt`;
  if(down){ dot(fx,fy,false); send({kind:'move',fx,fy}); }
}

pad.addEventListener('mousedown',e=>{
  e.preventDefault();
  const [fx,fy]=frac(e); down=true; dot(fx,fy,false);
  track(fx,fy); send({kind:'down',fx,fy});
});
if(window.PointerEvent){
  pad.addEventListener('pointermove',e=>track(...frac(e)));
  pad.addEventListener('pointerrawupdate',e=>track(...frac(e)));
}
window.addEventListener('mousemove',e=>track(...frac(e)));
window.addEventListener('mouseup',e=>{
  if(!down)return; down=false;
  const [fx,fy]=frac(e); dot(fx,fy,true);
  track(fx,fy); send({kind:'release',fx,fy}); log('gesture end');
});
setStatus('page loaded');
function setStatus(t){}
</script></body></html>
"""


class MiniWS:
    def __init__(self, sock: socket.socket):
        self.sock = sock
        self.lock = threading.Lock()
        self.closed = False

    def _frame(self, op: int, payload: bytes):
        with self.lock:
            if self.closed:
                return
            try:
                head = bytearray([0x80 | op])
                n = len(payload)
                if n < 126:
                    head.append(n)
                elif n <= 0xFFFF:
                    head.append(126)
                    head += int(n).to_bytes(2, "big")
                else:
                    head.append(127)
                    head += int(n).to_bytes(8, "big")
                self.sock.sendall(bytes(head) + payload)
            except OSError:
                self.closed = True

    def close(self):
        with self.lock:
            already = self.closed
            self.closed = True
        if not already:
            try:
                self.sock.close()
            except OSError:
                pass


def read_ws_frames(sock, ws, on_message):
    buf = b""
    try:
        while True:
            data = sock.recv(65536)
            if not data:
                break
            buf += data
            while True:
                if len(buf) < 2:
                    break
                op = buf[0] & 0x7F
                masked = (buf[1] & 0x80) != 0
                ln = buf[1] & 0x7F
                off = 2
                if ln == 126:
                    if len(buf) - off < 2:
                        break
                    ln = int.from_bytes(buf[off:off+2], "big"); off += 2
                elif ln == 127:
                    if len(buf) - off < 8:
                        break
                    ln = int.from_bytes(buf[off:off+8], "big"); off += 8
                mk = b""
                if masked:
                    if len(buf) - off < 4:
                        break
                    mk = buf[off:off+4]; off += 4
                if len(buf) - off < ln:
                    break
                payload = buf[off:off+ln]
                if masked and mk:
                    payload = bytes(p ^ mk[i % 4] for i, p in enumerate(payload))
                buf = buf[off+ln:]
                if op == 0x8:
                    ws.close()
                    return
                if op == 0x9:
                    ws._frame(0xA, payload)
                    continue
                on_message(op, payload)
    except OSError:
        pass


class CmdHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _json_response(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/apps.json":
            refresh = "refresh=1" in self.path
            with APPS_LOCK:
                need = refresh or APPS_STATE["list"] is None
            if need:
                try:
                    lst = run_on_loop(_fetch_apps_async())
                except Exception as e:
                    self._json_response(502, {"error": f"app list unavailable: {e}"})
                    return
                with APPS_LOCK:
                    APPS_STATE["list"] = lst
            with APPS_LOCK:
                lst = APPS_STATE["list"] or []
            self._json_response(200, {"apps": [
                {"bundleId": e["bundleId"], "name": e["name"]} for e in lst
            ]})
            return

        if path == "/apps/running.json":
            try:
                running = run_on_loop(_running_apps_async())
            except Exception as e:
                self._json_response(502, {"error": f"process list unavailable: {e}"})
                return
            self._json_response(200, {"running": running})
            return

        if path.startswith("/icon/") and path.endswith(".png"):
            bid = urllib.parse.unquote(path[len("/icon/"):-len(".png")])
            data = ICON_MEM.get(bid)
            if data is None:
                safe = bid.replace("/", "_")
                disk = ICON_CACHE_DIR / f"{safe}.png"
                if disk.is_file():
                    data = disk.read_bytes()
                else:
                    try:
                        data = run_on_loop(_fetch_icon_async(bid))
                    except Exception:
                        self.send_error(404)
                        return
                    disk.parent.mkdir(parents=True, exist_ok=True)
                    disk.write_bytes(data)
                ICON_MEM[bid] = data
            body = data
            self.send_response(200)
            self.send_header("Content-Type", "image/png")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Cache-Control", "public, max-age=86400")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        if path in ("/", "/index.html"):
            body = PAGE.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if path == "/ws" and self.headers.get("Upgrade"):
            key = self.headers.get("Sec-WebSocket-Key", "")
            accept = base64.b64encode(
                hashlib.sha1((key + WS_GUID).encode()).digest()).decode()
            resp = ("HTTP/1.1 101 Switching Protocols\r\n"
                    "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                    f"Sec-WebSocket-Accept: {accept}\r\n\r\n")
            self.wfile.write(resp.encode())
            self.wfile.flush()
            self.close_connection = False
            ws = MiniWS(self.connection)

            def on_message(op, payload):
                if op != 0x1:
                    return
                txt = payload.decode(errors="replace")
                try:
                    job = json.loads(txt)
                except Exception:
                    return
                loop, q = RS["loop"], RS["queue"]
                if loop and q is not None:
                    loop.call_soon_threadsafe(q.put_nowait, job)

            reader = threading.Thread(target=read_ws_frames,
                                      args=(self.connection, ws, on_message),
                                      daemon=True)
            reader.start()

            import time as _time
            while not ws.closed:
                _time.sleep(0.25)
            self.close_connection = True
            return
        self.send_error(404)

    def do_POST(self):
        path = self.path.split("?")[0]
        if path.startswith("/app/launch/"):
            bid = urllib.parse.unquote(path[len("/app/launch/"):])
            try:
                run_on_loop(_launch_app_async(bid))
            except Exception as e:
                self._json_response(502, {"error": f"launch failed: {e}"})
                return
            print(f"[ius] launch {bid}")
            self._json_response(200, {"ok": True})
            return
        if path.startswith("/app/kill/"):
            try:
                pid = int(urllib.parse.unquote(path.rsplit("/", 1)[1]))
                run_on_loop(_kill_pid_async(pid))
            except Exception as e:
                self._json_response(502, {"error": f"kill failed: {e}"})
                return
            print(f"[ius] kill pid {pid}")
            self._json_response(200, {"ok": True})
            return
        self.send_error(404)

    def log_message(self, *a):
        pass


# ── main ─────────────────────────────────────────────────────────────────────

async def amain():
    global UDID

    # ── parse args ───────────────────────────────────────────────────────
    args = sys.argv[1:]
    skip_wda = "--no-wda" in args
    udid_arg = None
    if "--udid" in args:
        idx = args.index("--udid")
        if idx + 1 < len(args):
            udid_arg = args[idx + 1]

    # ── detect device ────────────────────────────────────────────────────
    UDID = udid_arg or detect_udid()
    if not UDID:
        print("[!] no device detected - is it connected via USB?")
        print("    try: idevice_id -l")
        print("    or:  python3 tools/ius.py --udid <UDID>")
        sys.exit(1)
    print(f"[*] device: {UDID}")

    # ── start iproxy tunnels ─────────────────────────────────────────────
    start_iproxy_tunnels()

    # ── start tunneld ────────────────────────────────────────────────────
    _ensure_tunneld()
    await _wait_tunneld()

    # ── launch WDA on device ─────────────────────────────────────────────
    if not skip_wda:
        if _wda_alive():
            print("[+] WDA already running")
        else:
            _wda_launch_runner()

    # ── boot HID controller ──────────────────────────────────────────────
    RS["loop"] = asyncio.get_running_loop()
    RS["queue"] = asyncio.Queue()
    ACTION_CTX["send_lock"] = asyncio.Lock()
    asyncio.create_task(gesture_worker(RS["queue"]))
    asyncio.create_task(wda_writer())

    # ── warm app list ────────────────────────────────────────────────────
    async def warm_apps():
        await asyncio.sleep(1.5)
        try:
            lst = await _fetch_apps_async()
        except Exception as e:
            print(f"[!] app list warmup failed: {e}")
            return
        with APPS_LOCK:
            APPS_STATE["list"] = lst

    asyncio.create_task(warm_apps())

    # ── start HTTP server ────────────────────────────────────────────────
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 9001), CmdHandler)
    print("[+] IUS ready")
    print("    HID test pad : http://127.0.0.1:9001/")
    print("    MJPEG stream : http://127.0.0.1:9100/stream?fps=15&scale=0.5&q=0.4")
    print("    H.264 stream : http://127.0.0.1:9100/stream.html")
    print("    WDA          : http://127.0.0.1:8100/status")
    try:
        await asyncio.get_running_loop().run_in_executor(None, srv.serve_forever)
    finally:
        print("\n[*] shutting down...")
        stop_iproxy_tunnels()
        stop_tunneld()


if __name__ == "__main__":
    asyncio.run(amain())
