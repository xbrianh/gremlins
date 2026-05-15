from __future__ import annotations

import copy
import functools
import inspect
from collections.abc import Callable
from typing import Any, ParamSpec, TypeVar

P = ParamSpec("P")
R = TypeVar("R")


def swallow(
    *exc_types: type[BaseException],
) -> Callable[[Callable[P, R]], Callable[P, R | None]]:
    if not exc_types:
        raise ValueError("swallow() requires at least one exception type")

    def decorator(fn: Callable[P, R]) -> Callable[P, R | None]:
        if inspect.iscoroutinefunction(fn):

            @functools.wraps(fn)
            async def _async(*args: Any, **kwargs: Any) -> Any:
                try:
                    return await fn(*args, **kwargs)  # type: ignore[misc]
                except exc_types:
                    return None

            return _async  # type: ignore[return-value]

        @functools.wraps(fn)
        def _sync(*args: P.args, **kwargs: P.kwargs) -> R | None:
            try:
                return fn(*args, **kwargs)
            except exc_types:
                return None

        return _sync

    return decorator


def default_on_exception(
    default: R, *exc_types: type[BaseException]
) -> Callable[[Callable[P, R]], Callable[P, R]]:
    resolved = exc_types or (Exception,)

    def decorator(fn: Callable[P, R]) -> Callable[P, R]:
        if inspect.iscoroutinefunction(fn):

            @functools.wraps(fn)
            async def _async(*args: Any, **kwargs: Any) -> Any:
                try:
                    return await fn(*args, **kwargs)  # type: ignore[misc]
                except resolved:
                    return copy.copy(default)

            return _async  # type: ignore[return-value]

        @functools.wraps(fn)
        def _sync(*args: P.args, **kwargs: P.kwargs) -> R:
            try:
                return fn(*args, **kwargs)
            except resolved:
                return copy.copy(default)

        return _sync

    return decorator
