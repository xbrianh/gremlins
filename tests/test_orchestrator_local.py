import asyncio
import dataclasses
import json
import shutil

import pytest
from conftest import MINIMAL_EVENTS
from conftest import REVIEW_LABELS as _REVIEW_LABELS
from conftest import ReviewCreatingClient as _ReviewCreatingClient
from conftest import common_local_patches as _common_patches

from gremlins.clients.client import Client
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.run import run_pipeline
from gremlins.pipeline import Pipeline
from gremlins.pipeline.discovery import resolve_pipeline_path


def _local_pipeline_path(cwd):
    return resolve_pipeline_path("local", cwd)


# ---------------------------------------------------------------------------
# local_main smoke test (--plan mode: skips plan, runs implement→review→address)
# ---------------------------------------------------------------------------


def test_local_main_plan_mode(tmp_path, monkeypatch):
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_session_dir",
        lambda gremlin_id=None: session_dir,
    )
    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            **{lbl: MINIMAL_EVENTS for lbl in _REVIEW_LABELS},
            "address-code": MINIMAL_EVENTS,
        }
    )

    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(tmp_path),
            argv=["--plan", str(plan_file)],
            client=client,
        )
    )
    assert result == 0

    labels = [c.label for c in client.calls]
    assert labels[0] == "implement"
    assert labels[1] == "review-code"
    assert labels[2] == "address-code"


def test_local_main_resume_from_review_code_requires_git_changes(
    tmp_path, monkeypatch, capsys
):
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_session_dir",
        lambda gremlin_id=None: session_dir,
    )
    monkeypatch.setattr("gremlins.executor.run.in_git_repo", lambda: True)
    monkeypatch.setattr("gremlins.executor.run.has_dirty_worktree", lambda: False)
    monkeypatch.setattr("gremlins.executor.run.has_commits", lambda: False)

    with pytest.raises(SystemExit):
        asyncio.run(
            run_pipeline(
                _local_pipeline_path(tmp_path),
                argv=["--plan", str(plan_file), "--resume-from", "review-code"],
                client=FakeClaudeClient(fixtures={}),
            )
        )

    assert (
        "--resume-from review-code requires implementation changes in the worktree"
        in capsys.readouterr().err
    )


def test_local_main_resume_from_review_code_allows_existing_git_changes(
    tmp_path, monkeypatch
):
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_session_dir",
        lambda gremlin_id=None: session_dir,
    )
    monkeypatch.setattr("gremlins.executor.run.in_git_repo", lambda: True)
    monkeypatch.setattr("gremlins.executor.run.has_dirty_worktree", lambda: False)
    monkeypatch.setattr("gremlins.executor.run.has_commits", lambda: True)

    client = _ReviewCreatingClient(
        fixtures={
            "review-code": MINIMAL_EVENTS,
            "address-code": MINIMAL_EVENTS,
        }
    )

    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(tmp_path),
            argv=["--plan", str(plan_file), "--resume-from", "review-code"],
            client=client,
        )
    )

    assert result == 0
    assert [call.label for call in client.calls] == [
        "review-code",
        "address-code",
    ]


def test_local_main_client_specifier_model(tmp_path, monkeypatch):
    """Model from --client provider:model flows into stage run() calls."""
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_session_dir",
        lambda gremlin_id=None: session_dir,
    )
    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            "review-code": MINIMAL_EVENTS,
            "address-code": MINIMAL_EVENTS,
        }
    )

    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(tmp_path),
            argv=["--plan", str(plan_file), "--client", "copilot:gpt-4o"],
            client=client,
        )
    )
    assert result == 0
    assert client.calls[0].model == "gpt-4o"  # implement stage
    assert client.calls[1].label == "review-code"


def test_local_pipeline_stage_names(tmp_path):
    pipeline = Pipeline.from_yaml(resolve_pipeline_path("local", tmp_path))
    names = [s.name for s in pipeline.stages]
    assert names == [
        "inputs",
        "plan",
        "implement",
        "require-impl-progress",
        "review-code",
        "address-code",
        "normalize",
        "verify",
    ]


def test_local_main_writes_stage_to_state(tmp_path, monkeypatch, make_state_dir):
    gremlin_id = "test-gr-id"
    state_dir = make_state_dir(gremlin_id)

    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_session_dir",
        lambda gremlin_id=None: session_dir,
    )
    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            **{lbl: MINIMAL_EVENTS for lbl in _REVIEW_LABELS},
            "address-code": MINIMAL_EVENTS,
        }
    )

    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(tmp_path),
            argv=["--plan", str(plan_file)],
            client=client,
            gremlin_id=gremlin_id,
        )
    )
    assert result == 0

    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("stage") == "verify"


def test_local_main_env_file_vars_reach_verify(tmp_path, monkeypatch):
    """Vars from .gremlins/env are passed to exec subprocess environments."""
    import subprocess as _subprocess

    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    dot_gremlins = tmp_path / ".gremlins"
    dot_gremlins.mkdir()
    (dot_gremlins / "env").write_text(
        "export GREMLIN_ENV_TEST_SENTINEL=from_env_file\n"
    )

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_session_dir",
        lambda gremlin_id=None: session_dir,
    )

    captured_envs: list[dict] = []

    async def _capturing_shell(cmd, env=None, **kwargs):
        if env is not None:
            captured_envs.append(dict(env))
        return _subprocess.CompletedProcess(cmd, 0, "(noop)\n", "")

    monkeypatch.setattr("gremlins.stages.exec._proc.run_shell_async", _capturing_shell)

    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            **{lbl: MINIMAL_EVENTS for lbl in _REVIEW_LABELS},
            "address-code": MINIMAL_EVENTS,
        }
    )
    monkeypatch.delenv("GREMLIN_ENV_TEST_SENTINEL", raising=False)
    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(tmp_path),
            argv=["--plan", str(plan_file)],
            client=client,
        )
    )
    assert result == 0
    assert any(
        e.get("GREMLIN_ENV_TEST_SENTINEL") == "from_env_file" for e in captured_envs
    )


