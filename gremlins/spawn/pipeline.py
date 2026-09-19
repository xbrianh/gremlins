"""Internal spawn boundary: run a pipeline by path and record terminal state.

Usage: python -m gremlins.spawn.pipeline <gremlin_id> <pipeline_path> [args...]

Not intended for direct human invocation.
"""

from __future__ import annotations

import argparse
import asyncio
import atexit
import datetime
import json
import logging
import os
import pathlib
import secrets
import shutil
import signal
import sys
import traceback
import types
from collections.abc import Callable
from typing import Any

from _gremlins_core import Gremlin as PyGremlin
from _gremlins_core.clients import Client
from _gremlins_core.config import (
    project_root,
    scratch_root,
    state_root,
)
from _gremlins_core.executor import StateData
from _gremlins_core.utils.env_file import source_env_string
from _gremlins_core.utils.git import in_git_repo

from gremlins.errors import die
from gremlins.launcher import validate_gremlin_id
from gremlins.logging_setup import configure_logging
from gremlins.utils.git import stage_gremlins_overlay

logger = logging.getLogger(__name__)

_HANDLED_SIGS = tuple(
    getattr(signal, name)
    for name in ("SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT")
    if hasattr(signal, name)
)
_atexit_log_fn: Callable[[], None] | None = None


def _install_signal_handlers(gremlin: PyGremlin) -> None:
    """Install signal handlers that flush logs and re-raise the signal.

    No Python clients to reap — the Rust executor owns client lifecycle.
    """
    global _atexit_log_fn

    def handler(signum: int, _frame: types.FrameType | None) -> None:
        logger.warning(
            "received %s",
            signal.Signals(signum).name,
        )
        for h in logging.getLogger().handlers:
            try:
                h.flush()
            except Exception:
                pass
        signal.signal(signum, signal.SIG_DFL)
        os.kill(os.getpid(), signum)

    for sig in _HANDLED_SIGS:
        signal.signal(sig, handler)

    def _atexit_log() -> None:
        logger.warning("exiting via atexit")
        for h in logging.getLogger().handlers:
            try:
                h.flush()
            except Exception:
                pass

    if _atexit_log_fn is not None:
        atexit.unregister(_atexit_log_fn)
    _atexit_log_fn = _atexit_log
    atexit.register(_atexit_log)


def _parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--client", dest="client", default=None)
    parser.add_argument("--resume-from", dest="resume_from", default=None)
    return parser.parse_args(argv)


def _prepend_overlay_bin_to_path(overlay_dir: str) -> None:
    overlay_bin = pathlib.Path(overlay_dir) / "bin"
    if overlay_bin.is_dir():
        existing_path = os.environ.get("PATH", "")
        os.environ["PATH"] = (
            f"{overlay_bin}{os.pathsep}{existing_path}"
            if existing_path
            else str(overlay_bin)
        )


