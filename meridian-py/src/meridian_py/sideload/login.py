"""Interactive Apple Developer login helper using local Chrome via Playwright."""

from __future__ import annotations

import json
import logging
import time
from pathlib import Path
from typing import Optional

log = logging.getLogger(__name__)

TARGET_URL = "https://developer.apple.com/account/"
PROFILE_DIR = Path.home() / ".local" / "state" / "meridian" / "browser_profile"


ANISETTE_DIR = Path("/home/sooku/Documents/Dev/ius/tools/anisette")


def ensure_anisette_server(port: int = 6969) -> str:
    """Ensure local omnisette-server is running on port."""
    import socket
    import subprocess

    with socket.socket() as s:
        s.settimeout(0.3)
        if s.connect_ex(("127.0.0.1", port)) == 0:
            return f"http://127.0.0.1:{port}"

    server_bin = ANISETTE_DIR / "omnisette-server"
    if server_bin.exists():
        log.info("starting local omnisette-server on :%d...", port)
        subprocess.Popen(
            [str(server_bin), "--ip", "127.0.0.1", "--http-port", str(port)],
            cwd=str(ANISETTE_DIR),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        for _ in range(25):
            time.sleep(0.3)
            with socket.socket() as s:
                s.settimeout(0.3)
                if s.connect_ex(("127.0.0.1", port)) == 0:
                    log.info("local omnisette-server ready on :%d", port)
                    return f"http://127.0.0.1:{port}"
    return f"http://127.0.0.1:{port}"


def terminal_login(
    session_file: Path,
    email: Optional[str] = None,
    password: Optional[str] = None,
) -> dict:
    """Authenticates to Apple directly in the terminal with 2FA prompt using GrandSlam for Xcode."""
    import getpass
    from .gsa import GSAClient

    session_file = Path(session_file)
    session_file.parent.mkdir(parents=True, exist_ok=True)

    print("\n" + "═" * 60)
    print("  Apple ID Terminal Login (Xcode Developer Services)")
    print("═" * 60)

    if not email:
        email = input("Apple ID email: ").strip()
    if not password:
        password = getpass.getpass("Apple ID password: ")

    client = GSAClient(username=email)
    session_data = client.authenticate(password)

    with session_file.open("w") as f:
        json.dump(session_data, f, indent=2)

    log.info("Saved Apple Developer session to %s", session_file)
    return session_data


def browser_login(session_file: Path) -> dict:
    """Launch local Chrome so the user can log in with Apple ID + 2FA once."""
    from playwright.sync_api import sync_playwright

    session_file = Path(session_file)
    session_file.parent.mkdir(parents=True, exist_ok=True)
    PROFILE_DIR.mkdir(parents=True, exist_ok=True)

    print("\n" + "═" * 60)
    print("  Apple Developer Login")
    print("  A Chrome window will open. Please log in with your Apple ID.")
    print("  Once you approve 2FA, session credentials will be saved locally.")
    print("═" * 60 + "\n")

    captured: dict = {"cookies": [], "auth_headers": {}}

    with sync_playwright() as p:
        ctx = p.chromium.launch_persistent_context(
            user_data_dir=str(PROFILE_DIR),
            headless=False,
            channel="chrome",
            args=["--start-maximized"],
        )

        try:
            ctx.clear_cookies(domain="idmsa.apple.com")
        except Exception:
            ctx.clear_cookies()

        page = ctx.pages[0] if ctx.pages else ctx.new_page()

        def on_response(resp):
            url = resp.url
            if "appleauth" in url or "idmsa.apple.com" in url or "developer.apple.com" in url:
                low = {k.lower(): v for k, v in resp.headers.items()}
                for k, target in [
                    ("x-apple-id-session-id", "X-Apple-ID-Session-Id"),
                    ("x-apple-dsid", "X-Apple-DSID"),
                    ("scnt", "scnt"),
                ]:
                    if k in low:
                        captured["auth_headers"][target] = low[k]

        ctx.on("response", on_response)
        page.on("response", on_response)

        try:
            page.goto(TARGET_URL, timeout=60000)
        except Exception as e:
            log.debug("goto: %s", e)

        print("[*] Waiting for login in Chrome (password + 2FA)...")
        deadline = time.time() + 600
        logged_in = False

        while time.time() < deadline:
            try:
                cookies = ctx.cookies()
            except Exception:
                break  # browser closed by user

            captured["cookies"] = cookies
            names = {c["name"] for c in cookies}
            have_cookie = "myacinfo" in names
            have_header = "X-Apple-ID-Session-Id" in captured["auth_headers"]
            is_account = "/account" in page.url and "/auth" not in page.url and "/sign-in" not in page.url

            if have_cookie and (have_header or is_account):
                logged_in = True
                print("✓ Successfully authenticated to Apple Developer!")
                break
            time.sleep(1.5)

        try:
            ctx.close()
        except Exception:
            pass

    if not logged_in or not captured["cookies"]:
        raise RuntimeError("Login incomplete: myacinfo and auth headers not captured.")

    with session_file.open("w") as f:
        json.dump(captured, f, indent=2)

    log.info("Saved Apple Developer session to %s", session_file)
    return captured


def load_session(session_file: Path) -> Optional[dict]:
    """Load previously saved GSA session or sessions.json if it exists."""
    for p in (session_file.parent / "sessions.json", session_file):
        if not p.exists():
            continue
        try:
            with p.open("r") as f:
                data = json.load(f)
            if isinstance(data, dict):
                latest = data.get("latest:a")
                if latest and f"{latest}:a" in data:
                    sess = data[f"{latest}:a"]
                    gs = sess.get("gs_token")
                    dsid = sess.get("dsid")
                    if gs and dsid:
                        return {
                            "cookies": [{"name": "myacinfo", "value": gs, "domain": ".apple.com", "path": "/"}],
                            "auth_headers": {
                                "X-Apple-GS-Token": gs,
                                "X-Apple-ADSID": dsid,
                            },
                            "raw_gsa": sess,
                        }
                if "authToken" in data:
                    return data
                if "raw_gsa" in data:
                    return data
        except Exception as e:
            log.warning("could not read saved session from %s: %s", p, e)
    return None
