"""Stage-level tests for the gh pipeline stages (ghreview, ghaddress)."""

from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages.ghaddress import run_ghaddress_stage
from gremlins.stages.ghreview import run_ghreview_stage


def test_ghreview_stage_includes_code_style(tmp_path):
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    run_ghreview_stage(
        client=client,
        model="sonnet",
        pr_url="https://github.com/owner/repo/pull/1",
        artifacts_dir=tmp_path,
        code_style="Be good.",
    )
    assert "Be good." in client.calls[0].prompt


def test_ghaddress_stage_includes_code_style(tmp_path):
    client = FakeClaudeClient(fixtures={"ghaddress": MINIMAL_EVENTS})
    run_ghaddress_stage(
        client=client,
        model="sonnet",
        pr_url="https://github.com/owner/repo/pull/1",
        artifacts_dir=tmp_path,
        code_style="Be good.",
    )
    assert "Be good." in client.calls[0].prompt
