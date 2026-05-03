"""Tests for gremlins.logging_setup."""

import logging
import sys

from gremlins.logging_setup import configure_logging


def test_configure_logging_idempotent():
    configure_logging()
    n = len(logging.getLogger().handlers)
    configure_logging()
    assert len(logging.getLogger().handlers) == n


def test_configure_logging_respects_env(monkeypatch):
    monkeypatch.setenv("GREMLINS_LOG_LEVEL", "DEBUG")
    configure_logging()
    assert logging.getLogger().level == logging.DEBUG


def test_configure_logging_default_level():
    configure_logging()
    assert logging.getLogger().level == logging.INFO


def test_configure_logging_writes_to_stdout():
    configure_logging()
    root = logging.getLogger()
    assert any(
        isinstance(h, logging.StreamHandler) and h.stream is sys.stdout
        for h in root.handlers
    )


def test_configure_logging_utc_format(capsys):
    configure_logging()
    logging.getLogger("test.utc").info("probe")
    out = capsys.readouterr().out
    # UTC timestamp format: YYYY-MM-DDTHH:MM:SSZ
    import re
    assert re.search(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", out), (
        f"expected UTC timestamp in output, got: {out!r}"
    )