def test_local_main_pipeline_default_client_model(tmp_path, monkeypatch):
    """pipeline.default_client_spec model used when --model and --client are absent.

    Regression: the model was computed before pipeline loading, so the pipeline's
    default_client_spec model was never consulted. A pipeline with
    default_client: copilot:gpt-5.4 produced model=sonnet.
    """
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_session_dir",
        lambda gremlin_id=None: session_dir,
    )
    # Override Pipeline.from_yaml to inject default_client: copilot:gpt-5.4 and
    # re-fill stage clients so every stage inherits that model.
    from gremlins.pipeline import _fill_stage_clients

    _real_from_yaml = Pipeline.from_yaml

    def _strip_clients(stage):
        stage.client = None
        for child in getattr(stage, "body", []):
            _strip_clients(child)

    def _from_yaml_copilot_default(path):
        pipeline = _real_from_yaml(path)
        new_default = Client("copilot", "gpt-5.4")
        for s in pipeline.stages:
            _strip_clients(s)
        _fill_stage_clients(pipeline.stages, new_default)
        return dataclasses.replace(pipeline, default_client=new_default)

    monkeypatch.setattr(
        "gremlins.pipeline.Pipeline.from_yaml", _from_yaml_copilot_default
    )

    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            "review-code": MINIMAL_EVENTS,
            "address-code": MINIMAL_EVENTS,
        }
    )

    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(tmp_path),
            argv=["--plan", str(plan_file)],
            client=client,
        )
    )
    assert result == 0
    assert client.calls[0].model == "gpt-5.4"  # implement
    assert client.calls[1].label == "review-code"
    assert client.calls[1].model == "gpt-5.4"  # review


# ---------------------------------------------------------------------------
# stage_inputs wiring: local pipeline reads instructions from state.json
# ---------------------------------------------------------------------------


def test_local_stage_inputs_instructions_reach_plan(
    tmp_path, monkeypatch, make_state_dir
):
    """stage_inputs["instructions"] from state.json is passed to plan.Plan, and
    takes precedence over the CLI positional argument."""
    gremlin_id = "test-si-local"
    state_dir = make_state_dir(gremlin_id)

    sf = state_dir / "state.json"
    state = json.loads(sf.read_text())
    state["stage_inputs"] = {"instructions": "instr from state"}
    sf.write_text(json.dumps(state))

    session_dir = tmp_path / "session"
    session_dir.mkdir()
    # Pre-create plan.md so the implement stage can read it after the (no-op) plan stage.
    (session_dir / "plan.md").write_text("# Plan\nDo stuff.\n")

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_session_dir",
        lambda gremlin_id=None: session_dir,
    )
    received: list[str] = []

    from gremlins.stages import plan as _plan_mod

    async def _capturing_plan_run(self, state):
        received.append(state.instructions)

    monkeypatch.setattr(_plan_mod.Plan, "run", _capturing_plan_run)

    from gremlins.stages import agent as _agent_mod
    from gremlins.stages import exec as _exec_mod
    from gremlins.stages import loop as _loop_mod

    async def _noop(self, state):  # noqa: ARG001
        pass

    monkeypatch.setattr(_agent_mod.Agent, "run", _noop)
    monkeypatch.setattr(_exec_mod.Exec, "run", _noop)
    monkeypatch.setattr(_loop_mod.LoopStage, "run", _noop)

    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(tmp_path),
            argv=["instr from cli"],
            client=FakeClaudeClient(fixtures={}),
            gremlin_id=gremlin_id,
        )
    )

    assert result == 0
    assert received == ["instr from state"]


def test_startup_fails_in_non_git_dir(tmp_path, monkeypatch, capsys):
    """gremlins exits with a clear error when cwd is not a git repository."""
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(
        shutil, "which", lambda n: f"/fake/{n}" if n in ("claude", "git") else None
    )
    monkeypatch.setattr(
        "gremlins.executor.run._install_signal_handlers", lambda c, gid: None
    )
    monkeypatch.setattr("gremlins.executor.run.in_git_repo", lambda: False)
    with pytest.raises(SystemExit):
        asyncio.run(
            run_pipeline(
                _local_pipeline_path(tmp_path),
                argv=["--plan", str(plan_file)],
                client=FakeClaudeClient(fixtures={}),
            )
        )
    assert "not inside a git worktree" in capsys.readouterr().err


def test_claude_probe_conditional_on_provider(tmp_path, monkeypatch, capsys):
    """claude missing errors only for claude provider."""
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(
        shutil,
        "which",
        lambda n: None if n == "claude" else f"/fake/{n}" if n == "git" else None,
    )
    monkeypatch.setattr(
        "gremlins.executor.run._install_signal_handlers", lambda c, gid: None
    )
    monkeypatch.setattr("gremlins.executor.run.in_git_repo", lambda: True)
    monkeypatch.setenv("GREMLINS_TEST_NOOP_PIPELINE", "1")
    # non-claude succeeds
    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(tmp_path),
            argv=["--plan", str(plan_file)],
            client=Client("copilot", "gpt-4o"),
        )
    )
    assert result == 0
    # claude errors
    with pytest.raises(SystemExit):
        asyncio.run(
            run_pipeline(
                _local_pipeline_path(tmp_path),
                argv=["--plan", str(plan_file)],
                client=Client("claude", "sonnet"),
            )
        )
    assert "claude not found on PATH" in capsys.readouterr().err
