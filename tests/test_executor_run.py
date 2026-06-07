import os
import signal
from unittest.mock import patch

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.run import _HANDLED_SIGS, _install_signal_handlers


class _TrackingClient(FakeClaudeClient):
    def __init__(self):
        super().__init__()
        self.reap_calls = 0

    def reap_all(self):
        self.reap_calls += 1


@pytest.fixture(autouse=True)
def _restore_signals():
    old = {s: signal.getsignal(s) for s in _HANDLED_SIGS}
    yield
    for s, h in old.items():
        signal.signal(s, h)


@pytest.mark.parametrize("sig", _HANDLED_SIGS)
def test_signal_handler_reaps_and_redelivers(sig):
    client = _TrackingClient()
    with patch("gremlins.executor.run.atexit.register"):
        # Create a mock gremlin with None state for testing
        gremlin = type('MockGremlin', (), {'state': None})()
        _install_signal_handlers([client], gremlin)
    handler = signal.getsignal(sig)

    killed: list[tuple[int, int]] = []
    with patch.object(os, "kill", side_effect=lambda pid, s: killed.append((pid, s))):
        handler(sig, None)

    assert client.reap_calls == 1
    assert killed == [(os.getpid(), sig)]
    # handler should have reset to SIG_DFL so the next delivery is default
    assert signal.getsignal(sig) is signal.SIG_DFL


def test_atexit_log_logs_when_stage_set(caplog):
    registered: list = []
    with patch("gremlins.executor.run.atexit.register", side_effect=registered.append):
        gremlin = type('MockGremlin', (), {'state': None})()
        _install_signal_handlers([], gremlin)

    assert len(registered) == 1
    atexit_fn = registered[0]

    with patch(
        "gremlins.executor.run._load_stage_attempt",
        return_value=("my-stage", "attempt-1"),
    ):
        with caplog.at_level("WARNING"):
            atexit_fn()

    assert "exiting via atexit" in caplog.text
    assert "my-stage" in caplog.text
    assert "attempt-1" in caplog.text


def test_atexit_log_silent_on_clean_exit(caplog):
    registered: list = []
    with patch("gremlins.executor.run.atexit.register", side_effect=registered.append):
        gremlin = type('MockGremlin', (), {'state': None})()
        _install_signal_handlers([], gremlin)

    atexit_fn = registered[0]

    with patch("gremlins.executor.run._load_stage_attempt", return_value=("", "")):
        with caplog.at_level("WARNING"):
            atexit_fn()

    assert "exiting via atexit" not in caplog.text
