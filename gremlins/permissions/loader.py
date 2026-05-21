from __future__ import annotations

import pathlib
import tomllib
from collections.abc import Mapping
from typing import Any

import yaml

from gremlins.permissions.policy import Policy


def load_policy(
    *,
    cli_bypass: bool | None,
    cli_permissions_file: pathlib.Path | None,
    env: Mapping[str, str],
    cwd: pathlib.Path,
) -> Policy:
    bypass, blocks = _resolve(
        cli_bypass=cli_bypass,
        cli_permissions_file=cli_permissions_file,
        env=env,
        cwd=cwd,
    )
    return Policy(bypass=bypass, blocks=blocks)


def _resolve(
    *,
    cli_bypass: bool | None,
    cli_permissions_file: pathlib.Path | None,
    env: Mapping[str, str],
    cwd: pathlib.Path,
) -> tuple[bool, dict[str, dict[str, Any]]]:
    if cli_bypass is not None:
        return cli_bypass, _blocks_from_file(cli_permissions_file)

    env_bypass = env.get("GREMLINS_BYPASS_PERMISSIONS", "")
    if env_bypass:
        return _truthy(env_bypass), {}

    project_file = cwd / ".gremlins" / "permissions.yaml"
    if project_file.exists():
        data: dict[str, Any] = (
            yaml.safe_load(project_file.read_text(encoding="utf-8")) or {}
        )
        blocks: dict[str, dict[str, Any]] = dict(data.get("blocks", {}))
        return bool(data.get("bypass_permissions", False)), blocks

    user_config = pathlib.Path.home() / ".config" / "gremlins" / "config.toml"
    if user_config.exists():
        toml_data: dict[str, Any] = tomllib.loads(
            user_config.read_text(encoding="utf-8")
        )
        return bool(toml_data.get("bypass_permissions", False)), {}

    return False, {}


def _blocks_from_file(path: pathlib.Path | None) -> dict[str, dict[str, Any]]:
    if path is None or not path.exists():
        return {}
    data: dict[str, Any] = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
    return dict(data.get("blocks", {}))


def _truthy(value: str) -> bool:
    return value.strip().lower() in ("1", "true", "yes")
