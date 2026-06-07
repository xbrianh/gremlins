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

from gremlins import paths as _paths
from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.uri import Uri
from gremlins.clients.client import PACKAGE_DEFAULT
from gremlins.executor.gremlin import Gremlin
from gremlins.executor.state import StateData, validate_gremlin_id
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline.discovery import list_pipelines, resolve_pipeline_path
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
    return _paths.state_root()


def _resolve_description_and_slug(
    description: str | None,
) -> tuple[str, bool, str]:
    """Return (description, description_explicit, slug) from available inputs."""
    if description:
        slug = slugify(description) or "gremlin"
        return description[:60], True, slug
    return "", False, "gremlin"


def _build_spawn_env(gremlin_id: str) -> dict[str, str]:
    env = dict(os.environ)
    pkg_root = str(pathlib.Path(__file__).resolve().parent.parent)
    existing_pp = env.get("PYTHONPATH", "")
    parts = [p for p in [pkg_root, existing_pp] if p]
    env["PYTHONPATH"] = os.pathsep.join(parts)
    env["PYTHONSAFEPATH"] = "1"
    env["GREMLINS_GREMLIN_ID"] = gremlin_id
    env["GREMLINS_OVERLAY_DIR"] = str(
        _state_root() / gremlin_id / _paths.OVERLAY_DIRNAME
    )
    return env


@dataclasses.dataclass
class _Inputs:
    gremlin_id: str
    kind: str
    description: str
    description_explicit: bool
    parent_id: str
    project_root: str
    pipeline_path: str
    pipeline_args: list[str]
    client_label: str
    fetch_worktree: bool
    base_ref_name: str
    base_ref_sha: str
    stage_inputs: dict[str, Any]
    loaded_pipeline: _PipelineData | None = None


def _reject_pipeline_collision(gremlin_id: str) -> None:
    pipeline_names = {name for name, _ in list_pipelines(_paths.project_root())}
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
        if loaded_pipeline is not None and loaded_pipeline.github_integration:
            _branch = effective_base_ref.removeprefix("origin/")
            if _branch and _branch not in ("current", "HEAD"):
                try:
                    _git_mod.fetch_origin(_branch, cwd=project_root)
                except _git_mod.GitError as exc:
                    raise RuntimeError(
                        f"git fetch origin {_branch} failed: {exc}"
                    ) from exc
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
) -> _Inputs:
    from gremlins.cli.pipeline_args import launch_client_label, resolve_pipeline

    pr = stage_inputs.pop("pr", None) or None

    desc, desc_explicit, slug = _resolve_description_and_slug(description)

    if project_root is None:
        r = proc.run(["git", "rev-parse", "--show-toplevel"])
        if r.returncode == 0 and r.stdout.strip():
            project_root = r.stdout.strip()
        else:
            project_root = str(_paths.project_root())

    resolved_gremlin_id = _resolve_gremlin_id(slug, gremlin_id)

    resolved_pipeline_args, pipeline_path = resolve_pipeline(
        kind, pipeline_args, project_root
    )

    loaded_pipeline = None
    try:
        loaded_pipeline = _PipelineData.from_yaml(
            resolve_pipeline_path(pipeline_path, pathlib.Path(project_root))
        )
    except (FileNotFoundError, OSError, ValueError):
        pass

    if (
        loaded_pipeline is not None
        and loaded_pipeline.github_integration
        and shutil.which("gh") is None
    ):
        raise RuntimeError("gh CLI not found on PATH (required for gh pipeline)")

    if pr is not None:
        from gremlins.utils.pr import pr_arg_to_ref

        pr_ref = pr_arg_to_ref(pr.strip().lstrip("#"))
        base_ref_name = ""
        base_ref_sha = pr_ref
        fetch_worktree = True
    else:
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
        description_explicit=desc_explicit,
        parent_id=parent_id or "",
        project_root=project_root,
        pipeline_path=pipeline_path,
        pipeline_args=stored_args,
        client_label=client_label,
        fetch_worktree=fetch_worktree,
        base_ref_name=base_ref_name,
        base_ref_sha=base_ref_sha,
        stage_inputs=stage_inputs,
        loaded_pipeline=loaded_pipeline,
    )


def _prepare_state_dir(state_dir: pathlib.Path) -> None:
    state_dir.mkdir(parents=True, exist_ok=True)
    (state_dir / "artifacts").mkdir(exist_ok=True)


