"""Tests for gremlins.gh_utils."""

import json
import subprocess
from unittest.mock import MagicMock, patch

import pytest

from gremlins.gh_utils import (
    GET_PR_CI_STATUS_TIMEOUT,
    GET_REQUIRED_CHECK_NAMES_TIMEOUT,
    get_pr_ci_status,
    get_required_check_names,
)

PR_URL = "https://github.com/owner/repo/pull/42"


def _ok(stdout: str) -> MagicMock:
    m = MagicMock()
    m.returncode = 0
    m.stdout = stdout
    m.stderr = ""
    return m


def _fail(stderr: str = "error") -> MagicMock:
    m = MagicMock()
    m.returncode = 1
    m.stdout = ""
    m.stderr = stderr
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


def test_get_required_check_names_timeout_raises_runtime_error():
    with patch(
        "subprocess.run",
        side_effect=subprocess.TimeoutExpired(
            cmd="gh", timeout=GET_REQUIRED_CHECK_NAMES_TIMEOUT
        ),
    ):
        with pytest.raises(RuntimeError) as exc_info:
            get_required_check_names(PR_URL)

    msg = str(exc_info.value)
    assert str(GET_REQUIRED_CHECK_NAMES_TIMEOUT) in msg
    assert PR_URL in msg


def test_get_required_check_names_nonzero_raises_runtime_error():
    with patch("subprocess.run", return_value=_fail("auth error")):
        with pytest.raises(RuntimeError) as exc_info:
            get_required_check_names(PR_URL)
    assert "auth error" in str(exc_info.value)


def test_get_required_check_names_empty_array_returns_empty():
    with patch("subprocess.run", return_value=_ok("[]")):
        result = get_required_check_names(PR_URL)
    assert result == set()


def test_get_required_check_names_no_protection_returns_empty():
    no_protection = _fail("no required checks reported on the 'main' branch")
    no_protection.returncode = 1
    with patch("subprocess.run", return_value=no_protection):
        result = get_required_check_names(PR_URL)
    assert result == set()


def test_get_required_check_names_returns_names():
    data = [{"name": "lint"}, {"name": "tests"}]
    with patch("subprocess.run", return_value=_ok(json.dumps(data))):
        result = get_required_check_names(PR_URL)
    assert result == {"lint", "tests"}


def test_get_pr_ci_status_filters_optional_failing_check():
    """Optional check fails but required check passes: only required check returned."""
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
        side_effect=[
            _ok(json.dumps({"statusCheckRollup": rollup, "reviewDecision": ""})),
            _ok(json.dumps([{"name": "required-check"}])),
        ],
    ):
        result = get_pr_ci_status(PR_URL)

    assert len(result["checks"]) == 1
    assert result["checks"][0]["name"] == "required-check"


def test_get_pr_ci_status_optional_pending_excluded():
    """Required check passes, optional still pending: only required check returned."""
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
            "status": "IN_PROGRESS",
            "conclusion": None,
        },
    ]
    with patch(
        "subprocess.run",
        side_effect=[
            _ok(json.dumps({"statusCheckRollup": rollup, "reviewDecision": ""})),
            _ok(json.dumps([{"name": "required-check"}])),
        ],
    ):
        result = get_pr_ci_status(PR_URL)

    assert len(result["checks"]) == 1
    assert result["checks"][0]["name"] == "required-check"


def test_get_pr_ci_status_required_check_not_started_returns_pending_placeholder():
    """Required checks configured but not yet in statusCheckRollup: synthetic pending entry returned."""
    rollup: list = []
    with patch(
        "subprocess.run",
        side_effect=[
            _ok(json.dumps({"statusCheckRollup": rollup, "reviewDecision": ""})),
            _ok(json.dumps([{"name": "required-check"}])),
        ],
    ):
        result = get_pr_ci_status(PR_URL)

    assert len(result["checks"]) == 1
    assert result["checks"][0]["name"] == "__required_pending__"
    assert result["checks"][0]["status"] == "IN_PROGRESS"


def test_get_pr_ci_status_no_required_checks_returns_empty():
    """No required checks configured: checks list is empty (ci-gate will skip)."""
    rollup = [
        {
            "__typename": "CheckRun",
            "name": "optional-check",
            "status": "COMPLETED",
            "conclusion": "FAILURE",
        },
    ]
    with patch(
        "subprocess.run",
        side_effect=[
            _ok(json.dumps({"statusCheckRollup": rollup, "reviewDecision": ""})),
            _ok("[]"),
        ],
    ):
        result = get_pr_ci_status(PR_URL)

    assert result["checks"] == []
