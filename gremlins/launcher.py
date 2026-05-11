"""Launcher for background gremlins.

Public API:
    launch(kind, *, stage_inputs=None, plan=None, description=None,
           parent_id=None, project_root=None, base_ref="HEAD",
           pipeline_args=()) -> str
    resume(gr_id) -> None
    write_terminal_state(gr_id, exit_code) -> None
"""

from __future__ import annotations

import datetime
import json
import os
import pathlib
import secrets
import shutil
import subprocess
import sys
import tempfile
from typing import Any, cast

from gremlins import paths as _paths
from gremlins.clients.client import PACKAGE_DEFAULT
from gremlins.executor.state import patch_state, pipeline_uses_gh, write_state
from gremlins.pipeline import Pipeline
from gremlins.utils import git as _git_mod
from gremlins.utils import proc
from gremlins.utils.github import fetch_issue, parse_issue_ref
from gremlins.utils.text import read_markdown_title, slugify


class GremlinAlreadyRunning(RuntimeError):
    pass


def _state_root() -> pathlib.Path:
    return _paths.state_root()


def pipeline_uses_loop_handoff(pipeline: Pipeline) -> bool:
    first = pipeline.stages[0] if pipeline.stages else None
    return (
        first is not None
        and first.type == "loop"
        and any(b.type == "handoff" for b in (first.body or []))
    )


def _pipeline_setup_kind(pipeline: Pipeline) -> str:
    if pipeline_uses_gh(pipeline) or pipeline_uses_loop_handoff(pipeline):
        return "worktree-detached"
    return "worktree-branch"


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



def _stage_gremlins_overlay(project_root: str, state_dir: pathlib.Path) -> None:
    src = pathlib.Path(project_root) / ".gremlins"
    if src.is_dir():
        shutil.copytree(src, state_dir / ".gremlins", dirs_exist_ok=True)


def _setup_workdir(
    setup_kind: str,
    project_root: str,
    base_ref_sha: str,
    gr_id: str,
    state_dir: pathlib.Path,
) -> tuple[str, str, str, str]:
    """Return (workdir, branch, worktree_base, setup_kind)."""
    if not _git_mod.in_git_repo(cwd=project_root):
        return _git_mod.setup_copy(project_root), "", "", "copy"

    if setup_kind == "worktree-branch":
        workdir, branch = _setup_named_worktree(project_root, gr_id, base_ref_sha)
        _stage_gremlins_overlay(project_root, state_dir)
        return workdir, branch, "", "worktree-branch"

    # worktree-detached (gh, boss)
    workdir = _git_mod.setup_detached_worktree(project_root, base_ref_sha or "HEAD")
    _stage_gremlins_overlay(project_root, state_dir)
    return workdir, "", base_ref_sha, "worktree"


def _setup_named_worktree(
    project_root: str, gr_id: str, base_ref_sha: str
) -> tuple[str, str]:
    workdir = tempfile.mkdtemp(prefix="aibg-localgremlin.")
    os.rmdir(workdir)
    branch = f"bg/local/{gr_id}"
    r = proc.run(
        ["git", "worktree", "add", "-b", branch, workdir, base_ref_sha or "HEAD"],
        cwd=project_root,
    )
    if r.returncode != 0:
        raise RuntimeError(f"git worktree add -b {branch!r} failed: {r.stderr.strip()}")
    return workdir, branch


def _build_spawn_env(gr_id: str) -> dict[str, str]:
    env = os.environ.copy()
    pkg_root = str(pathlib.Path(__file__).resolve().parent.parent)
    existing_pp = env.get("PYTHONPATH", "")
    parts = [p for p in [pkg_root, existing_pp] if p]
    env["PYTHONPATH"] = os.pathsep.join(parts)
    env["PYTHONSAFEPATH"] = "1"
    env["GR_ID"] = gr_id
    env["GREMLINS_OVERLAY_DIR"] = str(_state_root() / gr_id / ".gremlins")
    return env


