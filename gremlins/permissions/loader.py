from __future__ import annotations

import pathlib
import tomllib
from collections.abc import Mapping
from typing import Any

from gremlins.permissions.policy import Policy
from gremlins.utils.yaml_io import load_yaml_file

_DEFAULTS_DIR = pathlib.Path(__file__).parent / "defaults"


def load_policy(
    *,
    cli_bypass: bool | None,
    cli_permissions_file: pathlib.Path | None,
    env: Mapping[str, str],
    cwd: pathlib.Path,
) -> Policy:
    bypass = _resolve_bypass(cli_bypass=cli_bypass, env=env, cwd=cwd)
    blocks = (
        _blocks_from_file(cli_permissions_file)
        if cli_permissions_file is not None
        else _blocks_from_project(cwd)
    )
    return Policy(bypass=bypass, blocks=blocks)


def has_default_block(provider: str) -> bool:
    return (_DEFAULTS_DIR / f"{provider}.yaml").exists()


def load_default_block(provider: str) -> dict[str, Any]:
    return load_yaml_file(_DEFAULTS_DIR / f"{provider}.yaml")


def _resolve_bypass(
    *,
    cli_bypass: bool | None,
    env: Mapping[str, str],
    cwd: pathlib.Path,
) -> bool:
    if cli_bypass is not None:
        return cli_bypass

    env_bypass = env.get("GREMLINS_BYPASS_PERMISSIONS", "")
    if env_bypass:
        return _truthy(env_bypass)

    project_file = cwd / ".gremlins" / "permissions.yaml"
    if project_file.exists():
        data = load_yaml_file(project_file)
        return bool(data.get("bypass_permissions", False))

    user_config = pathlib.Path.home() / ".config" / "gremlins" / "config.toml"
    if user_config.exists():
        toml_data: dict[str, Any] = tomllib.loads(
            user_config.read_text(encoding="utf-8")
        )
        return bool(toml_data.get("bypass_permissions", False))

    return False


def _blocks_from_project(cwd: pathlib.Path) -> dict[str, dict[str, Any]]:
    project_file = cwd / ".gremlins" / "permissions.yaml"
    if not project_file.exists():
        return {}
    data = load_yaml_file(project_file)
    return dict(data.get("blocks", {}))


def _blocks_from_file(path: pathlib.Path | None) -> dict[str, dict[str, Any]]:
    if path is None or not path.exists():
        return {}
    data = load_yaml_file(path)
    return dict(data.get("blocks", {}))


def _truthy(value: str) -> bool:
    return value.strip().lower() in ("1", "true", "yes")
