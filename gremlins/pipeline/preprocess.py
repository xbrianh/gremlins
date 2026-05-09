from __future__ import annotations

import pathlib
from typing import Any, cast

import yaml

from gremlins.pipeline.discovery import resolve_pipeline_name
from gremlins.pipeline.schema import BUNDLED_PROMPT_PREFIX
from gremlins.prompts import BUNDLED_PROMPT_DIR


def expand_pipeline(
    yaml_path: pathlib.Path, project_root: pathlib.Path | None = None
) -> dict[str, Any]:
    """Read YAML, resolve all include: and prompt: references, return self-contained dict."""
    if project_root is None:
        d = yaml_path.parent
        project_root = d.parent if d.name == ".gremlins" else d
    return _expand(yaml_path, project_root, chain=[])


def _expand(
    yaml_path: pathlib.Path,
    project_root: pathlib.Path,
    chain: list[pathlib.Path],
) -> dict[str, Any]:
    resolved = yaml_path.resolve()
    if resolved in chain:
        cycle = " -> ".join(str(p) for p in chain + [resolved])
        raise ValueError(f"include cycle detected: {cycle}")

    raw_text = yaml_path.read_text(encoding="utf-8")
    parsed = yaml.safe_load(raw_text)
    if not isinstance(parsed, dict):
        raise ValueError(f"pipeline file must be a YAML mapping: {yaml_path}")

    raw = cast(dict[str, Any], parsed)
    yaml_dir = yaml_path.parent
    prompt_dir = _resolve_prompt_dir(raw.get("prompt_dir"), yaml_dir)
    new_chain = chain + [resolved]

    named_prompts: dict[str, list[str]] = {}
    for name, value in cast(dict[str, Any], raw.get("prompts") or {}).items():
        # Named prompts can't reference each other — pass empty dict to prevent cycles.
        named_prompts[name] = _read_prompts(value, prompt_dir, {})

    expanded_stages: list[dict[str, Any]] = []
    for entry in cast(list[dict[str, Any]], raw.get("stages") or []):
        expanded_stages.extend(
            _expand_entry(entry, prompt_dir, project_root, new_chain, named_prompts)
        )

    result: dict[str, Any] = {
        k: v for k, v in raw.items() if k not in ("stages", "prompt_dir", "prompts")
    }
    result["stages"] = expanded_stages
    return result


def _expand_entry(
    entry: dict[str, Any],
    prompt_dir: pathlib.Path,
    project_root: pathlib.Path,
    chain: list[pathlib.Path],
    named_prompts: dict[str, list[str]],
) -> list[dict[str, Any]]:
    if "include" in entry and len(entry) == 1:
        name = entry["include"]
        if not isinstance(name, str) or not name:
            raise ValueError("include: value must be a non-empty string")
        included_path = resolve_pipeline_name(name, project_root)
        included = _expand(included_path, project_root, chain)
        return cast(list[dict[str, Any]], included.get("stages") or [])

    entry = dict(entry)

    if "prompt" in entry:
        entry["prompt"] = _read_prompts(entry["prompt"], prompt_dir, named_prompts)

    if "parallel" in entry and isinstance(entry["parallel"], list):
        expanded_parallel: list[dict[str, Any]] = []
        for child in cast(list[Any], entry["parallel"]):
            child_dict = cast(dict[str, Any], child)
            include_name = child_dict.get("include") if len(child_dict) == 1 else None
            expanded = _expand_entry(
                child_dict,
                prompt_dir,
                project_root,
                chain,
                named_prompts,
            )
            if len(expanded) == 0:
                raise ValueError(
                    "parallel child expanded to 0 stages via include; "
                    "includes inside parallel groups must resolve to at least one stage"
                )
            if len(expanded) == 1:
                expanded_parallel.append(expanded[0])
            else:
                name = include_name or f"sequence-{len(expanded_parallel)}"
                expanded_parallel.append(
                    {"name": name, "type": "sequence", "body": expanded}
                )
        entry["parallel"] = expanded_parallel

    if "body" in entry and isinstance(entry["body"], list):
        expanded_body: list[dict[str, Any]] = []
        for body_entry in cast(list[dict[str, Any]], entry["body"]):
            expanded_body.extend(
                _expand_entry(
                    body_entry, prompt_dir, project_root, chain, named_prompts
                )
            )
        entry["body"] = expanded_body

    return [entry]


def _resolve_prompt_dir(value: object, yaml_dir: pathlib.Path) -> pathlib.Path:
    if value is None:
        return yaml_dir
    if not isinstance(value, str):
        raise ValueError(f"prompt_dir must be a string, got {type(value)!r}")
    return (yaml_dir / value).resolve()


def _read_prompt_file(path: pathlib.Path) -> str:
    if not path.exists():
        raise FileNotFoundError(f"prompt file not found: {path}")
    text = path.read_text(encoding="utf-8")
    if not text.strip():
        raise ValueError(f"prompt file is empty: {path}")
    return text


def _read_prompts(
    prompt_field: object,
    prompt_dir: pathlib.Path,
    named_prompts: dict[str, list[str]],
) -> list[str]:
    if isinstance(prompt_field, str):
        raw: list[str] = [prompt_field]
    elif isinstance(prompt_field, list):
        raw = [str(item) for item in cast(list[Any], prompt_field)]
    else:
        raise ValueError(f"prompt must be a string or list, got {type(prompt_field)!r}")

    texts: list[str] = []
    for p in raw:
        if p in named_prompts:
            texts.extend(named_prompts[p])
        elif p.startswith(BUNDLED_PROMPT_PREFIX):
            name = p[len(BUNDLED_PROMPT_PREFIX) :]
            if not name:
                raise ValueError(
                    f"prompt {p!r} is missing a name after {BUNDLED_PROMPT_PREFIX!r}"
                )
            texts.append(_read_prompt_file((BUNDLED_PROMPT_DIR / name).resolve()))
        else:
            path = (prompt_dir / p).resolve()
            if not path.exists() and named_prompts:
                raise FileNotFoundError(
                    f"prompt {p!r} not found as a named entry or file under {prompt_dir}"
                )
            texts.append(_read_prompt_file(path))

    return texts
