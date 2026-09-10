import asyncio
import json
import shutil

import pytest
from _gremlins_core.clients import RustClient as Client
from _gremlins_core.schemas import Pipeline
from conftest import MINIMAL_EVENTS, write_done_from_shell_cmd
from conftest import REVIEW_LABELS as _REVIEW_LABELS
from conftest import ReviewCreatingClient as _ReviewCreatingClient
from conftest import common_local_patches as _common_patches

from gremlins.executor.run import run_pipeline
from tests.fake_client import FakeClient


def _local_pipeline_path():
    from conftest import PIPELINE_FIXTURES_DIR

    return PIPELINE_FIXTURES_DIR / "local.yaml"


# ---------------------------------------------------------------------------
# local_main smoke test (--plan mode: skips plan, runs implement→review→address)
# ---------------------------------------------------------------------------


def test_local_main_plan_mode(tmp_path, monkeypatch):
    gremlin_id = "test-gr-id"
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))
    state_dir = tmp_path / "state" / gremlin_id
    state_dir.mkdir(parents=True, exist_ok=True)
    artifact_dir = tmp_path / "scratch" / gremlin_id / "artifacts"
    artifact_dir.mkdir(parents=True)
    # Pre-seed plan artifact so the plan stage is skipped (skip_if_exists)
    (artifact_dir / "plan.md").write_text("# Plan\nDo stuff.\n")
    (artifact_dir.parent / "registry.json").write_text(
        json.dumps({"artifact://plan.md": "file://session/plan.md"})
    )

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            **{lbl: MINIMAL_EVENTS for lbl in _REVIEW_LABELS},
            "address-code": MINIMAL_EVENTS,
        }
    )

    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(),
            argv=[],
            gremlin_id=gremlin_id,
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
    gremlin_id = "test-gr-id"
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))
    state_dir = tmp_path / "state" / gremlin_id
    state_dir.mkdir(parents=True, exist_ok=True)
    artifact_dir = tmp_path / "scratch" / gremlin_id / "artifacts"
    artifact_dir.mkdir(parents=True)
    (artifact_dir / "plan.md").write_text("# Plan\nDo stuff.\n")
    (artifact_dir.parent / "registry.json").write_text(
        json.dumps({"artifact://plan.md": "file://session/plan.md"})
    )

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr("gremlins.executor.run.in_git_repo", lambda: True)
    monkeypatch.setattr("gremlins.executor.run.has_dirty_worktree", lambda: False)
    monkeypatch.setattr("gremlins.executor.run.has_commits", lambda: False)

    with pytest.raises(SystemExit):
        asyncio.run(
            run_pipeline(
                _local_pipeline_path(),
                argv=["--resume-from", "review-code"],
                client=FakeClient(fixtures={}),
            )
        )

    assert (
        "--resume-from review-code requires implementation changes in the worktree"
        in capsys.readouterr().err
    )


def test_local_main_resume_from_review_code_allows_existing_git_changes(
    tmp_path, monkeypatch
):
    gremlin_id = "test-gr-id"
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))
    state_dir = tmp_path / "state" / gremlin_id
    state_dir.mkdir(parents=True, exist_ok=True)
    artifact_dir = tmp_path / "scratch" / gremlin_id / "artifacts"
    artifact_dir.mkdir(parents=True)
    (artifact_dir / "plan.md").write_text("# Plan\nDo stuff.\n")
    (artifact_dir.parent / "registry.json").write_text(
        json.dumps({"artifact://plan.md": "file://session/plan.md"})
    )

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
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
            _local_pipeline_path(),
            argv=["--resume-from", "review-code"],
            gremlin_id=gremlin_id,
            client=client,
        )
    )

    assert result == 0
    # verify-check and verify-test each run cmd (mocked to succeed), which
    # writes the done file and satisfies the loop's stop_when_exists: done.
    # The fix stage is never invoked because done is already bound.
    assert [call.label for call in client.calls] == [
        "review-code",
        "address-code",
    ]


