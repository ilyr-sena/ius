"""Apple Developer API (developerservices2) client for free development accounts."""

from __future__ import annotations

import logging
import plistlib
import re
import uuid
from pathlib import Path
from typing import Any, Optional

import requests
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID

from .ca import get_apple_ca_bundle_path

log = logging.getLogger(__name__)

CLIENT_ID = "XABBG36SBA"
APP_ID_KEY = "ba2ec180e6ca6e6c6a542255453b24d6e6e5b2be0cc48bc1b0d8ad64cfe0228f"
BASE_URL = "https://developerservices2.apple.com/services/QH65B2"


class DeveloperServicesError(Exception):
    def __init__(self, code: int, msg: str, user_string: Optional[str] = None):
        super().__init__(f"[{code}] {user_string or msg}")
        self.code = code
        self.msg = msg
        self.user_string = user_string


class DeveloperServicesClient:
    """Speaks the developerservices2 protocol to mint certificates and profiles."""

    def __init__(self, session_data: list[dict] | dict):
        self.http = requests.Session()
        self.http.verify = get_apple_ca_bundle_path()
        self.auth_token: Optional[str] = None
        self.dsid: Optional[str] = None
        self.anisette: dict[str, str] = {}

        if isinstance(session_data, dict) and "authToken" in session_data:
            self.auth_token = session_data["authToken"]
            self.dsid = str(session_data.get("dsid", ""))
            self.anisette = session_data.get("anisette", {})
        else:
            cookie_list = session_data["cookies"] if isinstance(session_data, dict) and "cookies" in session_data else session_data
            for c in cookie_list:
                if isinstance(c, dict) and "name" in c and "value" in c:
                    self.http.cookies.set(
                        c["name"],
                        c["value"],
                        domain=c.get("domain", ".apple.com"),
                        path=c.get("path", "/"),
                    )
            if isinstance(session_data, dict):
                for k, v in session_data.get("auth_headers", {}).items():
                    self.http.headers[k] = v

    @property
    def myacinfo(self) -> str:
        for c in self.http.cookies:
            if c.name == "myacinfo":
                return c.value
        return ""

    def _headers(self) -> dict[str, str]:
        if self.auth_token:
            ani = dict(self.anisette)
            try:
                r = requests.get("http://127.0.0.1:6969", timeout=2)
                if r.ok:
                    fresh = r.json()
                    ani["X-Apple-I-Client-Time"] = fresh.get("X-Apple-I-Client-Time", ani.get("X-Apple-I-Client-Time", ""))
                    if "X-Apple-I-MD-M" in fresh:
                        ani["X-Apple-I-MD-M"] = fresh["X-Apple-I-MD-M"]
                    if "X-Apple-I-MD" in fresh:
                        ani["X-Apple-I-MD"] = fresh["X-Apple-I-MD"]
            except Exception:
                pass

            h = {
                "Content-Type": "text/x-xml-plist",
                "User-Agent": "Xcode",
                "Accept": "text/x-xml-plist",
                "Accept-Language": "en-us",
                "X-Apple-App-Info": "com.apple.gs.xcode.auth",
                "X-Xcode-Version": "11.2 (11B41)",
                "X-Apple-I-Identity-Id": str(self.dsid),
                "X-Apple-GS-Token": str(self.auth_token),
                **ani,
            }
            return h

        h = {
            "User-Agent": "Xcode",
            "X-Apple-Widget-Key": APP_ID_KEY,
            "Content-Type": "text/x-xml-plist",
            "Accept": "text/x-xml-plist",
            "Origin": "https://developer.apple.com",
        }
        for k, v in self.http.headers.items():
            h[k] = v
        return h

    def call(self, action: str, team_id: str = "", **extra: Any) -> dict[str, Any]:
        """Send an action request to developerservices2."""
        payload: dict[str, Any] = {
            "clientId": CLIENT_ID,
            "protocolVersion": "QH65B2",
            "requestId": str(uuid.uuid4()).upper(),
        }
        if not self.auth_token:
            payload["myacinfo"] = self.myacinfo
        if team_id:
            payload["teamId"] = team_id
        payload.update(extra)

        url = f"{BASE_URL}/{action}.action?clientId={CLIENT_ID}"
        body = plistlib.dumps(payload)
        resp = self.http.post(url, data=body, headers=self._headers(), timeout=30)
        resp.raise_for_status()

        if resp.content.strip().startswith(b"{"):
            import json as _json
            data = _json.loads(resp.content)
        else:
            try:
                data = plistlib.loads(resp.content)
            except plistlib.InvalidFileException:
                header = (
                    b"<?xml version='1.0' encoding='UTF-8'?>\n"
                    b"<!DOCTYPE plist PUBLIC '-//Apple//DTD PLIST 1.0//EN' 'http://www.apple.com/DTDs/PropertyList-1.0.dtd'>\n"
                )
                data = plistlib.loads(header + resp.content.strip())

        code = data.get("resultCode", 0)
        if code != 0:
            err_msg = data.get("errorMessage", "Unknown error")
            user_str = data.get("userString")
            raise DeveloperServicesError(code, err_msg, user_str)
        return data

    def list_teams(self) -> list[dict[str, Any]]:
        """List developer teams associated with this Apple account."""
        data = self.call("listTeams")
        teams = data.get("teams", [])
        if not teams and "myTeam" in data:
            teams = [data["myTeam"]]
        return teams

    def get_team_id(self, fallback: str = "SRTHYBYH35") -> str:
        """Get the primary team ID, falling back to personal team ID on free accounts."""
        teams = self.list_teams()
        if teams:
            return teams[0]["teamId"]
        return fallback

    def list_devices(self, team_id: str) -> list[dict[str, Any]]:
        data = self.call("ios/listDevices", team_id)
        return data.get("devices", [])

    def get_or_register_device(self, team_id: str, udid: str, device_name: str = "iPhone") -> str:
        """Find the deviceId for a UDID, registering it if needed."""
        norm_udid = udid.lower().strip()
        for dev in self.list_devices(team_id):
            if dev.get("deviceNumber", "").lower().strip() == norm_udid:
                return dev["deviceId"]

        log.info("registering device %s (%s) on developer account...", udid, device_name)
        data = self.call(
            "ios/addDevice",
            team_id,
            deviceNumber=udid,
            name=device_name,
            DTDK_Platform="ios",
        )
        dev = data.get("device") or {}
        if dev.get("deviceId"):
            return dev["deviceId"]
        return udid

    def list_certs(self, team_id: str) -> list[dict[str, Any]]:
        data = self.call("ios/listAllDevelopmentCerts", team_id)
        return data.get("certificates", [])

    def submit_csr(self, team_id: str, csr_pem: str) -> tuple[str, bytes]:
        """Submit a CSR to Apple and receive a signed development certificate."""
        data = self.call(
            "ios/submitDevelopmentCSR",
            team_id,
            csrContent=csr_pem,
            machineId=str(uuid.uuid4()).upper(),
            machineName="MacBook Pro",
        )
        req = data.get("certRequest") or {}
        cert_id = req.get("certificateId", "")
        content = req.get("certContent")
        raw_cert = bytes(content) if content else b""
        return cert_id, raw_cert

    def revoke_cert(self, team_id: str, cert_id: str, serial: str = "") -> None:
        kwargs: dict[str, Any] = {}
        if cert_id:
            kwargs["certificateId"] = cert_id
        if serial:
            kwargs["serialNumber"] = serial
        self.call("ios/revokeDevelopmentCert", team_id, **kwargs)

    def get_or_create_app_id(self, team_id: str, bundle_id: str, name: str) -> str:
        """Find an existing App ID or create a new one."""
        data = self.call("ios/listAppIds", team_id)
        for app in data.get("appIds", []):
            if app.get("identifier") == bundle_id:
                return app["appIdId"]

        log.info("creating new App ID for %s (%s)...", bundle_id, name)
        data = self.call(
            "ios/addAppId",
            team_id,
            identifier=bundle_id,
            name=name,
        )
        app = data.get("appId") or {}
        if not app.get("appIdId"):
            raise RuntimeError(f"addAppId failed: {data}")
        return app["appIdId"]

    def download_team_profile(self, team_id: str, app_id_id: str) -> bytes:
        """Download the active 7-day development provisioning profile for an App ID."""
        data = self.call(
            "ios/downloadTeamProvisioningProfile",
            team_id,
            appIdId=app_id_id,
            DTDK_Platform="ios",
        )
        prof = data.get("provisioningProfile") or {}
        content = prof.get("encodedProfile")
        raw = bytes(content) if content else b""
        if not raw:
            raise RuntimeError(f"Apple returned empty profile: {data}")
        return raw

    def create_or_regen_profile(
        self,
        team_id: str,
        app_id_id: str,
        device_id: str,
        cert_id: str,
        profile_name: str,
    ) -> bytes:
        """Create or regenerate a 7-day development provisioning profile."""
        try:
            return self.download_team_profile(team_id, app_id_id)
        except Exception as e:
            log.debug("downloadTeamProvisioningProfile fallback: %s", e)

        payload = {
            "appIdId": app_id_id,
            "deviceIds": [device_id],
            "certificateIds": [cert_id],
            "distributionType": "limited",
            "provisioningProfileName": profile_name,
        }

        for action in ("ios/createProvisioningProfile", "ios/regenProvisioningProfile"):
            try:
                data = self.call(action, team_id, **payload)
                prof = data.get("provisioningProfile")
                if prof and prof.get("encodedProfile"):
                    content = prof["encodedProfile"]
                    raw = bytes(content) if content else b""
                    return raw
            except DeveloperServicesError as e:
                log.debug("%s returned %s — trying next", action, e)

        raise RuntimeError(f"failed to create or regenerate profile '{profile_name}'")


