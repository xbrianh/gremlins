"""Stage-level tests for the gh pipeline stages (ghreview, ghaddress)."""

import pathlib

from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.pipeline import StageEntry
from gremlins.stages.context import StageContext
from gremlins.stages.ghaddress import GHAddress
from gremlins.stages.ghreview import GHReview

_BUNDLED_PROMPTS = (
    pathlib.Path(__file__).resolve().parent.parent
    / "gremlins"
    / "pipelines"
    / "prompts"
)


def _make_ctx(client, tmp_path, *, gr_id=None):
    return StageContext(client=client, session_dir=tmp_path, gr_id=gr_id)


def _make_ghreview(client, tmp_path, *, gr_id=None, pr_url):
    entry = StageEntry(
        name="ghreview",
        type="ghreview",
        client=None,
        prompt_paths=[_BUNDLED_PROMPTS / "ghreview.md"],
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


def test_ghreview_prompt_no_bail_section_without_gr_id(tmp_path):
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    stage = _make_ghreview(
        client,
        tmp_path,
        gr_id=None,
        pr_url="https://github.com/owner/repo/pull/1",
    )
    stage.run(None)
    assert "python -m gremlins.bail" not in client.calls[0].prompt


def test_ghreview_prompt_includes_bail_section_with_gr_id(tmp_path):
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    stage = _make_ghreview(
        client,
        tmp_path,
        gr_id="gr-test",
        pr_url="https://github.com/owner/repo/pull/1",
    )
    stage.run(None)
    assert "python -m gremlins.bail" in client.calls[0].prompt


def test_ghreview_bail_rubric(tmp_path):
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    stage = _make_ghreview(
        client,
        tmp_path,
        gr_id="gr-test",
        pr_url="https://github.com/owner/repo/pull/1",
    )
    stage.run(None)
    prompt = client.calls[0].prompt
    assert "30 seconds" in prompt
    assert "missing import" in prompt
    assert "anything a human should weigh in on" not in prompt


def _make_ghaddress(client, tmp_path, *, gr_id=None, pr_url):
    entry = StageEntry(
        name="ghaddress",
        type="ghaddress",
        client=None,
        prompt_paths=[_BUNDLED_PROMPTS / "ghaddress.md"],
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


def test_ghaddress_prompt_no_bail_section_without_gr_id(tmp_path):
    client = FakeClaudeClient(fixtures={"ghaddress": MINIMAL_EVENTS})
    stage = _make_ghaddress(
        client,
        tmp_path,
        gr_id=None,
        pr_url="https://github.com/owner/repo/pull/1",
    )
    stage.run(None)
    assert (
        "## Bail markers (running under a gremlin pipeline)"
        not in client.calls[0].prompt
    )


def test_ghaddress_prompt_includes_bail_section_with_gr_id(tmp_path):
    client = FakeClaudeClient(fixtures={"ghaddress": MINIMAL_EVENTS})
    stage = _make_ghaddress(
        client,
        tmp_path,
        gr_id="gr-test",
        pr_url="https://github.com/owner/repo/pull/1",
    )
    stage.run(None)
    assert (
        "## Bail markers (running under a gremlin pipeline)" in client.calls[0].prompt
    )
