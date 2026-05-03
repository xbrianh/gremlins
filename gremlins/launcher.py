"""Launcher for background gremlins.

Public API:
    launch(kind, *, instructions=None, plan=None, description=None,
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
import re
import secrets
import shutil
import subprocess
import sys
from typing import Any, cast

from . import git as _git_mod

VALID_KINDS = {"ghgremlin", "localgremlin", "bossgremlin"}

_KIND_SUBCOMMAND = {
    "localgremlin": "_local",
    "ghgremlin": "_gh",
    "bossgremlin": "_boss",
}


def _state_root() -> pathlib.Path:
    return (
        pathlib.Path(
            os.environ.get("XDG_STATE_HOME")
            or os.path.join(os.path.expanduser("~"), ".local", "state")
        )
        / "claude-gremlins"
    )


def _slugify(text: str, max_len: int = 40) -> str:
    """Reduce arbitrary text to [a-z0-9-]+, at most max_len chars."""
    slug = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    slug = re.sub(r"-+", "-", slug)
    if len(slug) > max_len:
        trimmed = slug[:max_len].rstrip("-")
        head, _, _ = trimmed.rpartition("-")
        if head and len(head) >= 20:
            trimmed = head
        slug = trimmed
    return slug


def _extract_h1(path: str) -> str:
    """Extract the first # heading text from a file. Returns '' on failure."""
    try:
        with open(path, encoding="utf-8") as f:
            for line in f:
                m = re.match(r"^#+\s+(.+)", line)
                if m:
                    return m.group(1).strip()
    except OSError:
        pass
    return ""


def _resolve_description_and_slug(
    kind: str,
    instructions: str | None,
    plan: str | None,
    description: str | None,
) -> tuple[str, bool, str]:
    """Return (description, description_explicit, slug) from available inputs."""
    if description:
        slug = _slugify(description) or "gremlin"
        return description[:60], True, slug

    if plan and os.path.isfile(plan):
        h1 = _extract_h1(plan)
        if h1:
            slug = _slugify(h1) or "gremlin"
            return h1[:60], False, slug
        base = os.path.splitext(os.path.basename(plan))[0]
        slug = _slugify(base) or "gremlin"
        return "", False, slug

    if plan:
        # Non-file plan arg (issue ref); orchestrator fills description later.
        slug = _slugify(plan) or "gremlin"
        return "", False, slug

    if instructions:
        slug = _slugify(instructions[:80]) or "gremlin"
        return instructions[:60], False, slug

    return "", False, "gremlin"


def _write_state(state_dir: pathlib.Path, data: dict[str, Any]) -> None:
    """Atomically write state.json."""
    sf = state_dir / "state.json"
    tmp = state_dir / f"state.json.{os.getpid()}.{secrets.token_hex(4)}.tmp"
    tmp.write_text(json.dumps(data), encoding="utf-8")
    os.replace(tmp, sf)


def _patch_state(state_dir: pathlib.Path, **fields: Any) -> None:
    """Merge fields into state.json atomically. Best-effort."""
    sf = state_dir / "state.json"
    try:
        data = json.loads(sf.read_text(encoding="utf-8"))
        data.update(fields)
        tmp = state_dir / f"state.json.{os.getpid()}.{secrets.token_hex(4)}.tmp"
        tmp.write_text(json.dumps(data), encoding="utf-8")
        os.replace(tmp, sf)
    except Exception:
        pass


def _default_pipeline_path(kind: str) -> str:
    # bossgremlin has no per-pipeline YAML; exclude it explicitly
    name = _KIND_SUBCOMMAND.get(kind)
    if name is None or kind == "bossgremlin":
        return ""
    p = (
        pathlib.Path(__file__).resolve().parent
        / "pipelines"
        / f"{name.removeprefix('_')}.yaml"
    )
    return str(p)


def _extract_impl_model(pipeline_args: list[str], kind: str) -> str:
    """Extract the implementation model alias from pipeline_args.

    Returns the alias as passed (e.g. 'opus', 'sonnet') or 'sonnet' as the
    default when no model flag is present — matching each orchestrator's default.
    """
    args = list(pipeline_args)
    if kind == "localgremlin":
        for i, a in enumerate(args):
            if a == "-i" and i + 1 < len(args):
                return args[i + 1]
        return "sonnet"
    else:  # ghgremlin, bossgremlin use --model
        for i, a in enumerate(args):
            if a == "--model" and i + 1 < len(args):
                return args[i + 1]
            if a.startswith("--model="):
                return a[len("--model=") :]
        return "sonnet"


def _delete_patch_state(
    state_dir: pathlib.Path, delete_keys: tuple[str, ...], **fields: Any
) -> None:
    """Remove delete_keys and merge fields into state.json atomically. Best-effort."""
    sf = state_dir / "state.json"
    try:
        data = json.loads(sf.read_text(encoding="utf-8"))
        for k in delete_keys:
            data.pop(k, None)
        data.update(fields)
        tmp = state_dir / f"state.json.{os.getpid()}.{secrets.token_hex(4)}.tmp"
        tmp.write_text(json.dumps(data), encoding="utf-8")
        os.replace(tmp, sf)
    except Exception:
        pass


