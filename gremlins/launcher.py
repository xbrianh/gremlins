"""Launcher for background gremlins.

Public API:
    launch(kind, *, stage_inputs=None, description=None, parent_id=None,
           project_root=None, base_ref=None, pipeline_args=(),
           gremlin_id=None) -> tuple[str, subprocess.Popen[bytes]]
    resume(gremlin_id, *, graft=None) -> None
"""

from __future__ import annotations

import dataclasses
import datetime
import json
import os
import pathlib
import secrets
import shutil
import subprocess
import sys
from typing import Any, cast

from _gremlins_core.artifacts import Uri
from _gremlins_core.config import project_root as _project_root_fn
from _gremlins_core.config import scratch_root as _scratch_root_fn
from _gremlins_core.config import state_root as _state_root_fn
from _gremlins_core.discovery import list_pipelines, resolve_pipeline_path

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.executor.gremlin import Gremlin, validate_gremlin_id, write_initial_state
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline.bootstrap import validate_source_values
from gremlins.pipelines import BUNDLED_PIPELINE_DIR
from gremlins.utils import git as _git_mod
from gremlins.utils import proc
from gremlins.utils.spawn_logged_process import (
    spawn_logged_process as _spawn_logged_process,
)
from gremlins.utils.text import slugify


class GremlinAlreadyRunning(RuntimeError):
    pass


class GremlinStateDirExists(RuntimeError):
    pass


def _state_root() -> pathlib.Path:
    return pathlib.Path(_state_root_fn())


def _resolve_description_and_slug(
    description: str | None,
) -> tuple[str, str]:
    """Return (description, slug) from available inputs."""
    if description:
        slug = slugify(description) or "gremlin"
        return description[:60], slug
    return "", "gremlin"


def _build_spawn_env(gremlin_id: str, *, telemetry: bool = False) -> dict[str, str]:
    """Return the child subprocess env — parent env plus gremlin overrides."""
    env = dict(os.environ)
    env["PYTHONSAFEPATH"] = "1"
    env["GREMLINS_GREMLIN_ID"] = gremlin_id
    env["GREMLINS_OVERLAY_DIR"] = str(_state_root() / gremlin_id / ".gremlins")
    if telemetry:
        env["GREMLINS_TELEMETRY"] = "1"
    return env


@dataclasses.dataclass
class _Inputs:
    gremlin_id: str
    kind: str
    description: str
    parent_id: str
    project_root: str
    pipeline_path: str
    pipeline_args: list[str]
    client_label: str
    fetch_worktree: bool
    base_ref_name: str
    base_ref_sha: str
    stage_inputs: dict[str, Any]
    telemetry: bool
    loaded_pipeline: _PipelineData | None = None


def _reject_pipeline_collision(gremlin_id: str) -> None:
    pipeline_names = {
        name
        for name, _ in list_pipelines(
            pathlib.Path(_project_root_fn()), BUNDLED_PIPELINE_DIR
        )
    }
    if gremlin_id in pipeline_names:
        raise ValueError(
            f"--gremlin-id {gremlin_id!r} shadows the name of a pipeline. Pick a different id."
        )


def _resolve_gremlin_id(slug: str, gremlin_id: str | None) -> str:
    if gremlin_id is not None:
        validate_gremlin_id(gremlin_id)
        _reject_pipeline_collision(gremlin_id)
        _existing = _state_root() / gremlin_id
        if _existing.exists():
            _sf = _existing / "state.json"
            if _sf.is_file():
                _st: dict[str, Any] = {}
                try:
                    _st = json.loads(_sf.read_text(encoding="utf-8"))
                except (OSError, ValueError):
                    pass
                _pid = _st.get("pid")
                if (
                    _st.get("status") == "running"
                    and _pid is not None
                    and int(_pid) > 0
                ):
                    try:
                        os.kill(int(_pid), 0)
                    except PermissionError:
                        raise GremlinAlreadyRunning(
                            f"gremlin {gremlin_id!r} is already running (pid {_pid})"
                        )
                    except (ProcessLookupError, ValueError):
                        pass
                    else:
                        raise GremlinAlreadyRunning(
                            f"gremlin {gremlin_id!r} is already running (pid {_pid})"
                        )
            raise GremlinStateDirExists(
                f"state dir for {gremlin_id!r} already exists. "
                f"Run 'gremlins rm {gremlin_id}' first, or pick a different --gremlin-id."
            )
        return gremlin_id
    return f"{slug}-{secrets.token_hex(3)}"


