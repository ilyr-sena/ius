#!/usr/bin/env python3
"""Minimal native-HID remote - persistent 60Hz contact-stream edition.

Run:   PYTHONUNBUFFERED=1 python3 tools/hid_test.py
Open:  http://127.0.0.1:9001/
"""
import asyncio
import base64
import hashlib
import http.server
import json
import pathlib
import socket
import threading
import urllib.parse

from pymobiledevice3.remote.core_device.app_service import AppServiceService
from pymobiledevice3.remote.core_device.hid_service import (
    HID_BUTTON_STATE_DOWN,
    HID_BUTTON_STATE_UP,
    IndigoHIDService,
    TOUCHSCREEN_STATE_CONTACT,
    TOUCHSCREEN_STATE_RELEASE,
    touch_session,
)
from pymobiledevice3.remote.core_device.icon_service import IconService
from pymobiledevice3.remote.remote_service_discovery import (
    RemoteServiceDiscoveryService,
)
from pymobiledevice3.tunneld.api import get_tunneld_devices

UDID = "00008110-000C694914F3801E"
WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
STREAM_HZ = 60                     # digitizer sample cadence while finger down

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


# ---- tiny websocket over a raw socket ---------------------------------------

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
    """Blocking reader thread: parses websocket frames from the raw socket."""
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


# ---- installed apps / icons over tunneld ------------------------------------

async def _rsd_for_device():
    rsds = await get_tunneld_devices(("127.0.0.1", 49151))
    rsd = next((r for r in rsds if r.udid == UDID), None)
    if rsd is None:
        raise RuntimeError("tunneld has no matching device")
    return rsd


async def _fetch_apps_async():
    """Downloaded (User) apps + regularly-used stock apps; User apps first."""
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
    for a in raw:
        bid = a.get("bundleIdentifier") or a.get("CFBundleIdentifier")
        if not bid or bid in seen:
            continue
        kind = str(a.get("applicationType") or "").lower()
        if kind != "user" and bid not in STOCK_BUNDLES:
            continue
        name = str(a.get("displayName") or a.get("name") or "").strip()
        if not name:
            continue
        # executable name from the bundle path: ".../Foo.app/Foo" -> "Foo"
        bpath = str(a.get("applicationBundlePath") or a.get("path") or "")
        exe = bpath.split(".app/")[-1].strip("/") if ".app/" in bpath else ""
        out.append({"bundleId": bid, "name": name, "exe": exe,
                    "user": kind == "user"})
        seen.add(bid)
    out.sort(key=lambda e: (0 if e.pop("user") else 1, e["name"].lower()))
    print(f"[+] app list: {len(out)} apps")
    return out


def _token_name(tok):
    """Best-effort executable name from a CoreDevice process token."""
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
    """[{bundleId, pid}] for app processes currently alive on the device."""
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
        await svc.send_signal_to_process(pid, 9)   # SIGKILL


async def _fetch_icon_async(bundle_id):
    rsd = await _rsd_for_device()
    async with IconService(rsd) as svc:
        icon = await svc.fetch_icon(
            bundle_identifier=bundle_id, width=90.0, height=90.0, scale=2.0
        )
    return icon.png_data


def run_on_loop(coro, timeout=90):
    """Run an async pymobiledevice3 call from the HTTP thread pool."""
    return asyncio.run_coroutine_threadsafe(coro, RS["loop"]).result(timeout)


# ---- HTTP server with WS upgrade --------------------------------------------

RS = {"loop": None, "queue": None}
ACTION_CTX = {"indigo": None}

# ---- installed-apps cache (CoreDevice appservice + iconservice) --------------
ICON_CACHE_DIR = pathlib.Path.home() / ".cache" / "ius" / "icons"
APPS_LOCK = threading.Lock()
APPS_STATE = {"list": None}      # [{"bundleId","name","exe"}] once warmed
ICON_MEM = {}                    # bundleId -> png bytes

