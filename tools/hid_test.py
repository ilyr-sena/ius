#!/usr/bin/env python3
"""Minimal native-HID remote - low-latency WebSocket edition.

Run:   python3 tools/hid_test.py
Open:  http://127.0.0.1:9001/
"""
import asyncio
import base64
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
from pymobiledevice3.remote.tunnel_service import (
    get_remote_pairing_tunnel_services,
    start_tunnel_over_remotepairing,
)
from pymobiledevice3.tunneld.api import get_tunneld_devices

UDID = "00008110-000C694914F3801E"
WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

PAGE = """<!doctype html>
<html><head><meta charset="utf-8"><title>HID test pad</title>
<style>
 body{background:#111;color:#ddd;font-family:ui-monospace,monospace;margin:0;padding:14px;text-align:center}
 #wrap{display:flex;justify-content:center}
 #pad{position:relative;height:min(84vh,860px);aspect-ratio:390/844;border:2px solid #4af;
      background:#16161d;cursor:crosshair;overflow:hidden;touch-action:none;user-select:none}
 .dot{position:absolute;width:9px;height:9px;margin:-4.5px;border-radius:50%;
      background:#4af;opacity:.9;pointer-events:none}
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
async function send(o){
  try{ ws.send(JSON.stringify(o)); }catch(e){ log('[send failed] '+e); }
}

const ws=new WebSocket('ws://'+location.host+'/ws');
ws.binaryType='arraybuffer';
ws.onopen=()=>setStatus('ws open');
ws.onclose=()=>{ setStatus('disconnected - reloading'); setTimeout(()=>location.reload(),1200); };
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

    def send_text(self, t: str):
        self._frame(0x1, t.encode())

    def send_binary(self, d: bytes):
        self._frame(0x2, d)

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


def norm(v):
    return max(0, min(65535, round(v * 65535)))


async def run_job(hid, indigo, job):
    kind = job.get("kind")
    if kind == "down":
        await hid.send_touchscreen(TOUCHSCREEN_STATE_CONTACT,
                                   norm(job["fx"]), norm(job["fy"]))
    elif kind == "move":
        await hid.send_touchscreen(TOUCHSCREEN_STATE_CONTACT,
                                   norm(job["fx"]), norm(job["fy"]))
    elif kind == "release":
        await hid.send_touchscreen(TOUCHSCREEN_STATE_RELEASE,
                                   norm(job["fx"]), norm(job["fy"]))


class CmdHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _send_html(self):
        body = PAGE.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = self.path.split("?")[0]
        if path in ("/", "/index.html"):
            self._send_html()
            return
        if path == "/ws" and "upgrade" in self.headers.get(
                "Upgrade", "").lower():
            key = self.headers.get("Sec-WebSocket-Key", "")
            accept = base64.b64encode(
                hashlib.sha1((key + WS_GUID).encode()).digest()).decode()
            resp = ("HTTP/1.1 101 Switching Protocols\r\n"
                    "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                    f"Sec-WebSocket-Accept: {accept}\r\n\r\n")
            self.wfile.write(resp.encode())
            self.close_connection = True
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
            return
        self.send_error(404)

    def do_POST(self):
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
                    while True:
                        job = await queue.get()
                        try:
                            await run_job(hid, ibtn, job)
                        except Exception as e:
                            print(f"[!] gesture error: {e}")
        except Exception as e:
            print(f"[!] dropped ({e}) - retrying in 3s")
            await asyncio.sleep(3)


async def amain():
    RS["loop"] = asyncio.get_running_loop()
    RS["queue"] = asyncio.Queue()
    asyncio.create_task(gesture_worker(RS["queue"]))

    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 9001), CmdHandler)
    print("[*] test pad: http://127.0.0.1:9001/")
    await asyncio.get_running_loop().run_in_executor(None, srv.serve_forever)


if __name__ == "__main__":
    asyncio.run(amain())
