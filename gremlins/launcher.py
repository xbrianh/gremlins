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
import re
import secrets
import shutil
import subprocess
import sys
from typing import Any, cast

import yaml

from gremlins import git as _git_mod
from gremlins import paths as _paths
from gremlins.clients import PACKAGE_DEFAULT
from gremlins.gh_utils import parse_issue_ref, view_issue
from gremlins.pipeline import (
    Pipeline,
    load_pipeline,
    resolve_pipeline_path,
)


class GremlinAlreadyRunning(RuntimeError):
    pass


def _state_root() -> pathlib.Path:
    return _paths.state_root()


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
    """Extract the first H1 heading text from a file. Returns '' on failure."""
    try:
        with open(path, encoding="utf-8") as f:
            for line in f:
                m = re.match(r"^#\s+(.+)", line)
                if m:
                    return m.group(1).strip()
    except OSError:
        pass
    return ""


_GH_STAGE_TYPES = frozenset(
    {
        "materialize-to-branch",
        "commit",
        "open-github-pr",
        "request-copilot",
        "wait-copilot",
        "ghaddress",
        "ghreview",
        "wait-ci",
    }
)


def _pipeline_mode(pipeline: Pipeline) -> str:
    if pipeline.stages and pipeline.stages[0].type == "chain":
        return "boss"
    if pipeline.name == "gh" or any(s.type in _GH_STAGE_TYPES for s in pipeline.stages):
        return "gh"
    return "local"


def _infer_mode_from_pipeline_kind(kind: str) -> str:
    _map = {
        "localgremlin": "local",
        "ghgremlin": "gh",
        "bossgremlin": "boss",
        "local": "local",
        "gh": "gh",
        "boss": "boss",
    }
    return _map.get(kind, "local")


def _gh_current_repo() -> str:
    """Return ``owner/name`` of the current repo via ``gh repo view``, or ''.

    Best-effort: returns '' on any error so callers can fall back.
    """
    try:
        r = subprocess.run(
            ["gh", "repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"],
            capture_output=True,
            text=True,
            check=False,
            timeout=10,
        )
    except (subprocess.SubprocessError, OSError):
        return ""
    if r.returncode != 0:
        return ""
    return r.stdout.strip()


def _fetch_issue(plan: str) -> dict[str, Any] | None:
    """Resolve an issue-ref --plan arg to its ``gh issue view`` JSON dict.

    Returns ``None`` when ``plan`` doesn't parse as an issue ref or any gh call
    fails. Single source of truth for issue lookups during launch — callers
    should pass the result around rather than re-fetching.
    """
    try:
        target_repo, issue_ref = parse_issue_ref(plan, "")
    except Exception:
        return None
    if issue_ref is None:
        return None
    if not target_repo:
        target_repo = _gh_current_repo()
    if not target_repo:
        return None
    try:
        return view_issue(issue_ref, target_repo)
    except Exception:
        return None


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
        # Non-file plan arg (issue ref); try to fetch the issue title for a
        # meaningful slug. Best-effort: fall back to slug-from-ref on any error.
        title = issue_title
        if not title:
            data = _fetch_issue(plan)
            if data:
                title = str(data.get("title") or "")
        if title:
            slug = _slugify(title) or "gremlin"
            return title[:60], False, slug
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


def _resolve_pipeline(
    kind: str, pipeline_args: tuple[str, ...], project_root: str
) -> tuple[list[str], str]:
    args = list(pipeline_args)
    pipeline_val: str | None = None
    filtered: list[str] = []
    i = 0
    while i < len(args):
        if args[i] == "--pipeline":
            if i + 1 < len(args):
                pipeline_val = args[i + 1]
                i += 2
            else:
                i += 1  # drop dangling flag
        elif args[i].startswith("--pipeline="):
            pipeline_val = args[i][len("--pipeline=") :]
            i += 1
        else:
            filtered.append(args[i])
            i += 1
    name = pipeline_val or kind
    resolved = str(resolve_pipeline_path(name, pathlib.Path(project_root)))
    return filtered, resolved


def _extract_client_spec(pipeline_args: list[str]) -> str:
    return _extract_arg_value(pipeline_args, "--client")


def _extract_arg_value(args: list[str], flag: str) -> str:
    value = ""
    i = 0
    while i < len(args):
        arg = args[i]
        if arg == flag:
            if i + 1 < len(args):
                value = args[i + 1]
                i += 2
                continue
            i += 1
            continue
        prefix = f"{flag}="
        if arg.startswith(prefix):
            value = arg[len(prefix) :]
        i += 1
    return value


def _pipeline_default_client_spec(pipeline_path: str) -> str:
    if not pipeline_path:
        return ""
    try:
        pipeline = load_pipeline(pathlib.Path(pipeline_path))
    except (FileNotFoundError, ValueError, yaml.YAMLError):
        return ""
    return str(pipeline.default_client) if pipeline.default_client else ""


def _launch_client_label(pipeline_args: list[str], pipeline_path: str) -> str:
    client_spec_str = _extract_client_spec(pipeline_args)
    if client_spec_str:
        return client_spec_str
    pipeline_default_str = _pipeline_default_client_spec(pipeline_path)
    if pipeline_default_str:
        return pipeline_default_str
    return str(PACKAGE_DEFAULT)


def _persisted_client_label(state: dict[str, Any]) -> str:
    client = str(state.get("client") or "")
    if client:
        return client

    stage_clients = state.get("stage_clients")
    if isinstance(stage_clients, dict):
        stored_stage_clients = cast(dict[object, object], stage_clients)
        stage = str(state.get("stage") or "")
        if stage:
            label = str(stored_stage_clients.get(stage) or "")
            if label:
                return label
        for client_spec in stored_stage_clients.values():
            label = str(client_spec or "")
            if label:
                return label

    return str(PACKAGE_DEFAULT)


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


