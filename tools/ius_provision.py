#!/usr/bin/env python3
"""Mint an iOS development provisioning profile purely on Linux.

Uses the legacy Xcode developerservices2 protocol (plist payloads):
  login -> team -> devices -> appId -> certs -> create/download profile

Password: use an APP-SPECIFIC password (account.apple.com -> Sign-In and
Security -> App-Specific Passwords). Regular passwords may trigger 2FA.
"""
import argparse
import base64
import getpass
import plistlib
import re
import ssl
import sys
import uuid

import requests

APP_ID_KEY = "ba2ec180e6ca6e6c6a542255453b24d6e6e5b2be0cc48bc1b0d8ad64cfe0228f"
BASE = "https://developerservices2.apple.com/services/QH65B2"


class AppleSession:
    def __init__(self):
        self.http = requests.Session()
        self.myacinfo = None
        self.session_id = None

    def _headers(self):
        h = {
            "User-Agent": "Xcode",
            "X-Apple-Widget-Key": APP_ID_KEY,
            "Accept": "application/json",
        }
        if self.session_id:
            h["X-Apple-ID-Session-Id"] = self.session_id
        return h

    def _post_plist(self, url, payload):
        body = plistlib.dumps(payload)
        r = self.http.post(url, data=body, headers={
            **self._headers(),
            "Content-Type": "text/x-xml-plist",
            "Accept": "text/x-xml-plist",
            "Origin": "https://developer.apple.com",
        }, timeout=30)
        r.raise_for_status()
        return plistlib.loads(r.content)

    ANISETTE_LIST = "https://raw.githubusercontent.com/SideStore/anisette-servers/main/servers.json"

    def _fetch_anisette(self, base: str) -> dict:
        last_err = None
        for suffix in ("/v3/get_headers", "/"):
            try:
                r = requests.get(base + suffix, timeout=10)
                if r.status_code == 200 and "X-Apple-I-MD" in r.text:
                    j = r.json()
                    if isinstance(j, dict) and "Headers" in j:
                        j = j["Headers"]
                    print(f"[+] anisette from {base}{suffix}")
                    return j
            except Exception as e:
                last_err = e
        raise RuntimeError(f"{base}: {last_err}")

    def login(self, apple_id: str, password: str):
        import time
        candidates = [lambda: self._fetch_anisette("https://ani.sidestore.io")]
        seen = set()
        try:
            r = requests.get(self.ANISETTE_LIST, timeout=10)
            entries = r.json().get("servers", [])
            for e in entries[:14]:
                url = e.get("address", "").rstrip("/")
                if url and url not in seen:
                    seen.add(url)
                    def make(u=url):
                        return lambda: self._fetch_anisette(u)
                    candidates.append(make())
        except Exception as e:
            print(f"[!] instance list unavailable ({e})")

        last_body = ""
        tried = 0
        for get_anisette in candidates:
            if tried >= 6:
                break
            try:
                anisette = get_anisette()
            except Exception as e:
                print(f"[!] anisette source failed: {e}")
                continue
            tried += 1
            headers = {
                "X-Apple-Widget-Key": APP_ID_KEY,
                "Accept": "application/json",
                "User-Agent": "Xcode",
                "X-MMe-Client-Info": "<MacBookPro> <macOS;14.5> <23F79>",
                **anisette,
            }
            print(f"[*] login attempt {tried} (anisette {list(anisette)[:2]}…)")
            r = self.http.post(
                "https://idmsa.apple.com/appleauth/auth/signin",
                json={"accountName": apple_id, "password": password, "rememberMe": True},
                headers=headers,
                timeout=30,
            )
            if r.status_code == 503:
                last_body = r.text[:120]
                print("[!] 503 — anisette rejected, rotating…")
                time.sleep(2)
                continue
            if r.status_code == 409:
                raise SystemExit("[!] 2FA challenge required — use an app-specific password")
            if r.status_code not in (200, 204):
                raise SystemExit(f"[!] login failed ({r.status_code}): {r.text[:300]}")
            names = [c.name for c in self.http.cookies]
            if not any("myacinfo" in n for n in names):
                raise SystemExit(f"[!] no myacinfo cookie after auth: {names}")
            print("[+] login OK")
            return
        raise SystemExit(f"[!] all anisette attempts rejected by Apple (last: {last_body})")

    def load_cookies(self, path: str):
        import json as _json
        for c in _json.load(open(path)):
            self.http.cookies.set(c["name"], c["value"],
                                  domain=c.get("domain", ".apple.com"),
                                  path=c.get("path", "/"))
        print(f"[+] loaded {len(c) if False else 'session'} cookies from {path}")

    def _myacinfo(self) -> str:
        for c in self.http.cookies:
            if c.name == "myacinfo":
                return c.value
        return ""

    def call(self, action: str, team_id: str, **extra):
        payload = {
            "clientId": "XABBG36SBA",
            "myacinfo": self._myacinfo(),
            "protocolVersion": "QH65B2",
            "requestId": str(uuid.uuid4()).upper(),
            "teamId": team_id,
            "userLocale": "en_US",
        }
        payload.update(extra)
        url = f"{BASE}/{action}.action?clientId=XABBG36SBA"
        return self._post_plist(url, payload)

    def get_team(self) -> str:
        data = self.call("listTeams", "")
        teams = data.get("teams", [])
        if not teams:
            raise SystemExit("[!] no teams listed — pass --team-id (see cert OU)")
        tid = teams[0]["teamId"]
        print(f"[+] team: {tid} ({teams[0].get('name')})")
        return tid

    def get_device_id(self, team_id: str, udid: str) -> str:
        data = self.call("ios/listDevices", team_id)
        for dev in data.get("devices", []):
            if dev.get("deviceNumber", "").lower() == udid.lower():
                print(f"[+] device registered: {dev.get('name', '')} deviceId={dev['deviceId']}")
                return dev["deviceId"]
        raise SystemExit("[!] device UDID not found on account — was it added before?")

    def get_app_id(self, team_id: str, bundle_id: str, name: str) -> str:
        data = self.call("ios/listAppIds", team_id)
        for app in data.get("appIds", []):
            if app.get("identifier") == bundle_id:
                print(f"[+] appId exists: {app['appIdId']}")
                return app["appIdId"]
        print("[*] registering new App ID…")
        data = self.call(
            "addAppId", team_id,
            identifier=bundle_id,
            entitlements=[],
            appIdName=name,
            name=name,
        )
        app = data.get("appId") or {}
        if not app.get("appIdId"):
            raise SystemExit(f"[!] addAppId failed: {str(data)[:300]}")
        print(f"[+] appId created: {app['appIdId']}")
        return app["appIdId"]

    def find_cert(self, team_id: str, cert_der: bytes) -> str:
        data = self.call("ios/listAllDevelopmentCerts", team_id)
        for c in data.get("certificates", []):
            content = c.get("certContent")
            raw = bytes(content) if isinstance(content, plistlib.Data) else content
            if raw and raw.strip() == cert_der.strip():
                print(f"[+] matching cert found: serial={c['serialNumber']}")
                return c["certificateId"]
        raise SystemExit("[!] uploaded cert not present on account — cannot bind profile to it")

    def create_profile(self, team_id: str, app_id_id: str, device_ids: list,
                       cert_ids: list, name: str) -> bytes:
        base_payload = {
            "clientId": "XABBG36SBA",
            "protocolVersion": "QH65B2",
            "requestId": str(uuid.uuid4()).upper(),
            "appIdId": app_id_id,
            "deviceIds": device_ids,
            "certificateIds": cert_ids,
            "distributionType": "limited",
            "provisioningProfileName": name,
            "teamId": team_id,
            "userLocale": "en_US",
        }
        for regen in (False, True):
            action = "regenProvisioningProfile" if regen else "createProvisioningProfile"
            try:
                data = self.call(f"ios/{action}", team_id, **base_payload)
                prof = data.get("provisioningProfile")
                if prof:
                    content = prof.get("encodedProfile")
                    raw = bytes(content) if isinstance(content, plistlib.Data) else content
                    print(f"[+] profile ready ({action})")
                    return raw
                print(f"[*] {action}: {str(data)[:200]}")
            except SystemExit:
                raise
            except Exception as e:
                print(f"[*] {action} error: {e}")
        raise SystemExit("[!] profile creation failed on both create and regenerate")


