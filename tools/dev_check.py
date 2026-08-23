#!/usr/bin/env python3
"""M0: verify device is visible and paired over usbmuxd."""
import asyncio
import inspect
import json
import sys

from pymobiledevice3.lockdown import create_using_usbmux


async def main() -> int:
    try:
        if inspect.iscoroutinefunction(create_using_usbmux):
            lockdown = await create_using_usbmux()      # new async API
        else:
            lockdown = create_using_usbmux()            # old sync API

        v = lockdown.all_values
        if inspect.isawaitable(v):                      # all_values may also be async
            v = await v
    except Exception as e:
        print(f"[FAIL] cannot reach device over usbmuxd: {e}")
        print("       usbmuxd running? cable? 'idevice_id -l' shows UDID?")
        return 1

    keys = ("DeviceName", "ProductType", "ProductVersion", "BuildVersion",
            "UniqueDeviceID")
    print("[OK] lockdown connection established")
    print(json.dumps({k: v.get(k) for k in keys}, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))