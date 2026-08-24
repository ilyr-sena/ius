#!/usr/bin/env python3
"""Remote-control page for the phone over USB.

Serves a single page at http://127.0.0.1:9000/ that shows the live MJPEG
stream (from the probe app, port 9100) and turns your mouse into taps,
long-presses and swipes delivered as W3C actions to WebDriverAgent
(port 8100). Requires: stream_run.py --wda, IUSProbe running (picker
accepted), WDA runner launched via xcuitest.
"""
import http.server

PAGE = """<!doctype html>
<html><head><meta charset="utf-8"><title>IUS remote</title>
<style>
 body{background:#0b0b0f;color:#ddd;font-family:ui-monospace,monospace;text-align:center;margin:0;padding:10px}
 #img{user-select:none;-webkit-user-drag:none;max-width:96vw;max-height:86vh;border:1px solid #333;cursor:crosshair;background:#000}
 #s{margin-top:8px;font-size:13px;opacity:.85}
 button{font-family:inherit;background:#222;color:#ddd;border:1px solid #444;padding:4px 10px;margin:2px}
 input{width:70px}
</style></head>
<body>
<h3>IUS remote (MJPEG + WDA)</h3>
<img id="img" draggable="false" src="http://127.0.0.1:9100/stream?fps=20&scale=0.6&q=0.45">
<div id="s">initializing...</div>
<div>
 Home <button data-k="home">home</button>
</div>
<script>
const WDA = 'http://127.0.0.1:8100';
const img = document.getElementById('img'), st = document.getElementById('s');
let sid = null, rect = {w: 390, h: 844};

async function api(method, path, body){
  const r = await fetch(WDA + path, {
    method,
    headers: body ? {'Content-Type': 'application/json'} : {},
    body: body ? JSON.stringify(body) : undefined
  });
  let j = null;
  try { j = await r.json(); } catch(e){}
  return [r.status, j];
}

async function ensureSession(){
  if (sid) return true;
  try {
    const [code, j] = await api('POST', '/session', {capabilities: {}});
    if (code >= 200 && code < 300 && j && j.value && j.value.sessionId) {
      sid = j.value.sessionId;
      const [rc, rr] = await api('GET', '/session/' + sid + '/window/rect');
      if (rc === 200 && rr && rr.value) rect = {w: rr.value.width, h: rr.value.height};
      setStatus('session ' + sid.slice(0,8) + ' | screen ' + rect.w + 'x' + rect.h);
      return true;
    }
    setStatus('WDA session failed (' + code + ') - is the runner active?');
  } catch(e){ setStatus('WDA unreachable: ' + e); }
  return false;
}

function setStatus(t){ st.textContent = t; }

function mapXY(ev){
  const r = img.getBoundingClientRect();
  const iw = img.naturalWidth || 1, ih = img.naturalHeight || 1;
  const fx = Math.min(Math.max((ev.clientX - r.left) / r.width, 0), 1);
  const fy = Math.min(Math.max((ev.clientY - r.top) / r.height, 0), 1);
  return [Math.round(fx * rect.w), Math.round(fy * rect.h)];
}

const mv = (x,y,d)=>({type:'pointerMove', x:Math.round(x), y:Math.round(y),
                      duration:d||0, origin:'viewport'});
const dn = ()=>({type:'pointerDown', button:0});
const up = ()=>({type:'pointerUp', button:0});
const SRC = [{type:'pointer', id:'ius-finger', parameters:{pointerType:'touch'}}];

async function sendActions(acts){
  if (!await ensureSession()) return;
  const [code, j] = await api('POST', '/session/' + sid + '/actions',
                                 {actions: [...SRC, ...acts]});
  setStatus((code >= 200 && code < 300 ? 'ok' : 'http ' + code) + ': ' +
            acts.length + ' steps');
}

// ---- mouse -> gestures ----
let dragging=false, moved=false, sx=0, sy=0, t0=0;

img.addEventListener('dragstart', ev => ev.preventDefault());
img.addEventListener('mousedown', ev => {
  if (ev.button !== 0) return;
  ev.preventDefault();
  [sx, sy] = mapXY(ev); dragging = true; moved = false; t0 = performance.now();
});
window.addEventListener('mousemove', ev => {
  if (!dragging) return;
  const [x, y] = mapXY(ev);
  if (Math.hypot(x - sx, y - sy) > 6) moved = true;
});
window.addEventListener('mouseup', async ev => {
  if (!dragging) return;
  dragging = false;
  const [ex, ey] = mapXY(ev);
  if (!await ensureSession()) return;
  const held = Math.min(800, Math.max(60, Math.round(performance.now() - t0)));
  let acts;
  if (!moved) {
    acts = [mv(sx, sy), dn(), {type:'pause', duration: held}, up()];
  } else {
    const n = 24;
    acts = [mv(sx, sy), dn()];
    for (let i = 1; i < n; i++)
      acts.push(mv(sx + (ex-sx)*i/n, sy + (ey-sy)*i/n, Math.max(4, Math.round(320/n))));
    acts.push(mv(ex, ey, 30)); acts.push(up());
  }
  await sendActions(acts);
});

// ---- buttons ---------------------------------------------------------------
document.querySelectorAll('button[data-k]').forEach(b => {
  b.addEventListener('click', async () => {
    if (!await ensureSession()) return;
    const [code] = await api('POST', '/session/' + sid + '/wda/pressButton',
                             {name: b.dataset.k});
    setStatus('press ' + b.dataset.k + ' -> http ' + code);
  });
});

ensureSession();
</script></body></html>
"""


class Handler(http.server.BaseHTTPRequestHandler):
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

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    print("[*] remote control page: http://127.0.0.1:9000/")
    print("    (needs stream_run.py --wda, IUSProbe capture on, WDA runner active)")
    http.server.ThreadingHTTPServer(("127.0.0.1", 9000), Handler).serve_forever()
