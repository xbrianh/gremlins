from unittest.mock import patch

from gremlins.utils.github import (
    fetch_issue,
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