def _resolve_base_ref(
    base_ref: str | None,
    project_root: str,
    loaded_pipeline: Any,
) -> tuple[str, str]:
    _pipeline_base_ref = (
        loaded_pipeline.base_ref if loaded_pipeline is not None else "current"
    )
    effective_base_ref = base_ref if base_ref is not None else _pipeline_base_ref
    if _git_mod.in_git_repo(cwd=project_root):
        try:
            return _git_mod.resolve_base_ref(effective_base_ref, cwd=project_root)
        except _git_mod.GitError as exc:
            raise RuntimeError(f"--base-ref: {exc}") from exc
    return effective_base_ref, ""


def _resolve_inputs(
    kind: str,
    stage_inputs: dict[str, Any],
    description: str | None,
    parent_id: str | None,
    project_root: str | None,
    base_ref: str | None,
    pipeline_args: tuple[str, ...],
    gremlin_id: str | None,
    telemetry: bool = False,
) -> _Inputs:
    from gremlins.cli.pipeline_args import launch_client_label, resolve_pipeline

    loaded_pipeline = None
    desc, slug = _resolve_description_and_slug(description)

    if project_root is None:
        r = proc.run(["git", "rev-parse", "--show-toplevel"])
        if r.returncode == 0 and r.stdout.strip():
            project_root = r.stdout.strip()
        else:
            project_root = _project_root_fn()

    resolved_gremlin_id = _resolve_gremlin_id(slug, gremlin_id)

    resolved_pipeline_args, pipeline_path = resolve_pipeline(
        kind, pipeline_args, project_root
    )

    try:
        loaded_pipeline = _PipelineData.from_yaml(
            resolve_pipeline_path(
                pipeline_path, pathlib.Path(project_root), BUNDLED_PIPELINE_DIR
            )
        )
    except (FileNotFoundError, OSError, ValueError):
        pass

    if loaded_pipeline is not None:
        validate_source_values(loaded_pipeline.bootstrap.source, stage_inputs)

    base_ref_name, base_ref_sha = _resolve_base_ref(
        base_ref, project_root, loaded_pipeline
    )
    fetch_worktree = False

    stored_args = list(resolved_pipeline_args)

    client_label = launch_client_label(stored_args, loaded_pipeline)

    return _Inputs(
        gremlin_id=resolved_gremlin_id,
        kind=kind,
        description=desc,
        parent_id=parent_id or "",
        project_root=project_root,
        pipeline_path=pipeline_path,
        pipeline_args=stored_args,
        client_label=client_label,
        fetch_worktree=fetch_worktree,
        base_ref_name=base_ref_name,
        base_ref_sha=base_ref_sha,
        stage_inputs=stage_inputs,
        telemetry=telemetry,
        loaded_pipeline=loaded_pipeline,
    )


def _prepare_state_dir(state_dir: pathlib.Path) -> None:
    state_dir.mkdir(parents=True, exist_ok=True)


def _make_name_unique(stage: dict[str, Any], used: set[str]) -> None:
    name = str(stage.get("name") or "")
    if not name or name not in used:
        if name:
            used.add(name)
        return
    n = 2
    while f"{name}-{n}" in used:
        n += 1
    stage["name"] = f"{name}-{n}"
    used.add(stage["name"])


def _disambiguate_graft_names(
    graft_stages: list[dict[str, Any]], existing_names: set[str]
) -> None:
    used = set(existing_names)
    for d in graft_stages:
        _make_name_unique(d, used)
        if d.get("type") == "parallel":
            for child in cast(list[dict[str, Any]], d.get("body") or []):
                _make_name_unique(child, used)


def _all_stage_names(stages: list[dict[str, Any]]) -> set[str]:
    names: set[str] = set()
    for s in stages:
        name = str(s.get("name") or "")
        if name:
            names.add(name)
        if s.get("type") == "parallel":
            for child in cast(list[dict[str, Any]], s.get("body") or []):
                child_name = str(child.get("name") or "")
                if child_name:
                    names.add(child_name)
    return names


