"""Stage-level tests for the gh pipeline stages (ghreview, ghaddress)."""

import pathlib

from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages.context import StageContext
from gremlins.stages.ghaddress import GhaddressOptions
from gremlins.stages.ghaddress import run as run_ghaddress
from gremlins.stages.ghreview import GhreviewOptions
from gremlins.stages.ghreview import run as run_ghreview

_BUNDLED_PROMPTS = (
    pathlib.Path(__file__).resolve().parent.parent
    / "gremlins"
    / "pipelines"
    / "prompts"
)


def _make_ctx(client, tmp_path, *, gr_id=None):
    return StageContext(client=client, session_dir=tmp_path, gr_id=gr_id)


def test_ghreview_prompt_includes_pr_url_and_code_style(tmp_path):
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    run_ghreview(
        _make_ctx(client, tmp_path),
        GhreviewOptions(
            model="sonnet",
            pr_url="https://github.com/owner/repo/pull/1",
            code_style="Be good.",
            prompt_path=_BUNDLED_PROMPTS / "ghreview.md",
        ),
    )
    prompt = client.calls[0].prompt
    assert "https://github.com/owner/repo/pull/1" in prompt
    assert "Be good." in prompt
    assert not prompt.startswith("/ghreview")
    assert "/ghreview" not in prompt


def test_ghreview_prompt_no_bail_section_without_gr_id(tmp_path):
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    run_ghreview(
        _make_ctx(client, tmp_path, gr_id=None),
        GhreviewOptions(
            model="sonnet",
            pr_url="https://github.com/owner/repo/pull/1",
            code_style="",
            prompt_path=_BUNDLED_PROMPTS / "ghreview.md",
        ),
    )
    assert "python -m gremlins.bail" not in client.calls[0].prompt


def test_ghreview_prompt_includes_bail_section_with_gr_id(tmp_path):
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    run_ghreview(
        _make_ctx(client, tmp_path, gr_id="gr-test"),
        GhreviewOptions(
            model="sonnet",
            pr_url="https://github.com/owner/repo/pull/1",
            code_style="",
            prompt_path=_BUNDLED_PROMPTS / "ghreview.md",
        ),
    )
    assert "python -m gremlins.bail" in client.calls[0].prompt


def test_ghaddress_prompt_includes_pr_url_and_code_style(tmp_path):
    client = FakeClaudeClient(fixtures={"ghaddress": MINIMAL_EVENTS})
    run_ghaddress(
        _make_ctx(client, tmp_path),
        GhaddressOptions(
            model="sonnet",
            pr_url="https://github.com/owner/repo/pull/1",
            code_style="Be good.",
            prompt_path=_BUNDLED_PROMPTS / "ghaddress.md",
        ),
    )
    prompt = client.calls[0].prompt
    assert "https://github.com/owner/repo/pull/1" in prompt
    assert "Be good." in prompt
    assert not prompt.startswith("/ghaddress")
    assert "/ghaddress" not in prompt


def test_ghaddress_prompt_no_bail_section_without_gr_id(tmp_path):
    client = FakeClaudeClient(fixtures={"ghaddress": MINIMAL_EVENTS})
    run_ghaddress(
        _make_ctx(client, tmp_path, gr_id=None),
        GhaddressOptions(
            model="sonnet",
            pr_url="https://github.com/owner/repo/pull/1",
            code_style="",
            prompt_path=_BUNDLED_PROMPTS / "ghaddress.md",
        ),
    )
    assert (
        "## Bail markers (running under a gremlin pipeline)"
        not in client.calls[0].prompt
    )


def test_ghaddress_prompt_includes_bail_section_with_gr_id(tmp_path):
    client = FakeClaudeClient(fixtures={"ghaddress": MINIMAL_EVENTS})
    run_ghaddress(
        _make_ctx(client, tmp_path, gr_id="gr-test"),
        GhaddressOptions(
            model="sonnet",
            pr_url="https://github.com/owner/repo/pull/1",
            code_style="",
            prompt_path=_BUNDLED_PROMPTS / "ghaddress.md",
        ),
    )
    assert (
        "## Bail markers (running under a gremlin pipeline)" in client.calls[0].prompt
    )
