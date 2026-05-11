"""Tests for open_github_pr utilities."""

from __future__ import annotations

from typing import Any

from gremlins.stages.open_github_pr import extract_pr_branch_from_events


def _bash_tool_use(cmd: str, tool_id: str = "tu_1") -> dict[str, Any]:
    return {
        "type": "assistant",
        "message": {
            "content": [
                {
                    "type": "tool_use",
                    "id": tool_id,
                    "name": "Bash",
                    "input": {"command": cmd},
                }
            ]
        },
    }


def test_extracts_head_from_gh_pr_create():
    events = [
        _bash_tool_use(
            "gh pr create --head issue-42-my-feature --base main --title 'T' --body 'B'"
        )
    ]
    assert extract_pr_branch_from_events(events) == "issue-42-my-feature"


def test_extracts_head_with_quotes():
    events = [
        _bash_tool_use(
            "gh pr create --head 'issue-42-my-feature' --base main --title 'T' --body 'B'"
        )
    ]
    assert extract_pr_branch_from_events(events) == "issue-42-my-feature"


def test_returns_last_matching_event():
    events = [
        _bash_tool_use("gh pr create --head first-branch --base main", "tu_1"),
        _bash_tool_use("gh pr create --head second-branch --base main", "tu_2"),
    ]
    assert extract_pr_branch_from_events(events) == "second-branch"


def test_returns_empty_when_no_gh_pr_create():
    events = [_bash_tool_use("git push origin HEAD:refs/heads/my-branch")]
    assert extract_pr_branch_from_events(events) == ""


def test_returns_empty_on_empty_events():
    assert extract_pr_branch_from_events([]) == ""


def test_ignores_non_bash_tool_calls():
    events = [
        {
            "type": "assistant",
            "message": {
                "content": [
                    {
                        "type": "tool_use",
                        "name": "Read",
                        "input": {"command": "gh pr create --head fake-branch"},
                    }
                ]
            },
        }
    ]
    assert extract_pr_branch_from_events(events) == ""
