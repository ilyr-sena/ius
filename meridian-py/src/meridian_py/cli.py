"""CLI: detection, watching, info, orchestration.

Every subcommand resolves the current config from layered sources and then
delegates. Nothing here does network/pproc work on its own.
"""

from __future__ import annotations

import argparse
import sys
import traceback

from .config import Config
from .log import init as init_log


def _verbosity_args() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(add_help=False)
    p.add_argument("-v", "--verbose", action="count", default=0, help="more logs")
    p.add_argument("-q", "--quiet", action="store_true", help="only errors")
    return p


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="meridian-py")

    # Global verbosity knobs are ignored in parsers constructed elsewhere;
    # they're consumed by cli.main.
    p.add_argument("-v", "--verbose", action="count", default=0)
    p.add_argument("-q", "--quiet", action="store_true")

    sub = p.add_subparsers(dest="cmd", required=True)

    sub.add_parser("list", help="show attached devices (enriched if possible)")

    p_info = sub.add_parser("info", help="full metadata for one device")
    p_info.add_argument("udid", nargs="?", default=None)

    sub.add_parser("watch", help="stream attach/detach events")

    p_run = sub.add_parser("run", help="full orchestrated session")
    p_run.add_argument("--udid", default=None)
    p_run.add_argument("--no-wda", action="store_true")
    p_run.add_argument("--no-hid", action="store_true")
    p_run.add_argument("--no-tunnels", action="store_true")
    p_run.add_argument(
        "--no-stream",
        action="store_true",
        help="skip launching the screen-stream probe on-device",
    )
    p_run.add_argument("--no-http", action="store_true", help="don't run the :9001 HTTP API")

    p_tun = sub.add_parser("tunnel", help="iproxy-style tunnels only")
    p_tun.add_argument("pairs", nargs="+");
    p_tun.add_argument("--udid", default=None)

    return p


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()

    cfg = Config.from_sources(cli_args=args)
    cfg.ensure_dirs()
    init_log(_log_level(cfg))

    try:
        match args.cmd:
            case "list":
                from .commands import list_devices_cmd
                return list_devices_cmd(cfg)
            case "info":
                from .commands import info_cmd
                return info_cmd(cfg, args.udid)
            case "watch":
                from .commands import watch_cmd
                return watch_cmd(cfg)
            case "run":
                from .runner import run_session
                return run_session(cfg, args)
            case "tunnel":
                from .commands import tunnel_cmd
                return tunnel_cmd(cfg, args.pairs, args.udid)
            case other:
                parser.error(f"unknown command: {other}")
    except (SystemExit, KeyboardInterrupt):
        return 130
    except Exception:
        traceback.print_exc()
        return 2
    return 0


def _log_level(cfg: Config) -> int:
    import logging
    return {
        "trace": 5,
        "debug": logging.DEBUG,
        "info": logging.INFO,
        "error": logging.ERROR,
    }.get(cfg.log_level, logging.INFO)


if __name__ == "__main__":
    sys.exit(main())
