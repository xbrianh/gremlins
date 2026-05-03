"""Stage-level tests for the gh pipeline stages (ghreview, ghaddress)."""

from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages.context import StageContext
from gremlins.stages.ghaddress import GhaddressOptions, run as run_ghaddress
from gremlins.stages.ghreview import GhreviewOptions, run as run_ghreview


def _make_ctx(client, tmp_path):
    return StageContext(client=client, session_dir=tmp_path, gr_id=None)


def test_ghreview_stage_includes_code_style(tmp_path):
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    run_ghreview(
        _make_ctx(client, tmp_path),
        GhreviewOptions(
            model="sonnet",
            pr_url="https://github.com/owner/repo/pull/1",
            code_style="Be good.",
        ),
    )
    assert "Be good." in client.calls[0].prompt


def test_ghaddress_stage_includes_code_style(tmp_path):
    client = FakeClaudeClient(fixtures={"ghaddress": MINIMAL_EVENTS})
    run_ghaddress(
        _make_ctx(client, tmp_path),
        GhaddressOptions(
            model="sonnet",
            pr_url="https://github.com/owner/repo/pull/1",
            code_style="Be good.",
        ),
    )
    assert "Be good." in client.calls[0].prompt
