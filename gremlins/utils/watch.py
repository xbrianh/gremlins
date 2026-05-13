from __future__ import annotations

import signal
import sys
import time
import types
from collections.abc import Callable


def watch_render(interval: int, render: Callable[[], object]) -> int:
    interval = max(1, interval)
    stop = [False]

    def _handle_sigint(_signum: int, _frame: types.FrameType | None) -> None:
        stop[0] = True

    old_handler = signal.signal(signal.SIGINT, _handle_sigint)

    try:
        while not stop[0]:
            sys.stdout.write("\033[2J\033[H")
            sys.stdout.flush()
            render()
            for _ in range(interval * 10):
                if stop[0]:
                    break
                time.sleep(0.1)
    finally:
        signal.signal(signal.SIGINT, old_handler)
    return 0
