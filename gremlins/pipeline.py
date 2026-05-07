"""YAML pipeline loader."""

from __future__ import annotations

import dataclasses
import importlib
import pathlib
from typing import Any, cast

import yaml

from gremlins.clients import ClientSpec
from gremlins.prompts import BUNDLED_PROMPT_DIR
from gremlins.stages.registry import STAGE_REGISTRY


def _ensure_registered() -> None:
    importlib.import_module("gremlins.stages.all")
    importlib.import_module("gremlins.clients")


@dataclasses.dataclass
class StageEntry:
    name: str
    type: str
    client: ClientSpec | None
    prompt_paths: list[pathlib.Path]
    options: dict[str, Any]
    children: list[StageEntry] = dataclasses.field(  # pyright: ignore[reportUnknownVariableType]
        default_factory=list
    )
    max_concurrent: int | None = None
    cancel_on_bail: bool = False
    bail_policy: str = "any"


@dataclasses.dataclass
class Pipeline:
    name: str
    path: pathlib.Path
    stages: list[StageEntry]
    default_client: ClientSpec | None = None


BUNDLED_PROMPT_PREFIX = "gremlins:"


def _resolve_prompt_dir(value: object, yaml_dir: pathlib.Path) -> pathlib.Path:
    """Pipeline-level `prompt_dir:` (relative to YAML); default = YAML dir."""
    if value is None:
        return yaml_dir
    if not isinstance(value, str):
        raise ValueError(f"prompt_dir must be a string, got {type(value)!r}")
    return (yaml_dir / value).resolve()


def _resolve_prompt_paths(
    prompt_field: object, prompt_dir: pathlib.Path
) -> list[pathlib.Path]:
    """Resolve prompt names. `gremlins:NAME` -> bundled; bare NAME -> prompt_dir."""
    if prompt_field is None:
        return []
    if isinstance(prompt_field, str):
        raw: list[str] = [prompt_field]
    elif isinstance(prompt_field, list):
        raw = [str(item) for item in cast(list[Any], prompt_field)]
    else:
        raise ValueError(f"prompt must be a string or list, got {type(prompt_field)!r}")
    resolved: list[pathlib.Path] = []
    for p in raw:
        if p.startswith(BUNDLED_PROMPT_PREFIX):
            base = BUNDLED_PROMPT_DIR
            name = p[len(BUNDLED_PROMPT_PREFIX) :]
        else:
            base = prompt_dir
            name = p
        path = (base / name).resolve()
        if not path.exists():
            raise FileNotFoundError(f"prompt file not found: {path}")
        resolved.append(path)
    return resolved


