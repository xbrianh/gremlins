"""Stage-level tests for the gh pipeline stages (ghreview, ghaddress)."""

import pathlib

from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.pipeline import StageEntry
from gremlins.stages.context import StageContext
from gremlins.stages.ghaddress import GHAddress
from gremlins.stages.ghreview import GHReview

_BUNDLED_PROMPTS = (
    pathlib.Path(__file__).resolve().parent.parent / "gremlins" / "prompts"
)


def _make_ctx(client, tmp_path, *, gr_id=None):
    return StageContext(client=client, session_dir=tmp_path, gr_id=gr_id)


def _make_ghreview(client, tmp_path, *, gr_id=None, pr_url):
    entry = StageEntry(
        name="ghreview",
        type="ghreview",
        client=None,
        prompt_paths=[_BUNDLED_PROMPTS / "review_gh.md"],
        options={},
    )
    stage = GHReview(entry, "sonnet", pr_url=pr_url)
    stage.bind(_make_ctx(client, tmp_path, gr_id=gr_id))
    return stage


def test_ghreview_prompt_includes_pr_url(tmp_path):
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    stage = _make_ghreview(
        client,
        tmp_path,
        pr_url="https://github.com/owner/repo/pull/1",
    )
    stage.run(None)
    prompt = client.calls[0].prompt
    assert "https://github.com/owner/repo/pull/1" in prompt
    assert not prompt.startswith("/ghreview")
    assert "/ghreview" not in prompt


def test_ghreview_prompt_includes_bail_content(tmp_path):
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    stage = _make_ghreview(
        client,
        tmp_path,
        pr_url="https://github.com/owner/repo/pull/1",
    )
    stage.run(None)
    assert "python -m gremlins.bail" in client.calls[0].prompt


def test_ghreview_bail_rubric(tmp_path):
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    stage = _make_ghreview(
        client,
        tmp_path,
        pr_url="https://github.com/owner/repo/pull/1",
    )
    stage.run(None)
    prompt = client.calls[0].prompt
    assert "30 seconds" in prompt
    assert "missing import" in prompt
    assert "anything a human should weigh in on" not in prompt


def test_ghreview_parallel_child_uses_child_key_bail_command(tmp_path):
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    stage = _make_ghreview(
        client,
        tmp_path,
        pr_url="https://github.com/owner/repo/pull/1",
    )
    stage.bind(
        StageContext(
            client=client,
            session_dir=tmp_path,
            gr_id="gr-123",
            child_key="review-child",
        )
    )

    stage.run(None)

    assert "python -m gremlins.bail --child-key review-child" in client.calls[0].prompt


def _make_ghaddress(client, tmp_path, *, gr_id=None, pr_url):
    entry = StageEntry(
        name="ghaddress",
        type="ghaddress",
        client=None,
        prompt_paths=[
            _BUNDLED_PROMPTS / "address_gh.md",
            _BUNDLED_PROMPTS / "bail_section.md",
        ],
        options={},
    )
    stage = GHAddress(entry, "sonnet", pr_url=pr_url)
    stage.bind(_make_ctx(client, tmp_path, gr_id=gr_id))
    return stage


def test_ghaddress_prompt_includes_pr_url(tmp_path):
    client = FakeClaudeClient(fixtures={"ghaddress": MINIMAL_EVENTS})
    stage = _make_ghaddress(
        client,
        tmp_path,
        pr_url="https://github.com/owner/repo/pull/1",
    )
    stage.run(None)
    prompt = client.calls[0].prompt
    assert "https://github.com/owner/repo/pull/1" in prompt
    assert not prompt.startswith("/ghaddress")
    assert "/ghaddress" not in prompt


def test_ghaddress_prompt_includes_bail_content(tmp_path):
    client = FakeClaudeClient(fixtures={"ghaddress": MINIMAL_EVENTS})
    stage = _make_ghaddress(
        client,
        tmp_path,
        pr_url="https://github.com/owner/repo/pull/1",
    )
    stage.run(None)
    assert (
        "## Bail markers (running under a gremlin pipeline)" in client.calls[0].prompt
    )


def test_ghaddress_parallel_child_uses_child_key_bail_command(tmp_path):
    client = FakeClaudeClient(fixtures={"ghaddress": MINIMAL_EVENTS})
    stage = _make_ghaddress(
        client,
        tmp_path,
        pr_url="https://github.com/owner/repo/pull/1",
    )
    stage.bind(
        StageContext(
            client=client,
            session_dir=tmp_path,
            gr_id="gr-123",
            child_key="address-child",
        )
    )

    stage.run(None)

    assert "python -m gremlins.bail --child-key address-child" in client.calls[0].prompt
