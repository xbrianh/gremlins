"""Unified internal pipeline entry point."""

from __future__ import annotations

import argparse
import logging
import os
import pathlib
import shutil
import signal
import sys
import types
from collections.abc import Sequence

from gremlins.clients.client import Client
from gremlins.env_file import load_env_file
from gremlins.errors import die
from gremlins.executor.pipeline import Pipeline
from gremlins.executor.state import (
    patch_state,
    read_pr_url,
    resolve_session_dir,
    resolve_state_file,
)
from gremlins.logging_setup import configure_logging
from gremlins.pipeline import Pipeline as _Pipeline
from gremlins.stages.base import Stage
from gremlins.utils.git import has_commits, has_dirty_worktree, in_git_repo
from gremlins.utils.github import get_repo
from gremlins.utils.yaml_io import YamlLoadError

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


def _apply_client_override(stages: list[Stage], cli: Client) -> None:
    for stage in stages:
        stage.client = cli
        if stage.body:
            _apply_client_override(stage.body, cli)


def _propagate_client_models(stages: list[Stage]) -> None:
    for stage in stages:
        if stage.model is None and stage.client is not None:
            stage.model = stage.client.model
        if stage.body:
            _propagate_client_models(stage.body)


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


def run_pipeline(
    pipeline_path: pathlib.Path,
    *,
    argv: list[str],
    gr_id: str | None = None,
    client: Client | None = None,
) -> int:
    """Load pipeline YAML, build Pipeline, run. Sole internal pipeline entry point."""
    configure_logging()
    args = _parse_args(argv)

    cli_spec: Client | None = None
    if args.client:
        try:
            cli_spec = Client.parse(args.client)
        except ValueError as exc:
            die(str(exc))

    if os.environ.get("GREMLINS_TEST_NOOP_PIPELINE"):
        return 0

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

    try:
        pipeline = _Pipeline.from_yaml(pipeline_path)
    except (FileNotFoundError, ValueError, YamlLoadError) as exc:
        die(str(exc))

    gh = pipeline.needs_gh()

    repo = ""
    state_file = resolve_state_file(gr_id)

    if gh:
        if shutil.which("gh") is None:
            die("gh CLI not found")
        repo = get_repo()

    if cli_spec:
        _apply_client_override(list(pipeline.stages), cli_spec)

    _propagate_client_models(list(pipeline.stages))

    session_dir = resolve_session_dir(gr_id)

    _stage_clients = _unique_clients(list(pipeline.stages))
    _signal_clients = [client] if client is not None else _stage_clients

    plan_file = session_dir / "plan.md"

    # For local pipelines, pre-copy the plan file so stages that resume past
    # the plan stage can find plan.md. For gh pipelines, the plan stage itself
    # handles plan_source (may be a file path or an issue ref).
    if not gh and args.plan and not plan_file.exists():
        src = pathlib.Path(args.plan)
        if src.is_file():
            shutil.copyfile(src, plan_file)

    logger.info("session: %s", session_dir)

    try:
        pipe = Pipeline(
            pipeline.stages,
            args=args,
            session_dir=session_dir,
            gr_id=gr_id,
            pipeline_data=pipeline,
            repo=repo,
            state_file=state_file if gh else None,
            test_client=client,
        )
        pipe.validate_resume_target()
    except ValueError as exc:
        die(str(exc))

    if not gh and args.resume_from:
        _expanded_stage_names = [s.name for s in pipe.stages]

        def _type_idx(stage_type: str) -> int:
            for i, s in enumerate(pipe.stages):
                if s.type == stage_type:
                    return i
            return len(pipe.stages)

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
    pipe.run()

    total_cost = 0.0
    for c in [client] if client else _stage_clients:
        total_cost += getattr(c, "total_cost_usd", 0.0) or 0.0
    if total_cost > 0:
        patch_state(gr_id, total_cost_usd=total_cost)

    if gh:
        logger.info("done. PR: %s", read_pr_url(gr_id) or "(unknown)")
    else:
        logger.info("done. session artifacts in: %s", session_dir)
    if total_cost > 0:
        logger.info("total cost: $%.4f", total_cost)

    return 0
