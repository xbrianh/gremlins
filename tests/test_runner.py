import signal

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.runner import install_signal_handlers, run_stages


class _TrackingClient(FakeClaudeClient):
    def __init__(self):
        super().__init__()
        self.reap_calls = 0

    def reap_all(self):
        self.reap_calls += 1


def test_run_stages_executes_all_in_order():
    log = []
    stages = [
        ("a", lambda: log.append("a")),
        ("b", lambda: log.append("b")),
        ("c", lambda: log.append("c")),
    ]
    run_stages(stages)
    assert log == ["a", "b", "c"]


def test_run_stages_resume_from_skips_earlier():
    log = []
    stages = [
        ("a", lambda: log.append("a")),
        ("b", lambda: log.append("b")),
        ("c", lambda: log.append("c")),
    ]
    run_stages(stages, resume_from="b")
    assert log == ["b", "c"]


def test_run_stages_resume_from_unknown_raises():
    stages = [("a", lambda: None), ("b", lambda: None)]
    with pytest.raises(ValueError, match="unknown resume stage"):
        run_stages(stages, resume_from="z")


def test_run_stages_stops_at_first_exception():
    log = []

    def failing():
        raise RuntimeError("boom")

    stages = [
        ("a", lambda: log.append("a")),
        ("b", failing),
        ("c", lambda: log.append("c")),
    ]
    with pytest.raises(RuntimeError, match="boom"):
        run_stages(stages)
    assert log == ["a"]


def test_install_signal_handlers_calls_reap_on_sigint():
    client = _TrackingClient()
    old_sigint = signal.getsignal(signal.SIGINT)
    old_sigterm = signal.getsignal(signal.SIGTERM)
    try:
        install_signal_handlers(client)
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
        install_signal_handlers(client)
        handler = signal.getsignal(signal.SIGTERM)
        with pytest.raises(SystemExit) as exc_info:
            handler(signal.SIGTERM, None)
        assert exc_info.value.code == 130
        assert client.reap_calls == 1
    finally:
        signal.signal(signal.SIGINT, old_sigint)
        signal.signal(signal.SIGTERM, old_sigterm)
