#!/usr/bin/env python3
"""Opens developer.apple.com in real Chrome; captures session cookies AND
the appleauth session headers emitted during your interactive login.
Crash-proof: saves whatever it has on any exit path."""
import asyncio
import json
import sys
import time

from playwright.async_api import async_playwright

OUT = sys.argv[1] if len(sys.argv) > 1 else "/home/sooku/ipa-share/uploads/browser_session.json"
PROFILE = "/home/sooku/.ius-browser-profile"
TARGET = "https://developer.apple.com/account/"

captured = {"cookies": [], "auth_headers": {}}


def save():
    json.dump(captured, open(OUT, "w"), indent=1)


async def main():
    async with async_playwright() as p:
        ctx = await p.chromium.launch_persistent_context(
            user_data_dir=PROFILE,
            headless=False,
            channel="chrome",
            args=["--start-maximized"],
        )
        # force a fresh appleauth so the session headers get emitted
        try:
            await ctx.clear_cookies(domain="idmsa.apple.com")
        except TypeError:
            await ctx.clear_cookies()
        page = ctx.pages[0] if ctx.pages else await ctx.new_page()

        def on_response(resp):
            url = resp.url
            if "appleauth" in url or "idmsa.apple.com" in url:
                for k in ("X-Apple-ID-Session-Id", "X-Apple-DSID", "scnt"):
                    v = resp.headers.get(k)
                    if v:
                        captured["auth_headers"][k] = v
                save()

        ctx.on("response", on_response)
        page.on("response", on_response)

        try:
            await page.goto(TARGET, timeout=60000)
        except Exception as e:
            print(f"[i] goto: {e}")

        print("[*] Log in with your Apple ID (password + 2FA). KEEP THE WINDOW OPEN.")
        deadline = time.time() + 600
        while time.time() < deadline:
            try:
                cookies = await ctx.cookies()
            except Exception:
                break   # browser closed by user
            captured["cookies"] = cookies
            save()
            names = {c["name"] for c in cookies}
            have_cookie = "myacinfo" in names
            have_header = "X-Apple-ID-Session-Id" in captured["auth_headers"]
            if have_cookie and have_header:
                print(f"[+] COMPLETE. cookies={sorted(n for n in names if n in ('myacinfo','DES','dssid2'))} headers={sorted(captured['auth_headers'])}")
                save()
                break
            await asyncio.sleep(2)

    save()
    ok = "myacinfo" in {c["name"] for c in captured["cookies"]} and \
         "X-Apple-ID-Session-Id" in captured["auth_headers"]
    print("[+] session saved OK" if ok else "[!] saved partial — may be insufficient")


asyncio.run(main())