def _read_state_json(gremlin_id: str | None) -> dict[str, Any]:
    sf = pathlib.Path(state_root()) / gremlin_id / "state.json" if gremlin_id else None
    if sf is None or not sf.exists():
        return {}
    try:
        return json.loads(sf.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return {}


def _read_bootstrap_env(pipeline_path: pathlib.Path) -> str:
    """Read bootstrap.env from the raw pipeline YAML.

    We only need the env script string for the pre-launch env isolation block;
    the full pipeline parse (stages, clients, expansion) is done natively in
    Rust's ``Gremlin::launch``.
    """
    try:
        from _gremlins_core.utils.yaml_io import load_yaml_file

        raw = load_yaml_file(str(pipeline_path))
        if isinstance(raw, dict):
            bs = raw.get("bootstrap")
            if isinstance(bs, dict):
                return str(bs.get("env", ""))
    except Exception:
        logger.warning("failed to read bootstrap.env from pipeline YAML", exc_info=True)
    return ""


async def run_pipeline(
    pipeline_path: pathlib.Path,
    *,
    argv: list[str],
    gremlin_id: str | None = None,
    client: Client | None = None,
) -> int:
    """Load pipeline YAML, build Gremlin, run. Sole internal pipeline entry point."""
    configure_logging()
    args = _parse_args(argv)
    resume_from = (
        os.environ.pop("GREMLINS_RESUME_FROM", None) or args.resume_from or None
    )

    _project_root = project_root()

    if shutil.which("git") is None:
        die("git not found on PATH")

    if not in_git_repo():
        die(
            f"gremlins requires a git repository; {project_root()} is not inside a git worktree"
        )

    # --- pre-launch: resolve directories, resume info, and a valid id ---
    if not gremlin_id:
        ts = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        rand = secrets.token_hex(3)
        gremlin_id = f"{ts}-{rand}"
    validate_gremlin_id(gremlin_id)
    state_json = _read_state_json(gremlin_id)
    state_dir = pathlib.Path(state_root()) / gremlin_id
    artifact_dir = pathlib.Path(scratch_root(gremlin_id)) / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    _workdir = str(state_json.get("workdir") or "")
    worktree_dir = pathlib.Path(_workdir) if _workdir else None
    stage_inputs: dict[str, str] = {
        k: v
        for k, v in state_json.get("stage_inputs", {}).items()
        if v is not None and isinstance(v, str)
    }

    # Stage the gremlins overlay so Rust's launch (which also stages it) sees a
    # pre-populated destination — the Rust copy is a no-op when already present.
    stage_gremlins_overlay(str(_project_root), state_dir)

    # --- env isolation ---
    # Must happen *before* PyGremlin.create because the Rust Gremlin captures
    # std::env::vars() at construction time inside resolve_env().
    _bootstrap_env = _read_bootstrap_env(pipeline_path)

    _system = {
        k: v
        for k, v in {
            "GREMLINS_GREMLIN_ID": gremlin_id,
            "GREMLINS_PROJECT_ROOT": str(_project_root),
            "GREMLINS_OVERLAY_DIR": str(state_dir / ".gremlins"),
            "GREMLINS_WORKTREE_PATH": str(worktree_dir) if worktree_dir else None,
            "GREMLINS_ARTIFACT_DIR": str(artifact_dir),
            "GREMLIN_WORKSPACE_DIR": str(worktree_dir) if worktree_dir else None,
            "GREMLIN_STATE_DIR": str(state_dir),
        }.items()
        if v is not None
    }

    env_script = _bootstrap_env.strip()
    if env_script:
        _base = dict(os.environ)
        _base.update(_system)
        try:
            _env = source_env_string(
                env_script, base_env=_base, cwd=pathlib.Path(_project_root)
            )
        except RuntimeError as exc:
            die(str(exc))
    else:
        _env = dict(os.environ)
        _env.update(_system)

    # Clear os.environ entirely, then apply the sourced env followed
    # by system vars. System vars go last so users cannot override them.
    #
    # os.environ is shared process state — direct-call/test paths
    # after this point see the rebuilt env. The autouse
    # _restore_os_environ fixture in tests/conftest.py snapshots and
    # restores os.environ per-test to avoid cross-test contamination.
    # The real gremlin runs in its own subprocess, so the mutation is
    # harmless there.
    os.environ.clear()
    os.environ.update(_env)
    os.environ.update(_system)
    # --- end env isolation ---

    _prepend_overlay_bin_to_path(_system["GREMLINS_OVERLAY_DIR"])

    os.environ["GREMLINS_SCRATCH_DIR"] = str(pathlib.Path(scratch_root(gremlin_id)))

    # --- launch the native gremlin ---
    # PyGremlin.create does: worktree setup, state initialization, artifact
    # registration, env resolution (capturing the isolated os.environ above),
    # and overlay staging — everything the Python Gremlin.initialize_with_runtime
    # used to do plus the inline bootstrap block.
    try:
        gremlin = PyGremlin.create(
            id=gremlin_id,
            pipeline_path=pipeline_path,
            client_override=args.client or None,
            worktree_parent=None,
            resume_from=resume_from,
            stage_inputs=stage_inputs,
            fetch_worktree=False,
            worktree_dir=worktree_dir,
        )
    except Exception as exc:
        die(str(exc))

    _install_signal_handlers(gremlin)
    logger.info("running stages")
    exit_code = await gremlin.run()
    logger.info("done. artifacts in: %s", artifact_dir)
    return exit_code


def write_terminal_state(gremlin_id: str, exit_code: int) -> None:
    """Write terminal state for a completed gremlin."""
    validate_gremlin_id(gremlin_id)
    StateData(gremlin_id).write_terminal_state(exit_code)


def main(argv: list[str] | None = None) -> int:
    from _gremlins_core.config import init as _init_config

    from gremlins.logging_setup import configure_logging

    try:
        _init_config()
    except ValueError as exc:
        sys.stderr.write(f"run_pipeline: failed to load config: {exc}\n")
        return 1

    configure_logging()

    if argv is None:
        argv = sys.argv[1:]
    if len(argv) < 2:
        sys.stderr.write(
            "run_pipeline: usage: <gremlin_id> <pipeline_path> [args...]\n"
        )
        return 1

    gremlin_id, pipeline_arg, *args = argv
    try:
        validate_gremlin_id(gremlin_id)
    except ValueError as exc:
        sys.stderr.write(f"run_pipeline: {exc}\n")
        return 1

    rc = 1
    try:
        rc = asyncio.run(
            run_pipeline(pathlib.Path(pipeline_arg), argv=args, gremlin_id=gremlin_id)
        )
        if rc != 0:
            logger.warning("pipeline finished with exit code %d", rc)
    except SystemExit as e:
        rc = e.code if isinstance(e.code, int) else 1
        logger.info("pipeline exited with code %d", rc)
    except BaseException:
        rc = 1
        logger.exception("pipeline terminated by exception")
        traceback.print_exc()
    finally:
        logger.info("writing terminal state (exit_code=%d)", rc)
        for h in logging.getLogger().handlers:
            try:
                h.flush()
            except Exception:
                pass
        write_terminal_state(gremlin_id, rc)
    sys.exit(rc)


if __name__ == "__main__":
    sys.exit(main())
