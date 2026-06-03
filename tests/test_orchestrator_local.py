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
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir()
    # Pre-seed plan artifact so the plan stage is skipped (skip_if_exists)
    (artifact_dir / "plan.md").write_text("# Plan\nDo stuff.\n")
    (tmp_path / "registry.json").write_text(
        json.dumps({"plan-document": "file://session/plan.md"})
    )

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_artifact_dir",
        lambda gremlin_id=None: artifact_dir,
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
            argv=[],
            client=client,
        )
    )
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "plan" not in labels
    assert labels[0] == "implement"
    assert labels[1] == "review-code"
    assert labels[2] == "address-code"


def test_local_main_resume_from_review_code_requires_git_changes(
    tmp_path, monkeypatch, capsys
):
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir()
    (artifact_dir / "plan.md").write_text("# Plan\nDo stuff.\n")
    (tmp_path / "registry.json").write_text(
        json.dumps({"plan-document": "file://session/plan.md"})
    )

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_artifact_dir",
        lambda gremlin_id=None: artifact_dir,
    )
    monkeypatch.setattr("gremlins.executor.run.in_git_repo", lambda: True)
    monkeypatch.setattr("gremlins.executor.run.has_dirty_worktree", lambda: False)
    monkeypatch.setattr("gremlins.executor.run.has_commits", lambda: False)

    with pytest.raises(SystemExit):
        asyncio.run(
            run_pipeline(
                _local_pipeline_path(tmp_path),
                argv=["--resume-from", "review-code"],
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
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir()
    (artifact_dir / "plan.md").write_text("# Plan\nDo stuff.\n")
    (tmp_path / "registry.json").write_text(
        json.dumps({"plan-document": "file://session/plan.md"})
    )

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_artifact_dir",
        lambda gremlin_id=None: artifact_dir,
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
            argv=["--resume-from", "review-code"],
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
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir()
    # Pre-seed plan artifact so plan stage is skipped
    (artifact_dir / "plan.md").write_text("# Plan\nDo stuff.\n")
    (tmp_path / "registry.json").write_text(
        json.dumps({"plan-document": "file://session/plan.md"})
    )

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_artifact_dir",
        lambda gremlin_id=None: artifact_dir,
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
            argv=["--client", "copilot:gpt-4o"],
            client=client,
        )
    )
    assert result == 0
    assert client.calls[0].label == "implement"
    assert client.calls[0].model == "gpt-4o"
    assert client.calls[1].label == "review-code"
    assert client.calls[1].model == "gpt-4o"


def test_local_pipeline_stage_names(tmp_path):
    pipeline = Pipeline.from_yaml(resolve_pipeline_path("local", tmp_path))
    names = [s.name for s in pipeline.stages]
    assert names == [
        "inputs",
        "resolve-plan-input",
        "plan",
        "update-description",
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

    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir()

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_artifact_dir",
        lambda gremlin_id=None: artifact_dir,
    )
    client = _ReviewCreatingClient(
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": MINIMAL_EVENTS,
            **{lbl: MINIMAL_EVENTS for lbl in _REVIEW_LABELS},
            "address-code": MINIMAL_EVENTS,
        }
    )

    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(tmp_path),
            argv=[],
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

    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir()

    dot_gremlins = tmp_path / ".gremlins"
    dot_gremlins.mkdir()
    (dot_gremlins / "env").write_text(
        "export GREMLIN_ENV_TEST_SENTINEL=from_env_file\n"
    )

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_artifact_dir",
        lambda gremlin_id=None: artifact_dir,
    )

    captured_envs: list[dict] = []

    async def _capturing_shell(cmd, env=None, **kwargs):
        if env is not None:
            captured_envs.append(dict(env))
        return _subprocess.CompletedProcess(cmd, 0, "(noop)\n", "")

    monkeypatch.setattr("gremlins.stages.exec._proc.run_shell_async", _capturing_shell)

    client = _ReviewCreatingClient(
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": MINIMAL_EVENTS,
            **{lbl: MINIMAL_EVENTS for lbl in _REVIEW_LABELS},
            "address-code": MINIMAL_EVENTS,
        }
    )
    monkeypatch.delenv("GREMLIN_ENV_TEST_SENTINEL", raising=False)
    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(tmp_path),
            argv=[],
            client=client,
        )
    )
    assert result == 0
    assert any(
        e.get("GREMLIN_ENV_TEST_SENTINEL") == "from_env_file" for e in captured_envs
    )


