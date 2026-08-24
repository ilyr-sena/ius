#!/usr/bin/env python3
"""Native remote control over the developer tunnel - no WDA involved.

Opens the CoreDevice HID touch surface through a userspace tunnel and
serves a browser page showing the live MJPEG feed. Mouse taps, long-presses,
swipes and the home button are injected as native HID events.

Run:  python3 tools/remote_control.py
Then: python3 tools/stream_run.py            (or --wda, for the 8100 tunnel)
Open: http://127.0.0.1:9000/
"""
import asyncio
import http.server
import json

from pymobiledevice3.remote.core_device.hid_service import (
    IndigoHIDService,
    TOUCHSCREEN_STATE_CONTACT,
    TOUCHSCREEN_STATE_RELEASE,
    build_touchscreen_report,
    touch_session,
)
from pymobiledevice3.remote.remote_service_discovery import (
    RemoteServiceDiscoveryService,
)
from pymobiledevice3.remote.tunnel_service import (
    get_remote_pairing_tunnel_services,
    start_tunnel_over_remotepairing,
)

UDID = "00008110-000C694914F3801E"

PAGE = """<!doctype html>
<html><head><meta charset="utf-8"><title>IUS remote</title>
<style>
 body{background:#0b0b0f;color:#ddd;font-family:ui-monospace,monospace;text-align:center;margin:0;padding:10px}
 #img{max-width:96vw;max-height:86vh;border:1px solid #333;cursor:crosshair;background:#000;
      user-select:none;-webkit-user-drag:none}
 #s{margin-top:8px;font-size:13px;opacity:.85}
 button{font-family:inherit;background:#222;color:#ddd;border:1px solid #444;padding:4px 10px;margin:2px}
</style></head>
<body>
<h3>IUS remote (native HID)</h3>
<img id="img" draggable="false" src="http://127.0.0.1:9100/stream?fps=25&scale=0.5&q=0.5">
<div id="s">connecting...</div>
<div><button id="home">home</button></div>
<script>
const img = document.getElementById('img'), st = document.getElementById('s');
const ws = new WebSocket('ws://' + location.host + '/cmd');
ws.onopen = () => setStatus('connected - tap/drag on the image');
ws.onclose = () => setStatus('disconnected');
function setStatus(t){ st.textContent = t; }

let dragging=false, swiping=false, sx=0, sy=0, lx=0, ly=0, t0=0, lastSent=0;

function frac(ev){
  const r = img.getBoundingClientRect();
  const fx = Math.min(Math.max((ev.clientX - r.left) / r.width, 0), 1);
  const fy = Math.min(Math.max((ev.clientY - r.top) / r.height, 0), 1);
  return [fx, fy];
}
function send(o){ ws.send(JSON.stringify(o)); }

img.addEventListener('mousedown', ev => {
  if (ev.button !== 0) return;
  ev.preventDefault();
  [sx, sy] = frac(ev); lx = sx; ly = sy;
  dragging = true; swiping = false; t0 = performance.now();
});
window.addEventListener('mousemove', ev => {
  if (!dragging) return;
  const [fx, fy] = frac(ev);
  if (!swiping && Math.hypot(fx - sx, fy - sy) * 100 > 1.5) swiping = true;
  if (swiping) {
    const now = performance.now();
    if (now - lastSent >= 100) {
      lastSent = now;
      send({kind:'move', fx, fy});
      lx = fx; ly = fy;
    }
  }
});
window.addEventListener('mouseup', ev => {
  if (!dragging) return;
  dragging = false;
  const [fx, fy] = frac(ev);
  if (swiping) send({kind:'release', fx, fy});
  else send({kind:'tap', fx, fy, holdMs: Math.round(performance.now() - t0)});
});
document.getElementById('home').addEventListener('click', () => send({kind:'home'}));
</script></body></html>
"""


class RelayState:
    def __init__(self):
        self.loop = None
        self.queue = None


RS = RelayState()


class CmdHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path in ("/", "/index.html"):
            body = PAGE.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path != "/cmd":
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length)
        try:
            job = json.loads(raw)
        except Exception:
            self.send_error(400)
            return
        loop, q = RS.loop, RS.queue
        if loop is None or q is None:
            self.send_error(503)
            return

        def enqueue():
            q.put_nowait(job)

        loop.call_soon_threadsafe(enqueue)
        body = b'{"queued":true}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


def start_http():
    import http.server
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 9000), CmdHandler)
    srv.serve_forever()


# ---- HID gesture execution --------------------------------------------------

def norm(frac_val: float) -> int:
    return max(0, min(65535, round(frac_val * 65535)))


async def run_job(hid, indigo, job):
    kind = job.get("kind")

    if kind == "tap":
        x, y = norm(job["fx"]), norm(job["fy"])
        await hid.send_touchscreen(build_touchscreen_report(
            TOUCHSCREEN_STATE_CONTACT, x, y))
        await asyncio.sleep(max(0.02, job.get("holdMs", 60) / 1000))
        await hid.send_touchscreen(build_touchscreen_report(
            TOUCHSCREEN_STATE_RELEASE, x, y))

    elif kind == "move":
        x, y = norm(job["fx"]), norm(job["fy"])
        await hid.send_touchscreen(build_touchscreen_report(
            TOUCHSCREEN_STATE_CONTACT, x, y))
        await asyncio.sleep(0.008)

    elif kind == "release":
        x, y = norm(job["fx"]), norm(job["fy"])
        await hid.send_touchscreen(build_touchscreen_report(
            TOUCHSCREEN_STATE_RELEASE, x, y))

    elif kind == "home":
        await indigo.send_button(0x0C, 0x40, 1)   # Consumer/Menu DOWN
        await asyncio.sleep(0.05)
        await indigo.send_button(0x0C, 0x40, 2)   # UP


async def gesture_worker(queue):
    """Reconnect loop: keeps the userspace tunnel + authenticated HID session alive."""
    from pymobiledevice3.remote import userspace_tunnel

    while True:
        rsd = None
        try:
            print("[*] establishing userspace tunnel over USB")
            rsd = await userspace_tunnel.establish_userspace_rsd(serial=UDID)
            print(f"[+] tunnel up ({getattr(rsd, 'name', None) or UDID})")

            async def with_hid():
                indigo = IndigoHIDService(rsd)
                async with indigo as ibtn:
                    print("[+] button channel armed")
                    async with touch_session(rsd) as hid:
                        print("[+] touch stream authenticated - gestures live")
                        while True:
                            job = await queue.get()
                            await run_job(hid, ibtn, job)

            await with_hid()
        except asyncio.CancelledError:
            raise
        except Exception as e:
            print(f"[!] session dropped ({e}) - retrying in 3s")
            await asyncio.sleep(3)
        finally:
            if rsd is not None:
                try:
                    await rsd.close()
                except Exception:
                    pass


async def amain():
    RS.loop = asyncio.get_running_loop()
    RS.queue = asyncio.Queue()
    asyncio.create_task(gesture_worker(RS.queue))
    await asyncio.get_running_loop().run_in_executor(None, start_http)


if __name__ == "__main__":
    print("[*] starting native HID remote (no WDA)")
    print("    open http://127.0.0.1:9000/")
    asyncio.run(amain())
