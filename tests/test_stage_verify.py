"""Tests for gremlins.stages.verify.Verify."""

from __future__ import annotations

import json
import pathlib
from typing import Any

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.pipeline import StageEntry
from gremlins.stages.context import StageContext
from gremlins.stages.verify import Verify

_VERIFY_PROMPT = (
    pathlib.Path(__file__).resolve().parent.parent
    / "gremlins"
    / "prompts"
    / "verify_fix.md"
)


def _make_entry(cmds: list[str] | None = None, max_attempts: int = 3) -> StageEntry:
    return StageEntry(
        name="verify",
        type="verify",
        client=None,
        prompt_paths=[_VERIFY_PROMPT],
        options={
            "cmds": cmds if cmds is not None else [],
            "max_attempts": max_attempts,
        },
    )


def _make_stage(
    tmp_path: Any,
    *,
    cmds: list[str] | None = None,
    max_attempts: int = 3,
    client: Any = None,
    fix_model: str = "sonnet",
    is_git: bool = True,
    commit_after_fix: bool = False,
) -> tuple[Verify, Any]:
    if cmds is None:
        cmds = ["true"]
    if client is None:
        client = FakeClaudeClient(fixtures={})
    entry = _make_entry(cmds=cmds, max_attempts=max_attempts)
    stage = Verify(
        entry,
        fix_model,
        is_git=is_git,
        commit_after_fix=commit_after_fix,
    )
    ctx = StageContext(
        client=client, session_dir=tmp_path, gr_id=None, worktree=tmp_path
    )
    stage.bind(ctx)
    return stage, client


def test_green_on_first_attempt(tmp_path):
    stage, client = _make_stage(tmp_path, cmds=["true"])
    stage.run(None)
    assert len(client.calls) == 0
    assert (tmp_path / "verify-attempt-1.log").exists()


def test_no_op_when_cmds_empty(tmp_path):
    """Empty cmds list -> stage skips without invoking the shell or agent."""
    stage, client = _make_stage(tmp_path, cmds=[])
    stage.run(None)
    assert len(client.calls) == 0
    assert not (tmp_path / "verify-attempt-1.log").exists()


def test_single_cmd(tmp_path):
    """A single cmd in the list runs without shell-syntax error."""
    stage, client = _make_stage(tmp_path, cmds=["true"])
    stage.run(None)
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
    stage, _ = _make_stage(
        tmp_path,
        cmds=[check_cmd, "true"],
        max_attempts=3,
        client=client,
        fix_model="haiku",
    )
    stage.run(None)

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
    stage, _ = _make_stage(
        tmp_path, cmds=["false", "true"], max_attempts=3, client=client
    )

    with pytest.raises(RuntimeError, match="exhausted 3 attempts"):
        stage.run(None)

    assert len(client.calls) == 2
    assert (tmp_path / "verify-attempt-1.log").exists()
    assert (tmp_path / "verify-attempt-2.log").exists()
    assert (tmp_path / "verify-attempt-3.log").exists()


def test_exhaustion_with_max_1(tmp_path):
    stage, client = _make_stage(tmp_path, cmds=["false"], max_attempts=1)

    with pytest.raises(RuntimeError, match="exhausted 1 attempts"):
        stage.run(None)

    assert len(client.calls) == 0
    assert (tmp_path / "verify-attempt-1.log").exists()


def test_both_cmds_in_fix_prompt(tmp_path, monkeypatch):
    monkeypatch.delenv("GR_ID", raising=False)

    client = FakeClaudeClient(
        fixtures={
            "verify-fix-1": MINIMAL_EVENTS,
            "verify-fix-2": MINIMAL_EVENTS,
        }
    )
    stage, _ = _make_stage(
        tmp_path,
        cmds=["false", "make test"],
        max_attempts=3,
        client=client,
    )

    with pytest.raises(RuntimeError):
        stage.run(None)

    assert "false" in client.calls[0].prompt
    assert "make test" in client.calls[0].prompt


def test_log_file_captures_output(tmp_path):
    stage, client = _make_stage(tmp_path, cmds=["echo hello_check", "echo hello_test"])
    stage.run(None)

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
    stage, _ = _make_stage(tmp_path, cmds=["false"], max_attempts=3, client=client)

    with pytest.raises(RuntimeError):
        stage.run(None)

    assert len(client.calls) == 2


def test_exhaustion_emits_bail_to_state(tmp_path, make_state_dir):
    gr_id = "test-gr-id"
    state_dir = make_state_dir(gr_id)
    client = FakeClaudeClient(
        fixtures={"verify-fix-1": MINIMAL_EVENTS, "verify-fix-2": MINIMAL_EVENTS}
    )
    entry = _make_entry(cmds=["false"], max_attempts=3)
    stage = Verify(entry, "sonnet", is_git=True, commit_after_fix=False)
    ctx = StageContext(
        client=client, session_dir=tmp_path, gr_id=gr_id, worktree=tmp_path
    )
    stage.bind(ctx)

    with pytest.raises(RuntimeError, match="exhausted"):
        stage.run(None)

    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("bail_class") == "other"


def test_is_git_false_skips_diff(tmp_path):
    flag = tmp_path / "flag.txt"
    flag.write_text("fail\n")
    check_cmd = f"grep -q '^pass$' {flag}"

    captured_prompts = []

    class _FixingClient(FakeClaudeClient):
        def run(self, prompt, *, label, **kwargs):
            captured_prompts.append(prompt)
            flag.write_text("pass\n")
            return super().run(prompt, label=label, **kwargs)

    client = _FixingClient(fixtures={"verify-fix-1": MINIMAL_EVENTS})
    stage, _ = _make_stage(tmp_path, cmds=[check_cmd], client=client, is_git=False)
    stage.run(None)

    assert len(client.calls) == 1
    assert "```\n\n```" in captured_prompts[0]


def test_commit_after_fix_true_in_prompt(tmp_path):
    flag = tmp_path / "flag.txt"
    flag.write_text("fail\n")
    check_cmd = f"grep -q '^pass$' {flag}"

    class _FixingClient(FakeClaudeClient):
        def run(self, prompt, *, label, **kwargs):
            flag.write_text("pass\n")
            return super().run(prompt, label=label, **kwargs)

    client = _FixingClient(fixtures={"verify-fix-1": MINIMAL_EVENTS})
    stage, _ = _make_stage(
        tmp_path, cmds=[check_cmd], client=client, commit_after_fix=True
    )
    stage.run(None)

    assert len(client.calls) == 1
    assert "Fix failing checks" in client.calls[0].prompt
    assert "stage the changed files" in client.calls[0].prompt


def test_parallel_child_fix_prompt_uses_child_key_bail_command(tmp_path):
    client = FakeClaudeClient(fixtures={"verify-fix-1": MINIMAL_EVENTS})
    entry = StageEntry(
        name="verify",
        type="verify",
        client=None,
        prompt_paths=[
            _VERIFY_PROMPT,
            pathlib.Path(__file__).resolve().parent.parent
            / "gremlins"
            / "prompts"
            / "bail_section_fix.md",
        ],
        options={"cmds": ["false"], "max_attempts": 2},
    )
    stage = Verify(entry, "sonnet", is_git=True, commit_after_fix=False)
    stage.bind(
        StageContext(
            client=client,
            session_dir=tmp_path,
            gr_id="gr-verify",
            child_key="verify-child",
            worktree=tmp_path,
        )
    )

    with pytest.raises(RuntimeError, match="exhausted 2 attempts"):
        stage.run(None)

    assert "python -m gremlins.bail --child-key verify-child" in client.calls[0].prompt