def _spawn_pipeline(
    state_dir: pathlib.Path,
    workdir: str,
    gr_id: str,
    pipeline_path: str,
    pipeline_args: list[str],
    log_mode: str = "w",
) -> subprocess.Popen[bytes]:
    """Spawn the pipeline detached. Returns the Popen object (already running).

    log_mode: "w" (truncate, default for first launch) or "a" (append, for resumes).
    """
    cmd = [
        sys.executable,
        "-m",
        "gremlins.run_pipeline",
        gr_id,
        pipeline_path,
        *pipeline_args,
    ]
    env = _build_spawn_env(gr_id)
    log_path = state_dir / "log"
    log_fh = open(log_path, log_mode)
    try:
        proc = subprocess.Popen(
            cmd,
            cwd=workdir,
            stdin=subprocess.DEVNULL,
            stdout=log_fh,
            stderr=log_fh,
            start_new_session=True,
            env=env,
        )
    finally:
        log_fh.close()
    return proc


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
) -> str:
    """Set up state dir + worktree, spawn the pipeline detached, return gremlin id.

    Synchronous through spawn; does not wait for the pipeline to finish.
    Raises ValueError on bad arguments, RuntimeError on infrastructure failure.
    """
    stage_inputs = {} if stage_inputs is None else dict(stage_inputs)
    instructions: str | None = stage_inputs.get("instructions")
    if plan is None:
        plan = stage_inputs.pop("plan", None)
    if plan and instructions:
        raise ValueError("--plan and instructions are mutually exclusive")
    if shutil.which("claude") is None:
        raise RuntimeError("claude CLI not found on PATH")

    if plan and os.path.isfile(plan) and os.path.getsize(plan) == 0:
        raise ValueError(f"--plan: file is empty: {plan}")

    # Validate and normalize spec_path to absolute
    if spec_path is not None:
        if not os.path.isfile(spec_path):
            raise ValueError(f"--spec: file not found: {spec_path}")
        if os.path.getsize(spec_path) == 0:
            raise ValueError(f"--spec: file is empty: {spec_path}")
        spec_path = str(pathlib.Path(spec_path).resolve())

    # Normalize plan path to absolute
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

    issue_data: dict[str, Any] | None = None
    if plan and not os.path.isfile(plan):
        issue_data = fetch_issue(plan)

    desc, desc_explicit, slug = _resolve_description_and_slug(
        instructions,
        plan,
        description,
        issue_title=str((issue_data or {}).get("title") or ""),
    )
    rand_hex = secrets.token_hex(3)
    gr_id = f"{slug}-{rand_hex}"

    if project_root is None:
        r = proc.run(["git", "rev-parse", "--show-toplevel"])
        if r.returncode == 0 and r.stdout.strip():
            project_root = r.stdout.strip()
        else:
            project_root = os.getcwd()

    from gremlins.cli.pipeline_args import launch_client_label, resolve_pipeline

    resolved_pipeline_args, pipeline_path = resolve_pipeline(
        kind, pipeline_args, project_root
    )

    _pipeline_base_ref = "current"
    _loaded_pipeline = None
    try:
        _loaded_pipeline = Pipeline.from_yaml(pathlib.Path(pipeline_path))
        _pipeline_base_ref = _loaded_pipeline.base_ref
    except (FileNotFoundError, OSError):
        pass

    if (
        _loaded_pipeline is not None
        and pipeline_uses_gh(_loaded_pipeline)
        and shutil.which("gh") is None
    ):
        raise RuntimeError("gh CLI not found on PATH (required for gh pipeline)")

    state_dir = _state_root() / gr_id
    state_dir.mkdir(parents=True, exist_ok=True)

    _effective_base_ref = base_ref if base_ref is not None else _pipeline_base_ref
    if _git_mod.in_git_repo(cwd=project_root):
        if _loaded_pipeline is not None and pipeline_uses_gh(_loaded_pipeline):
            _branch = _effective_base_ref.removeprefix("origin/")
            if _branch and _branch not in ("current", "HEAD"):
                try:
                    _git_mod.fetch_origin(_branch, cwd=project_root)
                except _git_mod.GitError as exc:
                    raise RuntimeError(
                        f"git fetch origin {_branch} failed: {exc}"
                    ) from exc
        try:
            base_ref_name, base_ref_sha = _git_mod.resolve_base_ref(
                _effective_base_ref, cwd=project_root
            )
        except _git_mod.GitError as exc:
            raise RuntimeError(f"--base-ref: {exc}") from exc
    else:
        base_ref_name, base_ref_sha = _effective_base_ref, ""

    workdir = None
    try:
        # pipeline_args for state.json: includes --plan and --spec when set
        stored_pipeline_args = list(resolved_pipeline_args)
        if spec_path and "--spec" not in stored_pipeline_args:
            stored_pipeline_args = ["--spec", spec_path] + stored_pipeline_args
        if plan and "--plan" not in stored_pipeline_args:
            stored_pipeline_args = ["--plan", plan] + stored_pipeline_args

        # Persist full instructions to sidecar (state.json truncates to 200 chars)
        instr_raw = instructions or ""
        (state_dir / "instructions.txt").write_text(instr_raw, encoding="utf-8")

        if issue_data and issue_data.get("body"):
            artifacts_dir = state_dir / "artifacts"
            artifacts_dir.mkdir(exist_ok=True)
            (artifacts_dir / "plan.md").write_text(issue_data["body"], encoding="utf-8")

        _setup_kind_arg = (
            _pipeline_setup_kind(_loaded_pipeline)
            if _loaded_pipeline is not None
            else "worktree-branch"
        )
        workdir, branch, worktree_base, setup_kind = _setup_workdir(
            _setup_kind_arg, project_root, base_ref_sha, gr_id, state_dir
        )

        now_iso = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
        state = {
            "id": gr_id,
            "kind": kind,
            "project_root": project_root,
            "workdir": workdir,
            "setup_kind": setup_kind,
            "worktree_base": worktree_base,
            "status": "running",
            "started_at": now_iso,
            "instructions": instr_raw[:200],
            "description": desc,
            "description_explicit": desc_explicit,
            "parent_id": parent_id or "",
            "pipeline_args": stored_pipeline_args,
            "client": launch_client_label(stored_pipeline_args, _loaded_pipeline),
            "pipeline_path": pipeline_path,
            "stage": "starting",
            "pid": None,
            "stage_inputs": stage_inputs,
            "base_ref_name": base_ref_name,
            "base_ref_sha": base_ref_sha,
            "issue_url": str(issue_data.get("url", "")) if issue_data else "",
            "issue_num": str(issue_data.get("number", "")) if issue_data else "",
        }
        write_state(state_dir, state)

        if setup_kind == "worktree-branch" and branch:
            from gremlins.executor.state import append_artifact

            append_artifact(gr_id, {"type": "branch", "name": branch})

        # Build args for the spawned _run-pipeline process
        spawn_args = list(stored_pipeline_args)
        if instructions:
            spawn_args.append(instructions)

        p = _spawn_pipeline(state_dir, workdir, gr_id, pipeline_path, spawn_args)
    except Exception:
        shutil.rmtree(state_dir, ignore_errors=True)
        if workdir:
            try:
                _git_mod.remove_worktree(project_root, workdir)
            except Exception:
                pass
        raise

    (state_dir / "pid").write_text(str(p.pid), encoding="utf-8")
    patch_state(gr_id, pid=p.pid)

    return gr_id


