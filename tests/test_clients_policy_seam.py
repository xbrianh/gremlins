"""Verify the policy seam: factory → constructor → stored attributes."""

from __future__ import annotations

import gremlins.clients  # noqa: F401 — registers CLIENT_FACTORIES as a side effect
from gremlins.clients.claude import SubprocessClaudeClient
from gremlins.clients.copilot import SubprocessCopilotClient
from gremlins.clients.fake import FakeClaudeClient
from gremlins.clients.registry import CLIENT_FACTORIES
from gremlins.permissions.policy import Policy

_CLAUDE_BLOCK: dict[str, object] = {"allowedTools": ["Read"]}
_COPILOT_BLOCK: dict[str, object] = {"allowedTools": ["Write"]}


def test_subprocess_claude_stores_bypass_and_block() -> None:
    c = SubprocessClaudeClient(bypass=True, native_block=_CLAUDE_BLOCK)  # type: ignore[arg-type]
    assert c._bypass is True  # pyright: ignore[reportPrivateUsage]
    assert c._native_block == _CLAUDE_BLOCK  # pyright: ignore[reportPrivateUsage]


def test_subprocess_claude_defaults() -> None:
    c = SubprocessClaudeClient()
    assert c._bypass is False  # pyright: ignore[reportPrivateUsage]
    assert c._native_block == {}  # pyright: ignore[reportPrivateUsage]


def test_subprocess_copilot_stores_bypass_and_block() -> None:
    c = SubprocessCopilotClient(bypass=False, native_block=_COPILOT_BLOCK)  # type: ignore[arg-type]
    assert c._bypass is False  # pyright: ignore[reportPrivateUsage]
    assert c._native_block == _COPILOT_BLOCK  # pyright: ignore[reportPrivateUsage]


def test_subprocess_copilot_defaults() -> None:
    c = SubprocessCopilotClient()
    assert c._bypass is False  # pyright: ignore[reportPrivateUsage]
    assert c._native_block == {}  # pyright: ignore[reportPrivateUsage]


def test_fake_client_stores_bypass_and_block() -> None:
    c = FakeClaudeClient(bypass=True, native_block=_CLAUDE_BLOCK)  # type: ignore[arg-type]
    assert c._bypass is True  # pyright: ignore[reportPrivateUsage]
    assert c._native_block == _CLAUDE_BLOCK  # pyright: ignore[reportPrivateUsage]


def test_fake_client_defaults() -> None:
    c = FakeClaudeClient()
    assert c._bypass is False  # pyright: ignore[reportPrivateUsage]
    assert c._native_block == {}  # pyright: ignore[reportPrivateUsage]


def test_claude_factory_threads_policy() -> None:
    policy = Policy(bypass=True, blocks={"claude": _CLAUDE_BLOCK})  # type: ignore[arg-type]
    impl = CLIENT_FACTORIES["claude"]("sonnet", policy)
    assert isinstance(impl, SubprocessClaudeClient)
    assert impl._bypass is True  # pyright: ignore[reportPrivateUsage]
    assert impl._native_block == _CLAUDE_BLOCK  # pyright: ignore[reportPrivateUsage]


def test_copilot_factory_threads_policy() -> None:
    policy = Policy(bypass=False, blocks={"copilot": _COPILOT_BLOCK})  # type: ignore[arg-type]
    impl = CLIENT_FACTORIES["copilot"]("gpt-4o", policy)
    assert isinstance(impl, SubprocessCopilotClient)
    assert impl._bypass is False  # pyright: ignore[reportPrivateUsage]
    assert impl._native_block == _COPILOT_BLOCK  # pyright: ignore[reportPrivateUsage]
