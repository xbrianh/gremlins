"""Tests for gremlins.logging_setup."""

import logging

import pytest

from gremlins.logging_setup import configure_logging


@pytest.fixture(autouse=True)
def _restore_root_logger():
    root = logging.getLogger()
    orig_level = root.level
    orig_handlers = root.handlers[:]
    yield
    root.setLevel(orig_level)
    root.handlers[:] = orig_handlers


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
