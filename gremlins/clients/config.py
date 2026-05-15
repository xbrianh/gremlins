from __future__ import annotations

import asyncio
import functools
import inspect
import time
from collections.abc import Callable
from typing import Any, ParamSpec, TypeVar

P = ParamSpec("P")
R = TypeVar("R")

STREAM_IDLE_TIMEOUT = 120
STREAM_IDLE_BACKOFF = (60, 300, 600)

# Substrings that identify transient provider errors (capacity, rate-limit, 5xx).
# Permanent errors (auth, bad request, content policy) won't match any of these.
_TRANSIENT_SUBSTRINGS = (
    "capacity",
    "rate limit",
    "rate_limit",
    "too many requests",
    "try again",
    "please retry",
    "server error",
    "service unavailable",
    "bad gateway",
    "gateway timeout",
    "overloaded",
    "timed out in queue",
    " 529",
)


def is_transient_stream_error(message: str) -> bool:
    lower = message.lower()
    return any(s in lower for s in _TRANSIENT_SUBSTRINGS)


def validate_max_retries(max_retries: int) -> None:
    if max_retries < 0:
        raise ValueError(f"max_retries must be >= 0, got {max_retries}")
    if max_retries > len(STREAM_IDLE_BACKOFF):
        raise ValueError(
            f"max_retries={max_retries} exceeds backoff schedule length {len(STREAM_IDLE_BACKOFF)}"
        )


def retry(
    *exc_types: type[BaseException],
    backoff: tuple[float, ...],
    on_retry: Callable[[int, BaseException, float], None] | None = None,
    classify: Callable[[BaseException], bool] | None = None,
) -> Callable[[Callable[P, R]], Callable[P, R]]:
    def decorator(fn: Callable[P, R]) -> Callable[P, R]:
        def _guard(attempt: int, exc: BaseException) -> float:
            if attempt == len(backoff) or (classify is not None and not classify(exc)):
                raise exc
            wait = backoff[attempt]
            if on_retry is not None:
                on_retry(attempt, exc, wait)
            return wait

        if inspect.iscoroutinefunction(fn):

            @functools.wraps(fn)
            async def _async(*args: Any, **kwargs: Any) -> Any:
                for attempt in range(len(backoff) + 1):
                    try:
                        return await fn(*args, **kwargs)  # type: ignore[misc]
                    except exc_types as exc:
                        await asyncio.sleep(_guard(attempt, exc))
                raise AssertionError("unreachable")

            return _async  # type: ignore[return-value]

        @functools.wraps(fn)
        def _sync(*args: P.args, **kwargs: P.kwargs) -> R:
            for attempt in range(len(backoff) + 1):
                try:
                    return fn(*args, **kwargs)
                except exc_types as exc:
                    time.sleep(_guard(attempt, exc))
            raise AssertionError("unreachable")

        return _sync

    return decorator
