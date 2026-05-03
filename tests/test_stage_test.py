"""Tests for gremlins.stages.test."""

import json
import logging
from typing import Any

import pytest
from conftest import MINIMAL_EVENTS

import gremlins.orchestrators.boss as boss_mod
from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages.context import StageContext
from gremlins.stages.test import TestOptions, run as run_test


def _make_ctx(client: Any, tmp_path: Any) -> StageContext:
    return StageContext(client=client, session_dir=tmp_path, gr_id=None)


def _run(
    tmp_path: Any,
    *,
    test_cmd: Any,
    max_attempts: int = 3,
    client: Any = None,
    is_git: bool = False,
    code_style: str = "Be good.",
    test_fix_model: str = "sonnet",
) -> Any:
    if client is None:
        client = FakeClaudeClient(fixtures={})
    ctx = _make_ctx(client, tmp_path)
    run_test(
        ctx,
        TestOptions(
            test_cmd=test_cmd,
            max_attempts=max_attempts,
            test_fix_model=test_fix_model,
            is_git=is_git,
            cwd=tmp_path,
            code_style=code_style,
        ),
    )
    return client


def test_noop_when_no_test_cmd(tmp_path, caplog):
    client = FakeClaudeClient(fixtures={})
    ctx = _make_ctx(client, tmp_path)
    with caplog.at_level(logging.INFO):
        run_test(
            ctx,
            TestOptions(
                test_cmd=None,
                max_attempts=3,
                test_fix_model="sonnet",
                is_git=False,
                cwd=tmp_path,
                code_style="Be good.",
            ),
        )
    assert len(client.calls) == 0
    assert "skipped" in caplog.text
    assert "no --test" in caplog.text


def test_green_on_first_try(tmp_path):
    client = _run(tmp_path, test_cmd="true")
    assert len(client.calls) == 0
    assert (tmp_path / "test-attempt-1.log").exists()


def test_fix_then_green(tmp_path):
    flag = tmp_path / "flag.txt"
    flag.write_text("fail\n")
    test_cmd = f"grep -q '^pass$' {flag}"

    class _FixingClient(FakeClaudeClient):
        def run(self, prompt, *, label, **kwargs):
            flag.write_text("pass\n")
            return super().run(prompt, label=label, **kwargs)

    client = _FixingClient(fixtures={"test-fix-1": MINIMAL_EVENTS})
    ctx = _make_ctx(client, tmp_path)
    run_test(
        ctx,
        TestOptions(
            test_cmd=test_cmd,
            max_attempts=3,
            test_fix_model="haiku",
            is_git=False,
            cwd=tmp_path,
            code_style="Be good.",
        ),
    )

    assert len(client.calls) == 1
    assert client.calls[0].label == "test-fix-1"
    assert client.calls[0].model == "haiku"
    assert (tmp_path / "test-attempt-1.log").exists()
    assert (tmp_path / "test-attempt-2.log").exists()
    assert (tmp_path / "stream-test-1.jsonl").exists()


def test_attempts_exhausted_bails(tmp_path, monkeypatch):
    monkeypatch.delenv("GR_ID", raising=False)

    client = FakeClaudeClient(
        fixtures={
            "test-fix-1": MINIMAL_EVENTS,
            "test-fix-2": MINIMAL_EVENTS,
        }
    )
    ctx = _make_ctx(client, tmp_path)

    with pytest.raises(RuntimeError, match="exhausted 3 attempts"):
        run_test(
            ctx,
            TestOptions(
                test_cmd="false",
                max_attempts=3,
                test_fix_model="sonnet",
                is_git=False,
                cwd=tmp_path,
                code_style="Be good.",
            ),
        )

    assert len(client.calls) == 2
    assert (tmp_path / "test-attempt-1.log").exists()
    assert (tmp_path / "test-attempt-2.log").exists()
    assert (tmp_path / "test-attempt-3.log").exists()


def test_attempts_exhausted_with_max_1(tmp_path):
    client = FakeClaudeClient(fixtures={})
    ctx = _make_ctx(client, tmp_path)

    with pytest.raises(RuntimeError, match="exhausted 1 attempts"):
        run_test(
            ctx,
            TestOptions(
                test_cmd="false",
                max_attempts=1,
                test_fix_model="sonnet",
                is_git=False,
                cwd=tmp_path,
                code_style="Be good.",
            ),
        )

    assert len(client.calls) == 0
    assert (tmp_path / "test-attempt-1.log").exists()


