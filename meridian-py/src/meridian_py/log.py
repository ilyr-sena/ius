"""Structured, colorized logging with levels.

Defaults to INFO; honors -v/-vv, RUST_LOG-style env override of level via
MERIDIAN_LOG_LEVEL, and falls back to plain text when not a TTY.
"""

import logging
import os
import sys

_TERM_COLOR = sys.stderr.isatty()

_COLORS = {
    "TRACE": "\033[90m",   # gray
    "DEBUG": "\033[36m",   # cyan
    "INFO": "\033[32m",    # green
    "WARNING": "\033[33m", # yellow
    "ERROR": "\033[31m",   # red
    "CRITICAL": "\033[1;31m",
}
_RESET = "\033[0m"
_DIM = "\033[2m"

# Add the custom TRACE level below DEBUG.
TRACE = 5
logging.addLevelName(TRACE, "TRACE")


class _Formatter(logging.Formatter):
    def format(self, record: logging.LogRecord) -> str:
        level = record.levelname
        name = record.name
        sym = {
            "TRACE": "·", "DEBUG": "·", "INFO": "●",
            "WARNING": "!", "ERROR": "✗", "CRITICAL": "✗✗",
        }.get(level, "?")
        if _TERM_COLOR:
            colored_level = f"{_COLORS.get(level, '')}{sym}{_RESET}"
            dim_source = f"{_DIM}{name}{_RESET}"
            return f" {colored_level} {dim_source} {record.getMessage()}"
        return f"{level} {name}: {record.getMessage()}"


def init(level: int = logging.INFO) -> None:
    env_level = os.environ.get("MERIDIAN_LOG_LEVEL", "").upper()
    if env_level:
        if env_level == "TRACE":
            level = TRACE
        else:
            level = getattr(logging, env_level, level)

    handler = logging.StreamHandler(sys.stdout)
    handler.setFormatter(_Formatter())
    for name in ("meridian", "meridian_py"):
        r = logging.getLogger(name)
        r.handlers.clear()
        r.addHandler(handler)
        r.setLevel(level)
        r.propagate = False

    # Quiet down the noisy channels.
    for noisy in ("uvicorn", "uvicorn.error", "uvicorn.access",
                  "pymobiledevice3", "pymobiledevice3.tunneld", "urllib3", "requests"):
        logging.getLogger(noisy).setLevel(max(level, logging.WARNING))
