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


def browser_login(session_file: Path) -> list[dict]:
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

    captured_cookies = []

    with sync_playwright() as p:
        ctx = p.chromium.launch_persistent_context(
            user_data_dir=str(PROFILE_DIR),
            headless=False,
            channel="chrome",
            args=["--start-maximized"],
        )

        page = ctx.pages[0] if ctx.pages else ctx.new_page()

        try:
            page.goto(TARGET_URL, timeout=60000)
        except Exception as e:
            log.debug("goto: %s", e)

        print("[*] Waiting for login in Chrome (timeout: 5 minutes)...")
        deadline = time.time() + 300
        logged_in = False

        while time.time() < deadline:
            try:
                cookies = ctx.cookies()
            except Exception:
                break  # browser closed by user

            names = {c["name"] for c in cookies}
            if "myacinfo" in names:
                captured_cookies = cookies
                logged_in = True
                print("✓ Successfully authenticated to Apple Developer!")
                break
            time.sleep(1.5)

        try:
            ctx.close()
        except Exception:
            pass

    if not logged_in or not captured_cookies:
        raise RuntimeError("Login incomplete: myacinfo session cookie not found.")

    with session_file.open("w") as f:
        json.dump(captured_cookies, f, indent=2)

    log.info("Saved Apple Developer session to %s", session_file)
    return captured_cookies


def load_session(session_file: Path) -> Optional[list[dict]]:
    """Load previously saved cookies if they exist."""
    session_file = Path(session_file)
    if not session_file.exists():
        return None
    try:
        with session_file.open("r") as f:
            cookies = json.load(f)
        names = {c.get("name") for c in cookies if isinstance(c, dict)}
        if "myacinfo" in names:
            return cookies
    except Exception as e:
        log.warning("could not read saved session: %s", e)
    return None
