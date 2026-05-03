"""Tests for gremlins.stages.verify."""

from __future__ import annotations

import json
from typing import Any

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages.context import StageContext
from gremlins.stages.verify import VerifyOptions
from gremlins.stages.verify import run as run_verify


def _make_ctx(client: Any, tmp_path: Any, *, gr_id: Any = None) -> StageContext:
    return StageContext(client=client, session_dir=tmp_path, gr_id=gr_id)


def _run(
    tmp_path: Any,
    *,
    check_cmd: str = "true",
    test_cmd: str = "true",
    max_attempts: int = 3,
    client: Any = None,
    code_style: str = "Be good.",
    fix_model: str = "sonnet",
) -> Any:
    if client is None:
        client = FakeClaudeClient(fixtures={})
    ctx = _make_ctx(client, tmp_path)
    run_verify(
        ctx,
        VerifyOptions(
            fix_model=fix_model,
            cwd=tmp_path,
            code_style=code_style,
            check_cmd=check_cmd,
            test_cmd=test_cmd,
            max_attempts=max_attempts,
        ),
    )
    return client


def test_green_on_first_attempt(tmp_path):
    client = _run(tmp_path, check_cmd="true", test_cmd="true")
    assert len(client.calls) == 0
    assert (tmp_path / "verify-attempt-1.log").exists()


def test_fix_then_green(tmp_path):
    flag = tmp_path / "flag.txt"
    flag.write_text("fail\n")
    check_cmd = f"grep -q '^pass$' {flag}"

    class _FixingClient(FakeClaudeClient):
        def run(self, prompt, *, label, **kwargs):
            flag.write_text("pass\n")
            return super().run(prompt, label=label, **kwargs)

    client = _FixingClient(fixtures={"verify-fix-1": MINIMAL_EVENTS})
    ctx = _make_ctx(client, tmp_path)
    run_verify(
        ctx,
        VerifyOptions(
            fix_model="haiku",
            cwd=tmp_path,
            code_style="Be good.",
            check_cmd=check_cmd,
            test_cmd="true",
            max_attempts=3,
        ),
    )

    assert len(client.calls) == 1
    assert client.calls[0].label == "verify-fix-1"
    assert client.calls[0].model == "haiku"
    assert (tmp_path / "verify-attempt-1.log").exists()
    assert (tmp_path / "verify-attempt-2.log").exists()
    assert (tmp_path / "stream-verify-1.jsonl").exists()


def test_attempts_exhausted_raises(tmp_path, monkeypatch):
    monkeypatch.delenv("GR_ID", raising=False)

    client = FakeClaudeClient(
        fixtures={
            "verify-fix-1": MINIMAL_EVENTS,
            "verify-fix-2": MINIMAL_EVENTS,
        }
    )
    ctx = _make_ctx(client, tmp_path)

    with pytest.raises(RuntimeError, match="exhausted 3 attempts"):
        run_verify(
            ctx,
            VerifyOptions(
                fix_model="sonnet",
                cwd=tmp_path,
                code_style="Be good.",
                check_cmd="false",
                test_cmd="true",
                max_attempts=3,
            ),
        )

    assert len(client.calls) == 2
    assert (tmp_path / "verify-attempt-1.log").exists()
    assert (tmp_path / "verify-attempt-2.log").exists()
    assert (tmp_path / "verify-attempt-3.log").exists()


def test_exhaustion_with_max_1(tmp_path):
    client = FakeClaudeClient(fixtures={})
    ctx = _make_ctx(client, tmp_path)

    with pytest.raises(RuntimeError, match="exhausted 1 attempts"):
        run_verify(
            ctx,
            VerifyOptions(
                fix_model="sonnet",
                cwd=tmp_path,
                code_style="Be good.",
                check_cmd="false",
                test_cmd="true",
                max_attempts=1,
            ),
        )

    assert len(client.calls) == 0
    assert (tmp_path / "verify-attempt-1.log").exists()