def _append_graft(
    state_dir: pathlib.Path, graft_pipeline_name: str, project_root: str
) -> str:
    from _gremlins_core.discovery import resolve_pipeline_name
    from _gremlins_core.schemas import expand_pipeline as _expand_pipeline
    from _gremlins_core.schemas import fill_names

    from gremlins.pipelines import BUNDLED_PIPELINE_DIR
    from gremlins.prompts import BUNDLED_PROMPT_DIR
    from gremlins.recipes import BUNDLED_STAGE_DEF_DIR
    from gremlins.utils.yaml_io import dump_yaml_text, load_yaml_file

    hermetic = state_dir / "pipeline.yaml"
    if not hermetic.is_file():
        raise RuntimeError(f"no persisted pipeline.yaml in {state_dir} — cannot graft")

    graft_path = resolve_pipeline_name(
        graft_pipeline_name, pathlib.Path(project_root), BUNDLED_PIPELINE_DIR
    )

    def _resolve(n, pr):
        return resolve_pipeline_name(n, pr, BUNDLED_PIPELINE_DIR)

    expanded = _expand_pipeline(
        str(graft_path),
        str(project_root),
        str(BUNDLED_STAGE_DEF_DIR),
        str(BUNDLED_PROMPT_DIR),
        _resolve,
    )
    graft_stages = list(expanded.get("stages") or [])
    if not graft_stages:
        raise RuntimeError(f"graft pipeline {graft_pipeline_name!r} has no stages")
    fill_names(graft_stages)

    current = load_yaml_file(hermetic)
    top_stages: list[dict[str, Any]] = list(
        cast(list[dict[str, Any]], current.get("stages") or [])
    )
    existing_names = _all_stage_names(top_stages)
    _disambiguate_graft_names(graft_stages, existing_names)
    top_stages.extend(graft_stages)
    current["stages"] = top_stages
    hermetic.write_text(dump_yaml_text(current), encoding="utf-8")
    name = str(graft_stages[0].get("name") or "")
    if not name:
        raise RuntimeError(
            f"first stage of graft {graft_pipeline_name!r} has no name after expansion"
        )
    return name


def _bake_prefix_clients(
    expanded: dict[str, Any],
    prefix_map: tuple[dict[str, str], dict[str, str]],
) -> None:
    """Apply global config client rules to the expanded pipeline dict.

    Modifies ``expanded["stages"]`` in place, adding a ``client`` field to
    stages whose names match an entry and which don't already have an
    explicit ``client:``.  Child lists (``parallel``, ``body``) are recursed
    into so loop/sequence/parallel stages are covered.

    ``prefix_map`` is a ``(exact_map, prefix_map)`` tuple.  Exact-map keys
    are matched by equality (``name == key``) and take priority over prefix
    matches.  Prefix-map keys are matched by ``name.startswith(prefix)``.

    When multiple prefix entries match a stage name the **longest** prefix
    wins (most specific match).  A tied length is resolved by dict insertion
    order (last with that length wins).
    """
    _exact_map, _prefix_map = prefix_map
    if not _exact_map and not _prefix_map:
        return
    for stage in cast(list[dict[str, Any]], expanded.get("stages") or []):
        _bake_prefix_clients_into_stage(stage, _exact_map, _prefix_map)


def _bake_prefix_clients_into_stage(
    stage: dict[str, Any],
    exact_map: dict[str, str],
    prefix_map: dict[str, str],
) -> None:
    name = stage.get("name", "")
    if isinstance(name, str) and "client" not in stage:
        # Exact match takes priority over prefix match.
        if name in exact_map:
            stage["client"] = exact_map[name]
        else:
            # Longest matching prefix wins (most specific match).
            best_prefix: str | None = None
            best_client: str | None = None
            for prefix, client_spec in prefix_map.items():
                if name.startswith(prefix) and (
                    best_prefix is None or len(prefix) > len(best_prefix)
                ):
                    best_prefix = prefix
                    best_client = client_spec
            if best_prefix is not None:
                stage["client"] = best_client
    # Recurse into child containers. The authoritative set of container
    # keys is defined by the stage-type-to-container mapping; see
    # gremlins/pipeline/loader.py (STAGE_TYPES) for the canonical
    # list of stage types and their child fields.
    for key in ("parallel", "body"):
        for child in cast(list[dict[str, Any]], stage.get(key) or []):
            _bake_prefix_clients_into_stage(child, exact_map, prefix_map)


