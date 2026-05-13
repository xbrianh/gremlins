"""Stage-level tests for the gh pipeline stages (github-review-pull-request, github-address-pull-request-reviews)."""

import pathlib

from conftest import MINIMAL_EVENTS
from conftest import gh_pipeline as _gh_pipeline

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData
from gremlins.stages.address_code import GitHubAddressPullRequestReviews
from gremlins.stages.review_code import GitHubReviewPullRequest

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
        data=StateData(gr_id=gr_id),
        client=client,
        session_dir=tmp_path,
        pipeline_data=_gh_pipeline(),
    )


def _make_gh_review(
    client: FakeClaudeClient,
    tmp_path: pathlib.Path,
    *,
    gr_id: str | None = None,
    pr_url: str,
) -> GitHubReviewPullRequest:
    prompts = [
        (_BUNDLED_PROMPTS / "github_review_pull_request.md").read_text(encoding="utf-8")
    ]
    stage = GitHubReviewPullRequest(
        "github-review-pull-request", "sonnet", prompts, {}, pr_url=pr_url
    )
    return stage


def test_gh_review_prompt_includes_pr_url(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"github-review-pull-request": MINIMAL_EVENTS})
    state = _make_state(client, tmp_path)
    stage = _make_gh_review(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    stage.run(state)
    prompt = client.calls[0].prompt
    assert "https://github.com/owner/repo/pull/1" in prompt
    assert not prompt.startswith("/github-review-pull-request")
    assert "/github-review-pull-request" not in prompt


def test_gh_review_prompt_includes_bail_content(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"github-review-pull-request": MINIMAL_EVENTS})
    state = _make_state(client, tmp_path)
    stage = _make_gh_review(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    stage.run(state)
    assert "python -c" in client.calls[0].prompt


def test_gh_review_bail_rubric(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"github-review-pull-request": MINIMAL_EVENTS})
    state = _make_state(client, tmp_path)
    stage = _make_gh_review(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    stage.run(state)
    prompt = client.calls[0].prompt
    assert "30 seconds" in prompt
    assert "missing import" in prompt
    assert "anything a human should weigh in on" not in prompt


def test_gh_review_parallel_child_uses_new_bail_command(
    tmp_path: pathlib.Path,
) -> None:
    client = FakeClaudeClient(fixtures={"github-review-pull-request": MINIMAL_EVENTS})
    stage = _make_gh_review(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    state = RuntimeState(
        data=StateData(gr_id="gr-123"),
        client=client,
        session_dir=tmp_path,
        child_key="review-child",
        pipeline_data=_gh_pipeline(),
    )
    stage.run(state)
    assert "python -c" in client.calls[0].prompt
    assert "GREMLIN_STATE_DIR" in client.calls[0].prompt
    assert "gremlins.bail" not in client.calls[0].prompt


def _make_gh_address(
    client: FakeClaudeClient,
    tmp_path: pathlib.Path,
    *,
    gr_id: str | None = None,
    pr_url: str,
) -> GitHubAddressPullRequestReviews:
    prompts = [
        (_BUNDLED_PROMPTS / "github_address_pull_request_reviews.md").read_text(
            encoding="utf-8"
        ),
        (_BUNDLED_PROMPTS / "bail_section.md").read_text(encoding="utf-8"),
    ]
    stage = GitHubAddressPullRequestReviews(
        "github-address-pull-request-reviews", "sonnet", prompts, {}, pr_url=pr_url
    )
    return stage


def test_gh_address_prompt_includes_pr_url(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(
        fixtures={"github-address-pull-request-reviews": MINIMAL_EVENTS}
    )
    state = _make_state(client, tmp_path)
    stage = _make_gh_address(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    stage.run(state)
    prompt = client.calls[0].prompt
    assert "https://github.com/owner/repo/pull/1" in prompt
    assert not prompt.startswith("/github-address-pull-request-reviews")
    assert "/github-address-pull-request-reviews" not in prompt


def test_gh_address_prompt_includes_bail_content(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(
        fixtures={"github-address-pull-request-reviews": MINIMAL_EVENTS}
    )
    state = _make_state(client, tmp_path)
    stage = _make_gh_address(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    stage.run(state)
    assert (
        "## Bail markers (running under a gremlin pipeline)" in client.calls[0].prompt
    )


def test_gh_address_parallel_child_uses_new_bail_command(
    tmp_path: pathlib.Path,
) -> None:
    client = FakeClaudeClient(
        fixtures={"github-address-pull-request-reviews": MINIMAL_EVENTS}
    )
    stage = _make_gh_address(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    state = RuntimeState(
        data=StateData(gr_id="gr-123"),
        client=client,
        session_dir=tmp_path,
        child_key="address-child",
        pipeline_data=_gh_pipeline(),
    )
    stage.run(state)
    assert "python -c" in client.calls[0].prompt
    assert "GREMLIN_STATE_DIR" in client.calls[0].prompt
    assert "gremlins.bail" not in client.calls[0].prompt
