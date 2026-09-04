"""Installs iOS packages (.ipa / .app) directly over USB via usbmuxd."""

from __future__ import annotations

import asyncio
import logging
from pathlib import Path
from typing import Callable, Optional

from pymobiledevice3.lockdown import create_using_usbmux
from pymobiledevice3.services.installation_proxy import InstallationProxyService

log = logging.getLogger(__name__)


def install_ipa(
    ipa_path: Path | str,
    udid: Optional[str] = None,
    progress_cb: Optional[Callable[[int, str], None]] = None,
) -> None:
    """Install an IPA file onto the connected iOS device over USB."""
    resolved_path = Path(ipa_path).resolve()
    if not resolved_path.exists():
        raise FileNotFoundError(f"IPA not found: {resolved_path}")

    async def _install():
        async with await create_using_usbmux(serial=udid) as lockdown:
            svc = InstallationProxyService(lockdown=lockdown)

            def _handler(pct: int, *args):
                log.info("install: %s%% Complete", pct)
                if progress_cb:
                    progress_cb(pct, f"{pct}% Complete")

            log.info("transferring and installing %s onto device...", resolved_path.name)
            await svc.install_from_local(str(resolved_path), handler=_handler)
            log.info("installation successful!")

    asyncio.run(_install())
