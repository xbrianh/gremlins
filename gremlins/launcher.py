"""Launcher for background gremlins.

Public API:
    launch(kind, *, stage_inputs=None, plan=None, description=None,
           parent_id=None, project_root=None, base_ref="HEAD",
           pipeline_args=()) -> tuple[str, subprocess.Popen[bytes]]
    resume(gremlin_id, *, graft=None, is_rescue=False) -> None
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
from gremlins.executor.state import StateData, validate_gremlin_id
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline.discovery import list_pipelines, resolve_pipeline_path
from gremlins.utils import git as _git_mod
from gremlins.utils import proc
from gremlins.utils.github import fetch_issue, parse_issue_ref
from gremlins.utils.spawn_logged_process import (
    spawn_logged_process as _spawn_logged_process,
)
from gremlins.utils.text import read_markdown_title, slugify


class GremlinAlreadyRunning(RuntimeError):
    pass


class GremlinStateDirExists(RuntimeError):
    pass


def _state_root() -> pathlib.Path:
    return _paths.state_root()


def _resolve_description_and_slug(
    instructions: str | None,
    plan: str | None,
    description: str | None,
    *,
    issue_title: str = "",
) -> tuple[str, bool, str]:
    """Return (description, description_explicit, slug) from available inputs.

    ``issue_title`` is an optional pre-fetched title for an issue-ref ``plan``
    so callers that already resolved the issue (e.g. boss pipeline) don't trigger
    a second ``gh`` round-trip here.
    """
    if description:
        slug = slugify(description) or "gremlin"
        return description[:60], True, slug

    if plan and os.path.isfile(plan):
        h1 = read_markdown_title(plan)
        if h1:
            slug = slugify(h1) or "gremlin"
            return h1[:60], False, slug
        base = os.path.splitext(os.path.basename(plan))[0]
        slug = slugify(base) or "gremlin"
        return "", False, slug

    if plan:
        # Non-file plan arg (issue ref); try to fetch the issue title for a
        # meaningful slug. Best-effort: fall back to slug-from-ref on any error.
        title = issue_title
        if not title:
            data = fetch_issue(plan)
            if data:
                title = str(data.get("title") or "")
        if title:
            slug = slugify(title) or "gremlin"
            return title[:60], False, slug
        slug = slugify(plan) or "gremlin"
        return "", False, slug

    if instructions:
        slug = slugify(instructions[:80]) or "gremlin"
        return instructions[:60], False, slug

    return "", False, "gremlin"


def _build_spawn_env(gremlin_id: str) -> dict[str, str]:
    env = os.environ.copy()
    pkg_root = str(pathlib.Path(__file__).resolve().parent.parent)
    existing_pp = env.get("PYTHONPATH", "")
    parts = [p for p in [pkg_root, existing_pp] if p]
    env["PYTHONPATH"] = os.pathsep.join(parts)
    env["PYTHONSAFEPATH"] = "1"
    env["GREMLIN_ID"] = gremlin_id
    env["GREMLINS_OVERLAY_DIR"] = str(
        _state_root() / gremlin_id / _paths.OVERLAY_DIRNAME
    )
    return env


@dataclasses.dataclass
class _Inputs:
    gremlin_id: str
    kind: str
    plan: str | None
    instructions: str
    description: str
    description_explicit: bool
    parent_id: str
    project_root: str
    pipeline_path: str
    pipeline_args: list[str]
    client_label: str
    setup_kind: str
    base_ref_name: str
    base_ref_sha: str
    stage_inputs: dict[str, Any]
    issue_data: dict[str, Any] | None
    pr_num: str = ""


def _validate_plan_args(
    plan: str | None,
    instructions: str | None,
    spec_path: str | None,
) -> tuple[str | None, str | None]:
    if plan and instructions:
        raise ValueError("--plan and instructions are mutually exclusive")

    if plan and os.path.isfile(plan) and os.path.getsize(plan) == 0:
        raise ValueError(f"--plan: file is empty: {plan}")

    if spec_path is not None:
        if not os.path.isfile(spec_path):
            raise ValueError(f"--spec: file not found: {spec_path}")
        if os.path.getsize(spec_path) == 0:
            raise ValueError(f"--spec: file is empty: {spec_path}")
        spec_path = str(pathlib.Path(spec_path).resolve())

    if plan and os.path.isfile(plan):
        plan = str(pathlib.Path(plan).resolve())

    if plan and not os.path.isfile(plan):
        if os.sep in plan or plan.endswith(".md"):
            raise ValueError(f"--plan: file not found: {plan}")
        _, _issue_ref = parse_issue_ref(plan, "")
        if _issue_ref is None:
            raise ValueError(
                f"--plan: not a file or recognized issue ref (use #N or owner/repo#N): {plan}"
            )

    return plan, spec_path


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
        if loaded_pipeline is not None and loaded_pipeline.needs_gh():
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
    plan: str | None,
    description: str | None,
    parent_id: str | None,
    project_root: str | None,
    base_ref: str | None,
    pipeline_args: tuple[str, ...],
    spec_path: str | None,
    gremlin_id: str | None,
    pr: str | None = None,
) -> _Inputs:
    from gremlins.cli.pipeline_args import launch_client_label, resolve_pipeline

    instructions: str | None = stage_inputs.get("instructions")
    if plan is None:
        plan = stage_inputs.pop("plan", None)

    plan, spec_path = _validate_plan_args(plan, instructions, spec_path)

    issue_data: dict[str, Any] | None = None
    if plan and not os.path.isfile(plan):
        issue_data = fetch_issue(plan)

    desc, desc_explicit, slug = _resolve_description_and_slug(
        instructions,
        plan,
        description,
        issue_title=str((issue_data or {}).get("title") or ""),
    )

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
        and loaded_pipeline.needs_gh()
        and shutil.which("gh") is None
    ):
        raise RuntimeError("gh CLI not found on PATH (required for gh pipeline)")

    if pr is not None:
        from gremlins.utils.github import view_pr
        from gremlins.utils.pr import pr_arg_to_ref

        pr_ref = pr_arg_to_ref(pr)
        base_ref_name = ""
        base_ref_sha = pr_ref
        setup_kind = "worktree-detached-from-ref"
        pr_data = view_pr(pr, project_root=project_root)
        pr_url = pr_data.get("url") or ""
        pr_num_raw = pr_data.get("number")
        if not pr_url or pr_num_raw is None:
            raise RuntimeError(
                f"gh pr view returned empty url or number for {pr!r}: {pr_data!r}"
            )
        pr_num = str(pr_num_raw)
    else:
        base_ref_name, base_ref_sha = _resolve_base_ref(
            base_ref, project_root, loaded_pipeline
        )
        setup_kind = (
            loaded_pipeline.setup_kind()
            if loaded_pipeline is not None
            else "worktree-branch"
        )
        pr_num = ""

    stored_args = list(resolved_pipeline_args)
    if spec_path and "--spec" not in stored_args:
        stored_args = ["--spec", spec_path] + stored_args
    if plan and "--plan" not in stored_args:
        stored_args = ["--plan", plan] + stored_args

    client_label = launch_client_label(stored_args, loaded_pipeline)

    return _Inputs(
        gremlin_id=resolved_gremlin_id,
        kind=kind,
        plan=plan,
        instructions=instructions or "",
        description=desc,
        description_explicit=desc_explicit,
        parent_id=parent_id or "",
        project_root=project_root,
        pipeline_path=pipeline_path,
        pipeline_args=stored_args,
        client_label=client_label,
        setup_kind=setup_kind,
        base_ref_name=base_ref_name,
        base_ref_sha=base_ref_sha,
        stage_inputs=stage_inputs,
        issue_data=issue_data,
        pr_num=pr_num,
    )


def _prepare_state_dir(state_dir: pathlib.Path, inputs: _Inputs) -> None:
    state_dir.mkdir(parents=True, exist_ok=True)
    (state_dir / "instructions.txt").write_text(inputs.instructions, encoding="utf-8")
    if inputs.issue_data and inputs.issue_data.get("body"):
        artifacts_dir = state_dir / "artifacts"
        artifacts_dir.mkdir(exist_ok=True)
        (artifacts_dir / "plan.md").write_text(
            inputs.issue_data["body"], encoding="utf-8"
        )


def _initial_state_data(inputs: _Inputs) -> StateData:
    now_iso = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
    return StateData(
        gremlin_id=inputs.gremlin_id,
        kind=inputs.kind,
        project_root=inputs.project_root,
        workdir="",
        setup_kind=inputs.setup_kind,
        worktree_base="",
        status="running",
        started_at=now_iso,
        instructions=inputs.instructions[:200],
        description=inputs.description,
        description_explicit=inputs.description_explicit,
        parent_id=inputs.parent_id,
        pipeline_args=inputs.pipeline_args,
        client=inputs.client_label,
        pipeline_path=inputs.pipeline_path,
        stage="starting",
        pid=None,
        stage_inputs=inputs.stage_inputs,
        base_ref_name=inputs.base_ref_name,
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
    if inputs.instructions:
        spawn_args.append(inputs.instructions)
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


def launch(
    kind: str,
    *,
    stage_inputs: dict[str, Any] | None = None,
    plan: str | None = None,
    description: str | None = None,
    parent_id: str | None = None,
    project_root: str | None = None,
    base_ref: str | None = None,
    pipeline_args: tuple[str, ...] = (),
    spec_path: str | None = None,
    gremlin_id: str | None = None,
    pr: str | None = None,
    bypass: bool = False,
    permissions_file: str = "",
) -> tuple[str, subprocess.Popen[bytes]]:
    """Set up state dir, spawn the pipeline detached, return gremlin id and process.

    Worktree setup is deferred to the child process via Gremlin.initialize_with_runtime().
    Synchronous through spawn; does not wait for the pipeline to finish.
    Raises ValueError on bad arguments, RuntimeError on infrastructure failure.
    """
    inputs = _resolve_inputs(
        kind,
        {} if stage_inputs is None else dict(stage_inputs),
        plan,
        description,
        parent_id,
        project_root,
        base_ref,
        pipeline_args,
        spec_path,
        gremlin_id,
        pr,
    )
    state_dir = _state_root() / inputs.gremlin_id
    try:
        _prepare_state_dir(state_dir, inputs)
        inputs.pipeline_path = _persist_expanded_pipeline(
            state_dir, inputs.pipeline_path
        )
        sd = _initial_state_data(inputs)
        sd.bypass = bypass
        sd.permissions_file = permissions_file
        sd.persist(state_dir)
        session_dir = state_dir / "artifacts"
        session_dir.mkdir(parents=True, exist_ok=True)
        registry = ArtifactRegistry(session_dir=session_dir)
        if inputs.pr_num:
            registry.bind("pr", Uri.parse(f"gh://pr/{inputs.pr_num}"))
        if inputs.base_ref_sha:
            registry.bind("base_sha", Uri.parse(f"git://commit/{inputs.base_ref_sha}"))
        registry.bind("spec", Uri.parse("file://session/spec.md"))
        if inputs.issue_data:
            _issue_num = str(inputs.issue_data.get("number", ""))
            if _issue_num:
                registry.bind("plan", Uri.parse(f"gh://issue/{_issue_num}"))
        p = _spawn(inputs.gremlin_id, inputs, state_dir)
    except Exception:
        shutil.rmtree(state_dir, ignore_errors=True)
        raise

    (state_dir / "pid").write_text(str(p.pid), encoding="utf-8")
    StateData.load(inputs.gremlin_id).patch(pid=p.pid)

    return inputs.gremlin_id, p


def _load_resume_state(gremlin_id: str) -> tuple[pathlib.Path, dict[str, Any]]:
    state_dir = _state_root() / gremlin_id
    sf = state_dir / "state.json"
    if not state_dir.is_dir() or not sf.is_file():
        raise RuntimeError(f"no state at {state_dir}")
    try:
        state = json.loads(sf.read_text(encoding="utf-8"))
    except Exception as exc:
        raise RuntimeError(f"could not read state.json: {exc}") from exc
    return state_dir, state


def _check_resume_preconditions(
    gremlin_id: str, state_dir: pathlib.Path, state: dict[str, Any], graft: str | None
) -> None:
    status = state.get("status", "")
    old_pid = state.get("pid")
    exit_code = state.get("exit_code")
    workdir = state.get("workdir", "")

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

    if graft is None and (state_dir / "finished").is_file() and exit_code == 0:
        raise RuntimeError(
            f"gremlin {gremlin_id} finished successfully — nothing to resume"
        )

    if workdir and not os.path.isdir(workdir):
        raise RuntimeError(f"worktree missing: {workdir}")


def _resolve_resume_pipeline(
    state: dict[str, Any], state_dir: pathlib.Path
) -> tuple[list[str], str, str]:
    from gremlins.cli.pipeline_args import resolve_pipeline

    kind = state.get("kind", "")
    pipeline_args = cast(list[str], state.get("pipeline_args") or [])
    pipeline_path = str(state.get("pipeline_path") or "")
    project_root = str(state.get("project_root") or _paths.project_root())

    try:
        pipeline_args, pipeline_path = resolve_pipeline(
            kind, tuple(pipeline_args), project_root
        )
    except FileNotFoundError:
        pass

    hermetic = state_dir / "pipeline.yaml"
    if hermetic.is_file():
        pipeline_path = str(hermetic)

    return pipeline_args, pipeline_path, project_root


def _load_pipeline_and_check_gh(
    gremlin_id: str, state_dir: pathlib.Path, project_root: str, pipeline_path: str
) -> Any:
    pipeline_data = None
    if pipeline_path:
        try:
            pipeline_data = _PipelineData.from_yaml(
                resolve_pipeline_path(pipeline_path, pathlib.Path(project_root))
            )
        except (FileNotFoundError, OSError, ValueError):
            pass

    if (
        pipeline_data is not None
        and pipeline_data.needs_gh()
        and shutil.which("gh") is None
    ):
        raise RuntimeError("gh CLI not found on PATH (required for gh pipeline)")

    return pipeline_data


def _determine_stage(state: dict[str, Any], pipeline_data: Any) -> str:
    stage = str(state.get("stage", ""))
    if not stage or stage == "starting":
        stage = "plan"
    if (
        pipeline_data is not None
        and pipeline_data.uses_loop_handoff()
        and stage not in ("review-chain", "address-chain")
    ):
        stage = "chain"
    return stage


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
    rescue_count = 0
    try:
        rescue_count = int(state.get("rescue_count") or 0)
    except (ValueError, TypeError):
        pass

    StateData.load(gremlin_id).patch(
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
        rescued_at=now_iso,
        resumed_from_stage=stage,
        rescue_count=rescue_count,
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
    state: dict[str, Any],
    pipeline_path: str,
    pipeline_args: list[str],
    stage: str,
    project_root: str,
) -> Any:
    has_plan = any(a == "--plan" or str(a).startswith("--plan=") for a in pipeline_args)

    spawn_args: list[str] = list(pipeline_args)
    if not has_plan:
        instr_file = state_dir / "instructions.txt"
        if instr_file.is_file():
            instructions = instr_file.read_text(encoding="utf-8")
        else:
            instructions = str(state.get("instructions") or "")
        if instructions:
            spawn_args.append(instructions)

    env = _build_spawn_env(gremlin_id)
    env["GREMLINS_RESUME_FROM"] = stage

    cmd = [
        sys.executable,
        "-m",
        "gremlins.spawn.pipeline",
        gremlin_id,
        pipeline_path,
        *spawn_args,
    ]
    return _spawn_logged_process(
        cmd, project_root, env, state_dir / "log", log_mode="a"
    )


def resume(gremlin_id: str, *, graft: str | None = None) -> None:
    state_dir, state = _load_resume_state(gremlin_id)
    _check_resume_preconditions(gremlin_id, state_dir, state, graft)
    pipeline_args, pipeline_path, project_root = _resolve_resume_pipeline(
        state, state_dir
    )
    pipeline_data = _load_pipeline_and_check_gh(
        gremlin_id, state_dir, project_root, pipeline_path
    )
    stage = _determine_stage(state, pipeline_data)
    if graft is not None:
        stage = _append_graft(state_dir, graft, project_root)
    _patch_state_for_resume(
        gremlin_id,
        state_dir,
        state,
        stage,
        pipeline_args,
        pipeline_path,
    )
    p = _spawn_resume(
        gremlin_id, state_dir, state, pipeline_path, pipeline_args, stage, project_root
    )
    (state_dir / "pid").write_text(str(p.pid), encoding="utf-8")
    StateData.load(gremlin_id).patch(pid=p.pid)
