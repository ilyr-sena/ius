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


def terminal_login(
    session_file: Path,
    email: Optional[str] = None,
    password: Optional[str] = None,
) -> dict:
    """Authenticates to Apple directly in the terminal with 2FA prompt using GrandSlam."""
    import asyncio
    import getpass
    from findmy import AsyncAppleAccount, LocalAnisetteProvider, LoginState

    session_file = Path(session_file)
    session_file.parent.mkdir(parents=True, exist_ok=True)

    print("\n" + "═" * 60)
    print("  Apple ID Terminal Login")
    print("═" * 60)

    if not email:
        email = input("Apple ID email: ").strip()
    if not password:
        password = getpass.getpass("Apple ID password: ")

    captured_state: dict = {}

    async def _do_login():
        ani = LocalAnisetteProvider()
        acc = AsyncAppleAccount(ani)

        orig_set_state = type(acc)._set_login_state

        def spying_set_login_state(self, state, data=None):
            if data:
                captured_state.update(data)
            return orig_set_state(self, state, data)

        type(acc)._set_login_state = spying_set_login_state

        state = await acc.login(email, password)
        if state == LoginState.REQUIRE_2FA:
            print("\n[*] Two-factor authentication required.")
            methods = await acc.get_2fa_methods()
            method = methods[0]
            if len(methods) > 1:
                for i, m in enumerate(methods):
                    print(f"  [{i}] {type(m).__name__}")
                try:
                    idx = int(input("Choose 2FA method [0]: ") or "0")
                    method = methods[idx]
                except Exception:
                    method = methods[0]
            if hasattr(method, "request"):
                await method.request()
            code = input("Enter 6-digit verification code sent to your Apple device: ").strip()

            from findmy.reports.account import AsyncTrustedDeviceSecondFactor
            if isinstance(method, AsyncTrustedDeviceSecondFactor):
                # Standard Apple GSA Trusted Device 2FA verification endpoint
                try:
                    await acc._sms_2fa_request(
                        "POST",
                        "https://gsa.apple.com/auth/verify/trusteddevice/securitycode",
                        data={"securityCode": {"code": code}},
                    )
                except Exception as e:
                    # Fallback to validate query
                    log.debug("trusteddevice/securitycode failed: %s — trying legacy validate", e)
                    headers = {
                        "security-code": code,
                        "Content-Type": "text/x-xml-plist",
                        "Accept": "text/x-xml-plist",
                    }
                    await acc._sms_2fa_request(
                        "GET",
                        acc._ENDPOINT_2FA_TD_SUBMIT,
                        headers=headers,
                    )
            else:
                data = {
                    "phoneNumber": {"id": getattr(method, "_selected_phone_number_id", 1)},
                    "securityCode": {"code": code},
                }
                await acc._sms_2fa_request(
                    "POST",
                    acc._ENDPOINT_2FA_SMS_SUBMIT,
                    data=data,
                )

            # 2FA code submission successfully verified session on Apple servers
            state = LoginState.AUTHENTICATED

        if state not in (LoginState.LOGGED_IN, LoginState.AUTHENTICATED):
            raise RuntimeError(f"Apple authentication failed (state: {state})")

        print("✓ Authentication successful!")
        return captured_state

    state_data = asyncio.run(_do_login())

    adsid = state_data.get("adsid", "")
    pet = state_data.get("idms_pet", "") or state_data.get("idms_token", "")
    dsid = str(state_data.get("dsid", ""))

    session_data = {
        "cookies": [
            {"name": "myacinfo", "value": pet, "domain": ".apple.com", "path": "/"}
        ],
        "auth_headers": {
            "X-Apple-ADSID": adsid,
            "X-Apple-ID-DSID": dsid,
        },
        "raw_gsa": state_data,
    }

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
                if "raw_gsa" in data:
                    return data
        except Exception as e:
            log.warning("could not read saved session from %s: %s", p, e)
    return None
