#!/usr/bin/env python3
"""M0: WDA health check through a USB tunnel.
Prereq: iproxy 8100 8100 running (see instructions)."""
import argparse, json, sys, time

import requests

BASE = "http://127.0.0.1:8100"
S = requests.Session()

def step(name, fn):
    try:
        out = fn()
        print(f"[OK] {name}")
        return out
    except Exception as e:
        print(f"[FAIL] {name}: {e}")
        sys.exit(1)

def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tap-test", action="store_true",
                    help="also perform one harmless tap at (100, 100)")
    args = ap.parse_args()

    def status():
        r = S.get(f"{BASE}/status", timeout=10).json()["value"]
        assert r.get("ready") is True, "WDA not ready"
        return r

    st = step("GET /status (WDA alive, ready)", status)
    print(f"     WDA build={st.get('buildVersion')} iOS={st.get('osVersion')}")

    def session():
        r = S.post(f"{BASE}/session", json={"capabilities": {}}, timeout=20).json()
        return r["value"]["sessionId"]

    sid = step("POST /session (automation session)", session)
    print(f"     sessionId={sid}")

    def size():
        r = S.get(f"{BASE}/session/{sid}/window/size", timeout=10).json()["value"]
        return r

    print("[OK] screen size (points):", step("GET window/size", size))

    if args.tap_test:
        body = {"actions": [{
            "type": "pointer", "id": "p1",
            "parameters": {"pointerType": "touch"},
            "actions": [
                {"type": "pointerMove", "duration": 0, "x": 100, "y": 100},
                {"type": "pointerDown", "button": 0},
                {"type": "pause", "duration": 60},
                {"type": "pointerUp", "button": 0},
            ]}]}
        step("POST /actions (tap @100,100)",
             lambda: S.post(f"{BASE}/session/{sid}/actions", json=body, timeout=20))

    step("DELETE /session",
         lambda: S.delete(f"{BASE}/session/{sid}", timeout=10))
    print("\nM0: ALL GREEN")
    return 0

if __name__ == "__main__":
    sys.exit(main())