"""Stage-level tests for the gh pipeline stages (ghreview, ghaddress)."""

import pathlib

from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.schema import PipelineDef as _PipelineDef
from gremlins.schema import StageEntry as _StageEntry
from gremlins.stages.address_code import AddressCode
from gremlins.stages.base import RuntimeState
from gremlins.stages.review_code import ReviewCode


def _gh_pipeline() -> _PipelineDef:
    return _PipelineDef(
        name="test",
        path=pathlib.Path("."),
        stages=[
            _StageEntry(
                name="open-github-pr",
                type="open-github-pr",
                client=None,
                prompts=[],
                options={},
            )
        ],
    )


_BUNDLED_PROMPTS = (
    pathlib.Path(__file__).resolve().parent.parent / "gremlins" / "prompts"
)


def _make_state(
    client: FakeClaudeClient,
    tmp_path: pathlib.Path,
    *,
    gr_id: str | None = None,
) -> RuntimeState:
    return RuntimeState(
        client=client,
        session_dir=tmp_path,
        gr_id=gr_id,
        is_git=True,
        pipeline_data=_gh_pipeline(),
    )


def _make_ghreview(
    client: FakeClaudeClient,
    tmp_path: pathlib.Path,
    *,
    gr_id: str | None = None,
    pr_url: str,
) -> ReviewCode:
    prompts = [(_BUNDLED_PROMPTS / "review_gh.md").read_text(encoding="utf-8")]
    stage = ReviewCode("ghreview", "sonnet", prompts, {}, pr_url=pr_url)
    return stage


def test_ghreview_prompt_includes_pr_url(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    state = _make_state(client, tmp_path)
    stage = _make_ghreview(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    stage.run(state)
    prompt = client.calls[0].prompt
    assert "https://github.com/owner/repo/pull/1" in prompt
    assert not prompt.startswith("/ghreview")
    assert "/ghreview" not in prompt


def test_ghreview_prompt_includes_bail_content(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    state = _make_state(client, tmp_path)
    stage = _make_ghreview(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    stage.run(state)
    assert "python -m gremlins.bail" in client.calls[0].prompt


def test_ghreview_bail_rubric(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    state = _make_state(client, tmp_path)
    stage = _make_ghreview(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    stage.run(state)
    prompt = client.calls[0].prompt
    assert "30 seconds" in prompt
    assert "missing import" in prompt
    assert "anything a human should weigh in on" not in prompt


def test_ghreview_parallel_child_uses_child_key_bail_command(
    tmp_path: pathlib.Path,
) -> None:
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    stage = _make_ghreview(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    state = RuntimeState(
        client=client,
        session_dir=tmp_path,
        gr_id="gr-123",
        child_key="review-child",
        is_git=True,
        pipeline_data=_gh_pipeline(),
    )
    stage.run(state)
    assert "python -m gremlins.bail --child-key review-child" in client.calls[0].prompt


def _make_ghaddress(
    client: FakeClaudeClient,
    tmp_path: pathlib.Path,
    *,
    gr_id: str | None = None,
    pr_url: str,
) -> AddressCode:
    prompts = [
        (_BUNDLED_PROMPTS / "address_gh.md").read_text(encoding="utf-8"),
        (_BUNDLED_PROMPTS / "bail_section.md").read_text(encoding="utf-8"),
    ]
    stage = AddressCode("ghaddress", "sonnet", prompts, {}, pr_url=pr_url)
    return stage


def test_ghaddress_prompt_includes_pr_url(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"ghaddress": MINIMAL_EVENTS})
    state = _make_state(client, tmp_path)
    stage = _make_ghaddress(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    stage.run(state)
    prompt = client.calls[0].prompt
    assert "https://github.com/owner/repo/pull/1" in prompt
    assert not prompt.startswith("/ghaddress")
    assert "/ghaddress" not in prompt


def test_ghaddress_prompt_includes_bail_content(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"ghaddress": MINIMAL_EVENTS})
    state = _make_state(client, tmp_path)
    stage = _make_ghaddress(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    stage.run(state)
    assert (
        "## Bail markers (running under a gremlin pipeline)" in client.calls[0].prompt
    )


def test_ghaddress_parallel_child_uses_child_key_bail_command(
    tmp_path: pathlib.Path,
) -> None:
    client = FakeClaudeClient(fixtures={"ghaddress": MINIMAL_EVENTS})
    stage = _make_ghaddress(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    state = RuntimeState(
        client=client,
        session_dir=tmp_path,
        gr_id="gr-123",
        child_key="address-child",
        is_git=True,
        pipeline_data=_gh_pipeline(),
    )
    stage.run(state)
    assert "python -m gremlins.bail --child-key address-child" in client.calls[0].prompt