# ---------------------------------------------------------------------------
# Cryptography helpers: private key and CSR generation
# ---------------------------------------------------------------------------

def generate_key_and_csr() -> tuple[rsa.RSAPrivateKey, str]:
    """Generate a standard RSA 2048-bit key and an Apple Developer CSR."""
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    csr = (
        x509.CertificateSigningRequestBuilder()
        .subject_name(
            x509.Name([
                x509.NameAttribute(NameOID.COUNTRY_NAME, "US"),
                x509.NameAttribute(NameOID.COMMON_NAME, "iPhone Developer"),
            ])
        )
        .sign(key, hashes.SHA256())
    )
    csr_pem = csr.public_bytes(serialization.Encoding.PEM).decode()
    return key, csr_pem


def save_key_and_cert(
    key: rsa.RSAPrivateKey,
    cert_der: bytes,
    key_path: Path,
    cert_path: Path,
) -> tuple[Path, Path]:
    """Save private key and certificate to disk."""
    key_path.parent.mkdir(parents=True, exist_ok=True)
    cert_path.parent.mkdir(parents=True, exist_ok=True)

    key_pem = key.private_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PrivateFormat.PKCS8,
        encryption_algorithm=serialization.NoEncryption(),
    )
    key_path.write_bytes(key_pem)

    cert = x509.load_der_x509_certificate(cert_der)
    cert_pem = cert.public_bytes(serialization.Encoding.PEM)
    cert_path.write_bytes(cert_pem)

    log.info("saved signing key to %s and cert to %s", key_path, cert_path)
    return key_path, cert_path