def test_local_main_env_file_sourced_with_overlay_dir_set(tmp_path, monkeypatch):
    """Env vars from .gremlins/env are loaded even when GREMLINS_OVERLAY_DIR points to an unstaged path.

    Regression: stage_gremlins_overlay used paths.project_overlay_dir() as its source, which
    respects GREMLINS_OVERLAY_DIR. When the launcher pre-sets that to state_dir/.gremlins (before
    staging), the source path didn't exist yet, the copy was skipped, and the env file was silently
    ignored.
    """
    import subprocess as _subprocess

    proj_dir = tmp_path / "proj"
    proj_dir.mkdir()
    (proj_dir / ".gremlins").mkdir()
    (proj_dir / ".gremlins" / "env").write_text(
        "export GREMLIN_ENV_TEST_SENTINEL=from_env_file\n"
    )

    state_dir = tmp_path / "state"
    state_dir.mkdir()
    artifact_dir = state_dir / "session"
    artifact_dir.mkdir()

    # Simulate the launcher: GREMLINS_OVERLAY_DIR points to state_dir/.gremlins, which
    # does not yet exist when run_pipeline starts.
    monkeypatch.setenv("GREMLINS_OVERLAY_DIR", str(state_dir / ".gremlins"))

    monkeypatch.chdir(proj_dir)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_artifact_dir",
        lambda gremlin_id=None: artifact_dir,
    )

    captured_envs: list[dict] = []

    async def _capturing_shell(cmd, env=None, **kwargs):
        if env is not None:
            captured_envs.append(dict(env))
        return _subprocess.CompletedProcess(cmd, 0, "(noop)\n", "")

    monkeypatch.setattr("gremlins.stages.exec._proc.run_shell_async", _capturing_shell)

    client = _ReviewCreatingClient(
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": MINIMAL_EVENTS,
            **{lbl: MINIMAL_EVENTS for lbl in _REVIEW_LABELS},
            "address-code": MINIMAL_EVENTS,
        }
    )
    monkeypatch.delenv("GREMLIN_ENV_TEST_SENTINEL", raising=False)
    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(proj_dir),
            argv=[],
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
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir()
    # Pre-seed plan artifact so plan stage is skipped
    (artifact_dir / "plan.md").write_text("# Plan\nDo stuff.\n")
    (tmp_path / "registry.json").write_text(
        json.dumps({"plan-document": "file://session/plan.md"})
    )

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_artifact_dir",
        lambda gremlin_id=None: artifact_dir,
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
            argv=[],
            client=client,
        )
    )
    assert result == 0
    assert client.calls[0].label == "implement"
    assert client.calls[0].model == "gpt-5.4"
    assert client.calls[1].label == "review-code"
    assert client.calls[1].model == "gpt-5.4"


def test_plan_skip_if_exists_on_resume(tmp_path, monkeypatch):
    """Resume: plan stage is skipped when plan artifact is already verified."""
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir()
    (artifact_dir / "plan.md").write_text("# Plan\nDo stuff.\n", encoding="utf-8")
    (tmp_path / "registry.json").write_text(
        json.dumps({"plan-document": "file://session/plan.md"})
    )

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_artifact_dir",
        lambda gremlin_id=None: artifact_dir,
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
            argv=[],
            client=client,
        )
    )
    assert result == 0
    labels = [c.label for c in client.calls]
    assert "plan" not in labels
    assert "implement" in labels


def test_startup_fails_in_non_git_dir(tmp_path, monkeypatch, capsys):
    """gremlins exits with a clear error when cwd is not a git repository."""
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
                argv=[],
                client=FakeClaudeClient(fixtures={}),
            )
        )
    assert "not inside a git worktree" in capsys.readouterr().err


def test_claude_probe_conditional_on_provider(tmp_path, monkeypatch, capsys):
    """claude missing errors only for claude provider."""
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

    import gremlins.executor.run

    _original_run_pipeline = gremlins.executor.run.run_pipeline

    async def _test_run_pipeline(pipeline_path, *, argv, gremlin_id=None, client=None):
        # Skip pipeline execution for non-claude providers
        if client is not None and client.provider != "claude":
            return 0
        return await _original_run_pipeline(
            pipeline_path, argv=argv, gremlin_id=gremlin_id, client=client
        )

    monkeypatch.setattr("gremlins.executor.run.run_pipeline", _test_run_pipeline)

    # non-claude succeeds (shutil.which returns a path, so no error)
    result = asyncio.run(
        gremlins.executor.run.run_pipeline(
            _local_pipeline_path(tmp_path),
            argv=[],
            client=Client("copilot", "gpt-4o"),
        )
    )
    assert result == 0

    # claude errors (shutil.which returns None for "claude")
    with pytest.raises(SystemExit):
        asyncio.run(
            gremlins.executor.run.run_pipeline(
                _local_pipeline_path(tmp_path),
                argv=[],
                client=Client("claude", "sonnet"),
            )
        )
    assert "claude not found on PATH" in capsys.readouterr().err
