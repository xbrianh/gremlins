from __future__ import annotations

import pathlib

import pytest

import gremlins.clients  # noqa: F401 — registers CLIENT_FACTORIES as a side effect
from gremlins.clients.registry import CLIENT_FACTORIES
from gremlins.permissions.loader import load_default_block, load_policy
from gremlins.permissions.policy import Policy
from gremlins.utils.yaml_io import YamlLoadError


def _load(
    *,
    cli_bypass: bool | None = None,
    cli_permissions_file: pathlib.Path | None = None,
    env: dict[str, str] | None = None,
    cwd: pathlib.Path | None = None,
    tmp_path: pathlib.Path,
) -> Policy:
    return load_policy(
        cli_bypass=cli_bypass,
        cli_permissions_file=cli_permissions_file,
        env=env or {},
        cwd=cwd or tmp_path,
    )


def test_cli_bypass_wins_over_env(tmp_path):
    policy = _load(
        cli_bypass=True,
        env={"GREMLINS_BYPASS_PERMISSIONS": "0"},
        tmp_path=tmp_path,
    )
    assert policy.bypass is True


def test_cli_bypass_false_wins_over_env(tmp_path):
    policy = _load(
        cli_bypass=False,
        env={"GREMLINS_BYPASS_PERMISSIONS": "1"},
        tmp_path=tmp_path,
    )
    assert policy.bypass is False


def test_env_wins_over_project_file(tmp_path):
    project_dir = tmp_path / ".gremlins"
    project_dir.mkdir()
    (project_dir / "permissions.yaml").write_text("bypass_permissions: false\n")

    policy = _load(
        env={"GREMLINS_BYPASS_PERMISSIONS": "1"},
        cwd=tmp_path,
        tmp_path=tmp_path,
    )
    assert policy.bypass is True


def test_project_file_wins_over_user_config(tmp_path, monkeypatch):
    project_dir = tmp_path / ".gremlins"
    project_dir.mkdir()
    (project_dir / "permissions.yaml").write_text("bypass_permissions: true\n")

    user_config_dir = tmp_path / "user_config"
    config_dir = user_config_dir / ".config" / "gremlins"
    config_dir.mkdir(parents=True)
    (config_dir / "config.toml").write_text("bypass_permissions = false\n")
    monkeypatch.setattr(pathlib.Path, "home", lambda *_: user_config_dir)

    policy = _load(cwd=tmp_path, tmp_path=tmp_path)
    assert policy.bypass is True


def test_user_config_honored(tmp_path, monkeypatch):
    user_config_dir = tmp_path / "user_config"
    config_dir = user_config_dir / ".config" / "gremlins"
    config_dir.mkdir(parents=True)
    (config_dir / "config.toml").write_text("bypass_permissions = true\n")
    monkeypatch.setattr(pathlib.Path, "home", lambda *_: user_config_dir)

    policy = _load(cwd=tmp_path, tmp_path=tmp_path)
    assert policy.bypass is True


def test_empty_inputs_give_default_policy(tmp_path):
    policy = _load(tmp_path=tmp_path)
    assert policy.bypass is False
    assert policy.blocks == {}
    for provider in CLIENT_FACTORIES:
        assert load_default_block(provider), (
            f"expected non-empty default block for {provider}"
        )


def test_missing_env_var_gives_default(tmp_path):
    policy = _load(env={}, tmp_path=tmp_path)
    assert policy.bypass is False


def test_block_for_returns_empty_dict_when_no_blocks():
    policy = Policy()
    assert policy.block_for("claude") == {}


def test_block_for_returns_block_when_present():
    policy = Policy(bypass=False, blocks={"claude": {"deny": ["bash"]}})
    assert policy.block_for("claude") == {"deny": ["bash"]}


def test_block_for_missing_provider_returns_empty():
    policy = Policy(bypass=False, blocks={"claude": {"deny": []}})
    assert policy.block_for("openai") == {}


def test_cli_permissions_file_loads_blocks(tmp_path):
    perm_file = tmp_path / "perms.yaml"
    perm_file.write_text("blocks:\n  claude:\n    deny:\n      - bash\n")
    policy = _load(
        cli_bypass=True,
        cli_permissions_file=perm_file,
        tmp_path=tmp_path,
    )
    assert policy.block_for("claude") == {"deny": ["bash"]}


def test_env_bypass_truthy_values(tmp_path):
    for val in ("1", "true", "yes", "True", "YES"):
        policy = _load(env={"GREMLINS_BYPASS_PERMISSIONS": val}, tmp_path=tmp_path)
        assert policy.bypass is True, f"expected bypass for {val!r}"


def test_env_bypass_falsy_values(tmp_path):
    for val in ("0", "false", "no"):
        policy = _load(env={"GREMLINS_BYPASS_PERMISSIONS": val}, tmp_path=tmp_path)
        assert policy.bypass is False, f"expected no bypass for {val!r}"


def test_project_override_stored_without_defaults(tmp_path):
    project_dir = tmp_path / ".gremlins"
    project_dir.mkdir()
    (project_dir / "permissions.yaml").write_text(
        "blocks:\n  claude:\n    allowed_tools: [Read]\n"
    )
    policy = _load(cwd=tmp_path, tmp_path=tmp_path)
    assert policy.block_for("claude") == {"allowed_tools": ["Read"]}
    for provider in CLIENT_FACTORIES:
        if provider != "claude":
            assert policy.block_for(provider) == {}, (
                f"unexpected override for {provider}"
            )


def test_corrupt_default_file_raises(tmp_path, monkeypatch):
    monkeypatch.setattr("gremlins.permissions.loader._DEFAULTS_DIR", tmp_path)
    (tmp_path / "claude.yaml").write_text(": bad: yaml: [\n")
    with pytest.raises(YamlLoadError):
        load_default_block("claude")
