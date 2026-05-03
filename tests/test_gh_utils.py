"""Tests for gremlins.gh_utils."""

import subprocess
from unittest.mock import patch

import pytest

from gremlins.gh_utils import GET_PR_CI_STATUS_TIMEOUT, get_pr_ci_status

PR_URL = "https://github.com/owner/repo/pull/42"


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
