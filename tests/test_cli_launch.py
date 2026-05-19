"""Tests for gremlins launch --wait."""

from __future__ import annotations

import argparse
from unittest.mock import MagicMock, patch

from gremlins.cli.launch import (
    _self_background_main,  # type: ignore[reportPrivateUsage]
)


def test_wait_blocks_and_returns_exit_code():
    fake_proc = MagicMock()
    fake_proc.wait.return_value = 42
    fake_id = "gr-wait01"
    args = argparse.Namespace(
        client=None,
        description=None,
        parent_id=None,
        base_ref=None,
        gremlin_id=None,
        print_id_only=False,
        print_id=False,
        wait=True,
        pr=None,
    )
    with patch("gremlins.cli.launch.launch", return_value=(fake_id, fake_proc)):
        rc = _self_background_main("some-pipeline", args, {})
    fake_proc.wait.assert_called_once()
    assert rc == 42


def test_no_wait_returns_zero():
    fake_proc = MagicMock()
    fake_id = "gr-nowait1"
    args = argparse.Namespace(
        client=None,
        description=None,
        parent_id=None,
        base_ref=None,
        gremlin_id=None,
        print_id_only=False,
        print_id=False,
        wait=False,
        pr=None,
    )
    with patch("gremlins.cli.launch.launch", return_value=(fake_id, fake_proc)):
        rc = _self_background_main("some-pipeline", args, {})
    fake_proc.wait.assert_not_called()
    assert rc == 0
