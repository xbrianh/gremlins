from __future__ import annotations

import pathlib
from typing import Any, cast

import yaml

from gremlins.prompts import BUNDLED_PROMPT_DIR


class YamlLoadError(Exception):
    """Raised when YAML can't be loaded or doesn't parse to a dict."""


class PromptLoadError(Exception):
    """Raised when a bundled prompt can't be loaded or rendered."""


def load_yaml_file(path: pathlib.Path) -> dict[str, Any]:
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        raise YamlLoadError(f"file not found: {path}") from None
    except (OSError, UnicodeDecodeError) as exc:
        raise YamlLoadError(f"could not read {path}: {exc}") from exc
    return _parse(text, str(path))


def load_bundled_prompt(name: str) -> str:
    path = BUNDLED_PROMPT_DIR / name
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        raise PromptLoadError(f"bundled prompt not found: {name}") from None
    except (OSError, UnicodeDecodeError) as exc:
        raise PromptLoadError(f"could not read bundled prompt {name}: {exc}") from exc
    if not text.strip():
        raise PromptLoadError(f"bundled prompt is empty: {name}")
    return text


def render_bundled_prompt(name: str, **kwargs: Any) -> str:
    text = load_bundled_prompt(name)
    try:
        return text.format(**kwargs)
    except (KeyError, ValueError) as exc:
        raise PromptLoadError(
            f"render failed for bundled prompt {name}: {exc}"
        ) from exc


def dump_yaml_text(data: dict[str, Any]) -> str:
    return yaml.safe_dump(data, default_flow_style=False, sort_keys=False)


def _parse(source: str | bytes, label: str) -> dict[str, Any]:
    try:
        parsed = yaml.safe_load(source)
    except yaml.YAMLError as exc:
        msg = f"YAML parse error in {label}"
        problem = getattr(exc, "problem", None) or " ".join(str(exc).split())
        mark = getattr(exc, "problem_mark", None)
        msg += f": {problem}"
        if mark is not None:
            msg += f" (line {mark.line + 1}, column {mark.column + 1})"
        raise YamlLoadError(msg) from exc
    if not isinstance(parsed, dict):
        raise YamlLoadError(
            f"expected a YAML mapping in {label}, got {type(parsed).__name__}"
        )
    return cast(dict[str, Any], parsed)
