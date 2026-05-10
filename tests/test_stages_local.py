import json
import pathlib
import subprocess

import pytest
from conftest import MINIMAL_EVENTS, ReviewCreatingClient

from gremlins.clients.fake import FakeClaudeClient
from gremlins.pipeline.discovery import resolve_pipeline_path
from gremlins.pipeline.loader import load_pipeline
from gremlins.schema import StageEntry as _StageEntry
from gremlins.stages import implement, plan
from gremlins.stages.address_code import AddressCode
from gremlins.stages.base import RuntimeState
from gremlins.stages.implement import _render_spec_block
from gremlins.stages.review_code import ReviewCode

_BUNDLED_PROMPTS = (
    pathlib.Path(__file__).resolve().parent.parent / "gremlins" / "prompts"
)


def test_local_yaml_loads_and_validates(tmp_path):
    pipeline = load_pipeline(resolve_pipeline_path("local", tmp_path))
    assert len(pipeline.stages) == 5
    names = [s.name for s in pipeline.stages]
    assert names == ["plan", "implement", "review-code", "address-code", "verify"]


def _make_state(client, session_dir, *, gr_id=None):
    return RuntimeState(client=client, session_dir=session_dir, gr_id=gr_id)


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
    (session_dir / "plan.md").write_text("task 1: do something", encoding="utf-8")
    (session_dir / "spec.md").write_text("overall spec body", encoding="utf-8")
    stage = implement.Implement(
        "implement",
        "sonnet",
        [(_BUNDLED_PROMPTS / "implement_local.md").read_text(encoding="utf-8")],
        {},
    )
    state = _make_state(client, session_dir)
    state.is_git = True
    stage.run(state)
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
    (session_dir / "plan.md").write_text("task 1: do something", encoding="utf-8")
    stage = implement.Implement(
        "implement",
        "sonnet",
        [(_BUNDLED_PROMPTS / "implement_local.md").read_text(encoding="utf-8")],
        {},
    )
    state = _make_state(client, session_dir)
    state.is_git = True
    stage.run(state)
    prompt = client.calls[0].prompt
    assert "Overarching goal" not in prompt


# ---------------------------------------------------------------------------
# plan stage
# ---------------------------------------------------------------------------


def test_plan_stage_raises_when_file_absent(tmp_path):
    client = FakeClaudeClient(fixtures={"plan": MINIMAL_EVENTS})
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    stage = plan.Plan(
        "plan",
        "sonnet",
        [(_BUNDLED_PROMPTS / "plan.md").read_text(encoding="utf-8")],
        {},
    )
    state = _make_state(client, session_dir)
    state.instructions = "do stuff"
    with pytest.raises(RuntimeError, match="plan stage did not produce"):
        stage.run(state)
    assert len(client.calls) == 1
    assert client.calls[0].label == "plan"
    assert client.calls[0].model == "sonnet"


def test_plan_stage_succeeds_when_file_exists(tmp_path):
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = session_dir / "plan.md"

    class _WritingClient(FakeClaudeClient):
        def run(self, prompt, *, label, **kwargs):
            plan_file.write_text("# Plan\nDo stuff.\n")
            return super().run(prompt, label=label, **kwargs)

    client = _WritingClient(fixtures={"plan": MINIMAL_EVENTS})
    stage = plan.Plan(
        "plan",
        "haiku",
        [(_BUNDLED_PROMPTS / "plan.md").read_text(encoding="utf-8")],
        {},
    )
    state = _make_state(client, session_dir)
    state.instructions = "do stuff"
    stage.run(state)
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
    (session_dir / "plan.md").write_text("# Plan\nDo stuff.\n", encoding="utf-8")
    stage = implement.Implement(
        "implement",
        "sonnet",
        [(_BUNDLED_PROMPTS / "implement_local.md").read_text(encoding="utf-8")],
        {},
    )
    state = _make_state(client, session_dir)
    state.is_git = True
    with pytest.raises(RuntimeError, match="no committed work"):
        stage.run(state)
    assert len(client.calls) == 1
    assert client.calls[0].label == "implement"


# ---------------------------------------------------------------------------
# review-code stage
# ---------------------------------------------------------------------------


def _make_review_code_stage(
    client: ReviewCreatingClient,
    session_dir,
    *,
    model: str = "sonnet",
    gr_id=None,
) -> ReviewCode:
    stage = ReviewCode(
        "review-code",
        model,
        [
            (_BUNDLED_PROMPTS / "code_style.md").read_text(encoding="utf-8"),
            (_BUNDLED_PROMPTS / "review" / "detail.md").read_text(encoding="utf-8"),
        ],
        {},
    )
    return stage


# address-code stage
# ---------------------------------------------------------------------------


def _make_address_code_stage(
    client: FakeClaudeClient,
    session_dir,
    *,
    model: str = "sonnet",
    gr_id=None,
) -> AddressCode:
    stage = AddressCode(
        "address-code",
        model,
        [(_BUNDLED_PROMPTS / "address.md").read_text(encoding="utf-8")],
        {},
    )
    return stage


def test_address_code_stage_calls_client_with_review_content(tmp_path):
    review_text = "# Detail Review\n\n## Findings\nLooks good.\n"
    (tmp_path / "review-code-sonnet.md").write_text(review_text)

    client = FakeClaudeClient(fixtures={"address-code": MINIMAL_EVENTS})
    stage = _make_address_code_stage(client, tmp_path)
    state = _make_state(client, tmp_path)
    stage.run(state)

    assert len(client.calls) == 1
    call = client.calls[0]
    assert call.label == "address-code"
    assert call.model == "sonnet"
    assert "Detail Review" in call.prompt


