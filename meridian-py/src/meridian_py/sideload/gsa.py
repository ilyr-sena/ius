"""Apple GrandSlam (GSA) authentication client.

Speaks SRP-6a to https://gsa.apple.com/grandslam/GsService2, handles 2FA,
and requests genuine Xcode delegation tokens (com.apple.gs.xcode.auth) using local Anisette.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import logging
import plistlib
import time
import uuid
from pathlib import Path
from typing import Any, Optional

import requests
import srp._pysrp as srp

srp.rfc5054_enable()
srp.no_username_in_x()
from cryptography.hazmat.primitives import hashes, padding
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from cryptography.hazmat.primitives.kdf.pbkdf2 import PBKDF2HMAC

from .ca import get_apple_ca_bundle_path
from .login import ensure_anisette_server

log = logging.getLogger(__name__)

GSA_ENDPOINT = "https://gsa.apple.com/grandslam/GsService2"
GSA_VALIDATE_ENDPOINT = "https://gsa.apple.com/grandslam/GsService2/validate"
APP_XCODE = "com.apple.gs.xcode.auth"


def hmac_sha256(key: bytes, strings: list[str | bytes]) -> bytes:
    h = hmac.new(key, digestmod=hashlib.sha256)
    for s in strings:
        h.update(s.encode() if isinstance(s, str) else s)
    return h.digest()


def decode_plist(data: bytes) -> Any:
    """Decode a plist file that may be missing XML headers."""
    plist_header = (
        b"<?xml version='1.0' encoding='UTF-8'?>\n"
        b"<!DOCTYPE plist PUBLIC '-//Apple//DTD PLIST 1.0//EN' 'http://www.apple.com/DTDs/PropertyList-1.0.dtd'>\n"
    )
    data = data.strip()
    if not data.startswith(b"<?xml") and not data.startswith(b"bplist"):
        data = plist_header + data
    return plistlib.loads(data)


def decrypt_spd_aes_cbc(session_key: bytes, ciphertext: bytes) -> dict[str, Any]:
    """Decrypt the spd dictionary returned by GSA complete response."""
    key = hmac_sha256(session_key, ["extra data key:"])
    iv = hmac_sha256(session_key, ["extra data iv:"])[:16]
    cipher = Cipher(algorithms.AES(key), modes.CBC(iv))
    decryptor = cipher.decryptor()
    padded = decryptor.update(ciphertext) + decryptor.finalize()
    padder = padding.PKCS7(128).unpadder()
    plaintext = padder.update(padded) + padder.finalize()
    return decode_plist(plaintext)


def decrypt_token_aes_gcm(sk: bytes, et: bytes) -> dict[str, Any]:
    """Decrypt the et token bundle returned by GSA apptokens response."""
    et_bytes = bytes(et)
    version_size = 3
    iv_size = 16
    aad = et_bytes[:version_size]
    nonce = et_bytes[version_size : version_size + iv_size]
    ciphertext_and_tag = et_bytes[version_size + iv_size :]
    aesgcm = AESGCM(sk)
    plaintext = aesgcm.decrypt(nonce, ciphertext_and_tag, aad)
    return decode_plist(plaintext)


def make_password_x(password: str, salt: bytes, iterations: int, is_hex: bool) -> bytes:
    p_bytes = password.encode()
    digest = hashlib.sha256(p_bytes).digest()
    input_digest = digest.hex().encode() if is_hex else digest
    kdf = PBKDF2HMAC(
        algorithm=hashes.SHA256(),
        length=len(digest),
        salt=salt,
        iterations=iterations,
    )
    return kdf.derive(input_digest)


def fetch_local_anisette_headers(user_id: str, device_id: str, port: int = 6969) -> dict[str, str]:
    """Query the local omnisette-server for genuine FairPlay Anisette headers."""
    ani_url = ensure_anisette_server(port)
    r = requests.get(ani_url, timeout=5)
    r.raise_for_status()
    headers = r.json()
    headers["X-Apple-I-MD-LU"] = base64.b64encode(user_id.encode()).decode()
    headers["X-Mme-Device-Id"] = device_id.upper()
    return headers


class GSAClient:
    """Manages the full GrandSlam SRP lifecycle to obtain com.apple.gs.xcode.auth tokens."""

    def __init__(self, username: str, anisette_port: int = 6969):
        self.username = username.strip().lower()
        self.anisette_port = anisette_port
        self.device_id = str(uuid.uuid4()).upper()

    def _client_cpd(self, anisette: dict[str, str]) -> dict[str, Any]:
        cpd: dict[str, Any] = {
            "bootstrap": True,
            "icscrec": True,
            "pbe": False,
            "prkgen": True,
            "svct": "iCloud",
            "loc": "en_US",
            "X-Apple-Locale": "en_US",
        }
        cpd.update(anisette)
        return cpd

    def _post_gsa(self, params: dict[str, Any], anisette: dict[str, str]) -> dict[str, Any]:
        body = {
            "Header": {"Version": "1.0.1"},
            "Request": {
                "cpd": self._client_cpd(anisette),
                **params,
            },
        }
        headers = {
            "Content-Type": "text/x-xml-plist",
            "Accept": "*/*",
            "User-Agent": "akd/1.0 CFNetwork/978.0.7 Darwin/18.7.0",
            "X-MMe-Client-Info": anisette.get("X-MMe-Client-Info", ""),
        }
        data = plistlib.dumps(body)
        r = requests.post(
            GSA_ENDPOINT,
            data=data,
            headers=headers,
            timeout=20,
            verify=get_apple_ca_bundle_path(),
        )
        r.raise_for_status()
        resp = decode_plist(r.content)
        return resp.get("Response", {})

    def authenticate(self, password: str) -> dict[str, Any]:
        """Perform full SRP authentication, handling 2FA, and returns Xcode auth tokens."""
        anisette = fetch_local_anisette_headers(self.username, self.device_id, self.anisette_port)

        # 1. Start SRP authentication (init)
        usr = srp.User(self.username, b"", hash_alg=srp.SHA256, ng_type=srp.NG_2048)
        _, a2k = usr.start_authentication()

        init_params = {
            "A2k": a2k,
            "ps": ["s2k", "s2k_fo"],
            "o": "init",
            "u": self.username,
        }
        resp_init = self._post_gsa(init_params, anisette)
        status = resp_init.get("Status", {})
        if status.get("ec") != 0:
            raise RuntimeError(f"GSA init failed: {status.get('em')}")

        c = resp_init["c"]
        s = resp_init["s"]
        i = resp_init["i"]
        B = resp_init["B"]
        sp = resp_init.get("sp", "s2k")

        # 2. Challenge response (complete)
        usr.p = make_password_x(password, s, i, sp == "s2k_fo")
        m1 = usr.process_challenge(s, B)
        if m1 is None:
            raise RuntimeError("Failed to compute SRP challenge proof M1")

        complete_params = {
            "c": c,
            "M1": m1,
            "o": "complete",
            "u": self.username,
        }
        resp_complete = self._post_gsa(complete_params, anisette)
        status = resp_complete.get("Status", {})
        ec = status.get("ec", 0)
        if ec != 0:
            raise RuntimeError(f"GSA password verification failed: {status.get('em')}")

        m2 = resp_complete.get("M2")
        usr.verify_session(m2)
        if not usr.authenticated():
            raise RuntimeError("GSA server session proof M2 verification failed")

        session_key = usr.get_session_key() or b""
        spd = decrypt_spd_aes_cbc(session_key, resp_complete["spd"])
        dsid = str(spd.get("adsid", ""))
        idms_token = spd.get("GsIdmsToken", "")

        # 3. Handle 2-Factor Authentication if required
        au = status.get("au")
        if au in ("secondaryAuth", "trustedDeviceSecondaryAuth"):
            print("\n" + "─" * 50)
            print("  Two-Factor Authentication Required")
            print("  Apple has sent a verification prompt to your Apple devices.")
            print("─" * 50)

            # Request 2FA trigger
            req_headers = self._make_2fa_headers(dsid, idms_token, anisette)
            requests.get(
                "https://gsa.apple.com/auth/verify/trusteddevice",
                headers=req_headers,
                timeout=15,
                verify=get_apple_ca_bundle_path(),
            )

            code = input("Enter 6-digit verification code: ").strip()

            # Submit 2FA verification code
            verify_headers = self._make_2fa_headers(dsid, idms_token, anisette)
            verify_headers["security-code"] = code
            r_verify = requests.get(
                GSA_VALIDATE_ENDPOINT,
                headers=verify_headers,
                timeout=15,
                verify=get_apple_ca_bundle_path(),
            )
            r_verify.raise_for_status()

            # Re-authenticate after 2FA validation to receive authenticated session key
            print("✓ Code verified. Finalizing developer session...")
            return self.authenticate(password)

        # 4. Request Xcode Delegation Token (apptokens)
        log.info("requesting Xcode delegation token (%s)...", APP_XCODE)
        sk = bytes(spd["sk"])
        checksum = hmac_sha256(sk, ["apptokens", dsid, APP_XCODE])
        apptokens_params = {
            "app": [APP_XCODE],
            "c": spd["c"],
            "checksum": checksum,
            "o": "apptokens",
            "t": idms_token,
            "u": dsid,
        }
        resp_tokens = self._post_gsa(apptokens_params, anisette)
        et = resp_tokens.get("et")
        if not et:
            raise RuntimeError(f"GSA did not return an encrypted token: {resp_tokens}")

        tokens_dict = decrypt_token_aes_gcm(sk, bytes(et))
        app_tokens = tokens_dict.get("t", {}).get(APP_XCODE, {})
        auth_token = app_tokens.get("token")
        if not auth_token:
            raise RuntimeError(f"Xcode auth token missing in response: {tokens_dict}")

        print("✓ Successfully acquired Xcode developer session token!")
        return {
            "authToken": auth_token,
            "dsid": dsid,
            "anisette": anisette,
        }

    def _make_2fa_headers(self, dsid: str, idms_token: str, anisette: dict[str, str]) -> dict[str, str]:
        ident = base64.b64encode(f"{dsid}:{idms_token}".encode()).decode()
        return {
            "Accept": "application/x-buddyml",
            "Accept-Language": "en-us",
            "Content-Type": "application/x-plist",
            "User-Agent": "Xcode",
            "X-Apple-App-Info": APP_XCODE,
            "X-Xcode-Version": "11.2 (11B41)",
            "X-Apple-Identity-Token": ident,
            **anisette,
        }