def cert_der_from_pem(path: str) -> bytes:
    import base64 as b64
    txt = open(path).read()
    body = re.search(r"-----BEGIN CERTIFICATE-----(.*?)-----END CERTIFICATE-----",
                     txt, re.S).group(1)
    return b64.b64decode("".join(body.split()))


def team_from_cert(path: str):
    import subprocess
    out = subprocess.run(["openssl", "x509", "-in", path, "-noout", "-subject"],
                         capture_output=True, text=True).stdout
    m = re.search(r"OU\s*=\s*([A-Z0-9]{10})", out)
    return m.group(1) if m else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--apple-id")
    ap.add_argument("--password")            # omitted -> prompt
    ap.add_argument("--cookies", help="browser cookies JSON from ius_browser_login.py")
    ap.add_argument("--team-id", help="overrides cert OU / listTeams")
    ap.add_argument("--bundle-id", required=True)
    ap.add_argument("--udid", required=True)
    ap.add_argument("--cert-pem", required=True)
    ap.add_argument("--profile-name", default="ius-wda")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    password = args.password or getpass.getpass("Apple ID password (app-specific): ") if not args.cookies else None
    cert_der = cert_der_from_pem(args.cert_pem)

    s = AppleSession()
    if args.cookies:
        s.load_cookies(args.cookies)
    else:
        s.login(args.apple_id, password)

    team = args.team_id or team_from_cert(args.cert_pem) or s.get_team()
    print(f"[+] using team {team}")
    device_id = s.get_device_id(team, args.udid)
    app_id_id = s.get_app_id(team, args.bundle_id, "ius wda")
    cert_id = s.find_cert(team, cert_der)

    raw = s.create_profile(team, app_id_id, [device_id], [cert_id], args.profile_name)
    open(args.out, "wb").write(raw)
    print(f"[+] wrote {args.out} ({len(raw)} bytes)")


if __name__ == "__main__":
    main()
