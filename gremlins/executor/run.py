"""Unified internal pipeline entry point."""

from __future__ import annotations

import argparse
import atexit
import datetime
import json
import logging
import math
import os
import pathlib
import secrets
import shutil
import signal
import types
from collections.abc import Callable, Sequence
from typing import Any

from _gremlins_core.artifacts import ArtifactRegistry
from _gremlins_core.clients import RustClient as Client
from _gremlins_core.config import (
    project_root,
    scratch_root,
    state_root,
)
from _gremlins_core.stages import Bail

from gremlins.env_file import source_env_string
from gremlins.errors import die
from gremlins.executor.gremlin import Gremlin
from gremlins.logging_setup import configure_logging
from gremlins.protocols import StageProtocol
from gremlins.utils.git import (
    has_commits,
    has_dirty_worktree,
    in_git_repo,
    stage_gremlins_overlay,
)

logger = logging.getLogger(__name__)

_HANDLED_SIGS = tuple(
    getattr(signal, name)
    for name in ("SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT")
    if hasattr(signal, name)
)
_atexit_log_fn: Callable[[], None] | None = None


def _load_stage_attempt(gremlin: Gremlin) -> tuple[str, str]:
    if gremlin.state and gremlin.state.data:
        return gremlin.state.data.stage or "", gremlin.state.data.attempt or ""
    return "", ""


def _install_signal_handlers(clients: Sequence[Client], gremlin: Gremlin) -> None:
    global _atexit_log_fn

    def handler(signum: int, _frame: types.FrameType | None) -> None:  # pyright: ignore[reportUnusedParameter]
        stage, attempt = _load_stage_attempt(gremlin)
        logger.warning(
            "received %s at stage=%s attempt=%s",
            signal.Signals(signum).name,
            stage or "(none)",
            attempt or "(none)",
        )
        for h in logging.getLogger().handlers:
            try:
                h.flush()
            except Exception:
                pass
        for c in clients:
            try:
                c.reap_all()
            except Exception:
                pass
        signal.signal(signum, signal.SIG_DFL)
        os.kill(os.getpid(), signum)

    for sig in _HANDLED_SIGS:
        signal.signal(sig, handler)

    def _atexit_log() -> None:
        stage, attempt = _load_stage_attempt(gremlin)
        if not stage:
            return
        logger.warning(
            "exiting via atexit at stage=%s attempt=%s",
            stage,
            attempt or "(none)",
        )
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


