#!/usr/bin/env python3
"""Minimal native-HID remote - persistent 60Hz contact-stream edition.

Run:   PYTHONUNBUFFERED=1 python3 tools/hid_test.py
Open:  http://127.0.0.1:9001/
"""
import asyncio
import base64
import random
import hashlib
import http.server
import json
import socket
import threading

from pymobiledevice3.remote.core_device.hid_service import (
    IndigoHIDService,
    TOUCHSCREEN_STATE_CONTACT,
    TOUCHSCREEN_STATE_RELEASE,
    touch_session,
)
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


# ---- HTTP server with WS upgrade --------------------------------------------

RS = {"loop": None, "queue": None}


class CmdHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        path = self.path.split("?")[0]
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
                async with touch_session(rsd) as hid:
                    print("[+] GESTURES LIVE")
                    await consume_and_stream(queue, hid)
        except Exception as e:
            print(f"[!] dropped ({e}) - retrying in 3s")
            await asyncio.sleep(3)


async def consume_and_stream(queue, hid):
    import random
    import time as _t

    SEND_LOCK = asyncio.Lock()
    AUTO_LIFT_AFTER = 0.30
    FLING_SPEED_MIN = 0.60        # frac/s - below this, stopping is just stopping

    state = {"down": False, "x": norm(0.5), "y": norm(0.5)}
    kin = {"lx": None, "ly": None, "lt": None, "vx": 0.0, "vy": 0.0}
    snx = 0.0; sny = 0.0
    last_move_wall = None
    settling = False
    settled_done = False
    settled_at = 0.0
    contacted = True

    async def send_contact(x, y):
        async with SEND_LOCK:
            await hid.send_touchscreen(TOUCHSCREEN_STATE_CONTACT, x, y)

    async def send_release(x, y):
        async with SEND_LOCK:
            await hid.send_touchscreen(TOUCHSCREEN_STATE_RELEASE, x, y)

    async def consumer():
        nonlocal snx, sny, last_move_wall
        nonlocal settling, settled_done, settled_at, contacted
        while True:
            job = await queue.get()
            kind = job.get("kind")
            t = _t.monotonic()

            if kind == "down":
                kin["lx"], kin["ly"], kin["lt"] = job["fx"], job["fy"], t
                kin["vx"] = kin["vy"] = 0.0
                state["down"] = True
                state["fx"], state["fy"] = job["fx"], job["fy"]
                contacted = True
                lnx = lny = None
                snx = sny = 0.0
                settling = False
                settled_done = False
                settled_at = 0.0
                last_move_wall = t
                await send_contact(state["x"], state["y"])

            elif kind == "move":
                dt = t - kin["lt"] if kin["lt"] else 0.016
                if dt <= 0: dt = 0.001
                nvx = (job["fx"] - kin["lx"]) / dt
                nvy = (job["fy"] - kin["ly"]) / dt
                kin["vx"] = 0.65 * kin["vx"] + 0.35 * nvx
                kin["vy"] = 0.65 * kin["vy"] + 0.35 * nvy
                kin["lx"], kin["ly"], kin["lt"] = job["fx"], job["fy"], t
                state["x"] = norm(job["fx"])
                state["y"] = norm(job["fy"])
                nx = norm(job["fx"]); ny = norm(job["fy"])
                if lnx is not None:
                    ddx, ddy = nx - lnx, ny - lny
                    if abs(ddx) + abs(ddy) > 25:
                        snx, sny = ddx, ddy
                lnx, lny = nx, ny
                settling = False
                last_move_wall = t

            elif kind == "release":
                state["down"] = False                   # stops streamer+easeout
                x = norm(job["fx"]); y = norm(job["fy"])
                state["x"], state["y"] = x, y
                await send_release(x, y)
                print("[ius] finger up")

    task = asyncio.create_task(consumer())
    try:
        period = 1.0 / STREAM_HZ
        while True:
            await asyncio.sleep(period)
            if not state["down"]:
                continue
            now = _t.monotonic()

            recent_move = (now - last_move_wall) < 0.12
            speed = (kin["vx"]**2 + kin["vy"]**2) ** 0.5

            if recent_move:
                settling = False
                jx = max(0, min(65535, state["x"] + random.randint(-2, 2)))
                jy = max(0, min(65535, state["y"] + random.randint(-2, 2)))
                await send_contact(jx, jy)
                continue

            # cursor froze: decide based on how fast the finger was moving
            if not settled_done and speed >= FLING_SPEED_MIN and \
                    (abs(snx) + abs(sny)) > 60:
                # FAST swipe froze: ease-out to bleed the fling off, then
                # auto-lift so no phantom swipe fires on button release
                settling = True
                print("[ius] fast swipe froze - easing out...")
                gx = float(state["x"]); gy = float(state["y"])
                sxg = snx * 0.30; syg = sny * 0.30
                for _i in range(14):
                    if not state["down"]:
                        break                           # released mid-ease
                    await asyncio.sleep(0.016)
                    gx += sxg; gy += syg
                    sxg *= 0.70; syg *= 0.70
                    gx = min(65535, max(0, gx)); gy = min(65535, max(0, gy))
                    await send_contact(round(gx), round(gy))
                state["x"] = int(min(65535, max(0, gx)))
                state["y"] = int(min(65535, max(0, gy)))
                settled_done = True
                settled_at = _t.monotonic()
            elif not settled_done:
                # SLOW swipe froze: low velocity, no fling risk -> simply
                # hold the contact still; lifting will be a clean stop
                pass

            if settling and settled_done:
                if (now - settled_at) > AUTO_LIFT_AFTER:
                    await send_release(state["x"], state["y"])
                    contacted = False
                    print("[ius] auto-lifted after settle")
            else:
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

    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 9001), CmdHandler)
    print("[*] test pad: http://127.0.0.1:9001/")
    await asyncio.get_running_loop().run_in_executor(None, srv.serve_forever)


if __name__ == "__main__":
    asyncio.run(amain())