def _stage_gremlins_overlay(project_root: str, state_dir: pathlib.Path) -> None:
    src = pathlib.Path(project_root) / ".gremlins"
    if src.is_dir():
        shutil.copytree(src, state_dir / ".gremlins", dirs_exist_ok=True)


def _setup_workdir(
    mode: str, project_root: str, base_ref_sha: str, gr_id: str, state_dir: pathlib.Path
) -> tuple[str, str, str, str]:
    """Return (workdir, branch, worktree_base, setup_kind)."""
    if not _git_mod.in_git_repo(cwd=project_root):
        return _git_mod.setup_copy(project_root), "", "", "copy"

    if mode == "local":
        workdir, branch = _git_mod.setup_worktree_branch(
            project_root, gr_id, base_ref=base_ref_sha or "HEAD"
        )
        _stage_gremlins_overlay(project_root, state_dir)
        return workdir, branch, "", "worktree-branch"

    if mode == "gh":
        workdir = _git_mod.setup_detached_worktree(project_root, base_ref_sha or "HEAD")
        _stage_gremlins_overlay(project_root, state_dir)
        return workdir, "", base_ref_sha, "worktree"

    # boss mode: detached worktree off base_ref_sha
    workdir = _git_mod.setup_detached_worktree(project_root, base_ref_sha or "HEAD")
    _stage_gremlins_overlay(project_root, state_dir)
    return workdir, "", "", "worktree"


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
        issue_data = _fetch_issue(plan)

    desc, desc_explicit, slug = _resolve_description_and_slug(
        instructions,
        plan,
        description,
        issue_title=str((issue_data or {}).get("title") or ""),
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

    resolved_pipeline_args, pipeline_path = _resolve_pipeline(
        kind, pipeline_args, project_root
    )

    _pipeline_base_ref = "current"
    try:
        _loaded_pipeline = load_pipeline(pathlib.Path(pipeline_path))
        pipeline_mode = _pipeline_mode(_loaded_pipeline)
        _pipeline_base_ref = _loaded_pipeline.base_ref
    except (FileNotFoundError, OSError):
        pipeline_mode = _infer_mode_from_pipeline_kind(kind)

    if pipeline_mode == "gh" and shutil.which("gh") is None:
        raise RuntimeError("gh CLI not found on PATH (required for gh pipeline)")

    state_dir = _state_root() / gr_id
    state_dir.mkdir(parents=True, exist_ok=True)

    _effective_base_ref = base_ref if base_ref is not None else _pipeline_base_ref
    if _git_mod.in_git_repo(cwd=project_root):
        if pipeline_mode == "gh":
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

        workdir, branch, worktree_base, setup_kind = _setup_workdir(
            pipeline_mode, project_root, base_ref_sha, gr_id, state_dir
        )

        now_iso = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
        state = {
            "id": gr_id,
            "kind": kind,
            "pipeline_kind": pipeline_mode,
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
            "client": _launch_client_label(stored_pipeline_args, pipeline_path),
            "pipeline_path": pipeline_path,
            "stage": "starting",
            "pid": None,
            "stage_inputs": stage_inputs,
            "base_ref_name": base_ref_name,
            "base_ref_sha": base_ref_sha,
        }
        _write_state(state_dir, state)

        # Build args for the spawned _run-pipeline process
        spawn_args = list(stored_pipeline_args)
        if instructions:
            spawn_args.append(instructions)

        proc = _spawn_pipeline(state_dir, workdir, gr_id, pipeline_path, spawn_args)
    except Exception:
        shutil.rmtree(state_dir, ignore_errors=True)
        if workdir:
            try:
                _git_mod.remove_worktree(project_root, workdir)
            except Exception:
                pass
        raise

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
    try:
        pipeline_args, pipeline_path = _resolve_pipeline(
            kind, tuple(pipeline_args), project_root
        )
    except FileNotFoundError:
        pass

    from gremlins.fleet.state import effective_pipeline_kind

    pipeline_mode = effective_pipeline_kind(state)
    if pipeline_path:
        try:
            _loaded = load_pipeline(pathlib.Path(pipeline_path))
            pipeline_mode = _pipeline_mode(_loaded)
        except (FileNotFoundError, OSError):
            pass

    if pipeline_mode == "gh" and shutil.which("gh") is None:
        raise RuntimeError("gh CLI not found on PATH (required for gh pipeline)")

    # Rewind stage if it never advanced past "starting"
    if not stage or stage == "starting":
        stage = "plan"

    if pipeline_mode == "boss" and stage not in ("review-chain", "address-chain"):
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
        pipeline_args=pipeline_args,
        pipeline_path=pipeline_path,
        client=_persisted_client_label(state),
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
    _patch_state(state_dir, pid=proc.pid)


def write_terminal_state(gr_id: str, exit_code: int) -> None:
    """Record terminal outcome for a finished pipeline run.

    Called by the _run-pipeline subcommand's finally block. Mirrors finish.sh:
    touches the finished marker, patches state.json, and on success removes
    the worktree (except boss pipeline). Best-effort throughout.
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

    if exit_code == 0:
        try:
            data = json.loads(sf.read_text(encoding="utf-8"))
            project_root = data.get("project_root", "")
            workdir = data.get("workdir", "")
            setup_kind = data.get("setup_kind", "")
            if (
                setup_kind in ("worktree", "worktree-branch")
                and project_root
                and workdir
            ):
                _git_mod.remove_worktree(project_root, workdir)
        except Exception:
            pass