def test_code_style_in_fix_prompt(tmp_path):
    flag = tmp_path / "flag.txt"
    flag.write_text("fail\n")
    check_cmd = f"grep -q '^pass$' {flag}"

    class _FixingClient(FakeClaudeClient):
        def run(self, prompt, *, label, **kwargs):
            flag.write_text("pass\n")
            return super().run(prompt, label=label, **kwargs)

    client = _FixingClient(fixtures={"verify-fix-1": MINIMAL_EVENTS})
    ctx = _make_ctx(client, tmp_path)
    run_verify(
        ctx,
        VerifyOptions(
            fix_model="sonnet",
            cwd=tmp_path,
            code_style="My custom style rules.",
            check_cmd=check_cmd,
            test_cmd="true",
            max_attempts=3,
        ),
    )

    assert len(client.calls) == 1
    assert "My custom style rules." in client.calls[0].prompt


def test_both_cmds_in_fix_prompt(tmp_path, monkeypatch):
    monkeypatch.delenv("GR_ID", raising=False)

    client = FakeClaudeClient(
        fixtures={
            "verify-fix-1": MINIMAL_EVENTS,
            "verify-fix-2": MINIMAL_EVENTS,
        }
    )
    ctx = _make_ctx(client, tmp_path)

    with pytest.raises(RuntimeError):
        run_verify(
            ctx,
            VerifyOptions(
                fix_model="sonnet",
                cwd=tmp_path,
                code_style="",
                check_cmd="false",
                test_cmd="make test",
                max_attempts=3,
            ),
        )

    # Both command strings must appear in the fix prompt
    assert "false" in client.calls[0].prompt
    assert "make test" in client.calls[0].prompt


def test_log_file_captures_output(tmp_path):
    client = FakeClaudeClient(fixtures={})
    ctx = _make_ctx(client, tmp_path)

    run_verify(
        ctx,
        VerifyOptions(
            fix_model="sonnet",
            cwd=tmp_path,
            code_style="",
            check_cmd="echo hello_check",
            test_cmd="echo hello_test",
            max_attempts=3,
        ),
    )

    log = tmp_path / "verify-attempt-1.log"
    assert log.exists()
    content = log.read_text()
    assert "hello_check" in content or "hello_test" in content


def test_no_pr_opened_on_exhaustion(tmp_path, monkeypatch):
    """Stage raises before commit-pr could open a PR."""
    monkeypatch.delenv("GR_ID", raising=False)

    client = FakeClaudeClient(
        fixtures={
            "verify-fix-1": MINIMAL_EVENTS,
            "verify-fix-2": MINIMAL_EVENTS,
        }
    )
    ctx = _make_ctx(client, tmp_path)

    with pytest.raises(RuntimeError):
        run_verify(
            ctx,
            VerifyOptions(
                fix_model="sonnet",
                cwd=tmp_path,
                code_style="",
                check_cmd="false",
                test_cmd="true",
                max_attempts=3,
            ),
        )

    # All fix attempts were consumed but still exhausted — no silent pass-through
    assert len(client.calls) == 2


def test_exhaustion_emits_bail_to_state(tmp_path, make_state_dir):
    gr_id = "test-gr-id"
    state_dir = make_state_dir(gr_id)
    client = FakeClaudeClient(
        fixtures={"verify-fix-1": MINIMAL_EVENTS, "verify-fix-2": MINIMAL_EVENTS}
    )
    ctx = _make_ctx(client, tmp_path, gr_id=gr_id)
    with pytest.raises(RuntimeError, match="exhausted"):
        run_verify(
            ctx,
            VerifyOptions(
                fix_model="sonnet",
                cwd=tmp_path,
                code_style="",
                check_cmd="false",
                test_cmd="true",
                max_attempts=3,
            ),
        )
    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("bail_class") == "other"
