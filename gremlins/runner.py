"""Stage sequencing.

``run_stages`` executes a list of ``(name, callable)`` pairs in order,
skipping stages before ``resume_from``. Stage callables raise on failure;
the runner does not catch — propagation is the orchestrator's signal that
a stage bailed.
"""

from __future__ import annotations

import logging
from collections.abc import Callable, Sequence

logger = logging.getLogger(__name__)

Stage = tuple[str, Callable[[], None]]


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
