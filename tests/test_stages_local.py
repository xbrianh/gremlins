import asyncio
import pathlib
import subprocess

import pytest
from conftest import MINIMAL_EVENTS, ReviewCreatingClient

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import StateData, build_state
from gremlins.pipeline import Pipeline
from gremlins.pipeline.discovery import resolve_pipeline_path
from gremlins.stages import plan
from gremlins.stages.agent import Agent

_BUNDLED_PROMPTS = (
    pathlib.Path(__file__).resolve().parent.parent / "gremlins" / "prompts"
)


def test_local_yaml_loads_and_validates(tmp_path):
    pipeline = Pipeline.from_yaml(resolve_pipeline_path("local", tmp_path))
    assert len(pipeline.stages) == 7
    names = [s.name for s in pipeline.stages]
    assert names == [
        "plan",
        "implement",
        "require-impl-progress",
        "review-code",
        "address-code",
        "normalize",
        "verify",
    ]


def _make_state(client, session_dir, *, gremlin_id=None, base_ref_sha=""):
    registry = ArtifactRegistry(session_dir)
    if base_ref_sha:
        registry.bind("base_sha", Uri.parse(f"git://commit/{base_ref_sha}"))
    state = build_state(
        data=StateData(gremlin_id=gremlin_id),
        client=client,
        session_dir=session_dir,
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
# plan stage
# ---------------------------------------------------------------------------


def test_plan_stage_raises_when_file_absent(tmp_path):
    client = FakeClaudeClient(fixtures={"plan": MINIMAL_EVENTS})
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    stage = plan.Plan(
        "plan",
        [(_BUNDLED_PROMPTS / "plan.md").read_text(encoding="utf-8")],
        {},
    )
    state = _make_state(client, session_dir)
    state.instructions = "do stuff"
    with pytest.raises(FileNotFoundError, match="plan.md"):
        asyncio.run(stage.run(state))
    assert len(client.calls) == 1
    assert client.calls[0].label == "plan"


def test_plan_stage_succeeds_when_file_exists(tmp_path):
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = session_dir / "plan.md"

    class _WritingClient(FakeClaudeClient):
        async def run(self, prompt, *, label, **kwargs):
            plan_file.write_text("# Plan\nDo stuff.\n")
            return await super().run(prompt, label=label, **kwargs)

    client = _WritingClient(fixtures={"plan": MINIMAL_EVENTS})
    stage = plan.Plan(
        "plan",
        [(_BUNDLED_PROMPTS / "plan.md").read_text(encoding="utf-8")],
        {},
    )
    state = _make_state(client, session_dir)
    state.instructions = "do stuff"
    asyncio.run(stage.run(state))
    assert plan_file.exists()
    assert client.calls[0].label == "plan"


# ---------------------------------------------------------------------------
# review-code stage
# ---------------------------------------------------------------------------


def _make_review_code_stage(
    client: ReviewCreatingClient,
    session_dir,
    *,
    model: str = "sonnet",
    gremlin_id=None,
) -> Agent:
    return Agent(
        "review-code",
        [
            (_BUNDLED_PROMPTS / "code_style.md").read_text(encoding="utf-8"),
            (_BUNDLED_PROMPTS / "review" / "detail.md").read_text(encoding="utf-8"),
            "`{session_dir}/{name}-{model}.md` is the canonical and required location.",
        ],
        {},
        out_map={"review-code": "file://session/{name}-{model}.md"},
    )


# ---------------------------------------------------------------------------
# code_style block appears in plan, review, and address prompts
# ---------------------------------------------------------------------------


def test_plan_stage_includes_style_from_prompts(tmp_path):
    client = FakeClaudeClient(fixtures={"plan": MINIMAL_EVENTS})
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    stage = plan.Plan(
        "plan",
        [
            "Be good.",
            (_BUNDLED_PROMPTS / "plan.md").read_text(encoding="utf-8"),
        ],
        {},
    )
    state = _make_state(client, session_dir)
    state.instructions = "do stuff"
    with pytest.raises(FileNotFoundError, match="plan.md"):
        asyncio.run(stage.run(state))
    assert "Be good." in client.calls[0].prompt


def test_review_code_stage_passes_worktree_cwd_to_client(tmp_path):
    """When state.worktree is set (parallel child), client.run gets cwd=worktree
    so claude -p reads/writes the isolated worktree, not the parent process cwd."""
    client = ReviewCreatingClient(fixtures={"review-code": MINIMAL_EVENTS})
    worktree = tmp_path / "wt"
    worktree.mkdir()
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    stage = _make_review_code_stage(client, session_dir)
    state = build_state(
        data=StateData(),
        client=client,
        session_dir=session_dir,
        worktree=worktree,
        artifacts=ArtifactRegistry(session_dir),
    )
    asyncio.run(stage.run(state))
    assert client.calls[0].cwd == worktree


def test_review_code_stage_includes_style_from_prompts(tmp_path):
    client = ReviewCreatingClient(fixtures={"review-code": MINIMAL_EVENTS})
    stage = Agent(
        "review-code",
        [
            "Be good.",
            (_BUNDLED_PROMPTS / "review" / "detail.md").read_text(encoding="utf-8"),
            "`{session_dir}/{name}-{model}.md` is the canonical and required location.",
        ],
        {},
        out_map={"review-code": "file://session/{name}-{model}.md"},
    )
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    state = _make_state(client, session_dir)
    asyncio.run(stage.run(state))
    assert "Be good." in client.calls[0].prompt
