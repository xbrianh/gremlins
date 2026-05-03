"""YAML pipeline loader and stage/client registries."""

from __future__ import annotations

import dataclasses
import pathlib
from collections.abc import Callable
from typing import Any, cast

import yaml

STAGE_REGISTRY: dict[str, Callable[..., Any]] = {}
CLIENT_FACTORIES: dict[str, Callable[[str], Any]] = {}


def register_stage(name: str, fn: Callable[..., Any]) -> None:
    STAGE_REGISTRY[name] = fn


def register_client_factory(provider: str, factory: Callable[[str], Any]) -> None:
    CLIENT_FACTORIES[provider] = factory


@dataclasses.dataclass
class ClientDef:
    provider: str
    model: str


@dataclasses.dataclass
class StageEntry:
    name: str
    type: str
    client_key: str | None
    prompt_paths: list[pathlib.Path]
    options: dict[str, Any]
    children: list[StageEntry] = dataclasses.field(  # pyright: ignore[reportUnknownVariableType]
        default_factory=list
    )


@dataclasses.dataclass
class Pipeline:
    name: str
    path: pathlib.Path
    clients: dict[str, Any]
    stages: list[StageEntry]


def _resolve_prompt_paths(
    prompt_field: object, yaml_dir: pathlib.Path
) -> list[pathlib.Path]:
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
        path = (yaml_dir / p).resolve()
        if not path.exists():
            raise FileNotFoundError(f"prompt file not found: {path}")
        resolved.append(path)
    return resolved


def _parse_stage_entry(
    entry: dict[str, Any],
    yaml_dir: pathlib.Path,
    client_keys: set[str],
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
            child = _parse_stage_entry(child_raw, yaml_dir, client_keys, depth=depth + 1)
            if child.name in seen:
                raise ValueError(
                    f"parallel group {name!r}: duplicate child name {child.name!r}"
                )
            seen.add(child.name)
            children.append(child)
        return StageEntry(
            name=name,
            type="parallel",
            client_key=None,
            prompt_paths=[],
            options={},
            children=children,
        )

    name = entry.get("name")
    if not isinstance(name, str) or not name:
        raise ValueError("stage entry must have a 'name' field")
    stage_type = entry.get("type")
    if not isinstance(stage_type, str) or not stage_type:
        raise ValueError(f"stage {name!r}: must have a 'type' field")
    if stage_type not in STAGE_REGISTRY:
        raise ValueError(f"stage {name!r}: unknown type {stage_type!r}")

    client_key = entry.get("client")
    if client_key is not None and not isinstance(client_key, str):
        raise ValueError(
            f"stage {name!r}: 'client' must be a string, got {type(client_key)!r}"
        )
    if client_key is not None and client_key not in client_keys:
        raise ValueError(f"stage {name!r}: unknown client key {client_key!r}")

    prompt_paths = _resolve_prompt_paths(entry.get("prompt"), yaml_dir)
    options = dict(cast(dict[str, Any], entry.get("options") or {}))
    return StageEntry(
        name=name,
        type=stage_type,
        client_key=client_key,
        prompt_paths=prompt_paths,
        options=options,
    )


def load_pipeline(path: pathlib.Path) -> Pipeline:
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

    pipeline_name = str(raw.get("name") or path.stem)

    clients_raw = cast(dict[str, dict[str, Any]], raw.get("clients") or {})
    resolved_clients: dict[str, Any] = {}
    for key, cfg in clients_raw.items():
        provider = str(cfg.get("provider") or "")
        if not provider:
            raise ValueError(f"client {key!r}: missing 'provider'")
        if provider not in CLIENT_FACTORIES:
            raise ValueError(f"client {key!r}: unknown provider {provider!r}")
        model = str(cfg.get("model") or "")
        if not model:
            raise ValueError(f"client {key!r}: missing 'model'")
        resolved_clients[key] = CLIENT_FACTORIES[provider](model)

    client_keys = set(resolved_clients.keys())

    stages: list[StageEntry] = []
    for entry in cast(list[dict[str, Any]], raw.get("stages") or []):
        stages.append(_parse_stage_entry(entry, yaml_dir, client_keys))

    return Pipeline(
        name=pipeline_name,
        path=path,
        clients=resolved_clients,
        stages=stages,
    )


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
    bundled = pathlib.Path(__file__).resolve().parent / "pipelines" / f"{name_or_path}.yaml"
    if bundled.exists():
        return bundled.resolve()
    raise FileNotFoundError(
        f"pipeline {name_or_path!r} not found in "
        f"{project_scoped.parent} or bundled pipelines"
    )


def _bootstrap() -> None:
    from gremlins.stages.address_code import run as _address_code
    from gremlins.stages.commit_pr import run as _commit_pr
    from gremlins.stages.ghaddress import run as _ghaddress
    from gremlins.stages.ghreview import run as _ghreview
    from gremlins.stages.implement import run as _implement
    from gremlins.stages.plan import run as _plan
    from gremlins.stages.request_copilot import run as _request_copilot
    from gremlins.stages.review_code import run as _review_code
    from gremlins.stages.test import run as _test
    from gremlins.stages.wait_ci import run as _wait_ci
    from gremlins.stages.wait_copilot import run as _wait_copilot
    register_stage("plan", _plan)
    register_stage("implement", _implement)
    register_stage("review-code", _review_code)
    register_stage("address-code", _address_code)
    register_stage("test", _test)
    register_stage("commit-pr", _commit_pr)
    register_stage("request-copilot", _request_copilot)
    register_stage("ghreview", _ghreview)
    register_stage("wait-copilot", _wait_copilot)
    register_stage("ghaddress", _ghaddress)
    register_stage("wait-ci", _wait_ci)
    from gremlins.clients.claude import SubprocessClaudeClient
    register_client_factory("claude", lambda _: SubprocessClaudeClient())


_bootstrap()