def _initial_state_data(inputs: _Inputs) -> StateData:
    now_iso = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
    return StateData(
        gremlin_id=inputs.gremlin_id,
        kind=inputs.kind,
        project_root=inputs.project_root,
        workdir="",
        setup_kind="worktree-detached",
        worktree_base="",
        status="running",
        started_at=now_iso,
        description=inputs.description,
        description_explicit=inputs.description_explicit,
        parent_id=inputs.parent_id,
        pipeline_args=inputs.pipeline_args,
        client=inputs.client_label,
        pipeline_path=inputs.pipeline_path,
        stage="starting",
        pid=None,
        stage_inputs=inputs.stage_inputs,
    )


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
    from gremlins.pipeline.discovery import resolve_pipeline_name
    from gremlins.pipeline.loader import fill_names
    from gremlins.pipeline.preprocess import expand_pipeline
    from gremlins.utils.yaml_io import dump_yaml_text, load_yaml_file

    hermetic = state_dir / "pipeline.yaml"
    if not hermetic.is_file():
        raise RuntimeError(f"no persisted pipeline.yaml in {state_dir} — cannot graft")

    graft_path = resolve_pipeline_name(graft_pipeline_name, pathlib.Path(project_root))
    expanded = expand_pipeline(graft_path, pathlib.Path(project_root))
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


def _persist_expanded_pipeline(state_dir: pathlib.Path, pipeline_path: str) -> str:
    from gremlins.pipeline.preprocess import expand_pipeline
    from gremlins.utils.yaml_io import dump_yaml_text

    expanded = expand_pipeline(pathlib.Path(pipeline_path))
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
        cmd, inputs.project_root, _build_spawn_env(gremlin_id), state_dir / "log"
    )


def _seed_registry_from_sources(
    registry: ArtifactRegistry,
    input_values: dict[str, str],
    sources: dict[str, Any],
    artifacts_dir: pathlib.Path,
) -> None:
    for key, source in sources.items():
        value = input_values.get(key) or None
        if not value:
            if not source.optional:
                raise ValueError(
                    f"required input source {key!r} (type: {source.types}) is not available"
                )
            continue
        for t in source.types:
            if t == "filepath" and os.path.isfile(value):
                src = pathlib.Path(value)
                ext = src.suffix or ".txt"
                dest = artifacts_dir / f"{key}{ext}"
                dest.write_bytes(src.read_bytes())
                registry.bind(key, Uri.parse(f"file://session/{key}{ext}"))
                break
            elif t == "string":
                dest = artifacts_dir / f"{key}.txt"
                dest.write_text(value, encoding="utf-8")
                registry.bind(key, Uri.parse(f"file://session/{key}.txt"))
                break
        else:
            if not source.optional:
                raise ValueError(
                    f"required input source {key!r} (type: {source.types}) could not be resolved"
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
    bypass: bool = False,
    permissions_file: str = "",
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
    )
    state_dir = _state_root() / inputs.gremlin_id
    try:
        _prepare_state_dir(state_dir)
        inputs.pipeline_path = _persist_expanded_pipeline(
            state_dir, inputs.pipeline_path
        )
        sd = _initial_state_data(inputs)
        sd.bypass = bypass
        sd.permissions_file = permissions_file
        sd.persist(state_dir)
        artifact_dir = state_dir / "artifacts"
        artifact_dir.mkdir(parents=True, exist_ok=True)
        registry = ArtifactRegistry(artifact_dir=artifact_dir)
        if inputs.base_ref_sha:
            registry.bind("base_sha", Uri.parse(f"git://commit/{inputs.base_ref_sha}"))
        if inputs.base_ref_name:
            registry.bind("base_ref", Uri.parse(f"git://ref/{inputs.base_ref_name}"))
        if (
            inputs.loaded_pipeline is not None
            and inputs.loaded_pipeline.input_sources is not None
        ):
            input_values = {
                k: v for k, v in inputs.stage_inputs.items() if isinstance(v, str) and v
            }
            _seed_registry_from_sources(
                registry,
                input_values,
                inputs.loaded_pipeline.input_sources.sources,
                artifact_dir,
            )
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
        client=str(state.get("client") or PACKAGE_DEFAULT),
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
    project_root = gremlin.project_root or str(_paths.project_root())
    if gremlin.pipeline_data.github_integration and shutil.which("gh") is None:
        raise RuntimeError("gh CLI not found on PATH (required for gh pipeline)")

    stage = gremlin.state_data.stage
    if not stage or stage == "starting":
        stage = "plan"
    if gremlin.pipeline_data.uses_loop_handoff() and stage not in (
        "review-chain",
        "address-chain",
    ):
        stage = "chain"

    if graft is not None:
        stage = _append_graft(gremlin.state_dir, graft, project_root)

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
        project_root,
    )
    (gremlin.state_dir / "pid").write_text(str(p.pid), encoding="utf-8")
    Gremlin.patch_state_for(gremlin_id, pid=p.pid)
