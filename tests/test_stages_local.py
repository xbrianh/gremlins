import json
import pathlib
import subprocess

import pytest
from conftest import MINIMAL_EVENTS, ReviewCreatingClient

from gremlins.clients.fake import FakeClaudeClient
from gremlins.pipeline import StageEntry, load_pipeline, resolve_pipeline_path
from gremlins.stages.address_code import AddressCode
from gremlins.stages.context import StageContext
from gremlins.stages.implement import ImplementOptions, _render_spec_block
from gremlins.stages.implement import run as run_implement
from gremlins.stages.plan import PlanOptions
from gremlins.stages.plan import run as run_plan
from gremlins.stages.review_code import ReviewCodeOptions
from gremlins.stages.review_code import run as run_review_code

_BUNDLED_PROMPTS = (
    pathlib.Path(__file__).resolve().parent.parent
    / "gremlins"
    / "pipelines"
    / "prompts"
)


def test_local_yaml_loads_and_validates(tmp_path):
    pipeline = load_pipeline(resolve_pipeline_path("local", tmp_path))
    assert len(pipeline.stages) == 5
    names = [s.name for s in pipeline.stages]
    assert names == ["plan", "implement", "review-code", "address-code", "verify"]


def _make_ctx(client, session_dir, *, gr_id=None):
    return StageContext(client=client, session_dir=session_dir, gr_id=gr_id)


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
# _render_spec_block
# ---------------------------------------------------------------------------


def test_render_spec_block_empty_string():
    assert _render_spec_block("") == ""


def test_render_spec_block_whitespace_only():
    assert _render_spec_block("   \n  ") == ""


def test_render_spec_block_nonempty():
    result = _render_spec_block("my spec content")
    assert "Overarching goal (north star)" in result
    assert "my spec content" in result
    assert "~~~~" in result
    assert "read-only context" in result


def test_render_spec_block_truncates_at_50000():
    long_spec = "x" * 60000
    result = _render_spec_block(long_spec)
    # no newlines → falls back to hard 50000-char cut
    assert "x" * 50000 in result
    assert "truncated" in result
    assert "60000 chars total" in result


def test_render_spec_block_truncates_at_newline_boundary():
    # 49990 x's, a newline, then a run of z's that push past the 50000 limit.
    # Using 'z' avoids false positives from the header prose (which contains 'y').
    long_spec = "x" * 49990 + "\n" + "z" * 10100
    result = _render_spec_block(long_spec)
    # cut at the newline before 50000 → no z's in the body
    assert "z" not in result
    assert "truncated" in result


def test_render_spec_block_no_truncation_note_when_short():
    result = _render_spec_block("short spec")
    assert "truncated" not in result


# ---------------------------------------------------------------------------
# implement stage spec_text rendering
# ---------------------------------------------------------------------------


def test_implement_renders_spec_block_when_present(tmp_path, monkeypatch):
    git_dir = tmp_path / "repo"
    git_dir.mkdir()
    _init_git_repo(git_dir)
    monkeypatch.chdir(git_dir)

    session_dir = tmp_path / "session"
    session_dir.mkdir()

    class _CommittingClient(FakeClaudeClient):
        def run(self, prompt, *, label, **kwargs):
            (git_dir / "newfile.txt").write_text("change\n")
            subprocess.run(
                ["git", "add", "newfile.txt"],
                cwd=git_dir,
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["git", "commit", "-m", "implement"],
                cwd=git_dir,
                check=True,
                capture_output=True,
            )
            return super().run(prompt, label=label, **kwargs)

    client = _CommittingClient(fixtures={"implement": MINIMAL_EVENTS})
    ctx = _make_ctx(client, session_dir)
    run_implement(
        ctx,
        ImplementOptions(
            impl_model="sonnet",
            plan_text="task 1: do something",
            code_style="Be good.",
            is_git=True,
            spec_text="overall spec body",
        ),
    )
    prompt = client.calls[0].prompt
    assert "Overarching goal (north star)" in prompt
    assert "overall spec body" in prompt
    assert prompt.index("overall spec body") < prompt.index("task 1: do something")


