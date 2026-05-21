from __future__ import annotations

import pathlib
from unittest.mock import MagicMock, patch

from gremlins.cli.launch import _self_background_main, build_launch_parser
from gremlins.permissions.policy import Policy
from gremlins.stages.base import Stage


def _fake_proc(poll_return=None):
    proc = MagicMock()
    proc.poll.return_value = poll_return
    return proc


def _args(**overrides):
    defaults = dict(
        client=None,
        description=None,
        parent_id=None,
        base_ref=None,
        gremlin_id=None,
        print_id_only=False,
        print_id=False,
        wait=False,
        pr=None,
        bypass=False,
        permissions_file=None,
    )
    defaults.update(overrides)
    import argparse

    return argparse.Namespace(**defaults)


def _run(args, *, fake_id="gr-perm01"):
    proc = _fake_proc(poll_return=None)
    with (
        patch("gremlins.cli.launch.launch", return_value=(fake_id, proc)),
        patch("gremlins.cli.launch.time.sleep"),
        patch("gremlins.cli.launch.time.time", side_effect=[0, 100]),
    ):
        rc = _self_background_main("some-pipeline", args, {})
    return rc, proc


# --- flag parsing ---


def test_bypass_flag_parsed_by_build_launch_parser():
    parser = build_launch_parser("some-pipeline", Stage)
    args = parser.parse_args(["--bypass"])
    assert args.bypass is True


def test_permissions_file_flag_parsed_by_build_launch_parser(tmp_path):
    perm_file = tmp_path / "perms.yaml"
    perm_file.write_text("blocks: {}\n")
    parser = build_launch_parser("some-pipeline", Stage)
    args = parser.parse_args(["--permissions-file", str(perm_file)])
    assert args.permissions_file == perm_file


# --- load_policy wiring ---


def test_bypass_flag_passes_cli_bypass_true(capsys):
    args = _args(bypass=True)
    with patch(
        "gremlins.cli.launch.load_policy", return_value=Policy(bypass=True)
    ) as mock_lp:
        _run(args)
    assert mock_lp.call_args.kwargs["cli_bypass"] is True


def test_no_bypass_flag_passes_cli_bypass_none(capsys):
    args = _args(bypass=False)
    with patch("gremlins.cli.launch.load_policy", return_value=Policy()) as mock_lp:
        _run(args)
    assert mock_lp.call_args.kwargs["cli_bypass"] is None


def test_permissions_file_flag_passes_path(tmp_path):
    perm_file = tmp_path / "p.yaml"
    perm_file.write_text("blocks: {}\n")
    args = _args(permissions_file=perm_file)
    with patch("gremlins.cli.launch.load_policy", return_value=Policy()) as mock_lp:
        _run(args)
    assert mock_lp.call_args.kwargs["cli_permissions_file"] == perm_file


# --- effective policy via real loader ---


def test_bypass_flag_yields_bypass_policy(capsys):
    args = _args(bypass=True)
    rc, _ = _run(args)
    assert rc == 0
    err = capsys.readouterr().err
    assert "permissions: bypass" in err


def test_env_var_bypass_yields_bypass_policy(monkeypatch, capsys):
    monkeypatch.setenv("GREMLINS_BYPASS_PERMISSIONS", "1")
    args = _args()
    rc, _ = _run(args)
    assert rc == 0
    assert "permissions: bypass" in capsys.readouterr().err


def test_project_file_bypass_yields_bypass_policy(monkeypatch, tmp_path, capsys):
    gremlins_dir = tmp_path / ".gremlins"
    gremlins_dir.mkdir()
    (gremlins_dir / "permissions.yaml").write_text("bypass_permissions: true\n")
    monkeypatch.chdir(tmp_path)
    args = _args()
    rc, _ = _run(args)
    assert rc == 0
    assert "permissions: bypass" in capsys.readouterr().err


def test_user_config_bypass_yields_bypass_policy(monkeypatch, tmp_path, capsys):
    config_dir = tmp_path / ".config" / "gremlins"
    config_dir.mkdir(parents=True)
    (config_dir / "config.toml").write_text("bypass_permissions = true\n")
    monkeypatch.setattr(pathlib.Path, "home", lambda *_: tmp_path)
    args = _args()
    rc, _ = _run(args)
    assert rc == 0
    assert "permissions: bypass" in capsys.readouterr().err


def test_cli_bypass_overrides_env_false(monkeypatch, capsys):
    monkeypatch.setenv("GREMLINS_BYPASS_PERMISSIONS", "0")
    args = _args(bypass=True)
    rc, _ = _run(args)
    assert rc == 0
    assert "permissions: bypass" in capsys.readouterr().err


# --- banner line ---


def test_default_mode_banner_line(capsys):
    args = _args()
    rc, _ = _run(args)
    assert rc == 0
    err = capsys.readouterr().err
    assert "permissions: default (allowlist)" in err


def test_bypass_mode_banner_line(capsys):
    args = _args(bypass=True)
    _run(args)
    err = capsys.readouterr().err
    assert "permissions: bypass" in err


def test_exactly_one_permissions_line_default(capsys):
    args = _args()
    _run(args)
    lines = [
        ln
        for ln in capsys.readouterr().err.splitlines()
        if ln.startswith("permissions:")
    ]
    assert len(lines) == 1


def test_exactly_one_permissions_line_bypass(capsys):
    args = _args(bypass=True)
    _run(args)
    lines = [
        ln
        for ln in capsys.readouterr().err.splitlines()
        if ln.startswith("permissions:")
    ]
    assert len(lines) == 1
