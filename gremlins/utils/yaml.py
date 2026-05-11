from __future__ import annotations

import pathlib
from typing import Any, cast

import yaml


class YamlLoadError(Exception):
    """Raised when YAML can't be loaded or doesn't parse to a dict."""


def load_yaml_file(path: pathlib.Path) -> dict[str, Any]:
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        raise YamlLoadError(f"file not found: {path}") from None
    except OSError as exc:
        raise YamlLoadError(f"could not read {path}: {exc}") from exc
    return _parse(text, str(path))


def parse_yaml_bytes(data: bytes) -> dict[str, Any]:
    return _parse(data, "<bytes>")


def parse_yaml_text(text: str) -> dict[str, Any]:
    return _parse(text, "<text>")


def dump_yaml_text(data: dict[str, Any]) -> str:
    return yaml.safe_dump(data, default_flow_style=False, sort_keys=False)


def _parse(source: str | bytes, label: str) -> dict[str, Any]:
    try:
        parsed = yaml.safe_load(source)
    except yaml.YAMLError as exc:
        msg = f"YAML parse error in {label}"
        problem = getattr(exc, "problem", None)
        mark = getattr(exc, "problem_mark", None)
        if problem:
            msg += f": {problem}"
        if mark is not None:
            msg += f" (line {mark.line + 1}, column {mark.column + 1})"
        raise YamlLoadError(msg) from exc
    if not isinstance(parsed, dict):
        raise YamlLoadError(
            f"expected a YAML mapping in {label}, got {type(parsed).__name__}"
        )
    return cast(dict[str, Any], parsed)
