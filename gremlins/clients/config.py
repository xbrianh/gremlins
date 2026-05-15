from __future__ import annotations

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
    "server error",
    "service unavailable",
    "bad gateway",
    "gateway timeout",
    "overloaded",
    "529",
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
