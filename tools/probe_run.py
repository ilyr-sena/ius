#!/usr/bin/env python3
"""M1: drive the on-device ScreenCaptureKit probe over USB."""
import json
import subprocess
import sys
import time
import urllib.request

BASE = "http://127.0.0.1:9100"


def req(method: str, path: str):
    r = urllib.request.Request(BASE + path, method=method)
    with urllib.request.urlopen(r, timeout=10) as resp:
        return json.loads(resp.read() or b"{}")


def main() -> int:
    print("[*] spawning iproxy 9100 -> device 9100")
    ip = subprocess.Popen(["iproxy", "9100", "9100"])
    time.sleep(0.5)
    try:
        print("[OK] probe alive:", req("GET", "/status"))
        req("POST", "/probe/start")
        print("[*] plan started.")
        print("[!] When phase prints 'awaiting-background':")
        print("    physically SWIPE UP to home on the phone, then leave it (unlocked, screen on).")
        last = None
        for _ in range(180):  # ≤ 6 min
            time.sleep(2)
            phase = req("GET", "/status").get("phase")
            if phase != last:
                last = phase
                print(f"    phase: {phase}")
            if phase == "done":
                break
        rep = req("GET", "/probe/report")
        print(json.dumps(rep, indent=2))
        ok = (rep.get("backgrounded") and rep.get("backgroundFps", 0) >= 40
              and rep.get("foregroundFps", 0) >= 40)
        print("\nVERDICT:", "GREEN" if ok else "NOT GREEN — see report above")
        return 0 if ok else 2
    finally:
        ip.terminate()


if __name__ == "__main__":
    sys.exit(main())