def _unique_clients(stages: Sequence[StageProtocol]) -> list[Client]:
    seen: set[int] = set()
    result: list[Client] = []
    for stage in stages:
        c = stage.client
        if c is not None and id(c) not in seen:
            seen.add(id(c))
            result.append(c)
        body = getattr(stage, "body", [])
        if body:
            for bc in _unique_clients(body):
                if id(bc) not in seen:
                    seen.add(id(bc))
                    result.append(bc)
    return result


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

    state_json = _read_state_json(gremlin_id)
    if gremlin_id:
        state_dir = pathlib.Path(state_root()) / gremlin_id
        artifact_dir = pathlib.Path(scratch_root(gremlin_id)) / "artifacts"
    else:
        ts = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        rand = secrets.token_hex(3)
        artifact_dir = (
            pathlib.Path(scratch_root(gremlin_id)) / f"{ts}-{rand}" / "artifacts"
        )
        state_dir = artifact_dir.parent
    artifact_dir.mkdir(parents=True, exist_ok=True)
    _workdir = str(state_json.get("workdir") or "")
    worktree_dir = pathlib.Path(_workdir) if _workdir else None
    _stored_project_root = str(state_json.get("project_root") or "")
    stage_inputs: dict[str, Any] = dict(state_json.get("stage_inputs") or {})

    # base_ref_sha and base_ref are bound in registry.json at launch time
    try:
        _registry = ArtifactRegistry(artifact_dir=artifact_dir)
        raw_base_sha = (
            _registry.content("artifact://base_sha")
            if _registry.is_registered("artifact://base_sha")
            else ""
        )
        # base_sha may be stored as a raw SHA or a git://commit/<sha> URI
        base_ref_sha = str(raw_base_sha).removeprefix("git://commit/")
        raw_base_ref = (
            _registry.content("artifact://base_ref")
            if _registry.is_registered("artifact://base_ref")
            else ""
        )
        # base_ref may be stored as a raw ref or a git://ref/<name> URI
        base_ref = str(raw_base_ref).removeprefix("git://ref/")
    except Exception:
        logger.warning(
            "failed to read base_sha/base_ref from registry.json", exc_info=True
        )
        base_ref_sha = ""
        base_ref = ""

    fetch_worktree = False

    try:
        gremlin = Gremlin.initialize_with_runtime(
            gremlin_id=gremlin_id,
            state_dir=state_dir,
            project_dir=pathlib.Path(_stored_project_root)
            if _stored_project_root
            else pathlib.Path(project_root()),
            pipeline_ref=str(pipeline_path),
            resume_from=resume_from,
            worktree_dir=worktree_dir,
            project_root=_stored_project_root,
            base_ref_sha=base_ref_sha,
            base_ref=base_ref,
            fetch_worktree=fetch_worktree,
            client_label=args.client or "",
            stage_inputs=stage_inputs,
            client=client,
        )
        gremlin.validate_resume_target()
    except ValueError as exc:
        die(str(exc))

    logger.info("artifact: %s", artifact_dir)
    stage_gremlins_overlay(str(_project_root), state_dir)

    # --- env isolation ---
    # Build the system vars table. These are available during
    # bootstrap.env sourcing and forcibly re-injected afterward.
    _system = {
        k: v
        for k, v in {
            "GREMLINS_GREMLIN_ID": gremlin_id or "",
            "GREMLINS_PROJECT_ROOT": str(_project_root),
            "GREMLINS_OVERLAY_DIR": str(state_dir / ".gremlins"),
            "GREMLINS_WORKTREE_PATH": str(gremlin.worktree_dir)
            if gremlin.worktree_dir
            else None,
            "GREMLINS_ARTIFACT_DIR": str(gremlin.artifact_dir),
            "GREMLIN_WORKSPACE_DIR": str(gremlin.worktree_dir)
            if gremlin.worktree_dir
            else None,
            "GREMLIN_STATE_DIR": str(gremlin.state_dir),
        }.items()
        if v is not None
    }

    # Source bootstrap.env inline. Write to a temp file so bash's `source`
    # builtin works.
    env_script = gremlin.pipeline_data.bootstrap.env.strip()
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

    os.environ["GREMLINS_SCRATCH_DIR"] = str(
        pathlib.Path(scratch_root(gremlin.gremlin_id))
    )

    _bootstrap = gremlin.pipeline_data.bootstrap
    _has_bootstrap = bool(
        _bootstrap.cmds or _bootstrap.launch_cmds or _bootstrap.cli_out
    )
    if gremlin.worktree_dir and not resume_from and _has_bootstrap:
        from gremlins.executor.bootstrap import run_pipeline_bootstrap

        try:
            await run_pipeline_bootstrap(
                _bootstrap,
                cwd=gremlin.worktree_dir,
                stage_inputs=stage_inputs,
                gremlin=gremlin,
                include_launch=True,
            )
        except Exception as exc:
            logger.exception("bootstrap failed")
            if gremlin.state:
                gremlin.state.data.write_bail_file(
                    "other",
                    f"bootstrap failed: {exc}"[:200],
                )
            return 1

    _stage_clients = _unique_clients(gremlin.stages)
    _signal_clients = [client] if client is not None else _stage_clients

    if resume_from:
        _expanded_stage_names = [s.name for s in gremlin.stages]

        def _name_idx(stage_name: str) -> int:
            for i, s in enumerate(gremlin.stages):
                if s.name == stage_name:
                    return i
            return len(gremlin.stages)

        start_idx = (
            _expanded_stage_names.index(resume_from)
            if resume_from in _expanded_stage_names
            else 0
        )
        if start_idx >= _name_idx("review-code"):
            if not has_dirty_worktree() and not has_commits():
                die(
                    f"--resume-from {resume_from} requires implementation changes in the worktree"
                )

    _install_signal_handlers(_signal_clients, gremlin)
    logger.info("running %d stages", len(gremlin.stages))
    try:
        await gremlin.run()
    except Bail as b:
        assert gremlin.state is not None
        gremlin.state.data.write_bail_file("other", b.reason)
        return 1
    except Exception as exc:
        logger.exception("unexpected error during pipeline execution")
        assert gremlin.state is not None
        gremlin.state.data.write_bail_file(
            "other",
            f"unexpected error: {exc}"[:200],
        )
        raise

    total_cost = 0.0
    for c in [client] if client else _stage_clients:
        total_cost += getattr(c, "total_cost_usd", 0.0) or 0.0
    assert gremlin.state is not None
    try:
        subprocess_cost = float(
            gremlin.state.data.read_str("subprocess_cost_usd") or 0.0
        )
    except (ValueError, TypeError):
        subprocess_cost = 0.0
    if math.isfinite(subprocess_cost) and subprocess_cost >= 0:
        total_cost += subprocess_cost
    if total_cost > 0:
        gremlin.state.data.patch(total_cost_usd=total_cost)

    logger.info("done. artifacts in: %s", artifact_dir)
    if total_cost > 0:
        logger.info("total cost: $%.4f", total_cost)

    return 0
