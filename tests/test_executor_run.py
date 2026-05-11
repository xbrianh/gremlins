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


@pytest.fixture(autouse=True)
def _restore_signals():
    old = {s: signal.getsignal(s) for s in (signal.SIGINT, signal.SIGTERM)}
    yield
    for s, h in old.items():
        signal.signal(s, h)


def test_install_signal_handlers_calls_reap_on_sigint():
    client = _TrackingClient()
    _install_signal_handlers([client])
    handler = signal.getsignal(signal.SIGINT)
    with pytest.raises(SystemExit) as exc_info:
        handler(signal.SIGINT, None)
    assert exc_info.value.code == 130
    assert client.reap_calls == 1


def test_install_signal_handlers_calls_reap_on_sigterm():
    client = _TrackingClient()
    _install_signal_handlers([client])
    handler = signal.getsignal(signal.SIGTERM)
    with pytest.raises(SystemExit) as exc_info:
        handler(signal.SIGTERM, None)
    assert exc_info.value.code == 130
    assert client.reap_calls == 1
