"""Tests for gremlins launch --wait."""

from __future__ import annotations

import argparse
import sys
from unittest.mock import MagicMock, patch

from gremlins.cli.launch import (
    _self_background_main,  # type: ignore[reportPrivateUsage]
    build_launch_parser,  # type: ignore[reportPrivateUsage]
)
from gremlins.pipeline import Pipeline
from gremlins.stages.exec import Exec


def _pipeline_with_inputs(in_map: dict[str, str] | None) -> Pipeline:
    inputs_stage = None
    if in_map is not None:
        inputs_stage = Exec("inputs", {}, in_map=in_map)
    p = MagicMock(spec=Pipeline)
    p.inputs = inputs_stage
    return p


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
        bypass=False,
        permissions_file=None,
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
    """PR passed via --pr lands in stage_inputs, which launch reads from."""
    fake_proc = MagicMock()
    fake_proc.poll.return_value = None
    fake_id = "gr-prtest1"
    parser = build_launch_parser("some-pipeline", _pipeline_with_inputs({"PR": "pr?"}))
    args = parser.parse_args(["--pr", "697"])
    stage_inputs = {"pr": args.pr}
    with (
        patch(
            "gremlins.cli.launch.launch", return_value=(fake_id, fake_proc)
        ) as mock_launch,
        patch("gremlins.cli.launch.time.sleep"),
        patch("gremlins.cli.launch.time.time", side_effect=[0, 100]),
    ):
        _self_background_main("some-pipeline", args, stage_inputs)
    mock_launch.assert_called_once()
    assert mock_launch.call_args.kwargs.get("stage_inputs", {}).get("pr") == "697"
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
        bypass=False,
        permissions_file=None,
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
        bypass=False,
        permissions_file=None,
    )
    with patch("gremlins.cli.launch.launch", return_value=(fake_id, fake_proc)):
        rc = _self_background_main("some-pipeline", args, {})
    assert rc == 2
    assert "exited early with code 2" in capsys.readouterr().err


def test_self_background_main_populates_registry_before_validation(monkeypatch):
    # conftest imports FakeClaudeClient which causes gremlins.clients.__init__ to
    # run and populate CLIENT_FACTORIES. Simulate the cold-import scenario by
    # clearing the dict and evicting the package so _self_background_main must
    # trigger registration itself.
    from gremlins.clients.registry import CLIENT_FACTORIES

    saved = dict(CLIENT_FACTORIES)
    CLIENT_FACTORIES.clear()
    monkeypatch.delitem(sys.modules, "gremlins.clients", raising=False)

    fake_proc = MagicMock()
    fake_proc.poll.return_value = None
    args = argparse.Namespace(
        client=None,
        description=None,
        parent_id=None,
        base_ref=None,
        gremlin_id=None,
        print_id_only=False,
        print_id=False,
        wait=False,
        bypass=False,
        permissions_file=None,
    )
    try:
        with (
            patch("gremlins.cli.launch.launch", return_value=("gr-reg01", fake_proc)),
            patch("gremlins.cli.launch.time.sleep"),
            patch("gremlins.cli.launch.time.time", side_effect=[0, 100]),
        ):
            rc = _self_background_main("some-pipeline", args, {})
    finally:
        CLIENT_FACTORIES.clear()
        CLIENT_FACTORIES.update(saved)

    assert rc == 0
