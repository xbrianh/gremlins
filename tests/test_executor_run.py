import signal

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.run import _install_signal_handlers


class _TrackingClient(FakeClaudeClient):
    def __init__(self):
        super().__init__()
        self.reap_calls = 0

    def reap_all(self):
        self.reap_calls += 1


def test_install_signal_handlers_calls_reap_on_sigint():
    client = _TrackingClient()
    old_sigint = signal.getsignal(signal.SIGINT)
    old_sigterm = signal.getsignal(signal.SIGTERM)
    try:
        _install_signal_handlers([client])
        handler = signal.getsignal(signal.SIGINT)
        with pytest.raises(SystemExit) as exc_info:
            handler(signal.SIGINT, None)
        assert exc_info.value.code == 130
        assert client.reap_calls == 1
    finally:
        signal.signal(signal.SIGINT, old_sigint)
        signal.signal(signal.SIGTERM, old_sigterm)


def test_install_signal_handlers_calls_reap_on_sigterm():
    client = _TrackingClient()
    old_sigint = signal.getsignal(signal.SIGINT)
    old_sigterm = signal.getsignal(signal.SIGTERM)
    try:
        _install_signal_handlers([client])
        handler = signal.getsignal(signal.SIGTERM)
        with pytest.raises(SystemExit) as exc_info:
            handler(signal.SIGTERM, None)
        assert exc_info.value.code == 130
        assert client.reap_calls == 1
    finally:
        signal.signal(signal.SIGINT, old_sigint)
        signal.signal(signal.SIGTERM, old_sigterm)
