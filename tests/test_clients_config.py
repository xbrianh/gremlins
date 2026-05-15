from __future__ import annotations

import pytest

from gremlins.clients.config import (
    STREAM_IDLE_BACKOFF,
    STREAM_IDLE_TIMEOUT,
    is_transient_stream_error,
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
        "529 overloaded",
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
