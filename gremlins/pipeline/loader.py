from __future__ import annotations

import importlib
import pathlib
from typing import Any, cast

from gremlins.clients.client import Client
from gremlins.pipeline.preprocess import expand_pipeline
from gremlins.schema import PipelineDef, RetryConfig, StageEntry
from gremlins.stages.registry import STAGE_REGISTRY


def _ensure_registered() -> None:
    importlib.import_module("gremlins.stages.all")
    importlib.import_module("gremlins.clients")


def _parse_stage_entry(
    entry: dict[str, Any],
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
            child = _parse_stage_entry(child_raw, depth=depth + 1)
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
            prompts=[],
            options={},
            body=children,
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
    stage_client: Client | None = None
    if client_spec_raw is not None:
        if not isinstance(client_spec_raw, str):
            raise ValueError(
                f"stage {name!r}: 'client' must be a string, got {type(client_spec_raw)!r}"
            )
        stage_client = Client.parse(client_spec_raw)

    raw_prompts = entry.get("prompt")
    if raw_prompts is not None and not isinstance(raw_prompts, list):
        raise ValueError(
            f"stage {name!r}: 'prompt' must be a list after preprocessing, got {type(raw_prompts)!r}"
        )
    prompts: list[str] = cast(list[str], raw_prompts) if raw_prompts is not None else []

    options = dict(cast(dict[str, Any], entry.get("options") or {}))

    body: list[StageEntry] = []
    if stage_type in ("loop", "sequence"):
        raw_body = entry.get("body")
        if raw_body is not None:
            if not isinstance(raw_body, list):
                raise ValueError(f"stage {name!r}: 'body' must be a list")
            for body_entry in cast(list[dict[str, Any]], raw_body):
                body.append(_parse_stage_entry(body_entry, depth=depth))

    return StageEntry(
        name=name,
        type=stage_type,
        client=stage_client,
        prompts=prompts,
        options=options,
        body=body,
    )


def _parse_retry(raw: Any, context: str) -> RetryConfig | None:
    if raw is None:
        return None
    if not isinstance(raw, dict):
        raise ValueError(f"{context}: 'retry' must be a mapping")
    d = cast(dict[str, Any], raw)
    idle_timeout_raw: Any = d.get("idle_timeout")
    backoff_raw: Any = d.get("backoff")
    return RetryConfig(
        idle_timeout=float(idle_timeout_raw) if idle_timeout_raw is not None else None,
        backoff=list(backoff_raw) if backoff_raw is not None else None,
    )


def load_pipeline(path: pathlib.Path) -> PipelineDef:
    _ensure_registered()
    path = path.resolve()
    if not path.exists():
        raise FileNotFoundError(f"pipeline file not found: {path}")

    raw = expand_pipeline(path)

    pipeline_name = str(raw.get("name") or path.stem)

    default_client: Client | None = None
    default_client_raw = raw.get("default_client")
    if default_client_raw is not None:
        if not isinstance(default_client_raw, str):
            raise ValueError(
                f"default_client must be a string, got {type(default_client_raw)!r}"
            )
        default_client = Client.parse(default_client_raw)

    base_ref_raw = raw.get("base_ref")
    if base_ref_raw is not None:
        if not isinstance(base_ref_raw, str) or not base_ref_raw.strip():
            raise ValueError("base_ref must be a non-empty string")
        pipeline_base_ref = base_ref_raw.strip()
    else:
        pipeline_base_ref = "current"

    pipeline_retry = _parse_retry(raw.get("retry"), context="pipeline retry")

    stages: list[StageEntry] = []
    for entry in cast(list[dict[str, Any]], raw.get("stages") or []):
        stages.append(_parse_stage_entry(entry))

    return PipelineDef(
        name=pipeline_name,
        path=path,
        stages=stages,
        default_client=default_client,
        base_ref=pipeline_base_ref,
        retry=pipeline_retry,
    )
