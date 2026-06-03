"""Subprocess entry point: execute one pipeline stage in a fresh process.

Spec file schema (JSON):
    {
        "stage_dict":      <dict>       parsed YAML dict; passed to parse_stage()
        "client":          <str>        "provider:model"
        "artifact_dir":     <str>        absolute path to artifacts directory
        "gremlin_id":      <str|null>   load StateData from disk when present
        "worktree":        <str|null>   absolute path to git worktree or null
        "worktree_parent": <str|null>   absolute path to worktree parent or null
        "pipeline_path":   <str|null>   absolute path to pipeline YAML or null
        "child_key":       <str|null>   parallel group child identifier or null
        "parent_stage":    <str>        parent stage name for sub-stage tracking
        "repo":            <str>        "owner/repo" for gh API calls
        "instructions":    <str>        freeform instructions forwarded from parent
    }

Result file schema (written to <spec_path>.result):
    {
        "status":     "done" | "bail" | "error"
        "detail":     <str>        reason for bail / error
        "returncode": <int|null>   always null
        "cost_usd":   <float>      total client cost accumulated during the stage
    }

Usage:
    python -m gremlins.run_child <spec_path>
"""

from __future__ import annotations

import asyncio
import importlib
import json
import logging
import os
import pathlib
import sys
import traceback
from typing import Any, cast

from gremlins import paths
from gremlins.clients.client import Client
from gremlins.clients.registry import CLIENT_FACTORIES
from gremlins.executor.state import State, StateData, build_state
from gremlins.executor.state_utils import validate_gremlin_id
from gremlins.permissions.loader import load_policy
from gremlins.permissions.validation import validate_policy_against_registry
from gremlins.pipeline import Pipeline
from gremlins.pipeline.loader import parse_stage
from gremlins.stages.outcome import Bail

logger = logging.getLogger(__name__)


def _load_spec(spec_path: pathlib.Path) -> dict[str, Any]:
    if not spec_path.exists():
        raise FileNotFoundError(f"spec file not found: {spec_path}")
    parsed: Any = json.loads(spec_path.read_text(encoding="utf-8"))
    if not isinstance(parsed, dict):
        raise ValueError(f"spec must be a JSON object, got {type(parsed).__name__}")
    return cast(dict[str, Any], parsed)


def _build_state(spec: dict[str, Any]) -> State:
    importlib.import_module(
        "gremlins.clients"
    )  # ensure CLIENT_FACTORIES are registered
    client_label = spec.get("client")
    if not isinstance(client_label, str) or not client_label:
        raise ValueError("spec missing required 'client' field")

    raw_session = spec.get("artifact_dir")
    if not isinstance(raw_session, str) or not raw_session:
        raise ValueError("spec missing required 'artifact_dir' field")
    artifact_dir = pathlib.Path(raw_session)

    artifact_dir.mkdir(parents=True, exist_ok=True)

    gremlin_id = spec.get("gremlin_id") or None
    if gremlin_id:
        validate_gremlin_id(gremlin_id)
    data = StateData.load(gremlin_id)

    project_root = (
        pathlib.Path(data.project_root) if data.project_root else paths.project_root()
    )
    perm_file = pathlib.Path(data.permissions_file) if data.permissions_file else None
    policy = load_policy(
        cli_bypass=data.bypass or None,
        cli_permissions_file=perm_file,
        env=os.environ,
        cwd=project_root,
    )
    validate_policy_against_registry(policy, set(CLIENT_FACTORIES))
    client = Client.parse(client_label, policy=policy)

    worktree: pathlib.Path | None = None
    if spec.get("worktree"):
        worktree = pathlib.Path(str(spec["worktree"]))

    worktree_parent: pathlib.Path | None = None
    if spec.get("worktree_parent"):
        worktree_parent = pathlib.Path(str(spec["worktree_parent"]))

    pipeline_data: Pipeline | None = None
    if spec.get("pipeline_path"):
        try:
            pipeline_data = Pipeline.from_yaml(pathlib.Path(str(spec["pipeline_path"])))
        except Exception:
            logger.warning(
                "failed to load pipeline from %s", spec["pipeline_path"], exc_info=True
            )

    return build_state(
        data=data,
        client=client,
        artifact_dir=artifact_dir,
        pipeline_data=pipeline_data,
        repo=str(spec.get("repo") or ""),
        child_key=spec.get("child_key") or None,
        parent_stage=str(spec.get("parent_stage") or ""),
        worktree=worktree,
        worktree_parent=worktree_parent,
    )


def _write_result(result_path: pathlib.Path, payload: dict[str, Any]) -> None:
    result_path.parent.mkdir(parents=True, exist_ok=True)
    result_path.write_text(json.dumps(payload), encoding="utf-8")


class _GremlinWrapper:
    """Minimal Gremlin-like wrapper for run_child subprocess."""

    def __init__(self, state: State) -> None:
        self.state = state
        self.registry = state.artifacts

    async def fork(
        self,
        state: State,
        target_id: str,
        *,
        parent_id: str = "",
        group_name: str = "",
        child_key: str = "",
        pipeline: Any | None = None,
    ) -> State:
        raise NotImplementedError("fork not supported in run_child")


async def _run(spec_path: pathlib.Path) -> int:
    spec = _load_spec(spec_path)
    result_path = pathlib.Path(str(spec_path) + ".result")

    stage_dict = spec.get("stage_dict")
    if not isinstance(stage_dict, dict):
        _write_result(
            result_path,
            {
                "status": "error",
                "detail": "spec missing required 'stage_dict' field",
                "returncode": None,
                "cost_usd": 0.0,
            },
        )
        return 2

    try:
        stage = parse_stage(cast(dict[str, Any], stage_dict))
        state = _build_state(spec)
    except Exception as exc:
        _write_result(
            result_path,
            {
                "status": "error",
                "detail": str(exc),
                "returncode": None,
                "cost_usd": 0.0,
            },
        )
        return 2

    if stage.client is None:
        stage.client = state.client

    gremlin = _GremlinWrapper(state)
    try:
        await stage.run(gremlin)
    except Bail as b:
        cost = getattr(state.client, "total_cost_usd", 0.0) or 0.0
        _write_result(
            result_path,
            {
                "status": "bail",
                "detail": b.reason,
                "returncode": None,
                "cost_usd": cost,
            },
        )
        return 1
    except Exception as exc:
        cost = getattr(state.client, "total_cost_usd", 0.0) or 0.0
        _write_result(
            result_path,
            {
                "status": "error",
                "detail": str(exc),
                "returncode": None,
                "cost_usd": cost,
            },
        )
        traceback.print_exc()
        return 2

    cost = getattr(state.client, "total_cost_usd", 0.0) or 0.0
    _write_result(
        result_path,
        {"status": "done", "detail": "", "returncode": None, "cost_usd": cost},
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]
    if len(argv) != 1:
        sys.stderr.write("run_child: usage: <spec_path>\n")
        return 1

    spec_path = pathlib.Path(argv[0])
    try:
        return asyncio.run(_run(spec_path))
    except (FileNotFoundError, ValueError) as exc:
        sys.stderr.write(f"run_child: {exc}\n")
        return 1
    except Exception:
        traceback.print_exc()
        return 2


if __name__ == "__main__":
    sys.exit(main())