def test_code_style_in_fix_prompt(tmp_path):
    flag = tmp_path / "flag.txt"
    flag.write_text("fail\n")
    test_cmd = f"grep -q '^pass$' {flag}"

    class _FixingClient(FakeClaudeClient):
        def run(self, prompt, *, label, **kwargs):
            flag.write_text("pass\n")
            return super().run(prompt, label=label, **kwargs)

    client = _FixingClient(fixtures={"test-fix-1": MINIMAL_EVENTS})
    ctx = _make_ctx(client, tmp_path)
    run_test(
        ctx,
        TestOptions(
            test_cmd=test_cmd,
            max_attempts=3,
            test_fix_model="sonnet",
            is_git=False,
            cwd=tmp_path,
            code_style="My custom style rules.",
        ),
    )

    assert len(client.calls) == 1
    assert "My custom style rules." in client.calls[0].prompt


def test_log_file_captures_output(tmp_path):
    client = FakeClaudeClient(fixtures={})
    ctx = _make_ctx(client, tmp_path)

    run_test(
        ctx,
        TestOptions(
            test_cmd="echo hello",
            max_attempts=3,
            test_fix_model="sonnet",
            is_git=False,
            cwd=tmp_path,
            code_style="",
        ),
    )

    log = tmp_path / "test-attempt-1.log"
    assert log.exists()
    assert "hello" in log.read_text()


def test_launch_child_forwards_test_flags(tmp_path, monkeypatch):
    gr_id = "test-boss-testcmd-aa1122"
    state_dir = tmp_path / gr_id
    state_dir.mkdir()
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gr_id,
                "project_root": str(tmp_path / "repo"),
                "current_head": "abc123def456abc1",
            }
        )
    )
    (state_dir / "boss_state.json").write_text(
        json.dumps(
            {
                "spec_path": "/some/spec.md",
                "chain_kind": "local",
                "chain_base_ref": "abc123def456abc1",
                "target_branch": "main",
                "current_plan": "/some/spec.md",
                "handoff_count": 0,
                "current_child_id": None,
                "children": [],
                "handoff_records": [],
                "operator_followups": [],
                "test_cmd": "pytest -x",
                "test_max_attempts": 5,
                "test_fix_model": "opus",
            }
        )
    )

    captured_pipeline_args = []

    def fake_launch(
        kind,
        *,
        plan=None,
        parent_id=None,
        project_root=None,
        base_ref="HEAD",
        pipeline_args=(),
        **kw,
    ):
        captured_pipeline_args.extend(pipeline_args)
        return "child-testcmd-bb2233"

    monkeypatch.setattr(boss_mod, "STATE_ROOT", str(tmp_path))
    monkeypatch.setattr(boss_mod, "_launch", fake_launch)

    result = boss_mod.launch_child(gr_id, "localgremlin", "/tmp/child-plan.md")

    assert result == "child-testcmd-bb2233"
    assert "--test" in captured_pipeline_args
    assert "pytest -x" in captured_pipeline_args
    assert "--test-max-attempts" in captured_pipeline_args
    assert "5" in captured_pipeline_args
    assert "-t" in captured_pipeline_args
    assert "opus" in captured_pipeline_args


def test_launch_child_no_test_flags_when_absent(tmp_path, monkeypatch):
    gr_id = "test-boss-notest-cc3344"
    state_dir = tmp_path / gr_id
    state_dir.mkdir()
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gr_id,
                "project_root": str(tmp_path / "repo"),
                "current_head": "abc123def456abc1",
            }
        )
    )
    (state_dir / "boss_state.json").write_text(
        json.dumps(
            {
                "spec_path": "/some/spec.md",
                "chain_kind": "local",
                "chain_base_ref": "abc123def456abc1",
                "target_branch": "main",
                "current_plan": "/some/spec.md",
                "handoff_count": 0,
                "current_child_id": None,
                "children": [],
                "handoff_records": [],
                "operator_followups": [],
                "test_cmd": "",
                "test_max_attempts": 3,
                "test_fix_model": "",
            }
        )
    )

    captured_pipeline_args = []

    def fake_launch(
        kind,
        *,
        plan=None,
        parent_id=None,
        project_root=None,
        base_ref="HEAD",
        pipeline_args=(),
        **kw,
    ):
        captured_pipeline_args.extend(pipeline_args)
        return "child-notest-dd4455"

    monkeypatch.setattr(boss_mod, "STATE_ROOT", str(tmp_path))
    monkeypatch.setattr(boss_mod, "_launch", fake_launch)

    boss_mod.launch_child(gr_id, "localgremlin", "/tmp/child-plan.md")

    assert "--test" not in captured_pipeline_args
    assert "--test-max-attempts" not in captured_pipeline_args
    assert captured_pipeline_args == []