def test_local_main_injected_client_model(tmp_path, monkeypatch):
    """Injected client.model flows into stage run() calls."""
    gremlin_id = "test-gr-id"
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))
    state_dir = tmp_path / "state" / gremlin_id
    state_dir.mkdir(parents=True, exist_ok=True)
    artifact_dir = tmp_path / "scratch" / gremlin_id / "artifacts"
    artifact_dir.mkdir(parents=True)
    (artifact_dir / "plan.md").write_text("# Plan\nDo stuff.\n")
    (artifact_dir.parent / "registry.json").write_text(
        json.dumps({"artifact://plan.md": str(artifact_dir / "plan.md")})
    )

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    client = _ReviewCreatingClient(
        model="gpt-4o",
        fixtures={
            "implement": MINIMAL_EVENTS,
            "review-code": MINIMAL_EVENTS,
            "address-code": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(),
            argv=[],
            gremlin_id=gremlin_id,
            client=client,
        )
    )
    assert result == 0
    assert client.calls[0].label == "implement"
    assert client.calls[0].model == "gpt-4o"
    assert client.calls[1].label == "review-code"
    assert client.calls[1].model == "gpt-4o"


def test_local_pipeline_stage_names():
    from conftest import PIPELINE_FIXTURES_DIR

    pipeline = Pipeline.from_yaml(PIPELINE_FIXTURES_DIR / "local.yaml")
    names = [s.name for s in pipeline.stages]
    assert names == [
        "plan",
        "set-description",
        "implement",
        "git-commit",
        "require-impl-progress",
        "review-code",
        "address-code",
        "normalize",
        "verify-check",
        "verify-test",
    ]


def test_local_main_writes_stage_to_state(tmp_path, monkeypatch):
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))
    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    gremlin_id = "test-gr-id"
    state_dir = tmp_path / "state" / gremlin_id
    state_dir.mkdir(parents=True, exist_ok=True)
    (state_dir / "state.json").write_text(json.dumps({"id": gremlin_id, "stage": ""}))
    artifact_dir = tmp_path / "scratch" / gremlin_id / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)
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
            _local_pipeline_path(),
            argv=[],
            client=client,
            gremlin_id=gremlin_id,
        )
    )
    assert result == 0

    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("stage") == "verify-test"


def test_local_main_env_file_vars_reach_verify(tmp_path, monkeypatch):
    """Vars from bootstrap.env are passed to exec subprocess environments."""
    import subprocess as _subprocess

    gremlin_id = "test-gr-id"
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))
    (tmp_path / "scratch" / gremlin_id / "artifacts").mkdir(parents=True)

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)

    # Create a pipeline YAML with bootstrap.env set.
    pipeline_yaml = tmp_path / ".gremlins" / "test-env.yaml"
    pipeline_yaml.parent.mkdir(exist_ok=True)
    pipeline_yaml.write_text(
        "default_client: 'xai:grok-4'\n"
        "base_ref: current\n"
        "bootstrap:\n"
        "  env: |\n"
        "    export GREMLIN_ENV_TEST_SENTINEL=from_env_file\n"
        "stages:\n"
        "  - { type: gremlins:plan, prompt: [gremlins:plan.md] }\n"
        "  - { type: gremlins:implement, prompt: [gremlins:implement_local.md] }\n"
        "  - { name: done, type: exec, options: { cmds: ['echo done'] } }\n"
    )

    captured_envs: list[dict] = []

    async def _capturing_shell(cmd, env=None, **kwargs):
        if env is not None:
            captured_envs.append(dict(env))
        write_done_from_shell_cmd(cmd)
        return _subprocess.CompletedProcess(cmd, 0, "(noop)\n", "")

    monkeypatch.setattr("_gremlins_core.utils.proc.run_shell_async", _capturing_shell)

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
            pipeline_yaml,
            argv=[],
            gremlin_id=gremlin_id,
            client=client,
        )
    )
    assert result == 0
    assert any(
        e.get("GREMLIN_ENV_TEST_SENTINEL") == "from_env_file" for e in captured_envs
    )


