"""Provisioning profile and certificate manager for local iOS code signing."""

from __future__ import annotations

import asyncio
import io
import logging
import plistlib
import re
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

from cryptography import x509
from cryptography.hazmat.primitives import serialization
from cryptography.x509.oid import NameOID

from .login import browser_login, load_session, terminal_login
from .provision import (
    DeveloperServicesClient,
    DeveloperServicesError,
    generate_key_and_csr,
    save_key_and_cert,
)

log = logging.getLogger(__name__)

STATE_DIR = Path.home() / ".local" / "state" / "meridian"
CERTS_DIR = STATE_DIR / "certs"
PROFILES_DIR = STATE_DIR / "profiles"
SESSION_FILE = STATE_DIR / "apple_session.json"

LEGACY_SHARE_UPLOADS = Path.home() / "ipa-share" / "uploads"


def parse_mobileprovision(data: bytes) -> dict:
    """Extract and parse the XML plist from a signed .mobileprovision file."""
    match = re.search(rb"<\?xml.*?</plist>", data, re.S)
    if not match:
        raise ValueError("No embedded XML plist found in provisioning profile")
    return plistlib.loads(match.group(0))


def cert_der_from_pem(pem_bytes: bytes) -> bytes:
    cert = x509.load_pem_x509_certificate(pem_bytes)
    return cert.public_bytes(serialization.Encoding.DER)


def team_id_from_cert(cert_path: Path) -> Optional[str]:
    """Extract Team ID (OU) from a developer certificate."""
    try:
        cert = x509.load_pem_x509_certificate(cert_path.read_bytes())
        ous = cert.subject.get_attributes_for_oid(NameOID.ORGANIZATIONAL_UNIT_NAME)
        if ous:
            return ous[0].value
    except Exception as e:
        log.debug("team_id_from_cert failed: %s", e)
    return None


def key_matches_cert(key_path: Path, cert_path: Path) -> bool:
    """True if private key corresponds to the certificate public key."""
    try:
        key = serialization.load_pem_private_key(key_path.read_bytes(), password=None)
        cert = x509.load_pem_x509_certificate(cert_path.read_bytes())
        k_der = key.public_key().public_bytes(
            serialization.Encoding.DER, serialization.PublicFormat.SubjectPublicKeyInfo
        )
        c_der = cert.public_key().public_bytes(
            serialization.Encoding.DER, serialization.PublicFormat.SubjectPublicKeyInfo
        )
        return k_der == c_der
    except Exception as e:
        log.debug("key_matches_cert check failed: %s", e)
        return False