def _build_spawn_env(gr_id: str) -> dict[str, str]:
    """Build the environment for the spawned pipeline process."""
    env = os.environ.copy()
    claude_home = os.path.join(os.path.expanduser("~"), ".claude")
    # Parent of the gremlins package directory — ensures the subprocess imports
    # from the same source tree as the current process even in dev worktrees
    # where ~/.claude/gremlins/ symlinks may be stale or absent.
    pkg_root = str(pathlib.Path(__file__).resolve().parent.parent)
    existing_pp = env.get("PYTHONPATH", "")
    parts = [p for p in [pkg_root, claude_home, existing_pp] if p]
    env["PYTHONPATH"] = os.pathsep.join(parts)
    env["PYTHONSAFEPATH"] = "1"
    env["GR_ID"] = gr_id
    return env


def _spawn_pipeline(
    state_dir: pathlib.Path,
    workdir: str,
    gr_id: str,
    kind_subcommand: str,
    pipeline_args: list[str],
    log_mode: str = "w",
) -> subprocess.Popen[bytes]:
    """Spawn the pipeline detached. Returns the Popen object (already running).

    log_mode: "w" (truncate, default for first launch) or "a" (append, for resumes).
    """
    cmd = [
        sys.executable,
        "-m",
        "gremlins.cli",
        "_run-pipeline",
        gr_id,
        kind_subcommand,
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
    instructions: str | None = None,
    plan: str | None = None,
    description: str | None = None,
    parent_id: str | None = None,
    project_root: str | None = None,
    base_ref: str = "HEAD",
    pipeline_args: tuple[str, ...] = (),
    spec_path: str | None = None,
) -> str:
    """Set up state dir + worktree, spawn the pipeline detached, return gremlin id.

    Synchronous through spawn; does not wait for the pipeline to finish.
    Raises ValueError on bad arguments, RuntimeError on infrastructure failure.
    """
    if kind not in VALID_KINDS:
        raise ValueError(
            f"invalid kind: {kind!r} (allowed: {', '.join(sorted(VALID_KINDS))})"
        )
    if plan and instructions:
        raise ValueError("--plan and instructions are mutually exclusive")
    if shutil.which("claude") is None:
        raise RuntimeError("claude CLI not found on PATH")
    if kind == "ghgremlin" and shutil.which("gh") is None:
        raise RuntimeError("gh CLI not found on PATH (required for ghgremlin)")

    # Validate localgremlin --plan before touching state
    if kind == "localgremlin" and plan:
        if not os.path.isfile(plan):
            raise ValueError(f"--plan: file not found: {plan}")
        if os.path.getsize(plan) == 0:
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

    desc, desc_explicit, slug = _resolve_description_and_slug(
        kind, instructions, plan, description
    )
    rand_hex = secrets.token_hex(3)
    gr_id = f"{slug}-{rand_hex}"

    if project_root is None:
        r = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            check=False,
        )
        if r.returncode == 0 and r.stdout.strip():
            project_root = r.stdout.strip()
        else:
            project_root = os.getcwd()

    state_dir = _state_root() / gr_id
    state_dir.mkdir(parents=True, exist_ok=True)

    # pipeline_args for state.json: includes --plan and --spec when set
    stored_pipeline_args = list(pipeline_args)
    if spec_path and "--spec" not in stored_pipeline_args:
        stored_pipeline_args = ["--spec", spec_path] + stored_pipeline_args
    if plan and "--plan" not in stored_pipeline_args:
        stored_pipeline_args = ["--plan", plan] + stored_pipeline_args

    # Persist full instructions to sidecar (state.json truncates to 200 chars)
    instr_raw = instructions or ""
    (state_dir / "instructions.txt").write_text(instr_raw, encoding="utf-8")

    # Worktree setup
    branch = ""
    worktree_base = ""
    if _git_mod.is_git_repo(project_root):
        if kind == "localgremlin":
            setup_kind = "worktree-branch"
            workdir, branch = _git_mod.setup_worktree_branch(
                project_root, gr_id, base_ref=base_ref
            )
        elif kind == "ghgremlin":
            default_branch = _git_mod.resolve_default_branch(project_root)
            refspec = (
                f"refs/heads/{default_branch}:refs/remotes/origin/{default_branch}"
            )
            fr = subprocess.run(
                ["git", "-C", project_root, "fetch", "origin", "--quiet", refspec],
                capture_output=True,
                text=True,
            )
            if fr.returncode != 0:
                raise RuntimeError(
                    f"git fetch origin {default_branch} failed: {fr.stderr.strip()}"
                )
            worktree_base = f"origin/{default_branch}"
            setup_kind = "worktree"
            workdir = _git_mod.setup_detached_worktree(project_root, worktree_base)
        else:  # bossgremlin
            setup_kind = "worktree"
            workdir = _git_mod.setup_detached_worktree(project_root, base_ref)
    else:
        setup_kind = "copy"
        workdir = _git_mod.setup_copy(project_root)

    now_iso = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
    state = {
        "id": gr_id,
        "kind": kind,
        "project_root": project_root,
        "workdir": workdir,
        "setup_kind": setup_kind,
        "branch": branch,
        "worktree_base": worktree_base,
        "status": "running",
        "started_at": now_iso,
        "instructions": instr_raw[:200],
        "description": desc,
        "description_explicit": desc_explicit,
        "parent_id": parent_id or "",
        "pipeline_args": stored_pipeline_args,
        "impl_model": _extract_impl_model(stored_pipeline_args, kind),
        "pipeline_path": _default_pipeline_path(kind),
        "stage": "starting",
        "pid": None,
    }
    _write_state(state_dir, state)

    # Build args for the spawned _run-pipeline process
    spawn_args = list(stored_pipeline_args)
    if instructions:
        spawn_args.append(instructions)

    proc = _spawn_pipeline(
        state_dir, workdir, gr_id, _KIND_SUBCOMMAND[kind], spawn_args
    )

    (state_dir / "pid").write_text(str(proc.pid), encoding="utf-8")
    _patch_state(state_dir, pid=proc.pid)

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

    if kind not in VALID_KINDS:
        raise RuntimeError(f"invalid kind in state.json: {kind!r}")
    if not workdir or not os.path.isdir(workdir):
        raise RuntimeError(f"worktree missing: {workdir}")
    if kind == "ghgremlin" and shutil.which("gh") is None:
        raise RuntimeError("gh CLI not found on PATH (required for ghgremlin)")

    if status == "running" and old_pid is not None:
        try:
            os.kill(int(old_pid), 0)
            raise RuntimeError(
                f"gremlin {gr_id} is still running (pid {old_pid}) — stop it first"
            )
        except (OSError, ValueError):
            pass  # process is gone

    if (state_dir / "finished").is_file() and exit_code == 0:
        raise RuntimeError(f"gremlin {gr_id} finished successfully — nothing to resume")

    # Rewind stage if it never advanced past "starting"
    if not stage or stage == "starting":
        stage = "plan"

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

    _delete_patch_state(
        state_dir,
        (
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
    )

    # Append resume header to log
    try:
        with open(state_dir / "log", "a", encoding="utf-8") as f:
            f.write(f"\n--- resume at {now_iso} (from stage: {stage}) ---\n")
    except OSError:
        pass

    # Rehydrate pipeline args from state.json
    pipeline_args = cast(list[str], state.get("pipeline_args") or [])
    has_plan = any(a == "--plan" or str(a).startswith("--plan=") for a in pipeline_args)

    # Build spawn args: pipeline_args + --resume-from + (instructions if no plan)
    spawn_args: list[str] = list(pipeline_args) + ["--resume-from", str(stage)]
    if not has_plan:
        instr_file = state_dir / "instructions.txt"
        if instr_file.is_file():
            instructions = instr_file.read_text(encoding="utf-8")
        else:
            instructions = str(state.get("instructions") or "")
        if instructions:
            spawn_args.append(instructions)

    kind_subcommand = _KIND_SUBCOMMAND[kind]
    proc = _spawn_pipeline(
        state_dir, workdir, gr_id, kind_subcommand, spawn_args, log_mode="a"
    )

    (state_dir / "pid").write_text(str(proc.pid), encoding="utf-8")
    _patch_state(state_dir, pid=proc.pid)


def write_terminal_state(gr_id: str, exit_code: int) -> None:
    """Record terminal outcome for a finished pipeline run.

    Called by the _run-pipeline subcommand's finally block. Mirrors finish.sh:
    touches the finished marker, patches state.json, and on success removes
    the worktree (except bossgremlin). Best-effort throughout.
    """
    state_dir = _state_root() / gr_id
    sf = state_dir / "state.json"

    # Touch finished marker first — the session-summary hook watches this.
    try:
        (state_dir / "finished").touch()
    except OSError:
        pass

    now_iso = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
    status = "done" if exit_code == 0 else "stopped"
    try:
        data = json.loads(sf.read_text(encoding="utf-8"))
        data["status"] = status
        data["ended_at"] = now_iso
        data["exit_code"] = exit_code
        tmp = sf.with_name(f"state.json.{os.getpid()}.{secrets.token_hex(4)}.tmp")
        tmp.write_text(json.dumps(data), encoding="utf-8")
        os.replace(tmp, sf)
    except Exception:
        pass

    # Worktree cleanup on success for non-bossgremlin
    if exit_code == 0:
        try:
            data = json.loads(sf.read_text(encoding="utf-8"))
            kind = data.get("kind", "")
            project_root = data.get("project_root", "")
            workdir = data.get("workdir", "")
            setup_kind = data.get("setup_kind", "")
            if (
                kind != "bossgremlin"
                and setup_kind in ("worktree", "worktree-branch")
                and project_root
                and workdir
            ):
                _git_mod.remove_worktree(project_root, workdir)
        except Exception:
            pass