def test_implement_omits_spec_block_when_absent(tmp_path, monkeypatch):
    git_dir = tmp_path / "repo"
    git_dir.mkdir()
    _init_git_repo(git_dir)
    monkeypatch.chdir(git_dir)

    session_dir = tmp_path / "session"
    session_dir.mkdir()

    class _CommittingClient(FakeClaudeClient):
        def run(self, prompt, *, label, **kwargs):
            (git_dir / "newfile.txt").write_text("change\n")
            subprocess.run(
                ["git", "add", "newfile.txt"],
                cwd=git_dir,
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["git", "commit", "-m", "implement"],
                cwd=git_dir,
                check=True,
                capture_output=True,
            )
            return super().run(prompt, label=label, **kwargs)

    client = _CommittingClient(fixtures={"implement": MINIMAL_EVENTS})
    ctx = _make_ctx(client, session_dir)
    run_implement(
        ctx,
        ImplementOptions(
            impl_model="sonnet",
            plan_text="task 1: do something",
            code_style="Be good.",
            is_git=True,
            spec_text="",
        ),
    )
    prompt = client.calls[0].prompt
    assert "Overarching goal" not in prompt


# ---------------------------------------------------------------------------
# plan stage
# ---------------------------------------------------------------------------


def test_plan_stage_raises_when_file_absent(tmp_path):
    client = FakeClaudeClient(fixtures={"plan": MINIMAL_EVENTS})
    plan_file = tmp_path / "plan.md"
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    ctx = _make_ctx(client, session_dir)
    # FakeClaudeClient won't create the plan file — stage must raise.
    with pytest.raises(RuntimeError, match="plan stage did not produce"):
        run_plan(
            ctx,
            PlanOptions(
                plan_model="sonnet",
                plan_file=plan_file,
                instructions="do stuff",
                code_style="Be good.",
                prompt_path=_BUNDLED_PROMPTS / "plan.md",
            ),
        )
    assert len(client.calls) == 1
    assert client.calls[0].label == "plan"
    assert client.calls[0].model == "sonnet"


def test_plan_stage_succeeds_when_file_exists(tmp_path):
    plan_file = tmp_path / "plan.md"
    session_dir = tmp_path / "session"
    session_dir.mkdir()

    class _WritingClient(FakeClaudeClient):
        def run(self, prompt, *, label, **kwargs):
            plan_file.write_text("# Plan\nDo stuff.\n")
            return super().run(prompt, label=label, **kwargs)

    client = _WritingClient(fixtures={"plan": MINIMAL_EVENTS})
    ctx = _make_ctx(client, session_dir)
    run_plan(
        ctx,
        PlanOptions(
            plan_model="haiku",
            plan_file=plan_file,
            instructions="do stuff",
            code_style="Be good.",
            prompt_path=_BUNDLED_PROMPTS / "plan.md",
        ),
    )
    assert plan_file.exists()
    assert client.calls[0].label == "plan"
    assert client.calls[0].model == "haiku"


# ---------------------------------------------------------------------------
# implement stage
# ---------------------------------------------------------------------------


def test_implement_stage_raises_on_empty_diff(tmp_path, monkeypatch):
    git_dir = tmp_path / "repo"
    git_dir.mkdir()
    _init_git_repo(git_dir)
    monkeypatch.chdir(git_dir)

    session_dir = tmp_path / "session"
    session_dir.mkdir()

    client = FakeClaudeClient(fixtures={"implement": MINIMAL_EVENTS})
    ctx = _make_ctx(client, session_dir)
    with pytest.raises(RuntimeError, match="no changes"):
        run_implement(
            ctx,
            ImplementOptions(
                impl_model="sonnet",
                plan_text="# Plan\nDo stuff.\n",
                code_style="Be good.",
                is_git=True,
            ),
        )
    assert len(client.calls) == 1
    assert client.calls[0].label == "implement"


# ---------------------------------------------------------------------------
# address-code stage
# ---------------------------------------------------------------------------


def _make_address_code_stage(
    client: FakeClaudeClient,
    session_dir,
    *,
    model: str = "sonnet",
    is_git: bool = False,
    code_style: str = "",
    gr_id=None,
) -> AddressCode:
    entry = StageEntry(
        name="address-code",
        type="address-code",
        client=None,
        prompt_paths=[],
        options={},
    )
    stage = AddressCode(entry, model, is_git=is_git, code_style=code_style)
    stage.bind(_make_ctx(client, session_dir, gr_id=gr_id))
    return stage


