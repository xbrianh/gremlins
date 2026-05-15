from __future__ import annotations

STREAM_IDLE_TIMEOUT = 120
STREAM_IDLE_BACKOFF = (60, 300, 600)


def validate_max_retries(max_retries: int) -> None:
    if max_retries < 0:
        raise ValueError(f"max_retries must be >= 0, got {max_retries}")
    if max_retries > len(STREAM_IDLE_BACKOFF):
        raise ValueError(
            f"max_retries={max_retries} exceeds backoff schedule length {len(STREAM_IDLE_BACKOFF)}"
        )
