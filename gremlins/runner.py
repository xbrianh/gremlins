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

import concurrent.futures
import logging
import signal
import sys
import types
from collections.abc import Callable, Sequence

from .clients.protocol import ClaudeClient

logger = logging.getLogger(__name__)

Stage = tuple[str, Callable[[], None]]


def install_signal_handlers(client: ClaudeClient) -> None:
    """Register SIGINT/SIGTERM handlers that reap claude children before
    exit. Pass the live ClaudeClient (real or fake) — its ``reap_all`` is
    what gets called."""

    def handler(signum: int, frame: types.FrameType | None) -> None:
        try:
            client.reap_all()
        finally:
            sys.exit(130)

    signal.signal(signal.SIGINT, handler)
    signal.signal(signal.SIGTERM, handler)


def make_parallel_wrapper(
    children: list[tuple[str, Callable[[], None]]],
    *,
    max_concurrent: int | None,
    resume_from: str | None,
    set_stage_fn: Callable[[], None],
) -> Callable[[], None]:
    """Return a callable that runs children concurrently in a thread pool.

    ``resume_from`` may be a child name; if so, children before it are skipped.
    All children run to completion before any exception is re-raised.
    """

    def _wrapper() -> None:
        set_stage_fn()
        child_names = [n for n, _ in children]
        active = children
        if resume_from is not None and resume_from in child_names:
            active = children[child_names.index(resume_from) :]
        if not active:
            return
        workers = max_concurrent if max_concurrent is not None else len(active)
        with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
            futs = [pool.submit(fn) for _, fn in active]
        errors = [e for fut in futs if (e := fut.exception()) is not None]
        if errors:
            for extra in errors[1:]:
                logger.error("parallel child also failed: %s", extra)
            raise errors[0]

    return _wrapper


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