def test_local_main_env_file_sourced_with_overlay_dir_set(tmp_path, monkeypatch):
    """Env vars from bootstrap.env are loaded even when GREMLINS_OVERLAY_DIR is set."""
    import subprocess as _subprocess

    proj_dir = tmp_path / "proj"
    proj_dir.mkdir()
    (proj_dir / ".gremlins").mkdir()
    pipeline_yaml = proj_dir / ".gremlins" / "test-overlay.yaml"
    pipeline_yaml.write_text(
        "default_client: 'xai:grok-4'\n"
        "bootstrap:\n"
        "  env: |\n"
        "    export GREMLIN_ENV_TEST_SENTINEL=from_env_file\n"
        "stages:\n"
        "  - { name: done, type: exec, options: { cmds: ['echo done'] } }\n"
    )

    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))
    state_root = tmp_path / "state"
    state_root.mkdir()
    gremlin_id = "test-gr-id"
    state_dir = state_root / gremlin_id
    state_dir.mkdir()
    artifact_dir = tmp_path / "scratch" / gremlin_id / "artifacts"
    artifact_dir.mkdir(parents=True)

    monkeypatch.setenv("GREMLINS_OVERLAY_DIR", str(state_dir / ".gremlins"))

    monkeypatch.chdir(proj_dir)
    _common_patches(monkeypatch)

    captured_envs: list[dict] = []

    async def _capturing_shell(cmd, env=None, **kwargs):
        if env is not None:
            captured_envs.append(dict(env))
        write_done_from_shell_cmd(cmd)
        return _subprocess.CompletedProcess(cmd, 0, "(noop)\n", "")

    monkeypatch.setattr("_gremlins_core.utils.proc.run_shell_async", _capturing_shell)

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
            pipeline_yaml,
            argv=[],
            gremlin_id=gremlin_id,
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
    default_client: openai:gpt-4o produced model=gpt-4o.
    """
    gremlin_id = "test-gr-id"
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))
    state_dir = tmp_path / "state" / gremlin_id
    state_dir.mkdir(parents=True, exist_ok=True)
    artifact_dir = tmp_path / "scratch" / gremlin_id / "artifacts"
    artifact_dir.mkdir(parents=True)
    (artifact_dir / "plan.md").write_text("# Plan\nDo stuff.\n")
    (artifact_dir.parent / "registry.json").write_text(
        json.dumps({"artifact://plan.md": "file://session/plan.md"})
    )

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    # Override Pipeline.from_yaml to inject default_client: openai:gpt-4o and
    # re-fill stage clients so every stage inherits that model.
    from _gremlins_core.schemas import fill_stage_clients

    _real_from_yaml = Pipeline.from_yaml

    def _strip_clients(stage):
        stage.client = None
        for child in getattr(stage, "body", []):
            _strip_clients(child)

    def _from_yaml_copilot_default(path, **kwargs):
        pipeline = _real_from_yaml(path, **kwargs)
        new_default = Client("openai", "gpt-4o")
        for s in pipeline.stages:
            _strip_clients(s)
        fill_stage_clients(pipeline.stages, new_default)
        pipeline.default_client = new_default
        return pipeline

    monkeypatch.setattr(
        "_gremlins_core.schemas.Pipeline.from_yaml", _from_yaml_copilot_default
    )

    client = _ReviewCreatingClient(
        model="gpt-4o",
        fixtures={
            "implement": MINIMAL_EVENTS,
            "review-code": MINIMAL_EVENTS,
            "address-code": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(),
            argv=[],
            gremlin_id=gremlin_id,
            client=client,
        )
    )
    assert result == 0
    assert client.calls[0].label == "implement"
    assert client.calls[0].model == "gpt-4o"
    assert client.calls[1].label == "review-code"
    assert client.calls[1].model == "gpt-4o"


def test_plan_skip_if_exists_on_resume(tmp_path, monkeypatch):
    """Resume: plan stage is skipped when plan artifact is already verified."""
    gremlin_id = "test-gr-id"
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))
    state_dir = tmp_path / "state" / gremlin_id
    state_dir.mkdir(parents=True, exist_ok=True)
    artifact_dir = tmp_path / "scratch" / gremlin_id / "artifacts"
    artifact_dir.mkdir(parents=True)
    (artifact_dir / "plan.md").write_text("# Plan\nDo stuff.\n", encoding="utf-8")
    (artifact_dir.parent / "registry.json").write_text(
        json.dumps({"artifact://plan.md": "file://session/plan.md"})
    )

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            **{lbl: MINIMAL_EVENTS for lbl in _REVIEW_LABELS},
            "address-code": MINIMAL_EVENTS,
        }
    )

    result = asyncio.run(
        run_pipeline(
            _local_pipeline_path(),
            argv=[],
            gremlin_id=gremlin_id,
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
                _local_pipeline_path(),
                argv=[],
                client=FakeClient(fixtures={}),
            )
        )
    assert "not inside a git worktree" in capsys.readouterr().err
