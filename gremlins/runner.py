"""Generic stage runner: signal handler installation and stage sequencing.

``run_stages`` executes a list of ``(name, callable)`` pairs in order,
skipping stages before ``resume_from``. Stage callables raise on failure;
the runner does not catch — propagation is the orchestrator's signal that
a stage bailed.

``install_signal_handlers`` wires SIGINT/SIGTERM to ``client.reap_all()``
followed by ``sys.exit(130)`` so a Ctrl-C'd run doesn't leave orphaned
``claude -p`` processes burning tokens (the parity contract for the bash
``trap 'kill -- -$$'`` shape).
"""

from __future__ import annotations

import logging
import signal
import sys
import types
from collections.abc import Callable, Sequence

from gremlins.clients.protocol import _ClientImpl

logger = logging.getLogger(__name__)

Stage = tuple[str, Callable[[], None]]


def install_signal_handlers(*clients: _ClientImpl) -> None:
    """Register SIGINT/SIGTERM handlers that reap claude children before
    exit. Pass the live ClaudeClient(s) (real or fake) — their ``reap_all`` is
    what gets called."""

    def handler(signum: int, frame: types.FrameType | None) -> None:
        for c in clients:
            try:
                c.reap_all()
            except Exception:
                pass
        sys.exit(130)

    signal.signal(signal.SIGINT, handler)
    signal.signal(signal.SIGTERM, handler)


def run_stages(stages: Sequence[Stage], *, resume_from: str | None = None) -> None:
    """Run stages in order. If ``resume_from`` names one of the stages, all
    stages strictly before it are skipped. Stops at the first exception
    (which the caller is expected to let propagate or handle)."""
    names = [name for name, _ in stages]
    start_idx = 0
    if resume_from is not None:
        if resume_from not in names:
            raise ValueError(f"unknown resume stage {resume_from!r}; valid: {names}")
        start_idx = names.index(resume_from)
    for _name, fn in list(stages)[start_idx:]:
        fn()
