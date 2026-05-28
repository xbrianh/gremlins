from __future__ import annotations

import json
import pathlib
from unittest.mock import MagicMock, patch

import pytest

from gremlins.cli.launch import _self_background_main, build_launch_parser
from gremlins.executor.state import StateData
from gremlins.permissions.policy import Policy


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
    parser = build_launch_parser("some-pipeline")
    args = parser.parse_args(["--bypass"])
    assert args.bypass is True


def test_permissions_file_flag_parsed_by_build_launch_parser(tmp_path):
    perm_file = tmp_path / "perms.yaml"
    perm_file.write_text("blocks: {}\n")
    parser = build_launch_parser("some-pipeline")
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


def test_user_config_bypass_yields_bypass_policy(sandbox, capsys):
    (sandbox.config / "config.toml").write_text("bypass_permissions = true\n")
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


# --- StateData round-trip ---


def test_statedata_bypass_roundtrip(tmp_path):
    sd = StateData(gremlin_id="g1", bypass=True, permissions_file="/tmp/p.yaml")
    sd.persist(tmp_path)
    raw = json.loads((tmp_path / "state.json").read_text())
    assert raw["bypass"] is True
    assert raw["permissions_file"] == "/tmp/p.yaml"


def test_statedata_bypass_load(tmp_path, monkeypatch):
    state_file = tmp_path / "state.json"
    state_file.write_text(json.dumps({"bypass": True, "permissions_file": "/p.yaml"}))
    monkeypatch.setattr(
        "gremlins.executor.state.resolve_state_file", lambda _: state_file
    )
    sd = StateData.load(None)
    assert sd.bypass is True
    assert sd.permissions_file == "/p.yaml"


# --- launch() persists bypass + permissions_file into state.json ---


def test_launch_persists_bypass(tmp_path, monkeypatch):
    import gremlins.launcher as launcher_mod

    monkeypatch.setattr(launcher_mod, "_state_root", lambda: tmp_path)
    with (
        patch.object(launcher_mod, "_resolve_inputs") as mock_ri,
        patch.object(launcher_mod, "_prepare_state_dir"),
        patch.object(
            launcher_mod, "_persist_expanded_pipeline", return_value="pipe.yaml"
        ),
        patch.object(launcher_mod, "_spawn") as mock_spawn,
    ):
        gid = "gr-bypass-test"
        inputs = MagicMock()
        inputs.gremlin_id = gid
        inputs.pipeline_path = "pipe.yaml"
        inputs.pr_artifact = None
        mock_ri.return_value = inputs
        proc = MagicMock()
        proc.pid = 99
        mock_spawn.return_value = proc

        state_dir = tmp_path / gid
        state_dir.mkdir(parents=True)

        sd = StateData(gremlin_id=gid)

        with patch.object(launcher_mod, "_initial_state_data", return_value=sd):
            launcher_mod.launch("some-kind", bypass=True, permissions_file="/p.yaml")

    raw = json.loads((state_dir / "state.json").read_text())
    assert raw["bypass"] is True
    assert raw["permissions_file"] == "/p.yaml"


# --- child _build_state reconstructs policy from StateData ---

_CHILD_MODULES = [
    ("gremlins.spawn.child", "gremlins.spawn.child"),
    ("gremlins.run_child", "gremlins.run_child"),
]


def _make_child_spec(mod_path: str, tmp_path: pathlib.Path) -> dict:
    """Build a minimal spec dict appropriate for the given child module."""
    session_dir = tmp_path / "session"
    session_dir.mkdir(exist_ok=True)
    if mod_path == "gremlins.spawn.child":
        # New schema: child_id required; StateData.load and paths.state_root are mocked.
        return {
            "client": "claude:claude-haiku-4-5-20251001",
            "child_id": "test-child-id",
        }
    # Legacy gremlins.run_child schema
    return {
        "client": "claude:claude-haiku-4-5-20251001",
        "session_dir": str(session_dir),
    }


def _child_module_patches(mod_path: str, tmp_path: pathlib.Path) -> list:
    """Return extra patches needed for the given child module."""
    if mod_path == "gremlins.spawn.child":
        # Patch paths.state_root so session_dir mkdir goes under tmp_path.
        import gremlins.spawn.child as _cm

        return [patch.object(_cm, "paths", **{"state_root.return_value": tmp_path})]
    return []


@pytest.mark.parametrize("mod_path,_label", _CHILD_MODULES)
def test_child_build_state_bypass_policy(mod_path, _label, tmp_path):
    import importlib
    from contextlib import ExitStack

    mod = importlib.import_module(mod_path)

    fake_data = StateData(
        gremlin_id=None,
        bypass=True,
        permissions_file="",
        project_root=str(tmp_path),
    )
    spec = _make_child_spec(mod_path, tmp_path)

    captured_policy: list[Policy] = []

    def fake_client_parse(label, policy=None):
        captured_policy.append(policy)
        return MagicMock()

    with ExitStack() as stack:
        stack.enter_context(patch(f"{mod_path}.StateData.load", return_value=fake_data))
        stack.enter_context(
            patch(f"{mod_path}.Client.parse", side_effect=fake_client_parse)
        )
        stack.enter_context(patch(f"{mod_path}.validate_policy_against_registry"))
        for p in _child_module_patches(mod_path, tmp_path):
            stack.enter_context(p)
        mod._build_state(spec)

    assert len(captured_policy) == 1
    assert captured_policy[0] is not None
    assert captured_policy[0].bypass is True


@pytest.mark.parametrize("mod_path,_label", _CHILD_MODULES)
def test_child_build_state_project_permissions_blocks(mod_path, _label, tmp_path):
    import importlib
    from contextlib import ExitStack

    mod = importlib.import_module(mod_path)

    gremlins_dir = tmp_path / ".gremlins"
    gremlins_dir.mkdir()
    (gremlins_dir / "permissions.yaml").write_text(
        "blocks:\n  claude:\n    allow_edits: true\n"
    )

    fake_data = StateData(
        gremlin_id=None,
        bypass=False,
        permissions_file="",
        project_root=str(tmp_path),
    )
    spec = _make_child_spec(mod_path, tmp_path)

    captured_policy: list[Policy] = []

    def fake_client_parse(label, policy=None):
        captured_policy.append(policy)
        return MagicMock()

    with ExitStack() as stack:
        stack.enter_context(patch(f"{mod_path}.StateData.load", return_value=fake_data))
        stack.enter_context(
            patch(f"{mod_path}.Client.parse", side_effect=fake_client_parse)
        )
        stack.enter_context(patch(f"{mod_path}.validate_policy_against_registry"))
        for p in _child_module_patches(mod_path, tmp_path):
            stack.enter_context(p)
        mod._build_state(spec)

    assert len(captured_policy) == 1
    policy = captured_policy[0]
    assert policy is not None
    assert "claude" in policy.blocks