# Regularly-used stock apps worth showing alongside downloaded (User) apps.
# Wrong/missing IDs simply don't appear - safe to extend freely.
STOCK_BUNDLES = {
    "com.apple.Preferences",         # Settings
    "com.apple.camera",              # Camera
    "com.apple.AppStore",            # App Store
    "com.apple.mobilenotes",         # Notes
    "com.apple.calculator",          # Calculator
    "com.apple.mobilecal",           # Calendar
    "com.apple.mobiletimer",         # Clock
    "com.apple.DocumentsApp",        # Files
    "com.apple.MobileSMS",           # Messages
    "com.apple.mail",                # Mail
    "com.apple.Maps",                # Maps
    "com.apple.mobilesafari",        # Safari
    "com.apple.mobileslideshow",     # Photos
    "com.apple.Music",               # Music
    "com.apple.weather",             # Weather
    "com.apple.reminders",           # Reminders
    "com.apple.Health",              # Health
    "com.apple.Wallet",              # Wallet
    "com.apple.facetime",            # FaceTime
    "com.apple.MobileAddressBook",   # Contacts
    "com.apple.podcasts",            # Podcasts
    "com.apple.iBooks",              # Books
    "com.apple.news",                # News
    "com.apple.tv",                  # TV
    "com.apple.Home",                # Home
    "com.apple.stocks",              # Stocks
    "com.apple.shortcuts",           # Shortcuts
    "com.apple.freeform",            # Freeform
    "com.apple.Journal",             # Journal
    "com.apple.VoiceMemos",          # Voice Memos
    "com.apple.compass",             # Compass
    "com.apple.measure",             # Measure
    "com.apple.findmy",              # Find My
    "com.apple.Fitness",             # Fitness
    "com.apple.Translate",           # Translate
    "com.apple.Bridge",              # Watch
}

# Named iOS hardware buttons -> (usage_page, usage_code, hold_seconds).
# Mirrors pymobiledevice3's own `developer core-device hid button` mapping:
# most physical buttons live on the Consumer page (0x0C); hold time is what
# makes iOS distinguish a tap (home/vol) from a press-and-hold action (lock).
NAMED_BUTTONS = {
    "home": (0x0C, 0x40, 0.05),        # Consumer / Menu
    "lock": (0x0C, 0x30, 0.5),         # Consumer / Power, held to sleep
    "volume-up": (0x0C, 0xE9, 0.05),
    "volume-down": (0x0C, 0xEA, 0.05),
    "mute": (0x0C, 0xE2, 0.05),
    "siri": (0x0C, 0xCF, 1.0),
}


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
            self.close_connection = False   # socket belongs to the ws layer now
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

            # park this handler thread; base class must not touch/close socket
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


# ---- HID session over tunneld ------------------------------------------------

async def gesture_worker(queue):
    while True:
        try:
            rsds = await get_tunneld_devices(("127.0.0.1", 49151))
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
                        await consume_and_stream(queue, hid)
                finally:
                    ACTION_CTX["indigo"] = None
        except Exception as e:
            print(f"[!] dropped ({e}) - retrying in 3s")
            await asyncio.sleep(3)


async def consume_and_stream(queue, hid):
    import random
    import time as _t

    send_lock = asyncio.Lock()
    state = {"down": False, "x": norm(0.5), "y": norm(0.5)}
    lnx = None
    lny = None
    snx = 0.0
    sny = 0.0
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
                last_move_wall = _t.monotonic()
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
                last_move_wall = _t.monotonic()
            elif kind == "release":
                state["down"] = False
                state["x"] = norm(job["fx"])
                state["y"] = norm(job["fy"])
                await send_release(state["x"], state["y"])
                print("[ius] finger up")
            elif kind == "action":
                await perform_action(str(job.get("name") or ""))

    task = asyncio.create_task(consumer())
    try:
        period = 1.0 / STREAM_HZ
        while True:
            await asyncio.sleep(period)
            if not state["down"]:
                continue
            now = _t.monotonic()
            active = (now - last_move_wall) < 0.12

            if active:
                settling = False
                jx = max(0, min(65535, state["x"] + random.randint(-2, 2)))
                jy = max(0, min(65535, state["y"] + random.randint(-2, 2)))
                await send_contact(jx, jy)
            elif not settled_done and (abs(snx) + abs(sny)) > 60:
                # cursor froze mid-gesture: ease-out along last direction
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
                # settled or stationary hold: gentle jitter keeps contact alive
                jx = max(0, min(65535, state["x"] + random.randint(-1, 1)))
                jy = max(0, min(65535, state["y"] + random.randint(-1, 1)))
                await send_contact(jx, jy)
    finally:
        task.cancel()
        print("[s] streamer stopped")


def norm(v):
    return max(0, min(65535, round(v * 65535)))


async def amain():
    RS["loop"] = asyncio.get_running_loop()
    RS["queue"] = asyncio.Queue()
    asyncio.create_task(gesture_worker(RS["queue"]))

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

    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 9001), CmdHandler)
    print("[*] test pad: http://127.0.0.1:9001/")
    await asyncio.get_running_loop().run_in_executor(None, srv.serve_forever)


if __name__ == "__main__":
    asyncio.run(amain())
