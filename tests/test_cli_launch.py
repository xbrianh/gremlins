"""Tests for gremlins launch --wait."""

from __future__ import annotations

import argparse
from unittest.mock import MagicMock, patch

from gremlins.cli.launch import (
    _self_background_main,  # type: ignore[reportPrivateUsage]
    build_launch_parser,  # type: ignore[reportPrivateUsage]
)
from gremlins.stages.base import Stage


def test_wait_blocks_and_returns_exit_code():
    fake_proc = MagicMock()
    fake_proc.poll.return_value = None
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
    with (
        patch("gremlins.cli.launch.launch", return_value=(fake_id, fake_proc)),
        patch("gremlins.cli.launch.time.sleep"),
        patch("gremlins.cli.launch.time.time", side_effect=[0, 100]),
    ):
        rc = _self_background_main("some-pipeline", args, {})
    fake_proc.wait.assert_called_once()
    assert rc == 42


def test_pr_flag_forwarded_to_launch():
    fake_proc = MagicMock()
    fake_proc.poll.return_value = None
    fake_id = "gr-prtest1"
    parser = build_launch_parser("some-pipeline", Stage)
    args = parser.parse_args(["--pr", "697"])
    with (
        patch(
            "gremlins.cli.launch.launch", return_value=(fake_id, fake_proc)
        ) as mock_launch,
        patch("gremlins.cli.launch.time.sleep"),
        patch("gremlins.cli.launch.time.time", side_effect=[0, 100]),
    ):
        _self_background_main("some-pipeline", args, {})
    mock_launch.assert_called_once()
    assert mock_launch.call_args.kwargs.get("pr") == "697"
    assert mock_launch.call_args.kwargs.get("base_ref") is None


def test_no_wait_returns_zero():
    fake_proc = MagicMock()
    fake_proc.poll.return_value = None
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
    with (
        patch("gremlins.cli.launch.launch", return_value=(fake_id, fake_proc)),
        patch("gremlins.cli.launch.time.sleep"),
        patch("gremlins.cli.launch.time.time", side_effect=[0, 100]),
    ):
        rc = _self_background_main("some-pipeline", args, {})
    fake_proc.wait.assert_not_called()
    assert rc == 0


def test_early_death_returns_exit_code(capsys):
    fake_proc = MagicMock()
    fake_proc.poll.return_value = 2
    fake_id = "gr-dead02"
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
    assert rc == 2
    assert "exited early with code 2" in capsys.readouterr().err
