from __future__ import annotations

import asyncio as _asyncio
import unittest.mock

import pytest

from gremlins.clients.config import (
    STREAM_IDLE_BACKOFF,
    STREAM_IDLE_TIMEOUT,
    is_transient_stream_error,
    retry,
    validate_max_retries,
)


def test_stream_idle_timeout_is_positive_number() -> None:
    assert isinstance(STREAM_IDLE_TIMEOUT, (int, float))
    assert STREAM_IDLE_TIMEOUT > 0


def test_stream_idle_backoff_shape() -> None:
    assert isinstance(STREAM_IDLE_BACKOFF, tuple)
    assert len(STREAM_IDLE_BACKOFF) > 0
    assert all(v > 0 for v in STREAM_IDLE_BACKOFF)
    assert all(
        STREAM_IDLE_BACKOFF[i] < STREAM_IDLE_BACKOFF[i + 1]
        for i in range(len(STREAM_IDLE_BACKOFF) - 1)
    )


def test_validate_max_retries_accepts_zero() -> None:
    validate_max_retries(0)


def test_validate_max_retries_accepts_max() -> None:
    validate_max_retries(len(STREAM_IDLE_BACKOFF))


def test_validate_max_retries_rejects_negative() -> None:
    with pytest.raises(ValueError, match="max_retries"):
        validate_max_retries(-1)


def test_validate_max_retries_rejects_over_schedule() -> None:
    with pytest.raises(ValueError, match="max_retries"):
        validate_max_retries(len(STREAM_IDLE_BACKOFF) + 1)


# ---------------------------------------------------------------------------
# Both backends raise the same exception on overrun
# ---------------------------------------------------------------------------


def test_claude_client_raises_on_overrun() -> None:
    from gremlins.clients.claude import SubprocessClaudeClient

    client = SubprocessClaudeClient()
    with pytest.raises(ValueError, match="max_retries"):
        client.run("x", label="t", max_retries=len(STREAM_IDLE_BACKOFF) + 1)


def test_openai_client_raises_on_overrun() -> None:
    from gremlins.clients.providers.openai_agents import OpenAIAgentsClient

    client = OpenAIAgentsClient("gpt-4o")
    with pytest.raises(ValueError, match="max_retries"):
        client.run("x", label="t", max_retries=len(STREAM_IDLE_BACKOFF) + 1)


# ---------------------------------------------------------------------------
# is_transient_stream_error classifier
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "message",
    [
        "The model is currently at capacity due to high demand. Please try again in a few minutes.",
        "rate limit exceeded",
        "Rate_Limit reached for the model",
        "Too Many Requests",
        "try again later",
        "Internal Server Error",
        "Service Unavailable",
        "Bad Gateway",
        "Gateway Timeout",
        "The server is overloaded",
        "HTTP error 529",
    ],
)
def test_is_transient_stream_error_transient(message: str) -> None:
    assert is_transient_stream_error(message)


@pytest.mark.parametrize(
    "message",
    [
        "Invalid API key provided",
        "Incorrect API key",
        "You exceeded your current quota",
        "Bad request: unknown model",
        "content_policy_violation: your request was rejected",
    ],
)
def test_is_transient_stream_error_permanent(message: str) -> None:
    assert not is_transient_stream_error(message)


# ---------------------------------------------------------------------------
# retry decorator
# ---------------------------------------------------------------------------


def test_retry_succeeds_on_first_try() -> None:
    calls = [0]

    @retry(ValueError, backoff=(1.0,))
    def fn():
        calls[0] += 1
        return "ok"

    with unittest.mock.patch("time.sleep"):
        assert fn() == "ok"
    assert calls[0] == 1


def test_retry_then_success() -> None:
    calls = [0]

    @retry(ValueError, backoff=(0.0, 0.0))
    def fn():
        calls[0] += 1
        if calls[0] < 2:
            raise ValueError("boom")
        return "ok"

    with unittest.mock.patch("time.sleep"):
        assert fn() == "ok"
    assert calls[0] == 2


def test_retry_unlisted_exception_propagates_immediately() -> None:
    calls = [0]

    @retry(ValueError, backoff=(0.0,))
    def fn():
        calls[0] += 1
        raise TypeError("wrong type")

    with pytest.raises(TypeError):
        with unittest.mock.patch("time.sleep"):
            fn()
    assert calls[0] == 1


def test_retry_classify_false_no_retry() -> None:
    calls = [0]

    @retry(ValueError, backoff=(0.0,), classify=lambda e: False)
    def fn():
        calls[0] += 1
        raise ValueError("not retryable")

    with pytest.raises(ValueError):
        with unittest.mock.patch("time.sleep"):
            fn()
    assert calls[0] == 1


def test_retry_on_retry_callback_args() -> None:
    received: list[tuple] = []

    @retry(
        ValueError, backoff=(5.0,), on_retry=lambda a, e, w: received.append((a, e, w))
    )
    def fn():
        raise ValueError("x")

    with pytest.raises(ValueError):
        with unittest.mock.patch("time.sleep"):
            fn()
    assert len(received) == 1
    attempt, exc, wait = received[0]
    assert attempt == 0
    assert isinstance(exc, ValueError)
    assert wait == 5.0


def test_retry_exhausted_propagates_last_exception() -> None:
    @retry(ValueError, backoff=(0.0, 0.0))
    def fn():
        raise ValueError("final")

    with pytest.raises(ValueError, match="final"):
        with unittest.mock.patch("time.sleep"):
            fn()


def test_retry_async_works() -> None:
    calls = [0]

    @retry(ValueError, backoff=(0.0,))
    async def fn():
        calls[0] += 1
        if calls[0] < 2:
            raise ValueError("async boom")
        return "async ok"

    result = _asyncio.run(fn())
    assert result == "async ok"
    assert calls[0] == 2


def test_retry_async_classify_false_no_retry() -> None:
    calls = [0]

    @retry(ValueError, backoff=(0.0,), classify=lambda e: False)
    async def fn():
        calls[0] += 1
        raise ValueError("not retryable")

    with pytest.raises(ValueError, match="not retryable"):
        _asyncio.run(fn())
    assert calls[0] == 1


def test_retry_async_on_retry_callback_args() -> None:
    received: list[tuple[int, BaseException, float]] = []

    @retry(
        ValueError,
        backoff=(0.0,),
        on_retry=lambda a, e, w: received.append((a, e, w)),
    )
    async def fn():
        raise ValueError("async x")

    with pytest.raises(ValueError):
        _asyncio.run(fn())
    assert len(received) == 1
    attempt, exc, wait = received[0]
    assert attempt == 0
    assert isinstance(exc, ValueError)
    assert wait == 0.0
