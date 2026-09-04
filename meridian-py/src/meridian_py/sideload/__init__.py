"""Native iOS sideloader: provisions, recursively signs, and installs IPAs over USB."""

from __future__ import annotations

import logging
import plistlib
import zipfile
from pathlib import Path
from typing import Optional

from .installer import install_ipa
from .profile import ProfileManager
from .signer import sign_ipa

log = logging.getLogger(__name__)


def read_ipa_metadata(ipa_path: Path) -> tuple[str, str]:
    """Read CFBundleIdentifier and CFBundleDisplayName from an unsigned IPA."""
    with zipfile.ZipFile(ipa_path) as z:
        for n in z.namelist():
            if n.startswith("Payload/") and n.endswith(".app/Info.plist") and n.count("/") == 2:
                info = plistlib.loads(z.read(n))
                bid = info.get("CFBundleIdentifier", "app")
                name = info.get("CFBundleDisplayName") or info.get("CFBundleName", "App")
                return bid, name
    return "app", "App"


def sideload_app(
    ipa_path: Path | str,
    udid: Optional[str] = None,
    bundle_id: Optional[str] = None,
    force_renew: bool = False,
    force_login: bool = False,
    custom_key: Optional[Path] = None,
    custom_cert: Optional[Path] = None,
) -> str:
    """Full end-to-end pipeline: resolve profile -> sign nested -> install over USB."""
    in_path = Path(ipa_path).resolve()
    if not in_path.exists():
        raise FileNotFoundError(f"IPA not found: {in_path}")

    # Resolve device UDID if omitted
    if not udid:
        from ..mux import MuxClient
        from ..config import Config
        cfg = Config.defaults()
        devices = MuxClient(cfg.mux_endpoint).list_devices()
        if not devices:
            raise RuntimeError("no iOS devices attached via USB")
        udid = devices[0].udid

    log.info("── sideloading %s onto %s ──", in_path.name, udid[:16])

    # 1. Read app metadata
    orig_bid, app_name = read_ipa_metadata(in_path)
    log.info("app metadata: %s (id: %s)", app_name, orig_bid)

    # 2. Provision / resolve signing identity
    mgr = ProfileManager(udid=udid)

    # Use existing team or default
    ident = mgr.find_local_signing_identity()
    team_id = ident[2] if ident else "SRTHYBYH35"

    target_bid = bundle_id
    if not target_bid:
        if orig_bid.endswith(f".{team_id}"):
            target_bid = orig_bid
        else:
            target_bid = f"{orig_bid}.{team_id}"

    log.info("target bundle id: %s", target_bid)

    key_p, cert_p, prof_p, team_id = mgr.resolve_or_provision(
        bundle_id=target_bid,
        app_name=app_name,
        force_renew=force_renew,
        force_login=force_login,
        custom_key=custom_key,
        custom_cert=custom_cert,
    )

    # 3. Sign IPA with zsign (recursively handles nested .xctest / .framework)
    signed_ipa = sign_ipa(
        input_ipa=in_path,
        key_path=key_p,
        cert_path=cert_p,
        profile_path=prof_p,
        bundle_id=target_bid,
        bundle_name=app_name,
    )

    # 4. Install over USB
    install_ipa(signed_ipa, udid=udid)

    log.info("── sideload complete: %s is installed and ready on device! ──", target_bid)
    return target_bid


__all__ = ["sideload_app", "sign_ipa", "install_ipa", "ProfileManager"]