def _persist_expanded_pipeline(state_dir: pathlib.Path, pipeline_path: str) -> str:
    from _gremlins_core.config import get_config as _get_config
    from _gremlins_core.discovery import resolve_pipeline_name as _resolve_pipeline_name
    from _gremlins_core.schemas import expand_pipeline as _expand_pipeline

    from gremlins.cli.pipeline_args import load_prefix_clients
    from gremlins.pipelines import BUNDLED_PIPELINE_DIR
    from gremlins.prompts import BUNDLED_PROMPT_DIR
    from gremlins.recipes import BUNDLED_STAGE_DEF_DIR
    from gremlins.utils.yaml_io import dump_yaml_text

    def _resolve(n, pr):
        return _resolve_pipeline_name(n, pr, BUNDLED_PIPELINE_DIR)

    expanded = _expand_pipeline(
        str(pipeline_path),
        None,
        str(BUNDLED_STAGE_DEF_DIR),
        str(BUNDLED_PROMPT_DIR),
        _resolve,
    )

    # Bake global config prefix rules into the persisted pipeline so the
    # child subprocess doesn't need to read the config file at runtime.
    _bake_prefix_clients(expanded, load_prefix_clients())  # (exact_map, prefix_map)

    # Inject default_client from global config if the pipeline doesn't
    # declare one.  The child subprocess loads the persisted YAML without
    # access to the config file, so we bake it in here.
    if "default_client" not in expanded:
        _global_client = _get_config().default_client
        if _global_client:
            expanded["default_client"] = _global_client

    expanded["__gremlins_expanded__"] = True
    dest = state_dir / "pipeline.yaml"
    dest.write_text(dump_yaml_text(expanded), encoding="utf-8")
    return str(dest)


def _spawn(gremlin_id: str, inputs: _Inputs, state_dir: pathlib.Path) -> Any:
    spawn_args = list(inputs.pipeline_args)
    cmd = [
        sys.executable,
        "-m",
        "gremlins.spawn.pipeline",
        gremlin_id,
        inputs.pipeline_path,
        *spawn_args,
    ]
    return _spawn_logged_process(
        cmd,
        inputs.project_root,
        _build_spawn_env(gremlin_id, telemetry=inputs.telemetry),
        state_dir / "log",
    )


def launch(
    kind: str,
    *,
    stage_inputs: dict[str, Any] | None = None,
    description: str | None = None,
    parent_id: str | None = None,
    project_root: str | None = None,
    base_ref: str | None = None,
    pipeline_args: tuple[str, ...] = (),
    gremlin_id: str | None = None,
    telemetry: bool = False,
) -> tuple[str, subprocess.Popen[bytes]]:
    """Set up state dir, spawn the pipeline detached, return (gremlin_id, process).

    Worktree setup is deferred to the child process via Gremlin.initialize_with_runtime().
    Synchronous through spawn; does not wait for the pipeline to finish.
    Raises ValueError on bad arguments, RuntimeError on infrastructure failure.
    stage_inputs may contain a 'pr' key to trigger a detached-from-ref checkout.
    """
    inputs = _resolve_inputs(
        kind,
        {} if stage_inputs is None else dict(stage_inputs),
        description,
        parent_id,
        project_root,
        base_ref,
        pipeline_args,
        gremlin_id,
        telemetry=telemetry,
    )
    state_dir = _state_root() / inputs.gremlin_id
    try:
        _prepare_state_dir(state_dir)
        inputs.pipeline_path = _persist_expanded_pipeline(
            state_dir, inputs.pipeline_path
        )
        now_iso = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
        write_initial_state(
            gremlin_id=inputs.gremlin_id,
            kind=inputs.kind,
            project_root=inputs.project_root,
            started_at=now_iso,
            description=inputs.description,
            parent_id=inputs.parent_id,
            pipeline_args=inputs.pipeline_args,
            client_label=inputs.client_label,
            pipeline_path=inputs.pipeline_path,
            stage_inputs=inputs.stage_inputs,
            state_dir=state_dir,
        )
        artifact_dir = pathlib.Path(_scratch_root_fn(inputs.gremlin_id)) / "artifacts"
        artifact_dir.mkdir(parents=True, exist_ok=True)
        registry = ArtifactRegistry(artifact_dir=artifact_dir)
        if inputs.base_ref_sha:
            registry.bind("base_sha", Uri.parse(f"git://commit/{inputs.base_ref_sha}"))
        if inputs.base_ref_name:
            registry.bind("base_ref", Uri.parse(f"git://ref/{inputs.base_ref_name}"))
        p = _spawn(inputs.gremlin_id, inputs, state_dir)
    except Exception:
        shutil.rmtree(state_dir, ignore_errors=True)
        raise

    (state_dir / "pid").write_text(str(p.pid), encoding="utf-8")
    Gremlin.patch_state_for(inputs.gremlin_id, pid=p.pid)

    return inputs.gremlin_id, p


