"""Orchestrator entry point for the boss pipeline.

Thin wrapper: parses boss-specific args, strips legacy flags, then delegates
to local_main with ``--pipeline boss``.

GR_ID env var is set by the launcher.
"""

from __future__ import annotations

import logging

from gremlins.logging_setup import configure_logging

logger = logging.getLogger(__name__)


def _strip_arg(argv: list[str], flag: str) -> list[str]:
    """Remove ``flag <value>`` or ``flag=<value>`` pairs from argv."""
    filtered: list[str] = []
    i = 0
    while i < len(argv):
        arg = argv[i]
        if arg == flag:
            i += 2 if i + 1 < len(argv) else 1
            continue
        if arg.startswith(f"{flag}="):
            i += 1
            continue
        filtered.append(arg)
        i += 1
    return filtered


def boss_main(argv: list[str], *, gr_id: str | None = None) -> int:
    configure_logging()
    # --chain-kind is no longer needed (child kind is configured in boss.yaml);
    # strip it so local_main's arg parser doesn't trip on an unknown flag.
    filtered = _strip_arg(argv, "--chain-kind")
    from gremlins.orchestrators.local import local_main

    return local_main(["--pipeline", "boss"] + filtered, gr_id=gr_id)
