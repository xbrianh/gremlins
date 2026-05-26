"""Stage-level tests for the gh pipeline stages (github-review-pull-request, github-address-pull-request-reviews)."""

import asyncio
import pathlib

from conftest import MINIMAL_EVENTS
from conftest import gh_pipeline as _gh_pipeline

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData, build_state
from gremlins.stages.github_address_pull_request_reviews import GitHubAddressPullRequestReviews
from gremlins.stages.review_code import GitHubReviewPullRequest

_BUNDLED_PROMPTS = (
    pathlib.Path(__file__).resolve().parent.parent / "gremlins" / "prompts"
)


def _make_state(
    client: FakeClaudeClient,
    tmp_path: pathlib.Path,
    *,
    gremlin_id: str | None = None,
) -> RuntimeState:
    return build_state(
        data=StateData(gremlin_id=gremlin_id),
        client=client,
        session_dir=tmp_path,
        pipeline_data=_gh_pipeline(),
        artifacts=ArtifactRegistry(tmp_path),
    )


def _make_gh_review(
    client: FakeClaudeClient,
    tmp_path: pathlib.Path,
    *,
    gremlin_id: str | None = None,
    pr_url: str,
) -> GitHubReviewPullRequest:
    prompts = [
        (_BUNDLED_PROMPTS / "github_review_pull_request.md").read_text(encoding="utf-8")
    ]
    stage = GitHubReviewPullRequest(
        "github-review-pull-request", prompts, {}, pr_url=pr_url
    )
    return stage


def test_gh_review_prompt_includes_pr_url(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"github-review-pull-request": MINIMAL_EVENTS})
    state = _make_state(client, tmp_path)
    stage = _make_gh_review(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    asyncio.run(stage.run(state))
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
    asyncio.run(stage.run(state))
    assert "BAIL:" in client.calls[0].prompt
    assert "python -c" not in client.calls[0].prompt


def test_gh_review_bail_rubric(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"github-review-pull-request": MINIMAL_EVENTS})
    state = _make_state(client, tmp_path)
    stage = _make_gh_review(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    asyncio.run(stage.run(state))
    prompt = client.calls[0].prompt
    assert "30 seconds" in prompt
    assert "missing import" in prompt
    assert "anything a human should weigh in on" not in prompt


def test_gh_review_parallel_child_prompt_includes_sentinel(
    tmp_path: pathlib.Path,
) -> None:
    client = FakeClaudeClient(fixtures={"github-review-pull-request": MINIMAL_EVENTS})
    stage = _make_gh_review(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    state = build_state(
        data=StateData(gremlin_id="gr-123"),
        client=client,
        session_dir=tmp_path,
        child_key="review-child",
        pipeline_data=_gh_pipeline(),
        artifacts=ArtifactRegistry(tmp_path),
    )
    asyncio.run(stage.run(state))
    assert "BAIL:" in client.calls[0].prompt
    assert "python -c" not in client.calls[0].prompt
    assert "GREMLIN_STATE_DIR" not in client.calls[0].prompt


def _make_gh_address(
    client: FakeClaudeClient,
    tmp_path: pathlib.Path,
    *,
    gremlin_id: str | None = None,
    pr_url: str,
) -> GitHubAddressPullRequestReviews:
    prompts = [
        (_BUNDLED_PROMPTS / "github_address_pull_request_reviews.md").read_text(
            encoding="utf-8"
        ),
        (_BUNDLED_PROMPTS / "bail_section.md").read_text(encoding="utf-8"),
    ]
    stage = GitHubAddressPullRequestReviews(
        "github-address-pull-request-reviews", prompts, {}, pr_url=pr_url
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
    asyncio.run(stage.run(state))
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
    asyncio.run(stage.run(state))
    assert (
        "## Bail markers (running under a gremlin pipeline)" in client.calls[0].prompt
    )


def test_gh_address_parallel_child_prompt_includes_sentinel(
    tmp_path: pathlib.Path,
) -> None:
    client = FakeClaudeClient(
        fixtures={"github-address-pull-request-reviews": MINIMAL_EVENTS}
    )
    stage = _make_gh_address(
        client, tmp_path, pr_url="https://github.com/owner/repo/pull/1"
    )
    state = build_state(
        data=StateData(gremlin_id="gr-123"),
        client=client,
        session_dir=tmp_path,
        child_key="address-child",
        pipeline_data=_gh_pipeline(),
        artifacts=ArtifactRegistry(tmp_path),
    )
    asyncio.run(stage.run(state))
    assert "BAIL:" in client.calls[0].prompt
    assert "python -c" not in client.calls[0].prompt
    assert "GREMLIN_STATE_DIR" not in client.calls[0].prompt
