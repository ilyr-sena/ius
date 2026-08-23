#!/usr/bin/env python3
"""Interactive GrandSlam login -> dumps full session state -> probes
developerservices2 with every plausible credential mapping found."""
import asyncio
import json
import getpass
import re
import uuid

from findmy import AsyncAppleAccount, RemoteAnisetteProvider, LoginState

STATE_OUT = "/home/sooku/ipa-share/uploads/gs_session.json"
FULL_OUT = "/home/sooku/ipa-share/uploads/gs_full.json"
TEAM = "SRTHYBYH35"
UDID = "00008110-000C694914F3801E"


def walk(obj, prefix=""):
    """Yield (path, value) for every leaf string."""
    if isinstance(obj, dict):
        for k, v in obj.items():
            yield from walk(v, f"{prefix}.{k}" if prefix else str(k))
    elif isinstance(obj, list):
        for i, v in enumerate(obj):
            yield from walk(v, f"{prefix}[{i}]")
    elif isinstance(obj, (str, int, float)):
        yield prefix, obj


async def main():
    email = input("Apple ID email: ")
    pw = getpass.getpass("Apple ID password (REAL): ")

    prov = RemoteAnisetteProvider("https://ani.sidestore.io")
    acc = AsyncAppleAccount(prov)

    # spy on state transitions: idms_pet / adsid / GsIdmsToken appear here
    # transiently and get overwritten by later stages
    captured_state = {}
    orig_set_state = type(acc)._set_login_state

    def spying_set_login_state(self, state, data=None):
        if data:
            captured_state.update(data)
        return orig_set_state(self, state, data)

    type(acc)._set_login_state = spying_set_login_state

    state = await acc.login(email, pw)
    if state == LoginState.REQUIRE_2FA:
        methods = await acc.get_2fa_methods()
        for i, m in enumerate(methods):
            print(f"  [{i}] {type(m).__name__}")
        idx = int(input("choose method index: "))
        method = methods[idx]
        if hasattr(method, "request"):
            await method.request()
        code = input("enter 2FA code: ").strip()
        state = await method.submit(code)
    print("[+] login state:", state)
    json.dump(captured_state, open(STATE_OUT, "w"), indent=1, default=str)
    print(f"[+] captured state keys: {list(captured_state.keys())}")

    # ---- probe developerservices2 with grand slam credentials ----
    import plistlib
    import requests

    adsid = captured_state.get("adsid", "")
    pet = captured_state.get("idms_pet", "")
    gs_idms = captured_state.get("idms_token") or captured_state.get("GsIdmsToken", "")
    dsid = captured_state.get("dsid") or str(getattr(acc, "_login_state_data", {}).get("dsid", ""))

    def probe(label, cookie_val, extra_headers):
        s = requests.Session()
        s.cookies.set("myacinfo", cookie_val, domain=".apple.com")
        payload = {"clientId": "XABBG36SBA", "myacinfo": cookie_val,
                   "protocolVersion": "QH65B2",
                   "requestId": str(uuid.uuid4()).upper(),
                   "teamId": TEAM, "userLocale": "en_US"}
        h = {"User-Agent": "Xcode", "Content-Type": "text/x-xml-plist",
             "Accept": "text/x-xml-plist",
             "Origin": "https://developer.apple.com", **extra_headers}
        r = s.post(
            "https://developerservices2.apple.com/services/QH65B2/ios/listDevices.action?clientId=XABBG36SBA",
            data=plistlib.dumps(payload), headers=h, timeout=20)
        m = re.search(r"<integer>(\d+)</integer>", r.text)
        code = m.group(1) if m else "?"
        has_dev = "deviceNumber" in r.text
        print(f"[probe] {label}: resultCode={code} devices={has_dev}")
        return code == "0"

    winner = None
    combos = []
    if pet and adsid:
        combos.append(("pet+adsid", pet, {"X-Apple-ADSID": adsid}))
    if gs_idms and adsid:
        combos.append(("gsidms+adsid", gs_idms, {"X-Apple-ADSID": adsid}))
    if dsid:
        for lbl, val, ex in list(combos):
            combos.append((lbl + "+dsid", val, {**ex, "X-Apple-ID-DSID": dsid}))

    for label, val, ex in combos:
        try:
            if probe(label, val, ex):
                winner = (label, val, ex)
                json.dump({"label": label, "cookie_val": val, "headers": ex,
                           "captured_state": captured_state},
                          open("/home/sooku/ipa-share/uploads/gs_working.json", "w"),
                          indent=1)
                print("[!!!] WORKING — saved gs_working.json")
                break
        except Exception as e:
            print(f"[probe] {label}: EXC {e}")

    if not winner:
        print("[!] none of the GS credential mappings worked")
    raise SystemExit(0 if winner else 1)


if __name__ == "__main__":
    asyncio.run(main())