def test_address_code_stage_calls_client_with_review_content(tmp_path):
    review_text = "# Detail Review\n\n## Findings\nLooks good.\n"
    (tmp_path / "review-code-sonnet.md").write_text(review_text)

    client = FakeClaudeClient(fixtures={"address-code": MINIMAL_EVENTS})
    stage = _make_address_code_stage(client, tmp_path)
    stage.run(None)

    assert len(client.calls) == 1
    call = client.calls[0]
    assert call.label == "address-code"
    assert call.model == "sonnet"
    assert "Detail Review" in call.prompt


# ---------------------------------------------------------------------------
# code_style block appears in plan, review, and address prompts
# ---------------------------------------------------------------------------


def test_plan_stage_includes_code_style(tmp_path):
    client = FakeClaudeClient(fixtures={"plan": MINIMAL_EVENTS})
    plan_file = tmp_path / "plan.md"
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    ctx = _make_ctx(client, session_dir)
    with pytest.raises(RuntimeError, match="plan stage did not produce"):
        run_plan(
            ctx,
            PlanOptions(
                plan_model="sonnet",
                plan_file=plan_file,
                instructions="do stuff",
                code_style="Be good.",
                prompt_path=_BUNDLED_PROMPTS / "plan.md",
            ),
        )
    assert "Be good." in client.calls[0].prompt


def test_review_code_stage_passes_worktree_cwd_to_client(tmp_path):
    """When ctx.worktree is set (parallel child), client.run gets cwd=worktree
    so claude -p reads/writes the isolated worktree, not the parent process cwd."""
    client = ReviewCreatingClient(fixtures={"review-code:sonnet": MINIMAL_EVENTS})
    worktree = tmp_path / "wt"
    worktree.mkdir()
    ctx = StageContext(
        client=client,
        session_dir=tmp_path,
        gr_id=None,
        worktree=worktree,
    )
    run_review_code(
        ctx,
        ReviewCodeOptions(
            plan_text="",
            is_git=False,
            code_style="",
            model="sonnet",
            stage_name="review-code",
            prompt_paths=[_BUNDLED_PROMPTS / "review" / "detail.md"],
        ),
    )
    assert client.calls[0].cwd == worktree


def test_review_code_stage_includes_code_style(tmp_path):
    client = ReviewCreatingClient(fixtures={"review-code:sonnet": MINIMAL_EVENTS})
    ctx = _make_ctx(client, tmp_path)
    run_review_code(
        ctx,
        ReviewCodeOptions(
            plan_text="",
            is_git=False,
            code_style="Be good.",
            model="sonnet",
            stage_name="review-code",
            prompt_paths=[_BUNDLED_PROMPTS / "review" / "detail.md"],
        ),
    )
    assert "Be good." in client.calls[0].prompt


def test_address_code_stage_includes_code_style(tmp_path):
    (tmp_path / "review-code-sonnet.md").write_text(
        "# Detail Review\n\n## Findings\nNone.\n"
    )
    client = FakeClaudeClient(fixtures={"address-code": MINIMAL_EVENTS})
    stage = _make_address_code_stage(client, tmp_path, code_style="Be good.")
    stage.run(None)
    assert "Be good." in client.calls[0].prompt


def test_review_code_stage_writes_stage_to_state(tmp_path, make_state_dir):
    gr_id = "test-gr-id"
    state_dir = make_state_dir(gr_id)
    client = ReviewCreatingClient(fixtures={"review-code:sonnet": MINIMAL_EVENTS})
    ctx = _make_ctx(client, tmp_path, gr_id=gr_id)
    run_review_code(
        ctx,
        ReviewCodeOptions(
            plan_text="",
            is_git=False,
            code_style="",
            model="sonnet",
            stage_name="review-code",
            prompt_paths=[_BUNDLED_PROMPTS / "review" / "detail.md"],
        ),
    )
    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("stage") == "review-code"


def test_address_code_stage_emits_bail_on_failure(tmp_path, make_state_dir):
    gr_id = "test-gr-id"
    state_dir = make_state_dir(gr_id)
    client = FakeClaudeClient(fixtures={})
    stage = _make_address_code_stage(client, tmp_path, gr_id=gr_id)
    with pytest.raises(FileNotFoundError):
        stage.run(None)
    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("bail_class") == "other"