class ProfileManager:
    """Resolves, reuses, and provisions Apple certificates and mobileprovision files."""

    def __init__(self, udid: str):
        self.udid = udid
        CERTS_DIR.mkdir(parents=True, exist_ok=True)
        PROFILES_DIR.mkdir(parents=True, exist_ok=True)

    def find_local_signing_identity(self) -> Optional[tuple[Path, Path, str]]:
        """Find an existing, valid (key.pem, cert.pem, team_id) pair."""
        candidates = [
            (CERTS_DIR / "key.pem", CERTS_DIR / "cert.pem"),
            (LEGACY_SHARE_UPLOADS / "key.pem", LEGACY_SHARE_UPLOADS / "cert-sookugs@gmail.com.pem"),
        ]

        for k, c in candidates:
            if k.exists() and c.exists() and key_matches_cert(k, c):
                tid = team_id_from_cert(c) or "SRTHYBYH35"
                # Check expiration
                try:
                    cert = x509.load_pem_x509_certificate(c.read_bytes())
                    if cert.not_valid_after_utc > datetime.now(timezone.utc):
                        log.info("using signing identity: %s (team %s)", c.name, tid)
                        return k, c, tid
                except Exception:
                    pass
        return None

    def find_cached_profile(self, bundle_id: str, cert_der: Optional[bytes] = None) -> Optional[Path]:
        """Check local disk cache for a valid, non-expired profile."""
        profile_path = PROFILES_DIR / f"{bundle_id}.mobileprovision"
        if not profile_path.exists():
            return None

        try:
            plist = parse_mobileprovision(profile_path.read_bytes())
            exp = plist.get("ExpirationDate")
            if isinstance(exp, str):
                exp = datetime.fromisoformat(exp.replace("Z", "+00:00"))
            if exp and exp.tzinfo is None:
                exp = exp.replace(tzinfo=timezone.utc)
            if exp and exp <= datetime.now(timezone.utc):
                log.info("cached profile for %s expired on %s", bundle_id, exp)
                return None

            devices = [d.lower() for d in plist.get("ProvisionedDevices", [])]
            if self.udid.lower() not in devices:
                log.info("cached profile does not include device %s", self.udid)
                return None

            if cert_der:
                cert_ders = [
                    bytes(c) if not isinstance(c, bytes) else c
                    for c in plist.get("DeveloperCertificates", [])
                ]
                if not any(c.strip() == cert_der.strip() for c in cert_ders):
                    log.info("cached profile is not bound to current certificate")
                    return None

            log.info("reusing cached profile for %s (expires %s)", bundle_id, exp)
            return profile_path
        except Exception as e:
            log.warning("error reading cached profile: %s", e)
            return None

    def extract_from_device(self, bundle_id: str, cert_der: Optional[bytes] = None) -> Optional[Path]:
        """Extract a matching installed profile directly from the connected iPhone."""
        from pymobiledevice3.lockdown import create_using_usbmux
        from pymobiledevice3.services.misagent import MisagentService

        async def _query():
            try:
                async with await create_using_usbmux(serial=self.udid) as lockdown:
                    mis = MisagentService(lockdown=lockdown)
                    return await mis.copy_all()
            except Exception as e:
                log.debug("could not query device profiles: %s", e)
                return []

        profiles = asyncio.run(_query())
        norm_bid = bundle_id.lower()

        for p in profiles:
            app_id = p.plist.get("Entitlements", {}).get("application-identifier", "").lower()
            if norm_bid in app_id:
                if cert_der:
                    c_ders = [
                        bytes(c) if not isinstance(c, bytes) else c
                        for c in p.plist.get("DeveloperCertificates", [])
                    ]
                    if not any(c.strip() == cert_der.strip() for c in c_ders):
                        continue

                out = PROFILES_DIR / f"{bundle_id}.mobileprovision"
                out.write_bytes(p.buf)
                log.info("extracted active profile for %s from device -> %s", bundle_id, out.name)
                return out

        return None

    def install_profile_to_device(self, profile_bytes: bytes) -> None:
        """Upload and install a newly minted profile to the connected iPhone."""
        from pymobiledevice3.lockdown import create_using_usbmux
        from pymobiledevice3.services.misagent import MisagentService

        async def _install():
            async with await create_using_usbmux(serial=self.udid) as lockdown:
                mis = MisagentService(lockdown=lockdown)
                await mis.install(io.BytesIO(profile_bytes))
                log.info("installed profile directly onto device")

        try:
            asyncio.run(_install())
        except Exception as e:
            log.warning("could not register profile on device misagent (will rely on embedded): %s", e)

    def resolve_or_provision(
        self,
        bundle_id: str,
        app_name: str = "App",
        force_renew: bool = False,
        force_login: bool = False,
        custom_key: Optional[Path] = None,
        custom_cert: Optional[Path] = None,
        apple_id: Optional[str] = None,
        password: Optional[str] = None,
        use_browser: bool = False,
    ) -> tuple[Path, Path, Path, str]:
        """Ensure valid key, cert, profile, and team_id exist for this bundle_id."""
        key_path = custom_key
        cert_path = custom_cert
        team_id = "SRTHYBYH35"

        # 1. Resolve signing identity (key + cert)
        if not (key_path and cert_path):
            ident = self.find_local_signing_identity()
            if ident:
                key_path, cert_path, team_id = ident

        cert_der = cert_der_from_pem(cert_path.read_bytes()) if cert_path and cert_path.exists() else None

        # 2. Check if we have an active profile matching key+cert
        if not force_renew and key_path and cert_path and cert_der:
            cached = self.find_cached_profile(bundle_id, cert_der=cert_der)
            if cached:
                return key_path, cert_path, cached, team_id

            from_dev = self.extract_from_device(bundle_id, cert_der=cert_der)
            if from_dev:
                return key_path, cert_path, from_dev, team_id

        # 3. Need to mint fresh credentials or profile via Apple Developer Services
        log.info("provisioning fresh profile for %s (team %s)...", bundle_id, team_id)
        session_cookies = None
        if not force_login and not force_renew:
            session_cookies = load_session(SESSION_FILE)

        if not session_cookies:
            if use_browser:
                session_cookies = browser_login(SESSION_FILE)
            else:
                try:
                    session_cookies = terminal_login(SESSION_FILE, email=apple_id, password=password)
                except Exception as e:
                    log.warning("terminal login failed (%s) — falling back to browser login...", e)
                    session_cookies = browser_login(SESSION_FILE)

        for attempt in range(2):
            client = DeveloperServicesClient(session_cookies)
            try:
                team_id = client.get_team_id()
                dev_id = client.get_or_register_device(team_id, self.udid, device_name="iPhone")
                break
            except DeveloperServicesError as e:
                if e.code in (1100, 3018, 3050) and attempt == 0:
                    log.info("session expired on Apple servers, re-authenticating...")
                    session_cookies = terminal_login(SESSION_FILE, email=apple_id, password=password)
                else:
                    raise
        log.info("using developer team: %s", team_id)

        # Check or submit certificate
        active_cert_id = None
        if cert_der and cert_path and key_path and key_matches_cert(key_path, cert_path):
            for c in client.list_certs(team_id):
                c_content = c.get("certContent")
                c_raw = bytes(c_content) if not isinstance(c_content, bytes) else c_content
                if c_raw and c_raw.strip() == cert_der.strip():
                    active_cert_id = c["certificateId"]
                    log.info("found active certificate on account: %s", active_cert_id)
                    break

        if not active_cert_id:
            log.info("generating a fresh private key and developer certificate on Linux...")
            new_key, csr_pem = generate_key_and_csr()
            try:
                active_cert_id, new_cert_der = client.submit_csr(team_id, csr_pem)
            except DeveloperServicesError as e:
                # If cert limit reached, revoke the oldest development cert and retry
                log.info("cert limit reached (%s) — revoking oldest development cert...", e)
                certs = client.list_certs(team_id)
                if certs:
                    old_id = certs[0]["certificateId"]
                    log.info("revoking cert %s", old_id)
                    client.revoke_cert(team_id, old_id)
                active_cert_id, new_cert_der = client.submit_csr(team_id, csr_pem)

            k_save = CERTS_DIR / "key.pem"
            c_save = CERTS_DIR / "cert.pem"
            key_path, cert_path = save_key_and_cert(new_key, new_cert_der, k_save, c_save)
            cert_der = new_cert_der

        # Get or create App ID
        app_id_id = client.get_or_create_app_id(team_id, bundle_id, app_name)

        # Create or regenerate provisioning profile
        prof_name = f"iOS Team Provisioning Profile: {bundle_id}"
        profile_bytes = client.create_or_regen_profile(
            team_id=team_id,
            app_id_id=app_id_id,
            device_id=dev_id,
            cert_id=active_cert_id,
            profile_name=prof_name,
        )

        out_prof = PROFILES_DIR / f"{bundle_id}.mobileprovision"
        out_prof.write_bytes(profile_bytes)
        log.info("saved profile to %s", out_prof)

        # Also push to device misagent
        self.install_profile_to_device(profile_bytes)

        return key_path, cert_path, out_prof, team_id
