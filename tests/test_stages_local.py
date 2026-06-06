import asyncio
import pathlib
import subprocess
from typing import TYPE_CHECKING, cast

from conftest import MINIMAL_EVENTS, MockGremlin, ReviewCreatingClient

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.uri import Uri
from gremlins.executor.state import State, StateData, build_state
from gremlins.pipeline import Pipeline
from gremlins.pipeline.discovery import resolve_pipeline_path
from gremlins.stages.agent import Agent

if TYPE_CHECKING:
    from gremlins.executor.gremlin import Gremlin

_BUNDLED_PROMPTS = (
    pathlib.Path(__file__).resolve().parent.parent / "gremlins" / "prompts"
)


def test_local_yaml_loads_and_validates(tmp_path):
    pipeline = Pipeline.from_yaml(resolve_pipeline_path("local", tmp_path))
    assert len(pipeline.stages) == 10
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


def _make_state(client, artifact_dir, *, gremlin_id=None, base_ref_sha=""):
    registry = ArtifactRegistry(artifact_dir)
    if base_ref_sha:
        registry.bind("base_sha", Uri.parse(f"git://commit/{base_ref_sha}"))
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
            (_BUNDLED_PROMPTS / "code_style.md").read_text(encoding="utf-8"),
            (_BUNDLED_PROMPTS / "review" / "detail.md").read_text(encoding="utf-8"),
            "`{artifact_dir}/{name}-{model}.md` is the canonical and required location.",
        ],
        {},
        out_map={"review-code": "file://session/{name}-{model}.md"},
    )


def test_review_code_stage_passes_worktree_cwd_to_client(tmp_path):
    """When state.worktree is set (parallel child), client.run gets cwd=worktree
    so claude -p reads/writes the isolated worktree, not the parent process cwd."""
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
            (_BUNDLED_PROMPTS / "review" / "detail.md").read_text(encoding="utf-8"),
            "`{artifact_dir}/{name}-{model}.md` is the canonical and required location.",
        ],
        {},
        out_map={"review-code": "file://session/{name}-{model}.md"},
    )
    artifact_dir = tmp_path / "session"
    artifact_dir.mkdir()
    state = _make_state(client, artifact_dir)
    asyncio.run(stage.run(cast("Gremlin", MockGremlin(state))))
    assert "Be good." in client.calls[0].prompt
