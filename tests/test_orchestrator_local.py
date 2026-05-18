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
    assert labels[1] == "review-code:sonnet"
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
            "review-code:sonnet": MINIMAL_EVENTS,
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
        "review-code:sonnet",
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
    review_label = "review-code:gpt-4o"
    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            review_label: MINIMAL_EVENTS,
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
    assert client.calls[1].label == review_label


def test_local_pipeline_stage_names(tmp_path):
    pipeline = Pipeline.from_yaml(resolve_pipeline_path("local", tmp_path))
    names = [s.name for s in pipeline.stages]
    assert names == ["plan", "implement", "review-code", "address-code", "verify"]


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
    """Vars from .gremlins/env are visible to verify subprocesses."""
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    dot_gremlins = tmp_path / ".gremlins"
    dot_gremlins.mkdir()
    env_file = dot_gremlins / "env"
    env_file.write_text("export GREMLIN_ENV_TEST_SENTINEL=from_env_file\n")

    verify_cmd = 'test "$GREMLIN_ENV_TEST_SENTINEL" = from_env_file'

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

    monkeypatch.delenv("GREMLIN_ENV_TEST_SENTINEL", raising=False)
    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(tmp_path),
            argv=["--plan", str(plan_file), "--cmd", verify_cmd],
            client=client,
        )
    )
    assert result == 0


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

    review_label = "review-code:gpt-5.4"
    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            review_label: MINIMAL_EVENTS,
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
    assert client.calls[1].label == review_label
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

    from gremlins.stages import address_code as _ac_mod
    from gremlins.stages import implement as _impl_mod
    from gremlins.stages import review_code as _rc_mod
    from gremlins.stages import verify as _v_mod

    async def _noop(self, state):  # noqa: ARG001
        pass

    monkeypatch.setattr(_impl_mod.Implement, "run", _noop)
    monkeypatch.setattr(_rc_mod.ReviewCode, "run", _noop)
    monkeypatch.setattr(_ac_mod.AddressCode, "run", _noop)
    monkeypatch.setattr(_v_mod.Verify, "run", _noop)

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
        "gremlins.executor.run._install_signal_handlers", lambda c: None
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
