"""Unified internal pipeline entry point."""

from __future__ import annotations

import argparse
import json
import logging
import os
import pathlib
import shutil
import signal
import sys
import types
from collections.abc import Sequence
from typing import Any

from gremlins.clients.client import Client
from gremlins.env_file import load_env_file
from gremlins.errors import die
from gremlins.executor.gremlin import Gremlin
from gremlins.executor.state import (
    StateData,
    resolve_session_dir,
    resolve_state_file,
)
from gremlins.stages.outcome import Bail
from gremlins.logging_setup import configure_logging
from gremlins.stages.base import Stage
from gremlins.utils.git import has_commits, has_dirty_worktree, in_git_repo
from gremlins.utils.github import get_repo

logger = logging.getLogger(__name__)


def _install_signal_handlers(clients: Sequence[Client]) -> None:
    def handler(signum: int, frame: types.FrameType | None) -> None:
        for c in clients:
            try:
                c.reap_all()
            except Exception:
                pass  # best-effort; don't let a broken client block shutdown
        sys.exit(130)  # 128 + SIGINT(2), conventional signal-interrupted exit

    signal.signal(signal.SIGINT, handler)
    signal.signal(signal.SIGTERM, handler)


def _parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--resume-from", dest="resume_from", default=None)
    parser.add_argument("--plan", dest="plan", default=None)
    parser.add_argument("--spec", default=None)
    parser.add_argument("--cmd", dest="cmds", action="append", default=None)
    parser.add_argument(
        "--test-max-attempts", dest="test_max_attempts", type=int, default=3
    )
    parser.add_argument("--client", dest="client", default=None)
    parser.add_argument("instructions", nargs="*")
    args = parser.parse_args(argv)
    if args.resume_from:
        args.instructions = [s for s in args.instructions if s]
    if args.plan and args.instructions:
        die("--plan and positional instructions are mutually exclusive")
    if not args.resume_from and not args.plan and not args.instructions:
        die("one of --plan, --resume-from, or positional instructions is required")
    if args.test_max_attempts <= 0:
        die("--test-max-attempts must be a positive integer")
    if args.cmds is not None:
        for c in args.cmds:
            if not c.strip():
                die("--cmd: command must be a non-empty string")
    return args


def _unique_clients(stages: list[Stage]) -> list[Client]:
    seen: set[int] = set()
    result: list[Client] = []
    for stage in stages:
        c = stage.client
        if c is not None and id(c) not in seen:
            seen.add(id(c))
            result.append(c)
        if stage.body:
            for bc in _unique_clients(stage.body):
                if id(bc) not in seen:
                    seen.add(id(bc))
                    result.append(bc)
    return result


def _read_state_json(gremlin_id: str | None) -> dict[str, Any]:
    sf = resolve_state_file(gremlin_id)
    if sf is None or not sf.exists():
        return {}
    return json.loads(sf.read_text(encoding="utf-8"))


def run_pipeline(
    pipeline_path: pathlib.Path,
    *,
    argv: list[str],
    gremlin_id: str | None = None,
    client: Client | None = None,
) -> int:
    """Load pipeline YAML, build Gremlin, run. Sole internal pipeline entry point."""
    configure_logging()
    args = _parse_args(argv)

    env_file = pathlib.Path(".gremlins/env")
    if env_file.is_file():
        try:
            os.environ.update(load_env_file(env_file))
        except RuntimeError as exc:
            die(str(exc))

    if shutil.which("claude") is None:
        die("claude CLI not found")

    if shutil.which("git") is None:
        die("git not found on PATH")

    if not in_git_repo():
        die(
            f"gremlins requires a git repository; {pathlib.Path.cwd()} is not inside a git worktree"
        )

    state_json = _read_state_json(gremlin_id)
    _workdir = str(state_json.get("workdir") or "")
    worktree_dir = pathlib.Path(_workdir) if _workdir else None
    project_root = str(state_json.get("project_root") or "")
    base_ref_sha = str(state_json.get("base_ref_sha") or "")
    setup_kind = str(state_json.get("setup_kind") or "worktree-branch")
    stage_inputs: dict[str, Any] = dict(state_json.get("stage_inputs") or {})
    instructions: str = str(
        stage_inputs.get("instructions") or " ".join(args.instructions or [])
    )

    session_dir = resolve_session_dir(gremlin_id)
    state_dir = session_dir.parent

    try:
        gremlin = Gremlin.build(
            gremlin_id=gremlin_id,
            state_dir=state_dir,
            session_dir=session_dir,
            project_dir=pathlib.Path(project_root)
            if project_root
            else pathlib.Path.cwd(),
            pipeline_ref=str(pipeline_path),
            instructions=instructions,
            resume_from=args.resume_from,
            spec=args.spec,
            plan=args.plan,
            cmds=args.cmds,
            test_max_attempts=args.test_max_attempts,
            worktree_dir=worktree_dir,
            project_root=project_root,
            base_ref_sha=base_ref_sha,
            setup_kind=setup_kind,
            client_label=args.client or "",
            test_client=client,
        )
        gremlin.validate_resume_target()
    except ValueError as exc:
        die(str(exc))

    gh = gremlin.pipeline_data.needs_gh()
    if gh:
        if shutil.which("gh") is None:
            die("gh CLI not found")
        gremlin.repo = get_repo()
        gremlin.state_file = resolve_state_file(gremlin_id)

    _stage_clients = _unique_clients(gremlin.stages)
    _signal_clients = [client] if client is not None else _stage_clients

    logger.info("session: %s", session_dir)

    gremlin.initialize_runtime()

    if os.environ.get("GREMLINS_TEST_NOOP_PIPELINE"):
        return 0

    plan_file = session_dir / "plan.md"

    if not gh and args.resume_from:
        _expanded_stage_names = [s.name for s in gremlin.stages]

        def _type_idx(stage_type: str) -> int:
            for i, s in enumerate(gremlin.stages):
                if s.type == stage_type:
                    return i
            return len(gremlin.stages)

        start_idx = (
            _expanded_stage_names.index(args.resume_from)
            if args.resume_from in _expanded_stage_names
            else 0
        )
        if start_idx >= _type_idx("implement"):
            if not plan_file.exists() or plan_file.stat().st_size == 0:
                die(f"--resume-from {args.resume_from} requires existing {plan_file}")
        if start_idx >= _type_idx("review-code"):
            if not has_dirty_worktree() and not has_commits():
                die(
                    f"--resume-from {args.resume_from} requires implementation changes in the worktree"
                )

    _install_signal_handlers(_signal_clients)
    try:
        gremlin.run()
    except Bail as b:
        StateData.load(gremlin_id).write_bail_file(
            "other", b.reason, attempt=StateData.load(gremlin_id).attempt
        )
        return 1
    except Exception as exc:
        StateData.load(gremlin_id).write_bail_file(
            "other",
            f"unexpected error: {exc}"[:200],
            attempt=StateData.load(gremlin_id).attempt,
        )
        raise

    total_cost = 0.0
    for c in [client] if client else _stage_clients:
        total_cost += getattr(c, "total_cost_usd", 0.0) or 0.0
    if total_cost > 0:
        StateData.load(gremlin_id).patch(total_cost_usd=total_cost)

    if gh:
        logger.info(
            "done. PR: %s", StateData.load(gremlin_id).read_pr_url() or "(unknown)"
        )
    else:
        logger.info("done. session artifacts in: %s", session_dir)
    if total_cost > 0:
        logger.info("total cost: $%.4f", total_cost)

    return 0
