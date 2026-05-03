import logging
import os
import sys
import time


def configure_logging(level: int = logging.INFO) -> None:
    """Set up root logger with a UTC timestamp formatter writing to stdout.

    Safe to call multiple times (replaces existing handlers).
    Respects GREMLINS_LOG_LEVEL env var (e.g. DEBUG, INFO, WARNING).
    """
    env_level = os.environ.get("GREMLINS_LOG_LEVEL")
    if env_level:
        level = getattr(logging, env_level.upper(), level)
    fmt = logging.Formatter(
        "%(asctime)s %(levelname)s %(name)s %(message)s",
        datefmt="%Y-%m-%dT%H:%M:%SZ",
    )
    fmt.converter = time.gmtime
    handler = logging.StreamHandler(sys.stdout)
    handler.setFormatter(fmt)
    root = logging.getLogger()
    root.setLevel(level)
    for h in root.handlers[:]:
        root.removeHandler(h)
        h.close()
    root.addHandler(handler)