def _parse_stage_entry(
    entry: dict[str, Any],
    prompt_dir: pathlib.Path,
    depth: int = 0,
) -> StageEntry:
    if "parallel" in entry:
        if depth > 0:
            raise ValueError(
                f"nested parallel groups are not allowed "
                f"(stage {entry.get('name', '?')!r})"
            )
        name = entry.get("name")
        if not isinstance(name, str) or not name:
            raise ValueError("parallel group must have a 'name' field")
        children_raw_untyped = entry["parallel"]
        if not isinstance(children_raw_untyped, list):
            raise ValueError(f"parallel group {name!r}: 'parallel' must be a list")
        children_raw = cast(list[dict[str, Any]], children_raw_untyped)
        seen: set[str] = set()
        children: list[StageEntry] = []
        for child_raw in children_raw:
            child = _parse_stage_entry(child_raw, prompt_dir, depth=depth + 1)
            if child.name in seen:
                raise ValueError(
                    f"parallel group {name!r}: duplicate child name {child.name!r}"
                )
            seen.add(child.name)
            children.append(child)
        max_concurrent = entry.get("max_concurrent")
        if max_concurrent is not None:
            if not isinstance(max_concurrent, int) or max_concurrent <= 0:
                raise ValueError(
                    f"parallel group {name!r}: 'max_concurrent' must be a positive integer"
                )
        raw_cancel_on_bail = entry.get("cancel_on_bail", False)
        if not isinstance(raw_cancel_on_bail, bool):
            raise ValueError(
                f"parallel group {name!r}: 'cancel_on_bail' must be a boolean"
            )
        cancel_on_bail = raw_cancel_on_bail
        bail_policy = str(entry.get("bail_policy") or "any")
        if bail_policy not in ("any", "all"):
            raise ValueError(
                f"parallel group {name!r}: 'bail_policy' must be 'any' or 'all'"
            )
        return StageEntry(
            name=name,
            type="parallel",
            client=None,
            prompt_paths=[],
            options={},
            children=children,
            max_concurrent=max_concurrent,
            cancel_on_bail=cancel_on_bail,
            bail_policy=bail_policy,
        )

    name = entry.get("name")
    if not isinstance(name, str) or not name:
        raise ValueError("stage entry must have a 'name' field")
    if "max_concurrent" in entry:
        raise ValueError(
            f"stage {name!r}: 'max_concurrent' is only valid on parallel groups"
        )
    stage_type = entry.get("type")
    if not isinstance(stage_type, str) or not stage_type:
        raise ValueError(f"stage {name!r}: must have a 'type' field")
    if stage_type not in STAGE_REGISTRY:
        raise ValueError(f"stage {name!r}: unknown type {stage_type!r}")

    client_spec_raw = entry.get("client")
    stage_client: ClientSpec | None = None
    if client_spec_raw is not None:
        if not isinstance(client_spec_raw, str):
            raise ValueError(
                f"stage {name!r}: 'client' must be a string, got {type(client_spec_raw)!r}"
            )
        stage_client = ClientSpec.parse(client_spec_raw)

    prompt_paths = _resolve_prompt_paths(entry.get("prompt"), prompt_dir)
    options = dict(cast(dict[str, Any], entry.get("options") or {}))
    return StageEntry(
        name=name,
        type=stage_type,
        client=stage_client,
        prompt_paths=prompt_paths,
        options=options,
    )


def load_pipeline(path: pathlib.Path) -> Pipeline:
    _ensure_registered()
    path = path.resolve()
    if not path.exists():
        raise FileNotFoundError(f"pipeline file not found: {path}")
    parsed = yaml.safe_load(path.read_text(encoding="utf-8"))
    if not isinstance(parsed, dict):
        raise ValueError(
            f"pipeline file must be a YAML mapping, got {type(parsed)!r}: {path}"
        )
    raw = cast(dict[str, Any], parsed)
    yaml_dir = path.parent
    prompt_dir = _resolve_prompt_dir(raw.get("prompt_dir"), yaml_dir)

    pipeline_name = str(raw.get("name") or path.stem)

    default_client: ClientSpec | None = None
    default_client_raw = raw.get("default_client")
    if default_client_raw is not None:
        if not isinstance(default_client_raw, str):
            raise ValueError(
                f"default_client must be a string, got {type(default_client_raw)!r}"
            )
        default_client = ClientSpec.parse(default_client_raw)

    stages: list[StageEntry] = []
    for entry in cast(list[dict[str, Any]], raw.get("stages") or []):
        stages.append(_parse_stage_entry(entry, prompt_dir))

    return Pipeline(
        name=pipeline_name,
        path=path,
        stages=stages,
        default_client=default_client,
    )


VALID_KINDS = {"ghgremlin", "localgremlin", "bossgremlin"}

KIND_SUBCOMMAND = {
    "localgremlin": "_local",
    "ghgremlin": "_gh",
    "bossgremlin": "_boss",
}


def resolve_pipeline_path(name_or_path: str, base_dir: pathlib.Path) -> pathlib.Path:
    candidate = pathlib.Path(name_or_path)
    if candidate.suffix == ".yaml" or len(candidate.parts) > 1:
        resolved = candidate.resolve()
        if not resolved.exists():
            raise FileNotFoundError(f"pipeline file not found: {resolved}")
        return resolved
    project_scoped = base_dir / ".gremlins" / "pipelines" / f"{name_or_path}.yaml"
    if project_scoped.exists():
        return project_scoped.resolve()
    bundled = (
        pathlib.Path(__file__).resolve().parent / "pipelines" / f"{name_or_path}.yaml"
    )
    if bundled.exists():
        return bundled.resolve()
    raise FileNotFoundError(
        f"pipeline {name_or_path!r} not found in "
        f"{project_scoped.parent} or bundled pipelines"
    )
