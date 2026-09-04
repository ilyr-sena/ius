"""Code signing engine: wraps zsign to recursively sign nested iOS bundles."""

from __future__ import annotations

import logging
import os
import shutil
import subprocess
from pathlib import Path
from typing import Optional

log = logging.getLogger(__name__)

_ZSIGN_FALLBACKS = [
    Path.home() / ".local" / "bin" / "zsign",
    Path("/usr/local/bin/zsign"),
    Path("/usr/bin/zsign"),
]


def find_zsign() -> Path:
    """Locate the zsign executable."""
    found = shutil.which("zsign")
    if found:
        return Path(found)
    for candidate in _ZSIGN_FALLBACKS:
        if candidate.exists() and os.access(candidate, os.X_OK):
            return candidate
    raise FileNotFoundError(
        "zsign binary not found in PATH or ~/.local/bin/zsign. "
        "Install zsign: https://github.com/zhlynn/zsign"
    )


def sign_ipa(
    input_ipa: Path | str,
    key_path: Path | str,
    cert_path: Optional[Path | str],
    profile_path: Path | str,
    output_ipa: Optional[Path | str] = None,
    bundle_id: Optional[str] = None,
    bundle_name: Optional[str] = None,
    entitlements_path: Optional[Path | str] = None,
    key_password: Optional[str] = None,
) -> Path:
    """Sign an unsigned or existing IPA with zsign, handling all nested bundles."""
    zsign_bin = find_zsign()

    in_path = Path(input_ipa).resolve()
    if not in_path.exists():
        raise FileNotFoundError(f"Input package not found: {in_path}")

    k_path = Path(key_path).resolve()
    if not k_path.exists():
        raise FileNotFoundError(f"Private key not found: {k_path}")

    m_path = Path(profile_path).resolve()
    if not m_path.exists():
        raise FileNotFoundError(f"Provisioning profile not found: {m_path}")

    if output_ipa is None:
        out_path = in_path.with_name(f"{in_path.stem}-signed.ipa")
    else:
        out_path = Path(output_ipa).resolve()

    out_path.parent.mkdir(parents=True, exist_ok=True)

    cmd = [
        str(zsign_bin),
        "-k", str(k_path),
        "-m", str(m_path),
        "-o", str(out_path),
        "-f",  # force sign without cache
    ]

    if cert_path:
        c_path = Path(cert_path).resolve()
        if not c_path.exists():
            raise FileNotFoundError(f"Certificate not found: {c_path}")
        cmd.extend(["-c", str(c_path)])

    if key_password:
        cmd.extend(["-p", key_password])

    if bundle_id:
        cmd.extend(["-b", bundle_id])

    if bundle_name:
        cmd.extend(["-n", bundle_name])

    if entitlements_path:
        cmd.extend(["-e", str(entitlements_path)])

    cmd.append(str(in_path))

    log.info("signing %s -> %s (zsign)", in_path.name, out_path.name)
    proc = subprocess.run(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )

    if proc.returncode != 0:
        log.error("zsign failed with code %d:\n%s", proc.returncode, proc.stdout)
        raise RuntimeError(f"zsign signing failed: {proc.stdout.strip()}")

    # Print summary from zsign output
    for line in proc.stdout.splitlines():
        if "Signed OK" in line or "AppName:" in line or "BundleId:" in line or "TeamId:" in line:
            log.info("  %s", line.strip())

    log.info("signing complete: %s (%d bytes)", out_path.name, out_path.stat().st_size)
    return out_path
