"""Tests for gremlins.stages.test."""

import json

import pytest
from conftest import MINIMAL_EVENTS

import gremlins.orchestrators.boss as boss_mod
from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages.test import run_test_stage

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _run(
    tmp_path,
    *,
    test_cmd,
    max_attempts=3,
    client=None,
    is_git=False,
    code_style="Be good.",
    test_fix_model="sonnet",
):
    if client is None:
        client = FakeClaudeClient(fixtures={})
    run_test_stage(
        client=client,
        session_dir=tmp_path,
        test_cmd=test_cmd,
        max_attempts=max_attempts,
        test_fix_model=test_fix_model,
        is_git=is_git,
        cwd=tmp_path,
        code_style=code_style,
    )
    return client


# ---------------------------------------------------------------------------
# No-op when test_cmd is None
# ---------------------------------------------------------------------------


def test_noop_when_no_test_cmd(tmp_path, capsys):
    client = FakeClaudeClient(fixtures={})
    run_test_stage(
        client=client,
        session_dir=tmp_path,
        test_cmd=None,
        max_attempts=3,
        test_fix_model="sonnet",
        is_git=False,
        cwd=tmp_path,
        code_style="Be good.",
    )
    assert len(client.calls) == 0
    out = capsys.readouterr().out
    assert "skipped" in out
    assert "no --test" in out


# ---------------------------------------------------------------------------
# Green on first try
# ---------------------------------------------------------------------------


def test_green_on_first_try(tmp_path):
    client = _run(tmp_path, test_cmd="true")
    assert len(client.calls) == 0
    assert (tmp_path / "test-attempt-1.log").exists()


# ---------------------------------------------------------------------------
# Fix then green
# ---------------------------------------------------------------------------


def test_fix_then_green(tmp_path):
    flag = tmp_path / "flag.txt"
    flag.write_text("fail\n")
    test_cmd = f"grep -q '^pass$' {flag}"

    class _FixingClient(FakeClaudeClient):
        def run(self, prompt, *, label, **kwargs):
            flag.write_text("pass\n")
            return super().run(prompt, label=label, **kwargs)

    client = _FixingClient(fixtures={"test-fix-1": MINIMAL_EVENTS})
    run_test_stage(
        client=client,
        session_dir=tmp_path,
        test_cmd=test_cmd,
        max_attempts=3,
        test_fix_model="haiku",
        is_git=False,
        cwd=tmp_path,
        code_style="Be good.",
    )

    assert len(client.calls) == 1
    assert client.calls[0].label == "test-fix-1"
    assert client.calls[0].model == "haiku"
    assert (tmp_path / "test-attempt-1.log").exists()
    assert (tmp_path / "test-attempt-2.log").exists()
    assert (tmp_path / "stream-test-1.jsonl").exists()


# ---------------------------------------------------------------------------
# Attempts exhausted → RuntimeError + bail
# ---------------------------------------------------------------------------


def test_attempts_exhausted_bails(tmp_path, monkeypatch):
    monkeypatch.delenv("GR_ID", raising=False)

    client = FakeClaudeClient(
        fixtures={
            "test-fix-1": MINIMAL_EVENTS,
            "test-fix-2": MINIMAL_EVENTS,
        }
    )

    with pytest.raises(RuntimeError, match="exhausted 3 attempts"):
        run_test_stage(
            client=client,
            session_dir=tmp_path,
            test_cmd="false",
            max_attempts=3,
            test_fix_model="sonnet",
            is_git=False,
            cwd=tmp_path,
            code_style="Be good.",
        )

    # 3 attempts: fix after 1, fix after 2, bail after 3 (no fix on last attempt)
    assert len(client.calls) == 2
    assert (tmp_path / "test-attempt-1.log").exists()
    assert (tmp_path / "test-attempt-2.log").exists()
    assert (tmp_path / "test-attempt-3.log").exists()


def test_attempts_exhausted_with_max_1(tmp_path):
    client = FakeClaudeClient(fixtures={})

    with pytest.raises(RuntimeError, match="exhausted 1 attempts"):
        run_test_stage(
            client=client,
            session_dir=tmp_path,
            test_cmd="false",
            max_attempts=1,
            test_fix_model="sonnet",
            is_git=False,
            cwd=tmp_path,
            code_style="Be good.",
        )

    assert len(client.calls) == 0  # no fix on last (only) attempt
    assert (tmp_path / "test-attempt-1.log").exists()


# ---------------------------------------------------------------------------
# code_style appears verbatim in fix prompt
# ---------------------------------------------------------------------------


def test_code_style_in_fix_prompt(tmp_path):
    flag = tmp_path / "flag.txt"
    flag.write_text("fail\n")
    test_cmd = f"grep -q '^pass$' {flag}"

    class _FixingClient(FakeClaudeClient):
        def run(self, prompt, *, label, **kwargs):
            flag.write_text("pass\n")
            return super().run(prompt, label=label, **kwargs)

    client = _FixingClient(fixtures={"test-fix-1": MINIMAL_EVENTS})
    run_test_stage(
        client=client,
        session_dir=tmp_path,
        test_cmd=test_cmd,
        max_attempts=3,
        test_fix_model="sonnet",
        is_git=False,
        cwd=tmp_path,
        code_style="My custom style rules.",
    )

    assert len(client.calls) == 1
    assert "My custom style rules." in client.calls[0].prompt


# ---------------------------------------------------------------------------
# Log file content
# ---------------------------------------------------------------------------


def test_log_file_captures_output(tmp_path):
    client = FakeClaudeClient(fixtures={})

    # "true" produces no output; check that log is created and can be empty
    run_test_stage(
        client=client,
        session_dir=tmp_path,
        test_cmd="echo hello",
        max_attempts=3,
        test_fix_model="sonnet",
        is_git=False,
        cwd=tmp_path,
        code_style="",
    )

    log = tmp_path / "test-attempt-1.log"
    assert log.exists()
    assert "hello" in log.read_text()


# ---------------------------------------------------------------------------
# boss: launch_child forwards --test flags via pipeline_args
# ---------------------------------------------------------------------------


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
    """When boss_state has no test_cmd, launch_child omits test flags."""
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
