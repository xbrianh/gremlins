from __future__ import annotations

import asyncio

import pytest

from gremlins.utils.decorators import default_on_exception, swallow

# --- swallow ---


def test_swallow_sync_returns_none_on_match():
    @swallow(ValueError)
    def boom():
        raise ValueError("oops")

    assert boom() is None


def test_swallow_sync_passes_through_on_no_exception():
    @swallow(ValueError)
    def fine():
        return 42

    assert fine() == 42


def test_swallow_sync_propagates_non_matching():
    @swallow(ValueError)
    def boom():
        raise TypeError("not a value error")

    with pytest.raises(TypeError):
        boom()


def test_swallow_async_returns_none_on_match():
    @swallow(ValueError)
    async def boom():
        raise ValueError("oops")

    assert asyncio.run(boom()) is None


def test_swallow_async_passes_through_on_no_exception():
    @swallow(ValueError)
    async def fine():
        return 99

    assert asyncio.run(fine()) == 99


def test_swallow_async_propagates_non_matching():
    @swallow(ValueError)
    async def boom():
        raise TypeError("nope")

    with pytest.raises(TypeError):
        asyncio.run(boom())


def test_swallow_preserves_metadata():
    @swallow(ValueError)
    def my_func():
        """my doc"""

    assert my_func.__name__ == "my_func"
    assert my_func.__doc__ == "my doc"


def test_swallow_empty_raises_at_decoration():
    with pytest.raises(ValueError):
        swallow()


# --- default_on_exception ---


def test_default_sync_returns_default_on_match():
    @default_on_exception("fallback")
    def boom():
        raise RuntimeError("bad")

    assert boom() == "fallback"


def test_default_sync_passes_through_on_no_exception():
    @default_on_exception("fallback")
    def fine():
        return "ok"

    assert fine() == "ok"


def test_default_sync_propagates_when_exc_type_specified_and_non_matching():
    @default_on_exception("fallback", ValueError)
    def boom():
        raise TypeError("not value")

    with pytest.raises(TypeError):
        boom()


def test_default_async_returns_default_on_match():
    @default_on_exception(0.0)
    async def boom():
        raise RuntimeError("bad")

    assert asyncio.run(boom()) == 0.0


def test_default_async_passes_through_on_no_exception():
    @default_on_exception(0.0)
    async def fine():
        return 1.5

    assert asyncio.run(fine()) == 1.5


def test_default_async_propagates_non_matching():
    @default_on_exception(0.0, ValueError)
    async def boom():
        raise TypeError("nope")

    with pytest.raises(TypeError):
        asyncio.run(boom())


def test_default_preserves_metadata():
    @default_on_exception(None)
    def my_func():
        """my doc"""

    assert my_func.__name__ == "my_func"
    assert my_func.__doc__ == "my doc"