def _check_resume_preconditions(gremlin: Gremlin, graft: str | None) -> None:
    state_data = gremlin.state_data
    status = state_data.status
    old_pid = state_data.pid
    workdir = state_data.workdir
    gremlin_id = gremlin.gremlin_id

    if status == "running":
        if graft is not None:
            raise GremlinAlreadyRunning(
                f"gremlin {gremlin_id} is still running — cannot graft onto a live gremlin"
            )
        if old_pid is not None:
            try:
                os.kill(int(old_pid), 0)
                raise GremlinAlreadyRunning(
                    f"gremlin {gremlin_id} is still running (pid {old_pid}) — stop it first"
                )
            except (OSError, ValueError):
                pass

    if graft is None and gremlin.finished:
        if state_data.exit_code == 0:
            raise RuntimeError(
                f"gremlin {gremlin_id} finished successfully — nothing to resume"
            )

    if workdir and not os.path.isdir(workdir):
        raise RuntimeError(f"worktree missing: {workdir}")


def _patch_state_for_resume(
    gremlin_id: str,
    state_dir: pathlib.Path,
    state: dict[str, Any],
    stage: str,
    pipeline_args: list[str],
    pipeline_path: str,
) -> None:
    for marker in ("finished", "summarized"):
        try:
            (state_dir / marker).unlink()
        except FileNotFoundError:
            pass

    now_iso = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")

    Gremlin.patch_state_for(
        gremlin_id,
        _delete=(
            "exit_code",
            "ended_at",
            "attempt",
            "sub_stage",
            "stage_updated_at",
            "bail_class",
            "bail_reason",
            "bail_detail",
        ),
        status="running",
        stage=stage,
        resumed_from_stage=stage,
        pid=None,
        pipeline_args=pipeline_args,
        pipeline_path=pipeline_path,
        client=str(state.get("client") or ""),
    )

    try:
        with open(state_dir / "log", "a", encoding="utf-8") as f:
            f.write(f"\n--- resume at {now_iso} (from stage: {stage}) ---\n")
    except OSError:
        pass


def _spawn_resume(
    gremlin_id: str,
    state_dir: pathlib.Path,
    pipeline_path: str,
    pipeline_args: list[str],
    stage: str,
    project_root: str,
) -> Any:
    spawn_args: list[str] = list(pipeline_args)

    env = _build_spawn_env(gremlin_id)

    cmd = [
        sys.executable,
        "-m",
        "gremlins.spawn.pipeline",
        gremlin_id,
        pipeline_path,
        "--resume-from",
        stage,
        *spawn_args,
    ]
    return _spawn_logged_process(
        cmd, project_root, env, state_dir / "log", log_mode="a"
    )


def resume(gremlin_id: str, *, graft: str | None = None) -> None:
    gremlin = Gremlin.open(gremlin_id)
    _check_resume_preconditions(gremlin, graft)
    _pr = gremlin.project_root or _project_root_fn()

    stage = gremlin.state_data.stage
    if not stage or stage == "starting":
        stage = "plan"
    if gremlin.pipeline_data.uses_loop_handoff() and stage not in (
        "review-chain",
        "address-chain",
    ):
        stage = "chain"

    if graft is not None:
        stage = _append_graft(gremlin.state_dir, graft, _pr)

    state_data = gremlin.state_data
    _patch_state_for_resume(
        gremlin_id,
        gremlin.state_dir,
        {
            "status": state_data.status,
            "client": state_data.client,
        },
        stage,
        gremlin.pipeline_args,
        gremlin.pipeline_path,
    )
    p = _spawn_resume(
        gremlin_id,
        gremlin.state_dir,
        gremlin.pipeline_path,
        gremlin.pipeline_args,
        stage,
        _pr,
    )
    (gremlin.state_dir / "pid").write_text(str(p.pid), encoding="utf-8")
    Gremlin.patch_state_for(gremlin_id, pid=p.pid)
