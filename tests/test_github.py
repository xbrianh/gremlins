import json
import subprocess
from unittest.mock import MagicMock, patch

import pytest

from gremlins.utils.github import (
    GET_PR_CI_STATUS_TIMEOUT,
    fetch_issue,
    get_pr_ci_status,
)

PR_URL = "https://github.com/owner/repo/pull/42"
ISSUE = {
    "number": 42,
    "url": "https://github.com/owner/repo/issues/42",
    "body": "body",
    "title": "title",
}


def test_fetch_issue_non_issue_ref_returns_none():
    assert fetch_issue("not-an-issue-ref") is None
    assert fetch_issue("path/to/file.md") is None
    assert fetch_issue("42") is None


def test_fetch_issue_bare_number_uses_current_repo():
    with patch("gremlins.utils.github.current_repo", return_value="owner/repo"):
        with patch("gremlins.utils.github.view_issue", return_value=ISSUE) as mock_view:
            result = fetch_issue("#42")
    mock_view.assert_called_once_with("42", "owner/repo")
    assert result == ISSUE


def test_fetch_issue_bare_number_no_repo_returns_none():
    with patch("gremlins.utils.github.current_repo", return_value=""):
        assert fetch_issue("#42") is None


def test_fetch_issue_explicit_repo():
    with patch("gremlins.utils.github.view_issue", return_value=ISSUE) as mock_view:
        result = fetch_issue("owner/repo#42")
    mock_view.assert_called_once_with("42", "owner/repo")
    assert result == ISSUE


def test_fetch_issue_view_error_returns_none():
    with patch("gremlins.utils.github.view_issue", side_effect=RuntimeError("fail")):
        assert fetch_issue("owner/repo#42") is None


def _ok(stdout: str) -> MagicMock:
    m = MagicMock()
    m.returncode = 0
    m.stdout = stdout
    m.stderr = ""
    return m


def test_get_pr_ci_status_timeout_raises_runtime_error():
    with patch(
        "subprocess.run",
        side_effect=subprocess.TimeoutExpired(
            cmd="gh", timeout=GET_PR_CI_STATUS_TIMEOUT
        ),
    ):
        with pytest.raises(RuntimeError) as exc_info:
            get_pr_ci_status(PR_URL)

    msg = str(exc_info.value)
    assert str(GET_PR_CI_STATUS_TIMEOUT) in msg
    assert PR_URL in msg


def test_get_pr_ci_status_returns_full_rollup():
    """All check-runs in statusCheckRollup are returned, regardless of required status."""
    rollup = [
        {
            "__typename": "CheckRun",
            "name": "required-check",
            "status": "COMPLETED",
            "conclusion": "SUCCESS",
        },
        {
            "__typename": "CheckRun",
            "name": "optional-check",
            "status": "COMPLETED",
            "conclusion": "FAILURE",
        },
    ]
    with patch(
        "subprocess.run",
        return_value=_ok(
            json.dumps({"statusCheckRollup": rollup, "reviewDecision": ""})
        ),
    ):
        result = get_pr_ci_status(PR_URL)

    assert len(result["checks"]) == 2
    names = {c["name"] for c in result["checks"]}
    assert names == {"required-check", "optional-check"}


def test_get_pr_ci_status_failing_non_required_check_included():
    """A failing non-required check is included so ci-gate enters its fix loop."""
    rollup = [
        {
            "__typename": "CheckRun",
            "name": "check",
            "status": "COMPLETED",
            "conclusion": "FAILURE",
        },
    ]
    with patch(
        "subprocess.run",
        return_value=_ok(
            json.dumps({"statusCheckRollup": rollup, "reviewDecision": ""})
        ),
    ):
        result = get_pr_ci_status(PR_URL)

    assert len(result["checks"]) == 1
    assert result["checks"][0]["conclusion"] == "FAILURE"


def test_get_pr_ci_status_empty_rollup_returns_empty_checks():
    """PR with no check-runs returns an empty checks list."""
    with patch(
        "subprocess.run",
        return_value=_ok(json.dumps({"statusCheckRollup": [], "reviewDecision": ""})),
    ):
        result = get_pr_ci_status(PR_URL)

    assert result["checks"] == []


def test_get_pr_ci_status_returns_review_decision_and_head_sha():
    rollup = [
        {
            "__typename": "CheckRun",
            "name": "ci",
            "status": "COMPLETED",
            "conclusion": "SUCCESS",
        }
    ]
    payload = {
        "statusCheckRollup": rollup,
        "reviewDecision": "APPROVED",
        "headRefOid": "abc123",
    }
    with patch("subprocess.run", return_value=_ok(json.dumps(payload))):
        result = get_pr_ci_status(PR_URL)

    assert result["review_decision"] == "APPROVED"
    assert result["head_sha"] == "abc123"
