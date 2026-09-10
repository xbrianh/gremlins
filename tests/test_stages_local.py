import asyncio
import pathlib
import subprocess
from typing import TYPE_CHECKING, cast

from _gremlins_core.artifacts import ArtifactRegistry
from _gremlins_core.schemas import Pipeline
from _gremlins_core.stages import Agent
from conftest import MINIMAL_EVENTS, MockGremlin, ReviewCreatingClient

from gremlins.executor.state import StateData, build_state
from gremlins.utils.yaml_io import load_bundled_prompt

if TYPE_CHECKING:
    from gremlins.executor.gremlin import Gremlin


def test_local_yaml_loads_and_validates():
    from conftest import PIPELINE_FIXTURES_DIR

    pipeline = Pipeline.from_yaml(PIPELINE_FIXTURES_DIR / "local.yaml")
    assert len(pipeline.stages) == 10
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


def _make_state(client, artifact_dir, *, gremlin_id=None, base_ref_sha=""):
    registry = ArtifactRegistry(artifact_dir)
    if base_ref_sha:
        registry._set("base_sha", f"git://commit/{base_ref_sha}")
    state = build_state(
        data=StateData(gremlin_id=gremlin_id),
        client=client,
        artifact_dir=artifact_dir,
        artifacts=registry,
    )
    return state


def _init_git_repo(path: pathlib.Path) -> None:
    """Create a git repo with a first commit so HEAD exists and the tree is clean."""
    subprocess.run(["git", "init"], cwd=path, check=True, capture_output=True)
    subprocess.run(
        ["git", "config", "user.email", "test@test.com"],
        cwd=path,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "Test"],
        cwd=path,
        check=True,
        capture_output=True,
    )
    (path / "README.md").write_text("init\n")
    subprocess.run(
        ["git", "add", "README.md"], cwd=path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "init"],
        cwd=path,
        check=True,
        capture_output=True,
    )


# ---------------------------------------------------------------------------
# review-code stage
# ---------------------------------------------------------------------------


def _make_review_code_stage(client: ReviewCreatingClient) -> Agent:
    return Agent(
        "review-code",
        [
            load_bundled_prompt("code_style.md"),
            load_bundled_prompt("review/detail.md"),
            "`{review-code}` is the canonical and required location.",
        ],
        {},
        bind_map={"review-code": "file://session/{name}-{model}.md"},
    )


def test_review_code_stage_passes_worktree_cwd_to_client(tmp_path):
    """When state.worktree is set (parallel child), client.run gets cwd=worktree
    so the model reads/writes the isolated worktree, not the parent process cwd."""
    client = ReviewCreatingClient(fixtures={"review-code": MINIMAL_EVENTS})
    worktree = tmp_path / "wt"
    worktree.mkdir()
    artifact_dir = tmp_path / "session"
    artifact_dir.mkdir()
    stage = _make_review_code_stage(client)
    state = build_state(
        data=StateData(),
        client=client,
        artifact_dir=artifact_dir,
        worktree=worktree,
        artifacts=ArtifactRegistry(artifact_dir),
    )
    asyncio.run(stage.run(cast("Gremlin", MockGremlin(state))))
    assert client.calls[0].cwd == worktree


def test_review_code_stage_includes_style_from_prompts(tmp_path):
    client = ReviewCreatingClient(fixtures={"review-code": MINIMAL_EVENTS})
    stage = Agent(
        "review-code",
        [
            "Be good.",
            load_bundled_prompt("review/detail.md"),
            "`{review-code}` is the canonical and required location.",
        ],
        {},
        bind_map={"review-code": "file://session/{name}-{model}.md"},
    )
    artifact_dir = tmp_path / "session"
    artifact_dir.mkdir()
    state = _make_state(client, artifact_dir)
    asyncio.run(stage.run(cast("Gremlin", MockGremlin(state))))
    assert "Be good." in client.calls[0].prompt
