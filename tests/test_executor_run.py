import os
import signal
from unittest.mock import patch

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.run import _install_signal_handlers


class _TrackingClient(FakeClaudeClient):
    def __init__(self):
        super().__init__()
        self.reap_calls = 0

    def reap_all(self):
        self.reap_calls += 1


_HANDLED_SIGS = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT)


@pytest.fixture(autouse=True)
def _restore_signals():
    old = {s: signal.getsignal(s) for s in _HANDLED_SIGS}
    yield
    for s, h in old.items():
        signal.signal(s, h)


@pytest.mark.parametrize("sig", [signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT])
def test_signal_handler_reaps_and_redelivers(sig):
    client = _TrackingClient()
    _install_signal_handlers([client], gremlin_id=None)
    handler = signal.getsignal(sig)

    killed: list[tuple[int, int]] = []
    with patch.object(os, "kill", side_effect=lambda pid, s: killed.append((pid, s))):
        handler(sig, None)

    assert client.reap_calls == 1
    assert killed == [(os.getpid(), sig)]
    # handler should have reset to SIG_DFL so the next delivery is default
    assert signal.getsignal(sig) is signal.SIG_DFL
