#!/usr/bin/env python3
"""Drive the on-device ScreenCaptureKit probe over USB."""
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request

BASE = "http://127.0.0.1:9100"


def req(method: str, path: str, timeout: float = 10):
    r = urllib.request.Request(BASE + path, method=method)
    with urllib.request.urlopen(r, timeout=timeout) as resp:
        return json.loads(resp.read() or b"{}")


def safe_req(method: str, path: str, timeout: float = 10):
    try:
        return req(method, path, timeout)
    except (urllib.error.URLError, OSError, json.JSONDecodeError) as e:
        return {"error": str(e)}


def main() -> int:
    print("[*] spawning iproxy 9100 -> device 9100")
    ip = subprocess.Popen(["iproxy", "9100", "9100"])
    time.sleep(0.5)
    try:
        st = safe_req("GET", "/status")
        if "phase" not in st:
            print(f"[!] probe app unreachable ({st.get('error')}) — is IUSProbe running in the foreground?")
            return 1
        print("[OK] probe alive:", st)
        req("POST", "/probe/start")
        print("[*] plan started.")
        print("[!] When the phone shows the screen-sharing picker: select the FULL DISPLAY.")
        print("[!] When phase prints 'awaiting-background':")
        print("    physically SWIPE UP to home on the phone, then leave it (unlocked, screen on).")
        last = None
        fails = 0
        for _ in range(180):  # ≤ 6 min
            time.sleep(2)
            phase = safe_req("GET", "/status", timeout=3).get("phase")
            if phase is None:
                fails += 1
                if fails == 3:
                    print("[!] device unreachable (app backgrounded/suspended) — continuing...")
                continue
            fails = 0
            if phase != last:
                last = phase
                print(f"    phase: {phase}")
            if phase == "done":
                break
        rep = None
        for _ in range(10):  # retry in case app was suspended; re-open it if needed
            rep = safe_req("GET", "/probe/report", timeout=3)
            if "verdict" in rep:
                break
            print("[!] report not ready/unreachable — if suspended, re-open IUSProbe, then wait...")
            time.sleep(3)
        print(json.dumps(rep, indent=2))
        ok = bool(rep.get("backgrounded")) and rep.get("backgroundFps", 0) >= 40 \
            and rep.get("foregroundFps", 0) >= 40
        print("\nVERDICT:", "GREEN" if ok else "NOT GREEN — see report above")
        return 0 if ok else 2
    finally:
        ip.terminate()


if __name__ == "__main__":
    sys.exit(main())
