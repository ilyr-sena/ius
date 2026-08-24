#!/usr/bin/env python3
"""Keeps USB tunnels to the phone alive until Ctrl+C.

Usage:
    python3 tools/stream_run.py           # probe app only (9100)
    python3 tools/stream_run.py --wda     # also tunnel WDA (8100)

Then on this PC:
    H.264 stream : http://127.0.0.1:9100/stream.html
    MJPEG stream : http://127.0.0.1:9100/stream?fps=15&scale=0.5&q=0.4
    WDA          : http://127.0.0.1:8100/status   (after xcuitest launch)
    Probe API    : http://127.0.0.1:9100/capture/stats
"""
import subprocess
import sys
import time

TUNNELS = [(9100, 9100)]
if "--wda" in sys.argv:
    TUNNELS.append((8100, 8100))

procs = []


def main():
    for hp, dp in TUNNELS:
        print(f"[*] iproxy {hp} -> {dp}")
        procs.append(subprocess.Popen(["iproxy", str(hp), str(dp)]))
    print("[*] tunnels up - Ctrl+C to stop")
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        print("\n[*] stopping")
    finally:
        for p in procs:
            p.terminate()


if __name__ == "__main__":
    main()
