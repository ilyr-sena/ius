#!/usr/bin/env python3
"""Minimal HID gesture test pad - no video, no MJPEG, no WDA.

Run:   python3 tools/hid_test.py
Open:  http://127.0.0.1:9001/
Draw/tap inside the box; every gesture is injected as a native HID touch
at the matching normalized screen position.
"""
import asyncio
import http.server
import json
import threading

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
<html><head><meta charset="utf-8"><title>HID test pad</title>
<style>
 body{background:#111;color:#ddd;font-family:ui-monospace,monospace;margin:0;padding:14px}
 #pad{position:relative;width:82vw;height:68vh;border:2px solid #4af;background:#16161d;
      cursor:crosshair;overflow:hidden;touch-action:none}
 .dot{position:absolute;width:10px;height:10px;margin:-5px;border-radius:50%;
      background:#4af;opacity:.85;pointer-events:none}
 .dot.rel{background:#f55}
 #readout{margin-top:10px;font-size:15px;white-space:pre}
 #log{margin-top:8px;font-size:12px;opacity:.8;max-height:180px;overflow:auto;text-align:left}
</style></head>
<body>
<h3>HID gesture test pad</h3>
<div id="pad"></div>
<div id="readout">fx=- fy=- | nx=- ny=-</div>
<div id="log"></div>
<script>
const pad=document.getElementById('pad'),ro=document.getElementById('readout'),
      lg=document.getElementById('log');
let dragging=false,sx=0,sy=0,lastSent=0,t0=0;

window.onerror=(m)=>{ log('JS ERROR: '+m); };
function log(t){
  const li=document.createElement('div');li.textContent=t;lg.prepend(li);
}
function frac(ev){
  const r=pad.getBoundingClientRect();
  const fx=Math.min(Math.max((ev.clientX-r.left)/r.width,0),1);
  const fy=Math.min(Math.max((ev.clientY-r.top)/r.height,0),1);
  return [fx,fy];
}
function dot(fx,fy,rel){
  const d=document.createElement('div');d.className='dot'+(rel?' rel':'');
  d.style.left=(fx*100)+'%';d.style.top=(fy*100)+'%';
  pad.appendChild(d);setTimeout(()=>d.remove(),1500);
}
async function send(o){
  try {
    const r=await fetch('/cmd',{method:'POST',
      headers:{'Content-Type':'application/json'},body:JSON.stringify(o)});
    if(!r.ok) log('[http '+r.status+']');
  } catch(e){ log('[send failed] '+e); }
}

pad.addEventListener('mousedown',e=>{
  e.preventDefault();
  const [fx,fy]=frac(e); sx=fx;sy=fy;
  dragging=true;t0=performance.now();
  dot(fx,fy,false);
  send({kind:'down',fx,fy});
});
window.addEventListener('mousemove',e=>{
  const [fx,fy]=frac(e);
  ro.textContent=`fx=${fx.toFixed(3)} fy=${fy.toFixed(3)} | nx=${Math.round(fx*65535)} ny=${Math.round(fy*65535)}`;
  if(!dragging)return;
  dot(fx,fy,false);
  const now=performance.now();
  if(Math.hypot(fx-sx,fy-sy)*65535>300 && now-lastSent>35){
    lastSent=now; send({kind:'move',fx,fy});
  }
});
window.addEventListener('mouseup',e=>{
  if(!dragging)return; dragging=false;
  const [fx,fy]=frac(e); dot(fx,fy,true);
  send({kind:'release',fx,fy});
  const held=Math.round(performance.now()-t0);
  log(`gesture done (held ${held}ms)`);
});
</script></body></html>
"""


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
        try:
            job = json.loads(self.rfile.read(length))
        except Exception:
            self.send_error(400)
            return
        loop, q = RS["loop"], RS["queue"]

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


RS = {"loop": None, "queue": None}


def norm(v):
    return max(0, min(65535, round(v * 65535)))


async def run_job(hid, indigo, job):
    kind = job.get("kind")

    if kind == "down" or kind == "move":
        x, y = norm(job["fx"]), norm(job["fy"])
        await hid.send_touchscreen(build_touchscreen_report(
            TOUCHSCREEN_STATE_CONTACT, x, y))
        await asyncio.sleep(0.008)

    elif kind == "tap":
        x, y = norm(job["fx"]), norm(job["fy"])
        await hid.send_touchscreen(build_touchscreen_report(
            TOUCHSCREEN_STATE_CONTACT, x, y))
        await asyncio.sleep(max(0.02, job.get("holdMs", 60) / 1000))
        await hid.send_touchscreen(build_touchscreen_report(
            TOUCHSCREEN_STATE_RELEASE, x, y))

    elif kind == "swipe":
        steps = int(job.get("steps", 24))
        dur = float(job.get("durMs", 350)) / 1000.0
        x1, y1 = norm(job["x1"]), norm(job["y1"])
        x2, y2 = norm(job["x2"]), norm(job["y2"])
        await hid.send_touchscreen(build_touchscreen_report(
            TOUCHSCREEN_STATE_CONTACT, x1, y1))
        per = dur / max(steps, 1)
        for i in range(1, steps + 1):
            await asyncio.sleep(per)
            xi = x1 + (x2 - x1) * i / steps
            yi = y1 + (y2 - y1) * i / steps
            await hid.send_touchscreen(build_touchscreen_report(
                TOUCHSCREEN_STATE_CONTACT, round(xi), round(yi)))
        await hid.send_touchscreen(build_touchscreen_report(
            TOUCHSCREEN_STATE_RELEASE, x2, y2))


async def gesture_worker(queue):
    while True:
        try:
            services = await get_remote_pairing_tunnel_services(udid=UDID)
            if not services:
                print("[!] phone not visible - retrying")
                await asyncio.sleep(3)
                continue
            rp = services[0]
            async with start_tunnel_over_remotepairing(rp) as tun:
                rsd = RemoteServiceDiscoveryService((tun.address, tun.port))
                async with rsd:
                    print("[+] tunnel up")
                    indigo = IndigoHIDService(rsd)
                    async with indigo as ibtn:
                        async with touch_session(rsd) as hid:
                            print("[+] HID gestures live")
                            while True:
                                job = await queue.get()
                                try:
                                    await run_job(hid, ibtn, job)
                                except Exception as e:
                                    print(f"[!] gesture error: {e}")
        except Exception as e:
            print(f"[!] dropped ({e}) - retrying in 3s")
            await asyncio.sleep(3)


RS = {"loop": None, "queue": None}


class RelayState:
    pass


async def amain():
    RS["loop"] = asyncio.get_running_loop()
    RS["queue"] = asyncio.Queue()
    asyncio.create_task(gesture_worker(RS["queue"]))
    import http.server
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 9001), CmdHandler)
    print("[*] test pad: http://127.0.0.1:9001/")
    srv.serve_forever()


if __name__ == "__main__":
    asyncio.run(amain())