def resume(gr_id: str) -> None:
    """Re-spawn the pipeline for an existing gremlin from its recorded stage.

    Raises RuntimeError on precondition violations or spawn failure.
    """
    state_dir = _state_root() / gr_id
    sf = state_dir / "state.json"
    if not state_dir.is_dir() or not sf.is_file():
        raise RuntimeError(f"no state at {state_dir}")

    try:
        state = json.loads(sf.read_text(encoding="utf-8"))
    except Exception as exc:
        raise RuntimeError(f"could not read state.json: {exc}") from exc

    kind = state.get("kind", "")
    workdir = state.get("workdir", "")
    stage = state.get("stage", "")
    status = state.get("status", "")
    old_pid = state.get("pid")
    exit_code = state.get("exit_code")

    if not workdir or not os.path.isdir(workdir):
        raise RuntimeError(f"worktree missing: {workdir}")

    if status == "running" and old_pid is not None:
        try:
            os.kill(int(old_pid), 0)
            raise GremlinAlreadyRunning(
                f"gremlin {gr_id} is still running (pid {old_pid}) — stop it first"
            )
        except (OSError, ValueError):
            pass  # process is gone

    if (state_dir / "finished").is_file() and exit_code == 0:
        raise RuntimeError(f"gremlin {gr_id} finished successfully — nothing to resume")

    pipeline_args = cast(list[str], state.get("pipeline_args") or [])
    pipeline_path = str(state.get("pipeline_path") or "")
    project_root = str(state.get("project_root") or os.getcwd())
    from gremlins.cli.pipeline_args import resolve_pipeline

    try:
        pipeline_args, pipeline_path = resolve_pipeline(
            kind, tuple(pipeline_args), project_root
        )
    except FileNotFoundError:
        pass

    _loaded_resume = None
    if pipeline_path:
        try:
            _loaded_resume = Pipeline.from_yaml(pathlib.Path(pipeline_path))
        except (FileNotFoundError, OSError):
            pass

    if (
        _loaded_resume is not None
        and pipeline_uses_gh(_loaded_resume)
        and shutil.which("gh") is None
    ):
        raise RuntimeError("gh CLI not found on PATH (required for gh pipeline)")

    # Rewind stage if it never advanced past "starting"
    if not stage or stage == "starting":
        stage = "plan"

    if (
        _loaded_resume is not None
        and pipeline_uses_loop_handoff(_loaded_resume)
        and stage not in ("review-chain", "address-chain")
    ):
        stage = "chain"

    # Clear terminal markers and patch state for the resumed run
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

    patch_state(
        gr_id,
        _delete=(
            "exit_code",
            "ended_at",
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
        rescue_count=rescue_count + 1,
        pid=None,
        pipeline_args=pipeline_args,
        pipeline_path=pipeline_path,
        client=str(state.get("client") or "") or str(PACKAGE_DEFAULT),
    )

    # Append resume header to log
    try:
        with open(state_dir / "log", "a", encoding="utf-8") as f:
            f.write(f"\n--- resume at {now_iso} (from stage: {stage}) ---\n")
    except OSError:
        pass

    has_plan = any(a == "--plan" or str(a).startswith("--plan=") for a in pipeline_args)

    spawn_args: list[str] = list(pipeline_args)
    spawn_args.extend(["--resume-from", str(stage)])
    if not has_plan:
        instr_file = state_dir / "instructions.txt"
        if instr_file.is_file():
            instructions = instr_file.read_text(encoding="utf-8")
        else:
            instructions = str(state.get("instructions") or "")
        if instructions:
            spawn_args.append(instructions)

    proc = _spawn_pipeline(
        state_dir,
        workdir,
        gr_id,
        pipeline_path,
        spawn_args,
        log_mode="a",
    )

    (state_dir / "pid").write_text(str(proc.pid), encoding="utf-8")
    patch_state(gr_id, pid=proc.pid)


def write_terminal_state(gr_id: str, exit_code: int) -> None:
    """Record terminal outcome for a finished pipeline run.

    Called by the _run-pipeline subcommand's finally block. Mirrors finish.sh:
    touches the finished marker, patches state.json, and on success removes
    the worktree for gh-mode pipelines only. Best-effort throughout.
    """
    state_dir = _state_root() / gr_id

    # Touch finished marker first — the session-summary hook watches this.
    try:
        (state_dir / "finished").touch()
    except OSError:
        pass

    now_iso = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
    status = "done" if exit_code == 0 else "stopped"
    try:
        patch_state(gr_id, status=status, ended_at=now_iso, exit_code=exit_code)
    except Exception:
        pass
