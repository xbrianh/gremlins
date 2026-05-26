"""Tests for the YAML-based implement stage-definition.

Covers:
- Pipeline shape: type: implement expands to implement (agent) + require-impl-progress (exec)
- The exec validator passes when commits exist since base_sha and HEAD is a fast-forward
- The exec validator raises Bail when no commits since base_sha (empty impl)
- The exec validator raises Bail when HEAD diverges from base_sha
- Resume regression: running the validator with prior commits passes
"""

from __future__ import annotations

import asyncio
import pathlib
import subprocess
from typing import Any

import pytest

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.schemes import EnvResolver
from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import StateData, build_state
from gremlins.pipeline import Pipeline
from gremlins.pipeline.discovery import resolve_pipeline_path
from gremlins.stages.exec import Exec
from gremlins.stages.outcome import Bail


def _make_state(project: pathlib.Path, base_sha: str):
    session_dir = project / "session"
    session_dir.mkdir(exist_ok=True)
    registry = ArtifactRegistry(
        session_dir, cwd=project, resolvers={"env": EnvResolver({"cwd": str(project)})}
    )
    registry.bind("env", Uri.parse("env://"))
    registry.bind("base_sha", Uri.parse(f"git://commit/{base_sha}"))
    return build_state(
        data=StateData(),
        client=FakeClaudeClient(),
        session_dir=session_dir,
        artifacts=registry,
        worktree=project,
    )


def _base_sha(project: pathlib.Path) -> str:
    return subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=project,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


def _make_commit(project: pathlib.Path, filename: str, content: str, message: str):
    (project / filename).write_text(content)
    subprocess.run(
        ["git", "add", filename], cwd=project, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", message], cwd=project, check=True, capture_output=True
    )


def _require_impl_progress_exec() -> Exec:
    cmds = [
        'git merge-base --is-ancestor "$base_sha" HEAD || { echo "implement diverged from $base_sha; expected fast-forward" >&2; exit 1; }',
        'test "$(git rev-list --count "$base_sha"..HEAD)" -gt 0 || { echo "implement produced no commits since $base_sha" >&2; exit 1; }',
    ]
    return Exec(
        "require-impl-progress",
        {"cmds": cmds},
        in_map={"base_sha": "base_sha"},
    )


# ---------------------------------------------------------------------------
# Pipeline shape
# ---------------------------------------------------------------------------


def test_gh_pipeline_implement_expands_to_two_stages(tmp_path: pathlib.Path) -> None:
    """type: implement in gh.yaml expands to implement (agent) + require-impl-progress (exec)."""
    pipeline = Pipeline.from_yaml(resolve_pipeline_path("gh", tmp_path))
    names = [s.name for s in pipeline.stages]
    impl_idx = names.index("implement")
    assert names[impl_idx] == "implement"
    assert names[impl_idx + 1] == "require-impl-progress"


# ---------------------------------------------------------------------------
# Exec validator happy path
# ---------------------------------------------------------------------------


def test_validator_passes_when_commits_exist(sandbox: Any) -> None:
    """Validator passes when HEAD is a fast-forward with commits since base_sha."""
    base_sha = _base_sha(sandbox.project)
    _make_commit(sandbox.project, "impl.txt", "impl\n", "feat: implement something")

    state = _make_state(sandbox.project, base_sha)
    stage = _require_impl_progress_exec()
    result = asyncio.run(stage.run(state))
    from gremlins.stages.outcome import Done

    assert isinstance(result, Done)


# ---------------------------------------------------------------------------
# Exec validator: empty impl
# ---------------------------------------------------------------------------


def test_validator_raises_bail_when_no_commits(sandbox: Any) -> None:
    """Validator raises Bail when no commits since base_sha."""
    base_sha = _base_sha(sandbox.project)
    # No new commits — HEAD == base_sha.
    state = _make_state(sandbox.project, base_sha)
    stage = _require_impl_progress_exec()
    with pytest.raises(Bail, match="exec require-impl-progress: exited 1"):
        asyncio.run(stage.run(state))


# ---------------------------------------------------------------------------
# Exec validator: divergent HEAD
# ---------------------------------------------------------------------------


def test_validator_raises_bail_when_head_diverges(sandbox: Any) -> None:
    """Validator raises Bail when HEAD is not a descendant of base_sha."""
    base_sha = _base_sha(sandbox.project)

    # Create an orphan branch — its history diverges from base_sha.
    subprocess.run(
        ["git", "checkout", "--orphan", "orphan"],
        cwd=sandbox.project,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "rm", "-rf", "."], cwd=sandbox.project, check=True, capture_output=True
    )
    _make_commit(sandbox.project, "orphan.txt", "orphan\n", "orphan commit")

    state = _make_state(sandbox.project, base_sha)
    stage = _require_impl_progress_exec()
    with pytest.raises(Bail, match="exec require-impl-progress: exited 1"):
        asyncio.run(stage.run(state))


# ---------------------------------------------------------------------------
# Resume regression: validator with prior commits passes
# ---------------------------------------------------------------------------


def test_validator_passes_on_resume_with_prior_commits(sandbox: Any) -> None:
    """Resume at require-impl-progress with existing impl commits passes."""
    base_sha = _base_sha(sandbox.project)
    # Simulate implement having already run: one commit above base.
    _make_commit(sandbox.project, "impl.txt", "impl\n", "feat: implement something")

    # State uses the original base_sha (before the impl commit).
    state = _make_state(sandbox.project, base_sha)
    stage = _require_impl_progress_exec()
    result = asyncio.run(stage.run(state))
    from gremlins.stages.outcome import Done

    assert isinstance(result, Done)