# ---------------------------------------------------------------------------
# code_style block appears in plan, review, and address prompts
# ---------------------------------------------------------------------------


def test_plan_stage_includes_style_from_prompts(tmp_path):
    client = FakeClaudeClient(fixtures={"plan": MINIMAL_EVENTS})
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    stage = plan.Plan(
        "plan",
        "sonnet",
        [
            "Be good.",
            (_BUNDLED_PROMPTS / "plan.md").read_text(encoding="utf-8"),
        ],
        {},
    )
    state = _make_state(client, session_dir)
    state.instructions = "do stuff"
    with pytest.raises(RuntimeError, match="plan stage did not produce"):
        stage.run(state)
    assert "Be good." in client.calls[0].prompt


def test_review_code_stage_passes_worktree_cwd_to_client(tmp_path):
    """When state.worktree is set (parallel child), client.run gets cwd=worktree
    so claude -p reads/writes the isolated worktree, not the parent process cwd."""
    client = ReviewCreatingClient(fixtures={"review-code:sonnet": MINIMAL_EVENTS})
    worktree = tmp_path / "wt"
    worktree.mkdir()
    stage = _make_review_code_stage(client, tmp_path)
    state = RuntimeState(
        client=client,
        session_dir=tmp_path,
        gr_id=None,
        worktree=worktree,
    )
    stage.run(state)
    assert client.calls[0].cwd == worktree


def test_review_code_stage_includes_style_from_prompts(tmp_path):
    client = ReviewCreatingClient(fixtures={"review-code:sonnet": MINIMAL_EVENTS})
    stage = ReviewCode(
        "review-code",
        "sonnet",
        [
            "Be good.",
            (_BUNDLED_PROMPTS / "review" / "detail.md").read_text(encoding="utf-8"),
        ],
        {},
    )
    state = _make_state(client, tmp_path)
    stage.run(state)
    assert "Be good." in client.calls[0].prompt


def test_address_code_stage_includes_style_from_prompts(tmp_path):
    (tmp_path / "review-code-sonnet.md").write_text(
        "# Detail Review\n\n## Findings\nNone.\n"
    )
    client = FakeClaudeClient(fixtures={"address-code": MINIMAL_EVENTS})
    stage = AddressCode(
        "address-code",
        "sonnet",
        [
            "Be good.",
            (_BUNDLED_PROMPTS / "address.md").read_text(encoding="utf-8"),
        ],
        {},
    )
    state = _make_state(client, tmp_path)
    stage.run(state)
    assert "Be good." in client.calls[0].prompt


def test_review_code_stage_writes_stage_to_state(tmp_path, make_state_dir):
    gr_id = "test-gr-id"
    state_dir = make_state_dir(gr_id)
    client = ReviewCreatingClient(fixtures={"review-code:sonnet": MINIMAL_EVENTS})
    stage = _make_review_code_stage(client, tmp_path, gr_id=gr_id)
    state = _make_state(client, tmp_path, gr_id=gr_id)
    stage.run(state)
    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("stage") == "review-code"


def test_address_code_stage_emits_bail_on_failure(tmp_path, make_state_dir):
    gr_id = "test-gr-id"
    state_dir = make_state_dir(gr_id)
    client = FakeClaudeClient(fixtures={})
    stage = _make_address_code_stage(client, tmp_path, gr_id=gr_id)
    state = _make_state(client, tmp_path, gr_id=gr_id)
    with pytest.raises(FileNotFoundError):
        stage.run(state)
    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("bail_class") == "other"


def test_address_code_finds_review_files_in_parallel_subdirs(tmp_path):
    # Simulate parallel group: reviews_dir/<child>/<stage>-<model>.md
    reviews_dir = tmp_path / "reviews"
    child1_dir = reviews_dir / "review-code"
    child2_dir = reviews_dir / "review-code-fidelity"
    child1_dir.mkdir(parents=True)
    child2_dir.mkdir(parents=True)
    (child1_dir / "review-code-sonnet.md").write_text("# Review 1\nFindings A.\n")
    (child2_dir / "review-code-fidelity-sonnet.md").write_text(
        "# Review 2\nFindings B.\n"
    )

    client = FakeClaudeClient(fixtures={"address-code": MINIMAL_EVENTS})
    stage = AddressCode(
        "address-code",
        "sonnet",
        [(_BUNDLED_PROMPTS / "address.md").read_text(encoding="utf-8")],
        {},
    )
    parallel_entry = _StageEntry(
        name="reviews",
        type="parallel",
        client=None,
        prompts=[],
        options={},
        body=[
            _StageEntry(
                name="review-code",
                type="review-code",
                client=None,
                prompts=[],
                options={},
            ),
            _StageEntry(
                name="review-code-fidelity",
                type="review-code",
                client=None,
                prompts=[],
                options={},
            ),
        ],
    )
    state = RuntimeState(
        client=client,
        session_dir=tmp_path,
        gr_id=None,
        current_scope=[parallel_entry],
    )
    stage.run(state)

    assert len(client.calls) == 1
    prompt = client.calls[0].prompt
    assert "Findings A." in prompt
    assert "Findings B." in prompt
